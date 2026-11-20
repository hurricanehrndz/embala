//! `embala info` reports what recipes need from an installer: a PE's
//! VERSIONINFO or an MSI's Property table. It reads untrusted vendor files,
//! so malformed input must give a clean error naming the file, never a panic.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tmp(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Minimal msi + setup config at version 0.1.0 with a dummy payload.
fn write_fixture(dir: &Path) -> PathBuf {
    fs::write(dir.join("hello.exe"), b"MZ dummy payload").unwrap();
    let config = r#"
[package]
name = "hello"
display-name = "Embala Hello"
version = "0.1.0"
identifier = "ca.hrndz.embala.hello"
publisher = "Carlos Hernandez"
description = "info integration fixture"

[msi]
arch = "x86_64"
main-executable = "hello.exe"
files = [{ src = "hello.exe", dest = "hello.exe" }]

[setup]
arch = "x86_64"
main-executable = "hello.exe"
files = [{ src = "hello.exe", dest = "hello.exe" }]
"#;
    let path = dir.join("embala.toml");
    fs::write(&path, config).unwrap();
    path
}

fn embala(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_embala"))
        .args(args)
        .output()
        .expect("run embala")
}

fn ok(result: &Output) {
    assert!(
        result.status.success(),
        "embala failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn info_json(file: &Path) -> serde_json::Value {
    let result = embala(&["info", "--json", file.to_str().unwrap()]);
    ok(&result);
    // A well-formed file warns about nothing; for the stub this also checks
    // that its uppercase `040904B0` key matches the translation.
    assert!(
        result.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).expect("info --json prints one JSON object")
}

/// A malformed file must fail cleanly: non-zero, not a panic (101), and the
/// message names the file so a recipe log says which download was bad.
fn assert_clean_error(file: &Path) -> String {
    let result = embala(&["info", "--json", file.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
    assert!(!result.status.success(), "{stderr}");
    assert_ne!(result.status.code(), Some(101), "panicked: {stderr}");
    assert!(stderr.contains(&file.display().to_string()), "{stderr}");
    stderr
}

#[test]
fn pe_reports_versioninfo() {
    // Why: sofcat compares the fixed FileVersion and the ProductName string.
    let dir = tmp("info-pe");
    let config = write_fixture(&dir);
    let stub = dir.join("stub.exe");
    ok(&embala(&[
        "dump-stub",
        "--config",
        config.to_str().unwrap(),
        "--version",
        "4.5.6",
        "--out",
        stub.to_str().unwrap(),
    ]));
    let info = info_json(&stub);
    assert_eq!(info["type"], "pe");
    assert_eq!(info["file_version"], "4.5.6.0");
    assert_eq!(info["product_version"], "4.5.6.0");
    assert_eq!(info["strings"]["FileVersion"], "4.5.6");
    assert_eq!(info["strings"]["ProductVersion"], "4.5.6");
    assert_eq!(info["strings"]["CompanyName"], "Carlos Hernandez");

    let human = embala(&["info", stub.to_str().unwrap()]);
    ok(&human);
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(human.starts_with("type: pe\n"), "{human}");
    assert!(human.contains("\nfile_version: 4.5.6.0\n"), "{human}");
    assert!(human.contains("\nstrings.FileVersion: 4.5.6\n"), "{human}");
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

#[test]
fn pe_string_table_not_matching_translation_warns() {
    // Why: Windows looks strings up under the first Translation's
    // `<lang><codepage>` key; a table under another key may be invisible to
    // it, so the recipe log should say so while still reporting the strings.
    let dir = tmp("info-pe-mismatch");
    let config = write_fixture(&dir);
    let stub = dir.join("stub.exe");
    ok(&embala(&[
        "dump-stub",
        "--config",
        config.to_str().unwrap(),
        "--version",
        "4.5.6",
        "--out",
        stub.to_str().unwrap(),
    ]));
    // Retag the stub's only string table (en-US) as en-GB.
    let mut bytes = fs::read(&stub).unwrap();
    let (from, to) = (utf16("040904B0"), utf16("080904B0"));
    let at: Vec<usize> = (0..bytes.len() - from.len())
        .filter(|&i| bytes[i..i + from.len()] == from[..])
        .collect();
    assert_eq!(at.len(), 1, "expected one 040904B0 table key in the stub");
    bytes[at[0]..at[0] + to.len()].copy_from_slice(&to);
    fs::write(&stub, bytes).unwrap();

    let result = embala(&["info", "--json", stub.to_str().unwrap()]);
    ok(&result);
    let info: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(info["strings"]["FileVersion"], "4.5.6");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("string table key 080904B0 does not match the first translation 040904b0"),
        "{stderr}"
    );
}

#[test]
fn pe_without_versioninfo_is_not_an_error() {
    // Why: the recipe decides what to do; every key is present either way.
    let dir = tmp("info-pe-bare");
    let stub = dir.join("stub.exe");
    ok(&embala(&["dump-stub", "--out", stub.to_str().unwrap()]));
    let info = info_json(&stub);
    assert_eq!(
        info,
        serde_json::json!({
            "type": "pe",
            "file_version": null,
            "product_version": null,
            "strings": {},
        })
    );
}

#[test]
fn msi_reports_the_property_table() {
    let dir = tmp("info-msi");
    let config = write_fixture(&dir);
    let out = dir.join("dist");
    ok(&embala(&[
        "build",
        "--config",
        config.to_str().unwrap(),
        "--formats",
        "msi",
        "--out-dir",
        out.to_str().unwrap(),
    ]));
    let info = info_json(&out.join("hello-0.1.0-x86_64.msi"));
    assert_eq!(info["type"], "msi");
    let properties = &info["properties"];
    assert_eq!(properties["ProductVersion"], "0.1.0");
    assert_eq!(properties["ProductName"], "Embala Hello");
    assert_eq!(properties["Manufacturer"], "Carlos Hernandez");
    // The locked GUIDs from embala-msi's guids.rs for this identifier.
    assert_eq!(
        properties["ProductCode"],
        "{6CB00C44-06B1-575C-904F-14223F55E843}"
    );
    assert_eq!(
        properties["UpgradeCode"],
        "{AEEE1525-B29B-5F4D-AA97-3E85DD62F6D2}"
    );
}

#[test]
fn other_files_are_rejected() {
    let dir = tmp("info-other");
    let text = dir.join("readme.txt");
    fs::write(&text, b"just some text, long enough").unwrap();
    assert!(assert_clean_error(&text).contains("not a PE or MSI"));
    // Shorter than the 8-byte CFB magic.
    let short = dir.join("short.bin");
    fs::write(&short, b"abc").unwrap();
    assert!(assert_clean_error(&short).contains("not a PE or MSI"));
    // CFB magic, but not an MSI.
    let cfb = dir.join("fake.msi");
    let mut bytes = vec![0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
    bytes.resize(512, 0);
    fs::write(&cfb, bytes).unwrap();
    assert!(assert_clean_error(&cfb).contains("not an MSI"));
}

/// PE32+ headers with one section and a resource directory at `rsrc_rva`;
/// the section maps RVA 0x1000.. to file offset `raw_offset`.
fn hostile_pe(rsrc_rva: u32, raw_offset: u32) -> Vec<u8> {
    let mut pe = vec![0u8; 0x200];
    pe[..2].copy_from_slice(b"MZ");
    pe[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    pe[0x40..0x44].copy_from_slice(b"PE\0\0");
    pe[0x46..0x48].copy_from_slice(&1u16.to_le_bytes()); // NumberOfSections
    pe[0x54..0x56].copy_from_slice(&0xf0u16.to_le_bytes()); // SizeOfOptionalHeader
    let opt = 0x58;
    pe[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes());
    pe[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
    pe[opt + 128..opt + 132].copy_from_slice(&rsrc_rva.to_le_bytes());
    pe[opt + 132..opt + 136].copy_from_slice(&0x100u32.to_le_bytes());
    let section = opt + 0xf0;
    pe[section + 12..section + 16].copy_from_slice(&0x1000u32.to_le_bytes());
    pe[section + 16..section + 20].copy_from_slice(&0x200u32.to_le_bytes());
    pe[section + 20..section + 24].copy_from_slice(&raw_offset.to_le_bytes());
    pe
}

#[test]
fn hostile_pes_fail_cleanly() {
    let dir = tmp("info-hostile");
    // Each case names the error it must hit, so failing earlier for some other
    // reason doesn't pass.
    let cases: [(&str, Vec<u8>, &str); 4] = [
        (
            "mz-only.exe",
            b"MZ".to_vec(),
            "truncated PE: 64 bytes at 0x0",
        ),
        // e_lfanew far past the end of the file.
        (
            "lfanew.exe",
            {
                let mut pe = hostile_pe(0x1000, 0x100);
                pe[0x3c..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
                pe
            },
            "truncated PE: 24 bytes at 0xffffffff",
        ),
        // Resource RVA in no section.
        (
            "rva.exe",
            hostile_pe(0xffff_ff00, 0x100),
            "resource RVA 0xffffff00 is outside every section",
        ),
        // Resource section's file offset past the end of the file.
        (
            "offset.exe",
            hostile_pe(0x1000, 0xffff_0000),
            "truncated PE: 16 bytes at 0xffff0000",
        ),
    ];
    for (name, bytes, expected) in cases {
        let file = dir.join(name);
        fs::write(&file, bytes).unwrap();
        let stderr = assert_clean_error(&file);
        assert!(stderr.contains(expected), "{name}: {stderr}");
    }
}
