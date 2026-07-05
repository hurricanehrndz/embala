//! Round-trip tests for the Phase 6 spike primitives.
//!
//! These encode the spike's promise: archives we write must be readable by
//! an independent xar/BOM implementation with names, sizes, content, and
//! ownership intact — the same properties `xar(1)` and `lsbom(8)` verify
//! on macOS.

use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;

use apple_bom::ParsedBom;
use apple_xar::reader::XarReader;
use embala_pkg::{XarEntry, write_bom, write_xar};

const HELLO: &[u8] = b"#!/bin/sh\necho hi\n";
const README: &[u8] = b"read me\n";

fn spike_entries() -> [XarEntry<'static>; 2] {
    [
        XarEntry {
            name: "embala-hello",
            data: HELLO,
            mode: 0o755,
        },
        XarEntry {
            name: "README",
            data: README,
            mode: 0o644,
        },
    ]
}

#[test]
fn xar_round_trips_through_independent_reader() {
    let mut archive = Vec::new();
    write_xar(&spike_entries(), &mut archive).unwrap();

    let mut reader = XarReader::new(Cursor::new(archive)).unwrap();

    // The TOC digest is what `xar(1)` checks before trusting the archive.
    assert!(reader.verify_table_of_contents_checksum().unwrap());

    // Note: sizes come from the <data> element. apple-xar's File::write_xml
    // does not serialize the top-level <size> element, so `file.size` reads
    // back as None; extraction only needs FileData and is unaffected.
    let files = reader.files().unwrap();
    let listed: Vec<(String, Option<u64>)> = files
        .iter()
        .map(|(name, file)| (name.clone(), file.data.as_ref().map(|d| d.size)))
        .collect();
    assert_eq!(
        listed,
        vec![
            ("embala-hello".to_string(), Some(HELLO.len() as u64)),
            ("README".to_string(), Some(README.len() as u64)),
        ]
    );

    // Extraction must decompress back to the exact original bytes.
    let hello = reader.get_file_data_from_path("embala-hello").unwrap();
    assert_eq!(hello.as_deref(), Some(HELLO));
    let readme = reader.get_file_data_from_path("README").unwrap();
    assert_eq!(readme.as_deref(), Some(README));

    // Modes drive the extracted permissions (pkg scripts must stay 0755).
    let modes: Vec<Option<String>> = files.iter().map(|(_, f)| f.mode.clone()).collect();
    assert_eq!(
        modes,
        vec![Some("0755".to_string()), Some("0644".to_string())]
    );
}

/// Build the spike tree (`bin/embala-hello` 0755, `share/doc/hello/README`
/// 0644) in a temporary directory.
fn build_tree(root: &std::path::Path) {
    let bin = root.join("bin");
    let doc = root.join("share/doc/hello");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&doc).unwrap();

    let hello = bin.join("embala-hello");
    std::fs::write(&hello, HELLO).unwrap();
    std::fs::set_permissions(&hello, std::fs::Permissions::from_mode(0o755)).unwrap();

    let readme = doc.join("README");
    std::fs::write(&readme, README).unwrap();
    std::fs::set_permissions(&readme, std::fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn bom_records_paths_modes_ownership_and_checksums() {
    let dir = tempfile::tempdir().unwrap();
    build_tree(dir.path());

    let bom = write_bom(dir.path(), 0, 0).unwrap();
    let parsed = ParsedBom::parse(&bom).unwrap();

    // lsbom-style tuples: (path, mode, uid, gid, size, crc32). These are
    // exactly the fields `lsbom -p fmugsc` compares against mkbom.
    let mut paths: Vec<(String, u16, u32, u32, usize, Option<u32>)> = parsed
        .paths()
        .unwrap()
        .into_iter()
        .map(|p| {
            (
                p.path().to_string(),
                p.file_mode(),
                p.user_id(),
                p.group_id(),
                p.size(),
                p.crc32(),
            )
        })
        .collect();
    paths.sort();

    let hello_crc = cksum(HELLO);
    let readme_crc = cksum(README);

    assert_eq!(
        paths,
        vec![
            (".".to_string(), 0o40755, 0, 0, 0, None),
            ("./bin".to_string(), 0o40755, 0, 0, 0, None),
            (
                "./bin/embala-hello".to_string(),
                0o100755,
                0,
                0,
                HELLO.len(),
                Some(hello_crc)
            ),
            ("./share".to_string(), 0o40755, 0, 0, 0, None),
            ("./share/doc".to_string(), 0o40755, 0, 0, 0, None),
            ("./share/doc/hello".to_string(), 0o40755, 0, 0, 0, None),
            (
                "./share/doc/hello/README".to_string(),
                0o100644,
                0,
                0,
                README.len(),
                Some(readme_crc)
            ),
        ]
    );
}

#[test]
fn bom_output_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    build_tree(dir.path());

    let first = write_bom(dir.path(), 0, 0).unwrap();
    let second = write_bom(dir.path(), 0, 0).unwrap();

    assert!(!first.is_empty());
    assert_eq!(first, second);
}

/// POSIX `cksum` CRC — the checksum BOMs store (mkbom/lsbom use cksum(1),
/// not IEEE CRC-32). Implemented locally so the test does not depend on
/// the same code path as the writer.
fn cksum(data: &[u8]) -> u32 {
    fn update(crc: u32, byte: u8) -> u32 {
        let mut entry = ((((crc >> 24) as u8) ^ byte) as u32) << 24;
        for _ in 0..8 {
            entry = if entry & 0x8000_0000 != 0 {
                (entry << 1) ^ 0x04c1_1db7
            } else {
                entry << 1
            };
        }
        (crc << 8) ^ entry
    }

    let mut crc = 0u32;
    for byte in data {
        crc = update(crc, *byte);
    }
    let mut length = data.len();
    while length != 0 {
        crc = update(crc, (length & 0xff) as u8);
        length >>= 8;
    }
    !crc
}
