//! End-to-end tests for the product-archive builder.
//!
//! These encode what `pkgutil --expand-full` and `installer` verified on
//! macOS 26.5: the archive layout (Distribution + `<name>.pkg/` component),
//! the pkgbuild-shaped PackageInfo/Bom/Payload, and the deterministic
//! output the release pipeline depends on.

use std::collections::BTreeMap;
use std::io::{Cursor, Read as _};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use apple_bom::ParsedBom;
use apple_xar::reader::XarReader;
use embala_pkg::{Error, FileSpec, PkgSpec, build};

const HELLO: &[u8] = b"#!/bin/sh\necho hello\n";
const README: &[u8] = b"read me\n";

fn write_src(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

/// The fixture shape the macOS verification used: one 0755 executable and
/// one 0644 doc file, install location `/usr/local`, user-home installs on.
fn spec(src_dir: &Path) -> PkgSpec {
    PkgSpec {
        name: "hello".to_string(),
        display_name: "Embala Hello".to_string(),
        identifier: "ca.hrndz.embala.hello".to_string(),
        version: "0.1.0".to_string(),
        install_location: "/usr/local".to_string(),
        enable_user_home: true,
        files: vec![
            FileSpec {
                src: write_src(src_dir, "hello.sh", HELLO, 0o755),
                dest: "bin/embala-hello".to_string(),
            },
            FileSpec {
                src: write_src(src_dir, "README", README, 0o644),
                dest: "share/doc/hello/README".to_string(),
            },
        ],
    }
}

fn build_archive(dir: &Path, name: &str) -> Vec<u8> {
    let out = dir.join(name);
    build(&spec(dir), &out).unwrap();
    std::fs::read(out).unwrap()
}

/// Attributes of every `<tag>` element in the document, in document order.
fn element_attrs(xml: &[u8], tag: &str) -> Vec<BTreeMap<String, String>> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(std::str::from_utf8(xml).unwrap());
    let mut found = Vec::new();
    loop {
        match reader.read_event().unwrap() {
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == tag.as_bytes() => {
                found.push(
                    e.attributes()
                        .map(|a| {
                            let a = a.unwrap();
                            (
                                String::from_utf8(a.key.as_ref().to_vec()).unwrap(),
                                String::from_utf8(a.value.to_vec()).unwrap(),
                            )
                        })
                        .collect(),
                );
            }
            Event::Eof => break,
            _ => {}
        }
    }
    found
}

struct CpioEntry {
    name: String,
    mode: u64,
    nlink: u64,
    inode: u64,
    data: Vec<u8>,
}

/// Parse odc/ASCII cpio, asserting the invariants pkgbuild's payloads have:
/// `070707` magic, 0:0 ownership, `TRAILER!!!` terminator, zero padding to
/// a 512-byte boundary.
fn parse_odc(archive: &[u8]) -> Vec<CpioEntry> {
    let mut entries = Vec::new();
    let mut rest = archive;
    loop {
        let header = &rest[..76];
        assert_eq!(&header[..6], b"070707", "odc magic");
        let field = |lo: usize, hi: usize| {
            u64::from_str_radix(std::str::from_utf8(&header[lo..hi]).unwrap(), 8).unwrap()
        };
        assert_eq!(field(6, 12), 0, "dev");
        assert_eq!(field(24, 30), 0, "uid");
        assert_eq!(field(30, 36), 0, "gid");
        assert_eq!(field(42, 48), 0, "rdev");
        let (inode, mode, nlink) = (field(12, 18), field(18, 24), field(36, 42));
        let (name_size, file_size) = (field(59, 65) as usize, field(65, 76) as usize);
        let name = std::str::from_utf8(&rest[76..76 + name_size - 1])
            .unwrap()
            .to_string();
        let data = rest[76 + name_size..76 + name_size + file_size].to_vec();
        rest = &rest[76 + name_size + file_size..];
        let trailer = name == "TRAILER!!!";
        entries.push(CpioEntry {
            name,
            mode,
            nlink,
            inode,
            data,
        });
        if trailer {
            assert!(rest.iter().all(|b| *b == 0), "trailer padding is zeros");
            break;
        }
    }
    entries
}

