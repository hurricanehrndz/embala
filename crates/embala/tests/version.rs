//! `--version` overrides `[package].version` so callers can pass a release
//! version (e.g. from a git tag) without rewriting embala.toml. The override
//! must reach every place the version lands and still pass the x.y.z check.

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
description = "version integration fixture"

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

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

#[test]
fn build_version_overrides_the_config() {
    let dir = tmp("version-build");
    let config = write_fixture(&dir);
    let out = dir.join("dist");
    let result = embala(&[
        "build",
        "--config",
        config.to_str().unwrap(),
        "--formats",
        "msi,setup",
        "--out-dir",
        out.to_str().unwrap(),
        "--version",
        "1.2.3",
    ]);
    assert!(
        result.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(out.join("hello-1.2.3-x86_64.msi").exists());
    let setup = fs::read(out.join("hello-1.2.3-x86_64-setup.exe")).unwrap();
    let needle = utf16("1.2.3");
    assert!(
        setup.windows(needle.len()).any(|w| w == needle),
        "setup.exe VERSIONINFO must carry the overridden version"
    );
    assert!(!out.join("hello-0.1.0-x86_64.msi").exists());
}

#[test]
fn build_version_is_validated_like_the_config() {
    // Why: the override must not bypass the x.y.z rule MSI depends on.
    let dir = tmp("version-invalid");
    let config = write_fixture(&dir);
    let result = embala(&[
        "build",
        "--config",
        config.to_str().unwrap(),
        "--out-dir",
        dir.join("dist").to_str().unwrap(),
        "--version",
        "1.2.3-beta",
    ]);
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("numeric x.y.z"), "{stderr}");
}

#[test]
fn dump_stub_version_brands_the_stub() {
    // Why: an externally signed stub must match the setup.exe `build` makes.
    let dir = tmp("version-dump-stub");
    let config = write_fixture(&dir);
    let stub = dir.join("stub.exe");
    let result = embala(&[
        "dump-stub",
        "--config",
        config.to_str().unwrap(),
        "--version",
        "4.5.6",
        "--out",
        stub.to_str().unwrap(),
    ]);
    assert!(
        result.status.success(),
        "dump-stub failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let bytes = fs::read(&stub).unwrap();
    let needle = utf16("4.5.6");
    assert!(bytes.windows(needle.len()).any(|w| w == needle));
}

#[test]
fn dump_stub_version_requires_config() {
    let dir = tmp("version-dump-stub-bare");
    let result = embala(&[
        "dump-stub",
        "--version",
        "1.2.3",
        "--out",
        dir.join("stub.exe").to_str().unwrap(),
    ]);
    assert!(!result.status.success());
}

#[test]
fn top_level_version_flag_still_prints_embala_version() {
    let result = embala(&["--version"]);
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains(env!("CARGO_PKG_VERSION")));
}
