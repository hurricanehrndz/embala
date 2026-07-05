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
fn major_upgrade_tables_are_authored() {
    let spec = test_spec(&tmp("payload-upgrade"));
    let out = tmp("upgrade.msi");
    build(&spec, &out).unwrap();
    let mut package = msi::Package::open(fs::File::open(&out).unwrap()).unwrap();

    // Upgrade: one remove-older row and one detect-newer (OnlyDetect) row,
    // both keyed on the stable UpgradeCode. These drive RemoveExistingProducts
    // and the downgrade LaunchCondition respectively.
    // Rows rendered as "UpgradeCode min..max attrs -> prop" for comparison.
    let rows: Vec<String> = package
        .select_rows(msi::Select::table("Upgrade"))
        .unwrap()
        .map(|row| {
            format!(
                "{} {}..{} {} -> {}",
                row[0].as_str().unwrap(),
                row[1].as_str().unwrap_or(""),
                row[2].as_str().unwrap_or(""),
                row[4].as_int().unwrap(),
                row[6].as_str().unwrap(),
            )
        })
        .collect();
    let upgrade_code = "{AEEE1525-B29B-5F4D-AA97-3E85DD62F6D2}";
    // Attributes: 1 = msidbUpgradeAttributesMigrateFeatures,
    //             2 = msidbUpgradeAttributesOnlyDetect.
    assert!(
        rows.contains(&format!("{upgrade_code} ..0.1.0 1 -> OLDPRODUCTFOUND")),
        "missing remove-older Upgrade row: {rows:?}"
    );
    assert!(
        rows.contains(&format!("{upgrade_code} 0.1.0.. 2 -> NEWERVERSIONDETECTED")),
        "missing detect-newer Upgrade row: {rows:?}"
    );
    assert_eq!(rows.len(), 2);

    // FindRelatedProducts only assigns the action properties if they are
    // secure; without this the elevated execute sequence can't see them.
    let secure: Vec<String> = package
        .select_rows(msi::Select::table("Property"))
        .unwrap()
        .filter(|row| row[0].as_str() == Some("SecureCustomProperties"))
        .map(|row| row[1].as_str().unwrap().to_string())
        .collect();
    assert_eq!(secure, vec!["OLDPRODUCTFOUND;NEWERVERSIONDETECTED"]);

    // The downgrade guard.
    let conditions: Vec<String> = package
        .select_rows(msi::Select::table("LaunchCondition"))
        .unwrap()
        .map(|row| row[0].as_str().unwrap().to_string())
        .collect();
    assert_eq!(conditions, vec!["NOT NEWERVERSIONDETECTED"]);

    // The actions that make the tables take effect must be sequenced:
    // detection + guard early, RemoveExistingProducts right after
    // InstallValidate (old product is gone before new files go down).
    let seq: Vec<(String, i32)> = package
        .select_rows(msi::Select::table("InstallExecuteSequence"))
        .unwrap()
        .map(|row| {
            (
                row[0].as_str().unwrap().to_string(),
                row[2].as_int().unwrap(),
            )
        })
        .collect();
    for expected in [
        ("FindRelatedProducts", 25),
        ("LaunchConditions", 100),
        ("MigrateFeatureStates", 1200),
        ("RemoveExistingProducts", 1401),
    ] {
        assert!(
            seq.iter().any(|(a, s)| (a.as_str(), *s) == expected),
            "InstallExecuteSequence missing {expected:?}: {seq:?}"
        );
    }
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
