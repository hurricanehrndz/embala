//! End-to-end build assertions: structural validity (via the msi crate),
//! byte-for-byte reproducibility, and a payload round-trip through msitools'
//! msiextract.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use embala_msi::{FileSpec, MsiArch, MsiSpec, build};

const PAYLOAD_EXE: &[u8] = b"MZ fake windows executable payload";
const PAYLOAD_TXT: &[u8] = b"readme contents\n";

fn tmp(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// A two-file spec (one nested dest) over a freshly written payload dir.
fn test_spec(payload_dir: &Path) -> MsiSpec {
    fs::create_dir_all(payload_dir.join("data")).unwrap();
    fs::write(payload_dir.join("hello.exe"), PAYLOAD_EXE).unwrap();
    fs::write(payload_dir.join("readme.txt"), PAYLOAD_TXT).unwrap();
    MsiSpec {
        name: "hello".to_string(),
        display_name: "Embala Hello".to_string(),
        version: "0.1.0".to_string(),
        identifier: "ca.hrndz.embala.hello".to_string(),
        publisher: "Carlos Hernandez".to_string(),
        description: "Embala end-to-end test fixture".to_string(),
        homepage: Some("https://github.com/hurricanehrndz/embala".to_string()),
        arch: MsiArch::X86_64,
        main_executable: "hello.exe".to_string(),
        files: vec![
            FileSpec {
                src: payload_dir.join("hello.exe"),
                dest: "hello.exe".to_string(),
            },
            FileSpec {
                src: payload_dir.join("readme.txt"),
                dest: "data/readme.txt".to_string(),
            },
        ],
    }
}

#[test]
fn build_is_reproducible_and_structurally_valid() {
    let spec = test_spec(&tmp("payload-repro"));
    let out1 = tmp("repro-1.msi");
    let out2 = tmp("repro-2.msi");
    build(&spec, &out1).unwrap();
    build(&spec, &out2).unwrap();

    // Reproducibility: identical spec => byte-identical installer.
    assert_eq!(
        fs::read(&out1).unwrap(),
        fs::read(&out2).unwrap(),
        "two consecutive builds must be byte-identical"
    );

    // Structural checks, reopening with the msi crate itself. These are the
    // msiexec acceptance gotchas: ANSI codepage, Template arch;lang,
    // compressed-source word count.
    let package = msi::Package::open(fs::File::open(&out1).unwrap()).unwrap();
    assert_eq!(package.database_codepage(), msi::CodePage::Windows1252);
    let summary = package.summary_info();
    assert_eq!(summary.codepage(), msi::CodePage::Windows1252);
    assert_eq!(summary.arch(), Some("x64"));
    assert_eq!(summary.languages(), vec![msi::Language::from_code(1033)]);
    assert_eq!(summary.word_count(), Some(2));
    assert_eq!(summary.page_count(), Some(200));
    assert!(summary.uuid().is_some(), "PackageCode must be set");
}

#[test]
fn msiextract_round_trips_the_payload() {
    let available = Command::new("msiextract").arg("--version").output().is_ok();
    assert!(
        available,
        "msiextract not on PATH — run tests inside the devenv shell"
    );

    let spec = test_spec(&tmp("payload-extract"));
    let out = tmp("extract.msi");
    build(&spec, &out).unwrap();

    let extract_dir = tmp("extracted");
    let _ = fs::remove_dir_all(&extract_dir);
    fs::create_dir_all(&extract_dir).unwrap();
    let status = Command::new("msiextract")
        .arg("-C")
        .arg(&extract_dir)
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success(), "msiextract failed");

    // msiextract lays files out under the resolved install path; find ours by
    // name and compare bytes with the original payload.
    let exe = find_file(&extract_dir, "hello.exe").expect("hello.exe extracted");
    assert_eq!(fs::read(exe).unwrap(), PAYLOAD_EXE);
    let txt = find_file(&extract_dir, "readme.txt").expect("readme.txt extracted");
    assert_eq!(fs::read(&txt).unwrap(), PAYLOAD_TXT);
    assert_eq!(
        txt.parent().and_then(|p| p.file_name()).unwrap(),
        "data",
        "nested dest must extract into its subdirectory"
    );
}

#[test]
fn non_ascii_spec_is_rejected() {
    let payload = tmp("payload-ascii");
    let mut spec = test_spec(&payload);
    spec.publisher = "Çarlos".to_string();
    let err = build(&spec, &tmp("ascii.msi")).unwrap_err();
    assert!(
        matches!(err, embala_msi::Error::NonAscii { .. }),
        "unexpected error: {err}"
    );
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == name) {
            return Some(path);
        }
    }
    None
}
