//! macOS flat-package (`.pkg`) builder — product archives on any OS.
//!
//! [`build`] writes the same shape `productbuild` emits: a xar archive
//! holding a `Distribution` script plus a `<name>.pkg/` component directory
//! with `PackageInfo`, `Bom`, a gzipped odc-cpio `Payload`, and (with
//! [`PkgSpec::scripts`]) a `Scripts` archive in the same format. Every layer
//! was pinned against Apple tooling on macOS 26.5 (pkgbuild/productbuild
//! output dissected byte-by-byte; results verified with `pkgutil
//! --expand-full`, `lsbom`, and `installer`).
//!
//! Lower-level pieces are public for reuse and testing: [`write_xar`]
//! (xar archives with nested directories) and [`write_bom`] (Bill of
//! Materials over a staged tree).
//!
//! Spike outcomes the implementation rests on:
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
//! - **cpio**: the Payload is odc/ASCII cpio (magic `070707`), hand-rolled
//!   here because the format is a 76-byte octal header and pkgbuild's
//!   conventions (sequential inodes, dir nlink = 2 + children, zero pad to
//!   a 512-byte boundary) need exact control no crate offered.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Write},
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
use quick_xml::{
    Writer as XmlWriter,
    events::{BytesDecl, BytesText, Event},
};
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

    #[error("pkg: no files to package")]
    NoFiles,

    #[error("pkg dest {0:?} must be a relative /-separated path without empty or '..' segments")]
    InvalidDest(String),

    #[error("pkg: two files share the dest {0:?}")]
    DuplicateDest(String),

    #[error("pkg dest {0:?} is both a file and a directory of another dest")]
    DestConflict(String),

    #[error("pkg install-location {0:?} must be an absolute path")]
    InvalidInstallLocation(String),

    #[error("pkg scripts {0} must be an existing directory")]
    ScriptsNotDir(PathBuf),

    #[error("pkg script {0} must be executable (installer cannot run it otherwise)")]
    ScriptNotExecutable(PathBuf),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Fixed timestamp used for deterministic output (2000-01-01T00:00:00Z).
const EPOCH_2000: i64 = 946_684_800;

/// SHA-1 digest length; the xar TOC checksum occupies heap `[0, 20)`.
const SHA1_LEN: u64 = 20;

/// A payload file: `src` on disk (already resolved to a real path), `dest`
/// the `/`-separated path relative to the install location.
#[derive(Debug, Clone)]
pub struct FileSpec {
    pub src: PathBuf,
    pub dest: String,
}

/// Everything needed to author one product archive. Deliberately independent
/// of the embala config types so the crate is usable on its own.
#[derive(Debug, Clone)]
pub struct PkgSpec {
    /// Package name; names the artifact and the inner `<name>.pkg` component.
    pub name: String,
    /// Human name: the Distribution `<title>` Installer.app displays.
    pub display_name: String,
    /// Reverse-DNS package identifier (`pkgutil` receipt id).
    pub identifier: String,
    pub version: String,
    /// Absolute path the payload tree installs under (e.g. `/usr/local`).
    pub install_location: String,
    /// Adds `enable_currentUserHome` to the Distribution's `<domains>` so
    /// `installer -target CurrentUserHomeDirectory` is legal (the sudo-free
    /// install path).
    pub enable_user_home: bool,
    pub files: Vec<FileSpec>,
    /// Directory packed as the component's `Scripts` archive, like
    /// `pkgbuild --scripts`: every file ships (helpers included), and a
    /// top-level `preinstall`/`postinstall` is referenced from PackageInfo.
    pub scripts: Option<PathBuf>,
}

/// One node of the payload tree, keyed by `/`-separated relative path.
enum Node {
    Dir,
    File { bytes: Vec<u8>, mode: u32 },
}

