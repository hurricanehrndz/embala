//! BOM assembly, ported from apple-bom 0.3.0's `builder.rs`
//! (Apache-2.0 OR MIT, © Gregory Szorc).
//!
//! Why a port instead of `apple_bom::builder::BomBuilder`: the released
//! 0.3.0 writer was never exercised end-to-end (its `build_bom()` panics
//! unconditionally on an embedded-NUL `CString`), and once past that it
//! diverges from what Apple's `lsbom`/`mkbom` produce and accept in several
//! ways. Every divergence below was found by diffing against `mkbom` /
//! `pkgbuild --root` output for the same tree and validating with `lsbom`
//! on macOS 26; each fix is marked with a `PORT:` comment at the site:
//!
//! - `CString::new` fed NUL-terminated bytes → unconditional panic (fixed
//!   upstream post-0.3.0, never released);
//! - the root `.` record hardcoded mode 0, uid 0, gid 0 (mkbom/pkgbuild
//!   record a real directory: 40755 + chosen ownership);
//! - checksums used IEEE CRC-32; BOMs store the POSIX `cksum` CRC;
//! - records were emitted depth-first; the Paths B-tree is keyed by
//!   (parent id, name), i.e. breadth-first order, and lsbom truncates the
//!   listing at the first out-of-order key;
//! - name blocks stored the full path instead of the leaf component;
//! - variable names were NUL-terminated with the NUL counted in the length
//!   byte (real BOMs store bare names);
//! - variable block indices pointed one block past their data;
//! - the index region lacked the trailing BOMFreeList, which Apple's
//!   BOMStream requires ("buffer overflow" abort);
//! - file-type path records were 31 bytes; Apple writes 35 (4 trailing
//!   zero bytes) and lsbom reads all 35;
//! - a pointer Paths block was always interposed between tree and leaf;
//!   Apple points the tree directly at a single leaf;
//! - leaf forward/backward links were off by one;
//! - per-record Tree/Paths/PathRecordPointer/TreePointer blocks were
//!   emitted "for parity with Apple tooling"; real BOMs have none;
//! - the header counted the initial NULL block in `number_of_blocks`.
//!
//! Everything else — block layout, variables, unknown-field constants — is
//! kept identical to upstream.

use std::{
    borrow::Cow,
    collections::{BTreeMap, VecDeque},
    ffi::CString,
    io::{Cursor, Read, Write},
    path::Path,
};

use apple_bom::{
    BomPath, BomPathType,
    format::{
        BomBlock, BomBlockBomInfo, BomBlockFile, BomBlockPathInfoIndex, BomBlockPathRecord,
        BomBlockPaths, BomBlockTree, BomBlockVIndex, BomBlocksEntry, BomBlocksIndex, BomHeader,
        BomInfoEntry, BomPathsEntry,
    },
};
use chrono::{DateTime, Utc};
use scroll::IOwrite;

use crate::{Error, Result};

/// POSIX `cksum` CRC (CRC-32/CKSUM: polynomial 0x04C11DB7, unreflected,
/// message length appended, final complement).
///
/// PORT: upstream hashes with `crc32fast` (IEEE CRC-32), but the checksum
/// `mkbom`/`pkgbuild` store — and `lsbom` prints — is the `cksum(1)`
/// algorithm (verified against both oracles: IEEE gives 0xe990f0b2 where
/// Apple tooling records 0x1a06f7cd for the same bytes).
fn cksum_path(path: &Path) -> std::io::Result<(u32, usize)> {
    const fn table() -> [u32; 256] {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut crc = (i as u32) << 24;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04C1_1DB7
                } else {
                    crc << 1
                };
                bit += 1;
            }
            table[i] = crc;
            i += 1;
        }
        table
    }
    const TABLE: [u32; 256] = table();

    let update =
        |crc: u32, byte: u8| -> u32 { (crc << 8) ^ TABLE[(((crc >> 24) as u8) ^ byte) as usize] };

    let mut fh = std::fs::File::open(path)?;
    let mut buffer = [0u8; 32768];
    let mut file_size = 0usize;
    let mut crc = 0u32;

    loop {
        let bytes_read = fh.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        file_size += bytes_read;
        for byte in &buffer[0..bytes_read] {
            crc = update(crc, *byte);
        }
    }

    // Append the message length, least-significant byte first, using the
    // minimum number of bytes.
    let mut length = file_size;
    while length != 0 {
        crc = update(crc, (length & 0xff) as u8);
        length >>= 8;
    }

    Ok((!crc, file_size))
}