#[test]
fn product_archive_has_productbuild_layout() {
    let dir = tempfile::tempdir().unwrap();
    let archive = build_archive(dir.path(), "hello-0.1.0.pkg");

    let mut reader = XarReader::new(Cursor::new(archive)).unwrap();
    assert!(reader.verify_table_of_contents_checksum().unwrap());

    // Component directory first with Bom/Payload/PackageInfo children,
    // Distribution last — the exact productbuild entry set.
    let files = reader.files().unwrap();
    let names: Vec<(String, String)> = files
        .iter()
        .map(|(name, file)| (name.clone(), file.file_type.to_string()))
        .collect();
    assert_eq!(
        names,
        vec![
            ("hello.pkg".to_string(), "directory".to_string()),
            ("hello.pkg/Bom".to_string(), "file".to_string()),
            ("hello.pkg/Payload".to_string(), "file".to_string()),
            ("hello.pkg/PackageInfo".to_string(), "file".to_string()),
            ("Distribution".to_string(), "file".to_string()),
        ]
    );
}

#[test]
fn package_info_matches_pkgbuild_shape() {
    let dir = tempfile::tempdir().unwrap();
    let archive = build_archive(dir.path(), "hello-0.1.0.pkg");
    let mut reader = XarReader::new(Cursor::new(archive)).unwrap();
    let xml = reader
        .get_file_data_from_path("hello.pkg/PackageInfo")
        .unwrap()
        .unwrap();

    let pkg_info = &element_attrs(&xml, "pkg-info")[0];
    for (key, value) in [
        ("overwrite-permissions", "true"),
        ("relocatable", "false"),
        ("identifier", "ca.hrndz.embala.hello"),
        ("postinstall-action", "none"),
        ("version", "0.1.0"),
        ("format-version", "2"),
        ("install-location", "/usr/local"),
        ("auth", "root"),
    ] {
        assert_eq!(pkg_info.get(key).map(String::as_str), Some(value), "{key}");
    }

    // 7 BOM paths (".", 4 dirs, 2 files); 2 file blocks + 4 dir blocks
    // of 512 bytes round up to 3 KB — pkgbuild's exact numbers for this
    // tree shape.
    let payload = &element_attrs(&xml, "payload")[0];
    assert_eq!(payload.get("numberOfFiles").unwrap(), "7");
    assert_eq!(payload.get("installKBytes").unwrap(), "3");

    // The empty marker children pkgbuild always writes.
    for tag in [
        "bundle-version",
        "upgrade-bundle",
        "update-bundle",
        "atomic-update-bundle",
        "strict-identifier",
        "relocate",
    ] {
        assert_eq!(element_attrs(&xml, tag).len(), 1, "{tag}");
    }
}

#[test]
fn distribution_enables_user_home_and_references_component() {
    let dir = tempfile::tempdir().unwrap();
    let archive = build_archive(dir.path(), "hello-0.1.0.pkg");
    let mut reader = XarReader::new(Cursor::new(archive)).unwrap();
    let xml = reader
        .get_file_data_from_path("Distribution")
        .unwrap()
        .unwrap();

    let script = &element_attrs(&xml, "installer-gui-script")[0];
    assert_eq!(script.get("minSpecVersion").unwrap(), "2");

    let domains = &element_attrs(&xml, "domains")[0];
    assert_eq!(domains.get("enable_localSystem").unwrap(), "true");
    assert_eq!(domains.get("enable_currentUserHome").unwrap(), "true");

    // Installer treats a missing hostArchitectures as "x86_64 only" and
    // demands Rosetta on Apple Silicon (observed live on macOS 26.5).
    let options = &element_attrs(&xml, "options")[0];
    assert_eq!(options.get("hostArchitectures").unwrap(), "x86_64,arm64");

    // Two pkg-refs: the bare one inside the choice and the full one that
    // points at the component (productbuild's shape).
    let pkg_refs = element_attrs(&xml, "pkg-ref");
    assert_eq!(pkg_refs.len(), 2);
    let full = &pkg_refs[1];
    assert_eq!(full.get("id").unwrap(), "ca.hrndz.embala.hello");
    assert_eq!(full.get("version").unwrap(), "0.1.0");
    assert_eq!(full.get("onConclusion").unwrap(), "none");
    assert_eq!(full.get("installKBytes").unwrap(), "3");
    let text = std::str::from_utf8(&xml).unwrap();
    assert!(text.contains("#hello.pkg</pkg-ref>"), "component reference");

    // Without enable_user_home the domains stay system-only.
    let mut system_only = spec(dir.path());
    system_only.enable_user_home = false;
    let out = dir.path().join("system.pkg");
    build(&system_only, &out).unwrap();
    let mut reader = XarReader::new(Cursor::new(std::fs::read(out).unwrap())).unwrap();
    let xml = reader
        .get_file_data_from_path("Distribution")
        .unwrap()
        .unwrap();
    let domains = &element_attrs(&xml, "domains")[0];
    assert_eq!(domains.get("enable_localSystem").unwrap(), "true");
    assert!(!domains.contains_key("enable_currentUserHome"));
}