/// Build the product-archive `.pkg` described by `spec` at `out` (parent
/// dirs are created). Output is deterministic: fixed timestamps everywhere
/// (xar TOC, cpio, gzip) and sorted payload entries.
pub fn build(spec: &PkgSpec, out: &Path) -> Result<()> {
    validate(spec)?;

    // Payload tree model: relative path -> node, sorted (BTreeMap order
    // matches pkgbuild's cpio/BOM ordering: parents before children).
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
    for file in &spec.files {
        for ancestor in ancestors(&file.dest) {
            nodes.insert(ancestor.to_string(), Node::Dir);
        }
        let bytes = std::fs::read(&file.src)?;
        let mode = if executable(&file.src)? { 0o755 } else { 0o644 };
        nodes.insert(file.dest.clone(), Node::File { bytes, mode });
    }

    // pkg-info bookkeeping. numberOfFiles counts every BOM path including
    // the root ".". installKBytes uses pkgbuild's rule (pinned empirically
    // against eight pkgbuild runs): every file occupies ceil(size/512)
    // 512-byte blocks, every directory except the root occupies one block,
    // and the total is rounded up to whole KiB.
    let mut blocks = 0u64;
    for node in nodes.values() {
        blocks += match node {
            Node::Dir => 1,
            Node::File { bytes, .. } => (bytes.len() as u64).div_ceil(512),
        };
    }
    let number_of_files = 1 + nodes.len() as u32;
    let install_kbytes = blocks.div_ceil(2);

    // BOM over the payload tree (root:wheel, 0:0).
    let bom = bom_from_nodes(&nodes, 0, 0)?;

    let payload = gzip(&payload_cpio(&nodes))?;
    let scripts = spec.scripts.as_deref().map(script_nodes).transpose()?;
    let script_archive = scripts
        .as_ref()
        .map(|s| gzip(&payload_cpio(s)))
        .transpose()?;
    // pkgbuild references only these two names; other files are helpers.
    let hooks: Vec<&str> = ["preinstall", "postinstall"]
        .into_iter()
        .filter(|name| {
            scripts
                .as_ref()
                .is_some_and(|s| matches!(s.get(*name), Some(Node::File { .. })))
        })
        .collect();
    let package_info = package_info_xml(spec, number_of_files, install_kbytes, &hooks)?;
    let distribution = distribution_xml(spec, install_kbytes)?;

    // Product-archive layout, mirroring productbuild: the component
    // directory first (children in Apple's order: Bom, Payload, Scripts,
    // PackageInfo), Distribution last. Payload and Scripts are already
    // gzipped, so they are stored raw (application/octet-stream) exactly as
    // Apple does.
    let component = format!("{}.pkg", spec.name);
    let mut children = vec![
        XarEntry {
            name: "Bom",
            kind: XarEntryKind::File {
                data: &bom,
                mode: 0o644,
                store: false,
            },
        },
        XarEntry {
            name: "Payload",
            kind: XarEntryKind::File {
                data: &payload,
                mode: 0o644,
                store: true,
            },
        },
    ];
    if let Some(script_archive) = &script_archive {
        children.push(XarEntry {
            name: "Scripts",
            kind: XarEntryKind::File {
                data: script_archive,
                mode: 0o644,
                store: true,
            },
        });
    }
    children.push(XarEntry {
        name: "PackageInfo",
        kind: XarEntryKind::File {
            data: &package_info,
            mode: 0o644,
            store: false,
        },
    });
    let entries = [
        XarEntry {
            name: &component,
            kind: XarEntryKind::Directory {
                mode: 0o700,
                children,
            },
        },
        XarEntry {
            name: "Distribution",
            kind: XarEntryKind::File {
                data: &distribution,
                mode: 0o644,
                store: false,
            },
        },
    ];

    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut archive = Vec::new();
    write_xar(&entries, &mut archive)?;
    std::fs::write(out, archive)?;
    Ok(())
}

fn validate(spec: &PkgSpec) -> Result<()> {
    if spec.files.is_empty() {
        return Err(Error::NoFiles);
    }
    if !spec.install_location.starts_with('/') {
        return Err(Error::InvalidInstallLocation(spec.install_location.clone()));
    }
    let mut seen = BTreeSet::new();
    for file in &spec.files {
        if file.dest.split('/').any(|s| s.is_empty() || s == "..") {
            return Err(Error::InvalidDest(file.dest.clone()));
        }
        if !seen.insert(file.dest.as_str()) {
            return Err(Error::DuplicateDest(file.dest.clone()));
        }
    }
    let dirs: BTreeSet<&str> = spec.files.iter().flat_map(|f| ancestors(&f.dest)).collect();
    for file in &spec.files {
        if dirs.contains(file.dest.as_str()) {
            return Err(Error::DestConflict(file.dest.clone()));
        }
    }
    Ok(())
}

