//! `optional = true` payload files: one config serves builds that stage a file
//! (VM test builds with credentials) and builds that do not (releases). An
//! absent optional file must vanish from the built payload, a present one must
//! ship exactly as a required one would, and anything else stays an error.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CREDENTIALS: &[u8] = b"[default]\naws_access_key_id = test\n";

fn tmp(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// pkg + setup config shipping `hello.exe` and, under `credential_keys`, the
/// osconfig-sync-style `secrets/credentials` entry (not staged here).
fn write_fixture(dir: &Path, credential_keys: &str) -> PathBuf {
    fs::write(dir.join("hello.exe"), b"MZ dummy payload").unwrap();
    let config = format!(
        r#"
[package]
name = "hello"
display-name = "Embala Hello"
version = "0.1.0"
identifier = "ca.hrndz.embala.hello"
publisher = "Carlos Hernandez"
description = "optional files fixture"

[pkg]
install-location = "/usr/local"
files = [
  {{ src = "hello.exe", dest = "bin/hello" }},
  {{ src = "secrets/credentials", dest = "credentials", mode = 0o600{credential_keys} }},
]

[setup]
arch = "x86_64"
main-executable = "hello.exe"
files = [
  {{ src = "hello.exe", dest = "hello.exe" }},
  {{ src = "secrets/credentials", dest = "credentials"{credential_keys} }},
]
"#
    );
    let path = dir.join("embala.toml");
    fs::write(&path, config).unwrap();
    path
}

fn stage_credentials(dir: &Path) {
    fs::create_dir_all(dir.join("secrets")).unwrap();
    fs::write(dir.join("secrets/credentials"), CREDENTIALS).unwrap();
}

fn build(config: &Path, formats: &str) -> Output {
    let out = config.parent().unwrap().join("dist");
    Command::new(env!("CARGO_BIN_EXE_embala"))
        .args(["build", "--config"])
        .arg(config)
        .args(["--formats", formats, "--out-dir"])
        .arg(out)
        .output()
        .expect("run embala")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `(name, mode)` of every entry in the built .pkg's cpio payload.
fn pkg_payload(dir: &Path) -> Vec<(String, u64)> {
    let archive = fs::read(dir.join("dist/hello-0.1.0.pkg")).unwrap();
    let gz = apple_xar::reader::XarReader::new(Cursor::new(archive))
        .unwrap()
        .get_file_data_from_path("hello.pkg/Payload")
        .unwrap()
        .expect("component Payload");
    let mut cpio = Vec::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_end(&mut cpio)
        .unwrap();
    // odc headers: 76 ASCII-octal bytes, then the NUL-terminated name and data.
    let mut entries = Vec::new();
    let mut rest = &cpio[..];
    loop {
        let field = |lo: usize, hi: usize| {
            u64::from_str_radix(std::str::from_utf8(&rest[lo..hi]).unwrap(), 8).unwrap()
        };
        let (mode, name_size, file_size) = (field(18, 24), field(59, 65), field(65, 76));
        let (name_size, file_size) = (name_size as usize, file_size as usize);
        let name = String::from_utf8(rest[76..76 + name_size - 1].to_vec()).unwrap();
        if name == "TRAILER!!!" {
            return entries;
        }
        entries.push((name, mode));
        rest = &rest[76 + name_size + file_size..];
    }
}

/// The built setup.exe's payload zip (`dest -> contents`) and install.lua.
fn setup_payload(dir: &Path) -> (BTreeMap<String, Vec<u8>>, String) {
    let bytes = fs::read(dir.join("dist/hello-0.1.0-x86_64-setup.exe")).unwrap();
    let trailer = embala_setup_overlay::Trailer::parse(
        &bytes[bytes.len() - embala_setup_overlay::TRAILER_LEN..],
    )
    .unwrap();
    let section = |s: embala_setup_overlay::Section| {
        bytes[s.offset as usize..(s.offset + s.len) as usize].to_vec()
    };
    let mut zip = zip::ZipArchive::new(Cursor::new(section(trailer.payload_zip))).unwrap();
    let mut files = BTreeMap::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).unwrap();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        files.insert(entry.name().to_string(), data);
    }
    let install_lua = String::from_utf8(section(trailer.install_lua)).unwrap();
    (files, install_lua)
}