/// Serialize the BOM variables index.
///
/// PORT: apple-bom's `BomVar::write` NUL-terminates variable names and
/// counts the NUL in the length byte, but real BOMs (verified against
/// `mkbom` output) store bare names with `length == strlen(name)` —
/// apple-bom's own parser then returns names like `"Paths\0"` and
/// `lsbom`/`ParsedBom` lookups fail. Write the on-disk layout directly.
fn vars_index_to_vec(vars: &[(u32, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend((vars.len() as u32).to_be_bytes());

    for (block_index, name) in vars {
        out.extend(block_index.to_be_bytes());
        out.push(name.len() as u8);
        out.extend(name.as_bytes());
    }

    out
}

fn validate_bom_path(s: &str) -> Result<()> {
    if s.starts_with('.') || s.starts_with('/') || s.contains('\\') {
        Err(Error::BadBomPath(s.to_string()))
    } else {
        Ok(())
    }
}

/// Entity for constructing new BOM data structures.
#[derive(Clone, Debug)]
pub struct BomBuilder {
    /// Paths to materialize.
    ///
    /// Directories are not tracked explicitly. Rather they are derived
    /// at BOM generation time.
    paths: BTreeMap<String, BomPath>,

    default_mtime: DateTime<Utc>,

    default_uid: u32,
    default_gid: u32,

    default_mode_file: u16,
    default_mode_dir: u16,
}

impl Default for BomBuilder {
    fn default() -> Self {
        Self {
            paths: Default::default(),
            default_mtime: Utc::now(),
            default_uid: 0,
            default_gid: 0,
            // PORT: plain octal literals instead of simple-file-manifest
            // constants (upstream's dir default also dropped o+x by OR-ing
            // S_IXGRP twice).
            default_mode_file: 0o100644,
            default_mode_dir: 0o40755,
        }
    }
}

impl BomBuilder {
    // PORT: `BomPath` has no public constructor or `Default` outside
    // apple-bom, so synthesize one through `BomPath::from_record`.
    fn default_file_path(&self) -> Result<BomPath> {
        let record = BomBlockPathRecord {
            path_type: BomPathType::File.into(),
            a: 1,
            architecture: 15,
            mode: self.default_mode_file,
            user: self.default_uid,
            group: self.default_gid,
            mtime: self.default_mtime.timestamp() as u32,
            size: 0,
            b: 1,
            checksum_or_type: 0,
            link_name_length: 0,
            link_name: None,
        };

        Ok(BomPath::from_record(String::new(), &record)?)
    }

    /// Set the default user ID (UID).
    pub fn default_user_id(&mut self, uid: u32) {
        self.default_uid = uid;
    }

    /// Set the default group ID (GID).
    pub fn default_group_id(&mut self, gid: u32) {
        self.default_gid = gid;
    }

    /// Set the default modified time.
    pub fn default_mtime(&mut self, mtime: DateTime<Utc>) {
        self.default_mtime = mtime;
    }

    /// Add a file to this BOM with file content derived from a filesystem path.
    ///
    /// A mutable reference to the just-added entry is returned to allow
    /// for further customization.
    pub fn add_file_from_path(
        &mut self,
        bom_path: impl ToString,
        path: impl AsRef<Path>,
    ) -> Result<&mut BomPath> {
        let bom_path = bom_path.to_string();
        validate_bom_path(&bom_path)?;

        let (cksum, file_size) = cksum_path(path.as_ref())?;

        let mut path = self.default_file_path()?;
        path.set_size(file_size);
        path.set_crc32(Some(cksum));

        self.paths.insert(bom_path.clone(), path);

        Ok(self.paths.get_mut(&bom_path).unwrap())
    }

    fn directory_record(&self) -> BomBlockPathRecord<'static> {
        BomBlockPathRecord {
            path_type: BomPathType::Directory.into(),
            a: 1,
            architecture: 15,
            mode: self.default_mode_dir,
            user: self.default_uid,
            group: self.default_gid,
            mtime: self.default_mtime.timestamp() as u32,
            size: 0,
            b: 1,
            checksum_or_type: 0,
            link_name_length: 0,
            link_name: None,
        }
    }

    /// Serialize the BOM data structure to bytes.
    pub fn build_bom(&self) -> Result<Vec<u8>> {
        let mut records = Vec::with_capacity(self.paths.len() + 1);

        // PORT: upstream hardcodes the root record as mode 0, uid 0, gid 0,
        // mtime 0, architecture 1; mkbom and pkgbuild record the root as a
        // real directory, so give it the same treatment as derived
        // directories.
        records.push((
            1u32,
            self.directory_record(),
            BomBlockFile {
                parent_path_id: 0,
                // PORT: no explicit NUL byte; `CString::new` adds it (this
                // is the upstream post-0.3.0 panic fix).
                name: Cow::from(CString::new(".").expect("valid C string")),
            },
        ));

        // PORT: three fixes to upstream's record emission, all verified
        // against mkbom output:
        // - records are emitted breadth-first: the Paths B-tree is keyed by
        //   (parent path ID, name), and with upstream's depth-first order
        //   lsbom silently truncates the listing at the first out-of-order
        //   key;
        // - the name block stores only the leaf component (upstream stored
        //   the full `./...` path; readers rebuild paths by chaining parent
        //   path IDs);
        // - path IDs are assigned in the same breadth-first order.
        #[derive(Default)]
        struct TreeNode<'p> {
            children: BTreeMap<&'p str, TreeNode<'p>>,
            entry: Option<&'p BomPath>,
        }

        let mut root = TreeNode::default();
        for (index_path, entry) in &self.paths {
            let mut node = &mut root;
            for part in index_path.split('/') {
                node = node.children.entry(part).or_default();
            }
            node.entry = Some(entry);
        }

        let mut next_id = 2u32;
        let mut queue = VecDeque::from([(1u32, root)]);

        while let Some((parent_path_id, node)) = queue.pop_front() {
            for (name, child) in node.children {
                let path_id = next_id;
                next_id += 1;

                let path_record = if let Some(entry) = child.entry {
                    BomBlockPathRecord {
                        path_type: entry.path_type().into(),
                        a: 1,
                        architecture: 15,
                        mode: entry.file_mode(),
                        user: entry.user_id(),
                        group: entry.group_id(),
                        mtime: entry.modified_time().timestamp() as _,
                        size: entry.size() as _,
                        b: 1,
                        checksum_or_type: entry.crc32().unwrap_or(0),
                        link_name_length: if let Some(link_name) = entry.link_name() {
                            link_name.len() as u32 + 1
                        } else {
                            0
                        },
                        link_name: entry.link_name_cstring().map(Cow::from),
                    }
                } else {
                    self.directory_record()
                };

                let file = BomBlockFile {
                    parent_path_id,
                    name: Cow::from(CString::new(name).expect("valid C string")),
                };

                records.push((path_id, path_record, file));
                queue.push_back((path_id, child));
            }
        }

        // We now have all our paths assembled. It is now time to produce the blocks.
        let mut blocks = vec![];
        let mut paths_entries = Vec::with_capacity(records.len());

        // Block at index 0 is the special empty block.
        blocks.push(BomBlock::Empty);

        // By convention, block at index 1 is BomInfo.
        blocks.push(BomBlock::BomInfo(BomBlockBomInfo {
            version: 1,
            // 1 extra record for the null path.
            number_of_paths: records.len() as u32 + 1,
            number_of_info_entries: 3,
            entries: vec![
                // We aren't sure what these values mean. But these are the values
                // written by Apple tooling.
                BomInfoEntry {
                    a: 0,
                    b: 0,
                    c: 8546296,
                    d: 0,
                },
                BomInfoEntry {
                    a: 16777223,
                    b: 0,
                    c: 37959280,
                    d: 0,
                },
                BomInfoEntry {
                    a: 16777228,
                    b: 0,
                    c: 25620800,
                    d: 0,
                },
            ],
        }));

        // PORT: upstream registers each variable with `blocks.len()` *after*
        // pushing the target block, pointing every variable one block past
        // its data. Real BOMs (mkbom: BomInfo→1, Paths→2, HLIndex→4,
        // VIndex→6, Size64→9) use the block's own index; register the
        // pushed block's index instead.
        let mut vars: Vec<(u32, &str)> = vec![(blocks.len() as u32 - 1, "BomInfo")];

        // If we wanted to adhere to the order in Apple's tooling, we would emit
        // data structures referred to by the BOM variables next. But since
        // order doesn't appear to matter, we take the simpler approach and emit
        // them last, after all paths entries.

        for (path_id, path_record, file) in records {
            let path_record_index = blocks.len() as u32;
            blocks.push(BomBlock::PathRecord(path_record));
            let file_index = blocks.len() as u32;
            blocks.push(BomBlock::File(file));
            let path_info_index = blocks.len() as u32;
            blocks.push(BomBlock::PathInfoIndex(BomBlockPathInfoIndex {
                path_id,
                path_record_index,
            }));

            paths_entries.push(BomPathsEntry {
                block_index: path_info_index,
                file_index,
            });
        }

        // PORT: upstream emits Tree + Paths + PathRecordPointer +
        // TreePointer blocks for every path record "for parity with Apple
        // tooling"; real mkbom/pkgbuild BOMs contain no such blocks (the
        // reference BOM's 31 blocks are exactly BomInfo, the four variable
        // trees/leaves, and 3 blocks per path). Omit them.

        // The Paths variable points to a Tree + Paths.
        //
        // The Paths blocks are a bit complicated. The primary Paths is a pointer
        // to another one. And, due to the 4096 block size limit, there can be multiple
        // Paths blocks.
        const PATHS_BLOCK_SIZE: u32 = 4096;

        vars.push((blocks.len() as u32, "Paths"));
        blocks.push(BomBlock::Tree(BomBlockTree {
            block_paths_index: blocks.len() as u32 + 1,
            block_size: PATHS_BLOCK_SIZE,
            path_count: paths_entries.len() as u32,
            ..Default::default()
        }));

        // Determine final set of Paths blocks holding meaningful records.
        let mut paths_blocks = vec![];
        let mut paths_block = BomBlockPaths {
            is_path_info: 1,
            ..Default::default()
        };
        for path_entry in paths_entries {
            paths_block.count += 1;
            paths_block.paths.push(path_entry);

            let remaining_bytes = PATHS_BLOCK_SIZE - 12 - 8 * paths_block.count as u32;

            // Running out of room. Flush block.
            if remaining_bytes < 16 {
                paths_blocks.push(paths_block.clone());
                paths_block = BomBlockPaths {
                    is_path_info: 1,
                    ..Default::default()
                };
            }
        }

        if paths_block.count > 0 || paths_blocks.is_empty() {
            paths_blocks.push(paths_block);
        }

        // PORT: upstream always inserts a non-leaf pointer Paths block
        // between the tree and the leaf; Apple tooling only emits a root
        // pointer when there is more than one leaf (mkbom's tree points
        // directly at the single leaf), and lsbom truncates the listing
        // after one entry when handed the always-pointer shape.
        if paths_blocks.len() > 1 {
            let leaves_start = blocks.len() as u32 + 1;
            blocks.push(BomBlock::Paths(BomBlockPaths {
                is_path_info: 0,
                count: paths_blocks.len() as u16,
                paths: paths_blocks
                    .iter()
                    .enumerate()
                    .map(|(i, paths)| BomPathsEntry {
                        block_index: leaves_start + i as u32,
                        file_index: paths
                            .paths
                            .last()
                            .map(|entry| entry.file_index)
                            .unwrap_or(0),
                    })
                    .collect(),
                ..Default::default()
            }));
        }

        for (i, paths) in paths_blocks.iter().enumerate() {
            // PORT: this leaf lands at index `blocks.len()`, so its
            // neighbors are at len()±1 (upstream pointed forward/backward
            // one block too far).
            blocks.push(BomBlock::Paths(BomBlockPaths {
                is_path_info: paths.is_path_info,
                count: paths.count,
                next_paths_block_index: if i == paths_blocks.len() - 1 {
                    0
                } else {
                    blocks.len() as u32 + 1
                },
                previous_paths_block_index: if i == 0 { 0 } else { blocks.len() as u32 - 1 },
                paths: paths.paths.clone(),
            }));
        }

        // Add records for other variables.

        // HLIndex is Tree + Paths.
        vars.push((blocks.len() as u32, "HLIndex"));
        blocks.push(BomBlock::Tree(BomBlockTree {
            block_paths_index: blocks.len() as u32 + 1,
            block_size: PATHS_BLOCK_SIZE,
            ..Default::default()
        }));
        blocks.push(BomBlock::Paths(BomBlockPaths {
            is_path_info: 1,
            ..Default::default()
        }));

        // VIndex is VIndex + Tree + Paths.
        vars.push((blocks.len() as u32, "VIndex"));
        blocks.push(BomBlock::VIndex(BomBlockVIndex {
            a: 1,
            tree_block_index: blocks.len() as u32 + 1,
            b: 0,
            c: 0,
        }));
        blocks.push(BomBlock::Tree(BomBlockTree {
            block_paths_index: blocks.len() as u32 + 1,
            block_size: PATHS_BLOCK_SIZE,
            ..Default::default()
        }));
        blocks.push(BomBlock::Paths(BomBlockPaths {
            is_path_info: 1,
            ..Default::default()
        }));

        // Size64 is Tree + Paths.
        vars.push((blocks.len() as u32, "Size64"));
        blocks.push(BomBlock::Tree(BomBlockTree {
            block_paths_index: blocks.len() as u32 + 1,
            block_size: PATHS_BLOCK_SIZE,
            ..Default::default()
        }));
        blocks.push(BomBlock::Paths(BomBlockPaths {
            is_path_info: 1,
            ..Default::default()
        }));

        // Now that we've assembled all the blocks as data structures, it is time to write
        // them out.
        //
        // The header contains offsets and sizes of variable length data, which we won't
        // know until we produced it. Furthermore, the blocks index refers to file level
        // offsets. There's kind of a chicken and egg problem here. We side step it by
        // starting blocks data at a fixed file offset, giving plenty of room for the
        // file header.
        const BLOCK_DATA_FILE_OFFSET: u32 = 512;

        let mut blocks_index = BomBlocksIndex::default();
        let mut blocks_writer = Cursor::new(Vec::<u8>::new());

        for block in &blocks {
            let start_offset = blocks_writer.position();
            block.write(&mut blocks_writer)?;

            // PORT: Apple's mkbom writes 35-byte path records for regular
            // files (4 trailing zero bytes after linkNameLength) but
            // 31-byte records for directories. lsbom reads the full 35
            // bytes for file-type records and silently truncates the
            // listing when the block is only 31 bytes long.
            if let BomBlock::PathRecord(record) = block {
                if record.path_type == u8::from(BomPathType::File) {
                    blocks_writer.write_all(&[0u8; 4])?;
                }
            }

            let end_offset = blocks_writer.position();

            blocks_index.count += 1;
            blocks_index.blocks.push(BomBlocksEntry {
                file_offset: BLOCK_DATA_FILE_OFFSET + start_offset as u32,
                length: (end_offset - start_offset) as _,
            });
        }

        let blocks_data = blocks_writer.into_inner();
        let vars_index_data = vars_index_to_vec(&vars);
        let mut blocks_index_data = blocks_index.to_vec()?;

        // PORT: the index region must end with a BOMFreeList — a u32 count
        // followed by (address, length) pointers — and the header's index
        // length includes it. mkbom appends an empty list with two zeroed
        // pointers; Apple's BOMStream reads it unconditionally, and without
        // it `lsbom` aborts with a "buffer overflow" error.
        blocks_index_data.extend([0u8; 20]);

        // The vars index is small. We can put it after the header.
        const VARS_INDEX_OFFSET: u32 = 128;

        // The blocks index goes after the blocks data. We align on 64 byte boundary
        // because why not.
        let blocks_index_offset = BLOCK_DATA_FILE_OFFSET
            + blocks_data.len() as u32
            + (64 - blocks_data.len() % 64) as u32;

        let header = BomHeader {
            magic: *b"BOMStore",
            version: 1,
            // PORT: counts populated blocks only — the initial NULL entry is
            // excluded (upstream counted it).
            number_of_blocks: blocks.len() as u32 - 1,
            blocks_index_offset,
            blocks_index_length: blocks_index_data.len() as _,
            vars_index_offset: VARS_INDEX_OFFSET,
            vars_index_length: vars_index_data.len() as _,
        };

        // We have all the requisite parts. Time to write it.
        let mut writer = Cursor::new(Vec::<u8>::new());
        writer.iowrite_with(header, scroll::BE)?;

        // Pad to vars index.
        for _ in 0..VARS_INDEX_OFFSET - writer.position() as u32 {
            writer.write_all(b"\0")?;
        }

        writer.write_all(&vars_index_data)?;

        // Pad to blocks data.
        for _ in 0..BLOCK_DATA_FILE_OFFSET - writer.position() as u32 {
            writer.write_all(b"\0")?;
        }

        writer.write_all(&blocks_data)?;

        // Pad to blocks index.
        for _ in 0..blocks_index_offset - writer.position() as u32 {
            writer.write_all(b"\0")?;
        }

        writer.write_all(&blocks_index_data)?;

        Ok(writer.into_inner())
    }
}