/// Load a `--scripts` directory as a cpio tree. Like pkgbuild, every entry
/// ships (helper files and subdirectories too), keeping its exec bit. Unlike
/// pkgbuild, which archives a non-executable hook that `installer` then fails
/// to run, a non-executable `preinstall`/`postinstall` is rejected here.
fn script_nodes(dir: &Path) -> Result<BTreeMap<String, Node>> {
    if !dir.is_dir() {
        return Err(Error::ScriptsNotDir(dir.to_path_buf()));
    }
    let mut nodes = BTreeMap::new();
    collect_nodes(dir, "", &mut nodes)?;
    for hook in ["preinstall", "postinstall"] {
        if let Some(Node::File { mode, .. }) = nodes.get(hook) {
            if mode & 0o111 == 0 {
                return Err(Error::ScriptNotExecutable(dir.join(hook)));
            }
        }
    }
    Ok(nodes)
}

fn collect_nodes(dir: &Path, prefix: &str, nodes: &mut BTreeMap<String, Node>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        // Follows symlinks: a linked script ships as the file it points to.
        let path = entry.path();
        if std::fs::metadata(&path)?.is_dir() {
            nodes.insert(rel.clone(), Node::Dir);
            collect_nodes(&path, &rel, nodes)?;
        } else {
            let mode = if executable(&path)? { 0o755 } else { 0o644 };
            let bytes = std::fs::read(&path)?;
            nodes.insert(rel, Node::File { bytes, mode });
        }
    }
    Ok(())
}

/// Proper ancestors of a `/`-separated relative path: `"a/b/c"` -> `a`, `a/b`.
fn ancestors(dest: &str) -> impl Iterator<Item = &str> {
    dest.char_indices()
        .filter(|(_, c)| *c == '/')
        .map(|(i, _)| &dest[..i])
}

/// Does this payload file install as `0o755` rather than `0o644`?
///
/// The source's own exec bit is authoritative wherever the host records one.
#[cfg(unix)]
fn executable(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o111 != 0)
}