#[test]
fn absent_optional_files_are_left_out_of_every_payload() {
    let dir = tmp("optional-absent");
    let config = write_fixture(&dir, ", optional = true");
    let result = build(&config, "pkg,setup");
    assert!(result.status.success(), "build failed: {}", stderr(&result));

    // One note per skipped entry, naming the src.
    let err = stderr(&result);
    let src = dir.join("secrets/credentials");
    for section in ["pkg", "setup"] {
        let note = format!("{section}: skipped optional {} (not found)", src.display());
        assert!(err.contains(&note), "missing {note:?} in: {err}");
    }

    let pkg = pkg_payload(&dir);
    assert!(
        pkg.contains(&("./bin/hello".to_string(), 0o100644)),
        "{pkg:?}"
    );
    assert!(
        !pkg.iter().any(|(n, _)| n.contains("credentials")),
        "{pkg:?}"
    );

    let (files, install_lua) = setup_payload(&dir);
    assert_eq!(files.keys().collect::<Vec<_>>(), ["hello.exe"]);
    // The lowered script must not copy a file the payload lacks.
    assert!(!install_lua.contains("credentials"), "{install_lua}");
}

#[test]
fn present_optional_files_ship_like_required_ones() {
    let dir = tmp("optional-present");
    let config = write_fixture(&dir, ", optional = true");
    stage_credentials(&dir);
    let result = build(&config, "pkg,setup");
    assert!(result.status.success(), "build failed: {}", stderr(&result));
    assert!(!stderr(&result).contains("skipped"), "{}", stderr(&result));

    let pkg = pkg_payload(&dir);
    assert!(
        pkg.contains(&("./credentials".to_string(), 0o100600)),
        "credentials must keep its 0o600 mode: {pkg:?}"
    );
    let (files, install_lua) = setup_payload(&dir);
    assert_eq!(
        files.get("credentials").map(Vec::as_slice),
        Some(CREDENTIALS)
    );
    assert!(install_lua.contains("embala.fs.copy(\"credentials\""));
}

#[test]
fn missing_required_files_still_fail() {
    let dir = tmp("optional-required");
    let config = write_fixture(&dir, "");
    // The errors are each writer's own, unchanged; embala-pkg's does not
    // name the path, so only setup's message is pinned.
    for (format, needle) in [
        ("pkg", "No such file"),
        ("setup", "credentials does not exist"),
    ] {
        let result = build(&config, format);
        assert!(!result.status.success(), "{format} must fail");
        assert!(stderr(&result).contains(needle), "{}", stderr(&result));
        assert!(!stderr(&result).contains("skipped"), "{}", stderr(&result));
    }
}

#[cfg(unix)]
#[test]
fn optional_dangling_symlink_fails_the_build() {
    // Why: a staging step that left a broken link is a bug to surface, not a
    // file to drop from the release.
    let dir = tmp("optional-dangling");
    let config = write_fixture(&dir, ", optional = true");
    fs::create_dir_all(dir.join("secrets")).unwrap();
    std::os::unix::fs::symlink(dir.join("nowhere"), dir.join("secrets/credentials")).unwrap();
    let result = build(&config, "pkg");
    assert!(!result.status.success());
    assert!(
        stderr(&result).contains("dangling symlink"),
        "{}",
        stderr(&result)
    );
    assert!(!dir.join("dist/hello-0.1.0.pkg").exists());
}

#[test]
fn skipped_file_named_by_main_executable_fails_before_writing() {
    let dir = tmp("optional-main-executable");
    let config = dir.join("embala.toml");
    fs::write(
        &config,
        r#"
[package]
name = "hello"
display-name = "Embala Hello"
version = "0.1.0"
identifier = "ca.hrndz.embala.hello"
publisher = "Carlos Hernandez"
description = "optional files fixture"

[msi]
arch = "x86_64"
main-executable = "agent.exe"
files = [{ src = "agent.exe", dest = "agent.exe", optional = true }]
"#,
    )
    .unwrap();
    let result = build(&config, "msi");
    assert!(!result.status.success());
    let err = stderr(&result);
    assert!(
        err.contains("msi: main-executable \"agent.exe\" is an optional file that was absent"),
        "{err}"
    );
    assert!(!dir.join("dist").exists(), "nothing may be written");
}

#[test]
fn dump_stub_tolerates_absent_optional_files() {
    // Why: external signing flows dump the stub from the same release config.
    let dir = tmp("optional-dump-stub");
    let config = write_fixture(&dir, ", optional = true");
    let result = Command::new(env!("CARGO_BIN_EXE_embala"))
        .args(["dump-stub", "--config"])
        .arg(&config)
        .arg("--out")
        .arg(dir.join("stub.exe"))
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", stderr(&result));
}
