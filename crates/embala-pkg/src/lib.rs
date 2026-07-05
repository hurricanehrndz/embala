//! macOS flat-package (`.pkg`) building blocks.
//!
//! Phase 6 spike code: xar archive assembly ([`write_xar`]) and BOM
//! generation ([`write_bom`]). Not yet wired into the CLI — Phase 7 shapes
//! the real API on top of these proven primitives.
//!
//! Spike outcomes (validated against Apple tooling on macOS 26.5):
//!
//! - **xar**: `apple-xar` 0.20 has no turn-key writer, but its primitives
//!   compose: `TableOfContents`/`File`/`FileData` are public with
//!   `to_xml()`, `ChecksumType::digest_data` provides digests, and
//!   `XarHeader` derives scroll `IOwrite`. The only wrinkle is that
//!   `TableOfContents`'s inner field is private with no constructor, so we
//!   bootstrap one by parsing a minimal template TOC via `from_reader` and
//!   mutating it through its `DerefMut<Target = XarToC>` impl. No
//!   hand-rolled writer needed.
//! - **BOM**: `apple-bom` 0.3's shipped `BomBuilder` is unusable — its
//!   `build_bom()` panics unconditionally, and beneath that lie a dozen
//!   more writer bugs (wrong checksum algorithm, wrong B-tree order,
//!   missing free list, ...; see `bom_builder.rs` for the catalog).
//!   Bounded fallback taken: `bom_builder` ports the assembly on top of
//!   apple-bom's public `format` types with those fixes; the output
//!   matches both `mkbom` and pkgbuild's embedded `Bom` under
//!   `lsbom -p fmugsc` (paths, modes, uid, gid, size, cksum). Per-file
//!   modes must include the file-type bits (e.g. `0o100755`).

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use apple_xar::{
    format::XarHeader,
    table_of_contents::{
        Checksum, ChecksumType, File as XarTocFile, FileChecksum, FileData, FileEncoding, FileType,
        TableOfContents,
    },
};
use flate2::{Compression, write::ZlibEncoder};
use scroll::IOwrite;