/// Windows has no exec bit to read, so fall back to sniffing the content for
/// the two things a macOS payload actually needs `+x` on: a Mach-O image
/// (thin or universal) and a `#!` script.
///
/// SHORTCUT: this is a heuristic, and it makes the mode host-dependent — a
/// non-Mach-O, non-script file that is `chmod +x` on Unix installs as `0o644`
/// when the same config is built on Windows. The upgrade path is an explicit
/// per-file `mode` (or `executable`) key in the `[[pkg.files]]` config, which
/// would make the mode host-independent on every platform and let this
/// function go away.
#[cfg(windows)]
fn executable(path: &Path) -> Result<bool> {
    use std::io::Read;

    let mut head = [0u8; 4];
    let mut fh = std::fs::File::open(path)?;
    let mut filled = 0;
    while filled < head.len() {
        match fh.read(&mut head[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(looks_executable(&head[..filled]))
}

/// The sniff itself, kept host-independent so it stays under test on hosts
/// that never call it.
#[cfg(any(windows, test))]
fn looks_executable(head: &[u8]) -> bool {
    if head.starts_with(b"#!") {
        return true;
    }
    let Some(magic) = head.get(..4) else {
        return false;
    };
    matches!(
        u32::from_be_bytes(magic.try_into().expect("4 bytes")),
        // Mach-O thin, 32- and 64-bit, both endiannesses, plus the universal
        // ("fat") wrappers — including the 0xcafebabe form Java class files
        // share, which is why a `.class` payload would false-positive here.
        0xfeed_face | 0xcefa_edfe | 0xfeed_facf | 0xcffa_edfe | 0xcafe_babe | 0xbeba_feca
    )
}

// ---------------------------------------------------------------------------
// Payload: gzipped odc/ASCII cpio, matching pkgbuild's conventions.

/// Serialize the payload tree as odc cpio the way pkgbuild does: entries
/// `.`/`./path` in sorted order, ownership 0:0, sequential inode numbers,
/// directory nlink = 2 + direct children, fixed mtime, `TRAILER!!!`
/// terminator (mtime 0), zero padding to a 512-byte boundary.
fn payload_cpio(nodes: &BTreeMap<String, Node>) -> Vec<u8> {
    // Direct-child counts drive the directory nlink values ("" = root).
    let mut child_count: BTreeMap<&str, u32> = BTreeMap::new();
    for path in nodes.keys() {
        let parent = match path.rfind('/') {
            Some(i) => &path[..i],
            None => "",
        };
        *child_count.entry(parent).or_insert(0) += 1;
    }
    let children = |path: &str| child_count.get(path).copied().unwrap_or(0);

    let mtime = EPOCH_2000 as u64;
    let mut out = Vec::new();
    let mut inode = 0;
    odc_record(&mut out, ".", 0o40755, 2 + children(""), inode, mtime, &[]);
    for (path, node) in nodes {
        inode += 1;
        let name = format!("./{path}");
        match node {
            Node::Dir => {
                let nlink = 2 + children(path.as_str());
                odc_record(&mut out, &name, 0o40755, nlink, inode, mtime, &[]);
            }
            Node::File { bytes, mode } => {
                odc_record(&mut out, &name, 0o100000 | mode, 1, inode, mtime, bytes);
            }
        }
    }
    odc_record(&mut out, "TRAILER!!!", 0, 1, inode + 1, 0, &[]);
    let padded = out.len().div_ceil(512) * 512;
    out.resize(padded, 0);
    out
}

/// One odc record: 76-byte octal ASCII header, NUL-terminated name, data.
fn odc_record(
    out: &mut Vec<u8>,
    name: &str,
    mode: u32,
    nlink: u32,
    inode: u32,
    mtime: u64,
    data: &[u8],
) {
    let header = format!(
        "070707{dev:06o}{inode:06o}{mode:06o}{uid:06o}{gid:06o}{nlink:06o}{rdev:06o}\
         {mtime:011o}{namesize:06o}{filesize:011o}",
        dev = 0,
        uid = 0,
        gid = 0,
        rdev = 0,
        namesize = name.len() + 1,
        filesize = data.len(),
    );
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.extend_from_slice(data);
}

/// gzip with a pinned zero mtime and no name, for byte-stable output.
fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    encoder.finish()
}

// ---------------------------------------------------------------------------
// PackageInfo and Distribution (quick-xml writer; shapes mirror the exact
// output of pkgbuild/productbuild on macOS 26.5).

type XmlResult = std::result::Result<Vec<u8>, std::io::Error>;

fn xml_writer() -> std::result::Result<XmlWriter<Cursor<Vec<u8>>>, std::io::Error> {
    let mut writer = XmlWriter::new_with_indent(Cursor::new(Vec::new()), b' ', 4);
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("utf-8"), None)))?;
    Ok(writer)
}

fn package_info_xml(
    spec: &PkgSpec,
    number_of_files: u32,
    install_kbytes: u64,
    hooks: &[&str],
) -> XmlResult {
    let mut writer = xml_writer()?;
    writer
        .create_element("pkg-info")
        .with_attribute(("overwrite-permissions", "true"))
        .with_attribute(("relocatable", "false"))
        .with_attribute(("identifier", spec.identifier.as_str()))
        .with_attribute(("postinstall-action", "none"))
        .with_attribute(("version", spec.version.as_str()))
        .with_attribute(("format-version", "2"))
        .with_attribute(("install-location", spec.install_location.as_str()))
        .with_attribute(("auth", "root"))
        .write_inner_content(|w| {
            w.create_element("payload")
                .with_attribute(("numberOfFiles", number_of_files.to_string().as_str()))
                .with_attribute(("installKBytes", install_kbytes.to_string().as_str()))
                .write_empty()?;
            for tag in [
                "bundle-version",
                "upgrade-bundle",
                "update-bundle",
                "atomic-update-bundle",
                "strict-identifier",
                "relocate",
            ] {
                w.create_element(tag).write_empty()?;
            }
            // pkgbuild's shape: `<scripts>` only when a hook exists, each
            // with its default 600 s timeout.
            if !hooks.is_empty() {
                w.create_element("scripts").write_inner_content(|w| {
                    for hook in hooks {
                        w.create_element(*hook)
                            .with_attribute(("file", format!("./{hook}").as_str()))
                            .with_attribute(("timeout", "600"))
                            .write_empty()?;
                    }
                    Ok::<(), std::io::Error>(())
                })?;
            }
            Ok::<(), std::io::Error>(())
        })?;
    Ok(writer.into_inner().into_inner())
}

