//! End-to-end overlay assertions: the appended sections locate and decode via
//! the shared trailer, the payload round-trips byte-identically, and two builds
//! of the same spec are byte-for-byte reproducible.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use embala_setup::{Error, FileSpec, SetupArch, SetupSpec, build};
use embala_setup_overlay::{TRAILER_LEN, Trailer};

const HELLO: &[u8] = b"MZ fake hello.exe payload";
const README: &[u8] = b"read me\n";

fn tmp(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// Two payload files written under a fresh dir; returns the spec.
fn two_file_spec(payload_dir: &Path) -> SetupSpec {
    std::fs::create_dir_all(payload_dir).unwrap();
    std::fs::write(payload_dir.join("hello.exe"), HELLO).unwrap();
    std::fs::write(payload_dir.join("readme.txt"), README).unwrap();
    SetupSpec {
        arch: SetupArch::X86_64,
        files: vec![
            FileSpec {
                src: payload_dir.join("hello.exe"),
                dest: "bin/hello.exe".to_string(),
            },
            FileSpec {
                src: payload_dir.join("readme.txt"),
                dest: "readme.txt".to_string(),
            },
        ],
    }
}

/// Read the trailer from the tail of a built setup.exe.
fn read_trailer(exe: &Path) -> Trailer {
    let bytes = std::fs::read(exe).unwrap();
    let tail = &bytes[bytes.len() - TRAILER_LEN..];
    Trailer::parse(tail).expect("valid trailer at EOF")
}

#[test]
fn overlay_sections_locate_and_payload_round_trips() {
    // Why: the runtime finds payload/scripts/manifest *only* through the trailer
    // table, then unzips the payload to disk. If any offset/len is wrong the
    // self-extractor reads the wrong bytes; this test is that read path.
    let spec = two_file_spec(&tmp("payload-roundtrip"));
    let out = tmp("roundtrip-setup.exe");
    build(&spec, &out).unwrap();

    let bytes = std::fs::read(&out).unwrap();
    let t = read_trailer(&out);

    // The embedded stub is a real PE, and the overlay begins right after it.
    assert_eq!(t.stub.offset, 0);
    assert_eq!(&bytes[0..2], b"MZ", "stub must be a PE");
    assert_eq!(t.payload_zip.offset, t.stub.len);

    // Sections tile the file with no gaps, ending at the trailer.
    assert_eq!(t.payload_zip.offset, t.stub.offset + t.stub.len);
    assert_eq!(
        t.install_lua.offset,
        t.payload_zip.offset + t.payload_zip.len
    );
    assert_eq!(
        t.uninstall_lua.offset,
        t.install_lua.offset + t.install_lua.len
    );
    assert_eq!(
        t.manifest.offset,
        t.uninstall_lua.offset + t.uninstall_lua.len
    );
    assert_eq!(
        t.manifest.offset + t.manifest.len,
        (bytes.len() - TRAILER_LEN) as u64
    );

    // uninstall.lua is empty this phase; install.lua is the plaintext placeholder.
    assert_eq!(t.uninstall_lua.len, 0);
    let install = section(&bytes, t.install_lua);
    assert!(
        install.starts_with(b"-- embala"),
        "install.lua placeholder: {:?}",
        String::from_utf8_lossy(install)
    );

    // Manifest is deterministic JSON naming the format version and arch.
    let manifest = std::str::from_utf8(section(&bytes, t.manifest)).unwrap();
    assert_eq!(manifest, "{\"format_version\":1,\"arch\":\"x86_64\"}");

    // Unzip the payload section and compare each file byte-for-byte.
    let extracted = unzip(section(&bytes, t.payload_zip));
    assert_eq!(extracted.len(), 2);
    assert_eq!(extracted["bin/hello.exe"], HELLO);
    assert_eq!(extracted["readme.txt"], README);
}

#[test]
fn builds_are_byte_reproducible() {
    // Why: byte-reproducibility is the invariant that makes signing/caching
    // viable later; it fails the moment a wall-clock/mtime leaks into the
    // payload zip or the overlay assembly.
    let spec = two_file_spec(&tmp("payload-repro"));
    let out1 = tmp("repro-1-setup.exe");
    let out2 = tmp("repro-2-setup.exe");
    build(&spec, &out1).unwrap();
    build(&spec, &out2).unwrap();
    assert_eq!(
        std::fs::read(&out1).unwrap(),
        std::fs::read(&out2).unwrap(),
        "two consecutive builds must be byte-identical"
    );
}

#[test]
fn traversal_and_duplicate_dests_are_rejected() {
    // Why: a corrupted spec must not be able to write outside the target dir at
    // extract time, and a dest collision would silently clobber a payload file.
    let dir = tmp("payload-invalid");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("f"), HELLO).unwrap();
    let file = |dest: &str| FileSpec {
        src: dir.join("f"),
        dest: dest.to_string(),
    };
    let fail = |files: Vec<FileSpec>| {
        build(
            &SetupSpec {
                arch: SetupArch::X86_64,
                files,
            },
            &tmp("invalid-setup.exe"),
        )
        .unwrap_err()
    };

    assert!(matches!(fail(vec![]), Error::NoFiles));
    assert!(matches!(fail(vec![file("../evil")]), Error::InvalidDest(_)));
    assert!(matches!(
        fail(vec![file("/etc/passwd")]),
        Error::InvalidDest(_)
    ));
    assert!(matches!(fail(vec![file("C:\\x")]), Error::InvalidDest(_)));
    // Backslashes normalize to forward slashes rather than becoming a dest.
    assert!(matches!(
        fail(vec![file("a\\..\\b")]),
        Error::InvalidDest(_)
    ));
    assert!(matches!(
        fail(vec![file("dup"), file("dup")]),
        Error::DuplicateDest(_)
    ));
}

/// Slice a section out of the whole-file bytes by its trailer entry.
fn section(bytes: &[u8], sec: embala_setup_overlay::Section) -> &[u8] {
    &bytes[sec.offset as usize..(sec.offset + sec.len) as usize]
}

/// Extract a zip byte-slice into a `dest -> contents` map.
fn unzip(bytes: &[u8]) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
    let mut out = std::collections::BTreeMap::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).unwrap();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        out.insert(entry.name().to_string(), data);
    }
    out
}