mod bom_builder;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("xar error: {0}")]
    Xar(#[from] apple_xar::Error),

    #[error("BOM error: {0}")]
    Bom(#[from] apple_bom::Error),

    #[error("invalid BOM path: {0}")]
    BadBomPath(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Fixed timestamp used for deterministic output (2000-01-01T00:00:00Z).
const EPOCH_2000: i64 = 946_684_800;

/// SHA-1 digest length; the xar TOC checksum occupies heap `[0, 20)`.
const SHA1_LEN: u64 = 20;

/// A file to store in a xar archive.
pub struct XarEntry<'a> {
    /// Archive-relative name, e.g. `PackageInfo`.
    pub name: &'a str,
    pub data: &'a [u8],
    /// Permission bits (e.g. `0o755`); only the low 12 bits are used.
    pub mode: u32,
}

/// Minimal TOC used to bootstrap a mutable [`TableOfContents`]: the struct's
/// inner field is private, but `from_reader` + `DerefMut` lets us replace
/// every part of it.
const TOC_TEMPLATE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xar><toc>
  <creation-time>2000-01-01T00:00:00</creation-time>
  <checksum style="sha1"><offset>0</offset><size>20</size></checksum>
  <file id="1"><type>file</type><name>placeholder</name></file>
</toc></xar>"#;

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn zlib_compress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    encoder.finish()
}

fn sha1_checksum(data: &[u8]) -> Result<FileChecksum> {
    Ok(FileChecksum {
        style: ChecksumType::Sha1,
        checksum: hex(&ChecksumType::Sha1.digest_data(data)?),
    })
}

/// Write a xar archive holding `entries` as flat (top-level) files.
///
/// Layout matches Apple `xar(1)` defaults: SHA-1 TOC checksum at heap
/// offset 0, zlib-compressed file data (`application/x-gzip`) following it.
pub fn write_xar<W: Write>(entries: &[XarEntry<'_>], mut writer: W) -> Result<()> {
    let mut toc = TableOfContents::from_reader(TOC_TEMPLATE.as_bytes())?;

    let mut heap = Vec::new();
    let mut files = Vec::with_capacity(entries.len());

    for (index, entry) in entries.iter().enumerate() {
        let compressed = zlib_compress(entry.data)?;

        files.push(XarTocFile {
            id: index as u64 + 1,
            ctime: None,
            mtime: None,
            atime: None,
            names: vec![entry.name.to_string()],
            file_type: FileType::File,
            mode: Some(format!("{:04o}", entry.mode & 0o7777)),
            deviceno: None,
            inode: None,
            uid: Some(0),
            gid: Some(0),
            user: None,
            group: None,
            size: Some(entry.data.len() as u64),
            data: Some(FileData {
                offset: SHA1_LEN + heap.len() as u64,
                size: entry.data.len() as u64,
                length: compressed.len() as u64,
                extracted_checksum: sha1_checksum(entry.data)?,
                archived_checksum: sha1_checksum(&compressed)?,
                encoding: FileEncoding {
                    style: "application/x-gzip".to_string(),
                },
            }),
            ea: None,
            finder_create_time: None,
            files: vec![],
        });

        heap.extend_from_slice(&compressed);
    }

    toc.creation_time = "2000-01-01T00:00:00".to_string();
    toc.checksum = Checksum {
        style: ChecksumType::Sha1,
        offset: 0,
        size: SHA1_LEN,
    };
    toc.files = files;
    toc.signature = None;
    toc.x_signature = None;

    let toc_xml = toc.to_xml()?;
    let toc_compressed = zlib_compress(&toc_xml)?;
    let toc_digest = ChecksumType::Sha1.digest_data(&toc_compressed)?;

    let header = XarHeader {
        magic: 0x7861_7221, // `xar!`
        size: 28,
        version: 1,
        toc_length_compressed: toc_compressed.len() as u64,
        toc_length_uncompressed: toc_xml.len() as u64,
        checksum_algorithm_id: 1, // SHA-1
    };

    writer.iowrite_with(header, scroll::BE)?;
    writer.write_all(&toc_compressed)?;
    writer.write_all(&toc_digest)?;
    writer.write_all(&heap)?;

    Ok(())
}

/// Build a BOM over the file tree rooted at `root`, recording `uid:gid` as
/// the owner of every entry (pkgbuild's convention is `0:0`, root:wheel).
///
/// Files record their on-disk permission bits (plus `S_IFREG`), size, and
/// CRC32; directories are derived from file paths and recorded as `40755`.
/// Output is deterministic: a fixed mtime is used and entries are sorted.
pub fn write_bom(root: &Path, uid: u32, gid: u32) -> Result<Vec<u8>> {
    use std::os::unix::fs::PermissionsExt;

    let mtime = chrono::DateTime::from_timestamp(EPOCH_2000, 0).expect("fixed timestamp is valid");

    let mut builder = bom_builder::BomBuilder::default();
    builder.default_user_id(uid);
    builder.default_group_id(gid);
    builder.default_mtime(mtime);

    let mut files = Vec::new();
    collect_files(root, PathBuf::new(), &mut files)?;
    files.sort();

    for relative in files {
        let full = root.join(&relative);
        let mode = full.metadata()?.permissions().mode();

        let entry = builder.add_file_from_path(relative.to_string_lossy(), &full)?;
        entry.set_file_mode((0o100000 | (mode & 0o7777)) as u16);
        entry.set_modified_time(mtime);
    }

    builder.build_bom()
}

/// Collect regular files under `dir` as paths relative to the walk root.
fn collect_files(dir: &Path, prefix: PathBuf, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let relative = prefix.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            collect_files(&entry.path(), relative, files)?;
        } else if file_type.is_file() {
            files.push(relative);
        }
    }

    Ok(())
}