fn distribution_xml(spec: &PkgSpec, install_kbytes: u64) -> XmlResult {
    let id = spec.identifier.as_str();
    let mut writer = xml_writer()?;
    writer
        .create_element("installer-gui-script")
        .with_attribute(("minSpecVersion", "2"))
        .write_inner_content(|w| {
            w.create_element("title")
                .write_text_content(BytesText::new(&spec.display_name))?;
            let domains = w
                .create_element("domains")
                .with_attribute(("enable_localSystem", "true"));
            if spec.enable_user_home {
                domains
                    .with_attribute(("enable_currentUserHome", "true"))
                    .write_empty()?;
            } else {
                domains.write_empty()?;
            }
            // Without hostArchitectures Installer assumes the payload is
            // x86_64-only and demands Rosetta on Apple Silicon; productbuild
            // emits exactly this options line.
            w.create_element("options")
                .with_attribute(("customize", "never"))
                .with_attribute(("require-scripts", "false"))
                .with_attribute(("hostArchitectures", "x86_64,arm64"))
                .write_empty()?;
            w.create_element("choices-outline")
                .write_inner_content(|w| {
                    w.create_element("line")
                        .with_attribute(("choice", "default"))
                        .write_inner_content(|w| {
                            w.create_element("line")
                                .with_attribute(("choice", id))
                                .write_empty()?;
                            Ok::<(), std::io::Error>(())
                        })?;
                    Ok::<(), std::io::Error>(())
                })?;
            w.create_element("choice")
                .with_attribute(("id", "default"))
                .write_empty()?;
            w.create_element("choice")
                .with_attribute(("id", id))
                .with_attribute(("visible", "false"))
                .write_inner_content(|w| {
                    w.create_element("pkg-ref")
                        .with_attribute(("id", id))
                        .write_empty()?;
                    Ok::<(), std::io::Error>(())
                })?;
            w.create_element("pkg-ref")
                .with_attribute(("id", id))
                .with_attribute(("version", spec.version.as_str()))
                .with_attribute(("onConclusion", "none"))
                .with_attribute(("installKBytes", install_kbytes.to_string().as_str()))
                .with_attribute(("updateKBytes", "0"))
                .write_text_content(BytesText::new(&format!("#{}.pkg", spec.name)))?;
            Ok::<(), std::io::Error>(())
        })?;
    Ok(writer.into_inner().into_inner())
}

// ---------------------------------------------------------------------------
// xar writer

/// An entry to store in a xar archive.
pub struct XarEntry<'a> {
    /// Name within the parent (no `/`), e.g. `PackageInfo`.
    pub name: &'a str,
    pub kind: XarEntryKind<'a>,
}

pub enum XarEntryKind<'a> {
    /// Regular file. `mode` holds the permission bits (e.g. `0o755`; only
    /// the low 12 bits are used). With `store` set the data goes into the
    /// heap verbatim (`application/octet-stream`) instead of
    /// zlib-compressed — Apple stores the already-gzipped Payload that way.
    File {
        data: &'a [u8],
        mode: u32,
        store: bool,
    },
    /// Directory holding child entries.
    Directory {
        mode: u32,
        children: Vec<XarEntry<'a>>,
    },
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