#[test]
fn payload_is_pkgbuild_shaped_cpio() {
    let dir = tempfile::tempdir().unwrap();
    let archive = build_archive(dir.path(), "hello-0.1.0.pkg");
    let mut reader = XarReader::new(Cursor::new(archive)).unwrap();
    let gz = reader
        .get_file_data_from_path("hello.pkg/Payload")
        .unwrap()
        .unwrap();

    let mut cpio = Vec::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_end(&mut cpio)
        .unwrap();
    assert_eq!(cpio.len() % 512, 0, "payload padded to 512-byte blocks");

    let entries = parse_odc(&cpio);
    let listed: Vec<(&str, u64, u64)> = entries
        .iter()
        .map(|e| (e.name.as_str(), e.mode, e.nlink))
        .collect();
    assert_eq!(
        listed,
        vec![
            (".", 0o40755, 4),
            ("./bin", 0o40755, 3),
            ("./bin/embala-hello", 0o100755, 1),
            ("./share", 0o40755, 3),
            ("./share/doc", 0o40755, 3),
            ("./share/doc/hello", 0o40755, 3),
            ("./share/doc/hello/README", 0o100644, 1),
            ("TRAILER!!!", 0, 1),
        ]
    );

    // Sequential inodes and intact file contents.
    let inodes: Vec<u64> = entries.iter().map(|e| e.inode).collect();
    assert_eq!(inodes, (0..8).collect::<Vec<u64>>());
    assert_eq!(entries[2].data, HELLO);
    assert_eq!(entries[6].data, README);
}

#[test]
fn bom_payload_and_pkginfo_agree() {
    let dir = tempfile::tempdir().unwrap();
    let archive = build_archive(dir.path(), "hello-0.1.0.pkg");
    let mut reader = XarReader::new(Cursor::new(archive)).unwrap();

    let bom = reader
        .get_file_data_from_path("hello.pkg/Bom")
        .unwrap()
        .unwrap();
    let bom_paths: Vec<String> = ParsedBom::parse(&bom)
        .unwrap()
        .paths()
        .unwrap()
        .into_iter()
        .map(|p| p.path().to_string())
        .collect();

    let gz = reader
        .get_file_data_from_path("hello.pkg/Payload")
        .unwrap()
        .unwrap();
    let mut cpio = Vec::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_end(&mut cpio)
        .unwrap();
    let cpio_paths: Vec<String> = parse_odc(&cpio)
        .iter()
        .map(|e| e.name.clone())
        .filter(|n| n != "TRAILER!!!")
        .collect();

    // Same tree in both (BOM paths come back in B-tree traversal order,
    // so compare sorted), and numberOfFiles counts every BOM path.
    let mut bom_paths = bom_paths;
    let mut cpio_paths = cpio_paths;
    bom_paths.sort();
    cpio_paths.sort();
    assert_eq!(bom_paths, cpio_paths);
    let xml = reader
        .get_file_data_from_path("hello.pkg/PackageInfo")
        .unwrap()
        .unwrap();
    let payload = &element_attrs(&xml, "payload")[0];
    assert_eq!(
        payload.get("numberOfFiles").unwrap(),
        &bom_paths.len().to_string()
    );
}

#[test]
fn builds_are_byte_stable() {
    let dir = tempfile::tempdir().unwrap();
    let first = build_archive(dir.path(), "first.pkg");
    let second = build_archive(dir.path(), "second.pkg");
    assert!(!first.is_empty());
    assert_eq!(first, second);
}

#[test]
fn rejects_bad_specs() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.pkg");

    let mut empty = spec(dir.path());
    empty.files.clear();
    assert!(matches!(build(&empty, &out), Err(Error::NoFiles)));

    let mut relative = spec(dir.path());
    relative.install_location = "usr/local".to_string();
    assert!(matches!(
        build(&relative, &out),
        Err(Error::InvalidInstallLocation(_))
    ));

    for bad in ["../evil", "bin//x", "/abs", "bin/.."] {
        let mut traversal = spec(dir.path());
        traversal.files[0].dest = bad.to_string();
        assert!(
            matches!(build(&traversal, &out), Err(Error::InvalidDest(_))),
            "dest {bad:?}"
        );
    }

    let mut duplicate = spec(dir.path());
    duplicate.files[1].dest = duplicate.files[0].dest.clone();
    assert!(matches!(
        build(&duplicate, &out),
        Err(Error::DuplicateDest(_))
    ));

    let mut conflict = spec(dir.path());
    conflict.files[1].dest = "bin".to_string();
    assert!(matches!(
        build(&conflict, &out),
        Err(Error::DestConflict(_))
    ));

    // Nothing above may leave a partial artifact behind.
    assert!(!out.exists());
}
