//! End-to-end overlay assertions: the appended sections locate and decode via
//! the shared trailer, the payload round-trips byte-identically, and two builds
//! of the same spec are byte-for-byte reproducible.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use embala_setup::{Error, FileSpec, InstallMode, ProductInfo, SetupArch, SetupSpec, build};
use embala_setup_overlay::{Manifest, TRAILER_LEN, Trailer};

const HELLO: &[u8] = b"MZ fake hello.exe payload";
const README: &[u8] = b"read me\n";

fn tmp(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// Product identity used across the writer tests.
fn product() -> ProductInfo {
    ProductInfo {
        name: "hello".to_string(),
        display_name: "Embala Hello".to_string(),
        version: "0.1.0".to_string(),
        identifier: "ca.hrndz.embala.hello".to_string(),
        publisher: "Carlos Hernandez".to_string(),
        description: "test fixture".to_string(),
        homepage: Some("https://example.com".to_string()),
        license: Some("MIT".to_string()),
    }
}

/// Two payload files written under a fresh dir; returns the spec.
fn two_file_spec(payload_dir: &Path) -> SetupSpec {
    std::fs::create_dir_all(payload_dir).unwrap();
    std::fs::write(payload_dir.join("hello.exe"), HELLO).unwrap();
    std::fs::write(payload_dir.join("readme.txt"), README).unwrap();
    SetupSpec {
        arch: SetupArch::X86_64,
        install_mode: InstallMode::PerUser,
        product: product(),
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
        install_lua: None,
        uninstall_lua: None,
        signed_stub: None,
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
    // No signed stub supplied: a zero-length section pinned at the manifest's end.
    assert_eq!(t.signed_stub.offset, t.manifest.offset + t.manifest.len);
    assert_eq!(t.signed_stub.len, 0);
    // signed_stub is the last section; a 0–7 byte alignment pad may sit between
    // it and the trailer so the unsigned length is 8-byte aligned.
    let trailer_start = (bytes.len() - TRAILER_LEN) as u64;
    let sections_end = t.signed_stub.offset + t.signed_stub.len;
    assert!(sections_end <= trailer_start);
    assert!(
        trailer_start - sections_end < 8,
        "alignment pad is under 8 bytes"
    );

    // uninstall.lua is empty this phase; install.lua is the plaintext placeholder.
    assert_eq!(t.uninstall_lua.len, 0);
    let install = section(&bytes, t.install_lua);
    assert!(
        install.starts_with(b"-- embala"),
        "install.lua placeholder: {:?}",
        String::from_utf8_lossy(install)
    );

    // Manifest deserializes via the shared overlay type with the resolved mode
    // and full product identity (spec R8).
    let manifest = Manifest::parse(section(&bytes, t.manifest)).expect("manifest parses");
    assert_eq!(manifest.format_version, 2);
    assert_eq!(manifest.arch, "x86_64");
    assert_eq!(manifest.install_mode, "per-user");
    assert_eq!(manifest.package.identifier, "ca.hrndz.embala.hello");

    // Unzip the payload section and compare each file byte-for-byte.
    let extracted = unzip(section(&bytes, t.payload_zip));
    assert_eq!(extracted.len(), 2);
    assert_eq!(extracted["bin/hello.exe"], HELLO);
    assert_eq!(extracted["readme.txt"], README);
}

#[test]
fn output_is_8_byte_aligned_and_trailer_round_trips() {
    // Why: signtool appends the cert table at the unsigned length; that length
    // must be 8-byte aligned so the table starts at the security-directory offset
    // the runtime reads as the overlay end. The trailer must still parse from the
    // last TRAILER_LEN bytes.
    let spec = two_file_spec(&tmp("payload-align"));
    let out = tmp("align-setup.exe");
    build(&spec, &out).unwrap();

    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(bytes.len() % 8, 0, "unsigned length must be 8-byte aligned");
    Trailer::parse(&bytes[bytes.len() - TRAILER_LEN..]).expect("trailer round-trips at EOF");
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
                install_mode: InstallMode::PerUser,
                product: product(),
                files,
                install_lua: None,
                uninstall_lua: None,
                signed_stub: None,
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

#[test]
fn scripts_ship_verbatim_in_the_overlay() {
    // Why: install.lua/uninstall.lua ship as plaintext (spec R7) and the runtime
    // runs them byte-for-byte; a writer that re-encoded or truncated the script
    // would silently change install behavior. Include spaces/unicode/newlines.
    let install = "-- install\nembala.log(\"héllo wörld\")\n"
        .as_bytes()
        .to_vec();
    let uninstall = "-- uninstall\nembala.fs.remove(\"a b/c.txt\")\n"
        .as_bytes()
        .to_vec();
    let mut spec = two_file_spec(&tmp("payload-scripts"));
    spec.install_lua = Some(install.clone());
    spec.uninstall_lua = Some(uninstall.clone());

    let out = tmp("scripts-setup.exe");
    build(&spec, &out).unwrap();
    let bytes = std::fs::read(&out).unwrap();
    let t = read_trailer(&out);

    assert_eq!(section(&bytes, t.install_lua), install.as_slice());
    assert_eq!(section(&bytes, t.uninstall_lua), uninstall.as_slice());
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