/// Write a xar archive holding `entries` (files and directories).
///
/// Layout matches Apple `xar(1)` defaults: SHA-1 TOC checksum at heap
/// offset 0, zlib-compressed file data (`application/x-gzip`) following it
/// in TOC document order. IDs are assigned in pre-order, the way
/// productbuild numbers a product archive.
pub fn write_xar<W: Write>(entries: &[XarEntry<'_>], mut writer: W) -> Result<()> {
    let mut toc = TableOfContents::from_reader(TOC_TEMPLATE.as_bytes())?;

    let mut heap = Vec::new();
    let mut next_id = 1;
    let mut files = Vec::with_capacity(entries.len());
    for entry in entries {
        files.push(toc_file(entry, &mut next_id, &mut heap)?);
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

/// Convert one entry (and, for directories, its subtree) into a TOC file
/// record, appending file data to the heap in document order.
fn toc_file(entry: &XarEntry<'_>, next_id: &mut u64, heap: &mut Vec<u8>) -> Result<XarTocFile> {
    let id = *next_id;
    *next_id += 1;

    let blank = XarTocFile {
        id,
        ctime: None,
        mtime: None,
        atime: None,
        names: vec![entry.name.to_string()],
        file_type: FileType::File,
        mode: None,
        deviceno: None,
        inode: None,
        uid: Some(0),
        gid: Some(0),
        user: None,
        group: None,
        size: None,
        data: None,
        ea: None,
        finder_create_time: None,
        files: vec![],
    };

    match &entry.kind {
        XarEntryKind::File { data, mode, store } => {
            let (archived, encoding) = if *store {
                (data.to_vec(), "application/octet-stream")
            } else {
                (zlib_compress(data)?, "application/x-gzip")
            };
            let file = XarTocFile {
                mode: Some(format!("{:04o}", mode & 0o7777)),
                size: Some(data.len() as u64),
                data: Some(FileData {
                    offset: SHA1_LEN + heap.len() as u64,
                    size: data.len() as u64,
                    length: archived.len() as u64,
                    extracted_checksum: sha1_checksum(data)?,
                    archived_checksum: sha1_checksum(&archived)?,
                    encoding: FileEncoding {
                        style: encoding.to_string(),
                    },
                }),
                ..blank
            };
            heap.extend_from_slice(&archived);
            Ok(file)
        }
        XarEntryKind::Directory { mode, children } => {
            let mut files = Vec::with_capacity(children.len());
            for child in children {
                files.push(toc_file(child, next_id, heap)?);
            }
            Ok(XarTocFile {
                file_type: FileType::Directory,
                mode: Some(format!("{:04o}", mode & 0o7777)),
                files,
                ..blank
            })
        }
    }
}

// ---------------------------------------------------------------------------
// BOM writer

/// Build a BOM over the in-memory payload tree, recording `uid:gid` as the
/// owner of every entry.
///
/// Modes come from the tree itself rather than from a staged copy on disk:
/// the host filesystem cannot round-trip them (Windows has no mode bits) and
/// its path separators are not BOM path separators.
fn bom_from_nodes(nodes: &BTreeMap<String, Node>, uid: u32, gid: u32) -> Result<Vec<u8>> {
    let mtime = chrono::DateTime::from_timestamp(EPOCH_2000, 0).expect("fixed timestamp is valid");

    let mut builder = bom_builder::BomBuilder::default();
    builder.default_user_id(uid);
    builder.default_group_id(gid);
    builder.default_mtime(mtime);

    // BTreeMap order already matches the BOM's sorted-path convention;
    // directories are derived from file paths by the builder.
    for (path, node) in nodes {
        let Node::File { bytes, mode } = node else {
            continue;
        };
        let entry = builder.add_file_from_bytes(path, bytes)?;
        entry.set_file_mode((0o100000 | (mode & 0o7777)) as u16);
        entry.set_modified_time(mtime);
    }

    builder.build_bom()
}

/// Build a BOM over the file tree rooted at `root`, recording `uid:gid` as
/// the owner of every entry (pkgbuild's convention is `0:0`, root:wheel).
///
/// Files record their on-disk permission bits (plus `S_IFREG`), size, and
/// CRC32; directories are derived from file paths and recorded as `40755`.
/// Output is deterministic: a fixed mtime is used and entries are sorted.
///
/// Unix-only: it reads mode bits off the filesystem, which is exactly what
/// makes it a faithful oracle against `mkbom` and exactly what no other host
/// can supply. [`build`] uses [`bom_from_nodes`] instead.
#[cfg(unix)]
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
#[cfg(unix)]
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

#[cfg(test)]
mod tests {
    use super::looks_executable;

    /// The mode a Windows-hosted build assigns is decided entirely by this
    /// sniff — on a host with no exec bit to read, a miss here ships a
    /// `.pkg` whose binary installs non-executable.
    #[test]
    fn sniff_accepts_mach_o_and_scripts_only() {
        // Mach-O 64-bit little-endian (the shape every arm64 binary has).
        assert!(looks_executable(&[0xcf, 0xfa, 0xed, 0xfe]));
        // Universal binary.
        assert!(looks_executable(&[0xca, 0xfe, 0xba, 0xbe]));
        assert!(looks_executable(b"#!/bin/sh\necho hi\n"));

        assert!(!looks_executable(b"read me\n"));
        assert!(!looks_executable(b"#"), "a lone '#' is not a shebang");
        assert!(!looks_executable(b""));
    }
}
