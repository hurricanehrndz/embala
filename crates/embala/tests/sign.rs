//! `[sign]` end-to-end: the compiled `embala` binary must invoke the configured
//! sign command on the right artifacts, in the right order (spec R6), fail the
//! build when the command fails, and — with no `[sign]` — stay byte-reproducible
//! (spec R7). Self-contained: a dummy payload in a tempdir, no `just fixtures`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const MARKER: &[u8] = b"SIGNED\n";

fn tmp(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a minimal fixture (dummy `hello.exe`) into `dir` and return the config
/// path. `sign_block` is appended verbatim (empty for the unsigned case).
fn write_fixture(dir: &Path, sign_block: &str) -> PathBuf {
    fs::write(dir.join("hello.exe"), b"MZ dummy payload").unwrap();
    let config = format!(
        r#"
[package]
name = "hello"
display-name = "Embala Hello"
version = "0.1.0"
identifier = "ca.hrndz.embala.hello"
publisher = "Carlos Hernandez"
description = "sign integration fixture"

[msi]
arch = "x86_64"
main-executable = "hello.exe"
files = [{{ src = "hello.exe", dest = "hello.exe" }}]

[setup]
arch = "x86_64"
main-executable = "hello.exe"
files = [{{ src = "hello.exe", dest = "hello.exe" }}]
{sign_block}
"#
    );
    let path = dir.join("embala.toml");
    fs::write(&path, config).unwrap();
    path
}

/// A sign script (`$1` = artifact) that logs its target then appends the marker.
/// Kept in a file so shell escapes never round-trip through TOML.
fn write_sign_script(dir: &Path, log: &Path) -> PathBuf {
    let script = dir.join("sign.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\nprintf 'SIGNED\\n' >> \"$1\"\n",
            log.display()
        ),
    )
    .unwrap();
    script
}

fn build(config: &Path, out_dir: &Path, formats: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_embala"))
        .args(["build", "--config"])
        .arg(config)
        .args(["--formats", formats, "--out-dir"])
        .arg(out_dir)
        .output()
        .expect("run embala")
}

#[test]
fn windows_sign_runs_on_stub_then_setup_and_msi_in_order() {
    let dir = tmp("sign-order");
    let log = dir.join("sign.log");
    let script = write_sign_script(&dir, &log);
    // embala substitutes `$f` before `sh` sees it, so `sh sign.sh $f` passes the
    // artifact as $1; the script logs absolute paths in invocation order.
    let sign_block = format!("[sign.windows]\ncommand = \"sh {} $f\"\n", script.display());
    let config = write_fixture(&dir, &sign_block);
    let out = dir.join("dist");

    let result = build(&config, &out, "msi,setup");
    assert!(
        result.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    let setup_exe = out.join("hello-0.1.0-x86_64-setup.exe");
    let msi = out.join("hello-0.1.0-x86_64.msi");

    // Both finished artifacts carry the appended marker (the sign command ran).
    for artifact in [&setup_exe, &msi] {
        let bytes = fs::read(artifact).unwrap();
        assert!(
            bytes.ends_with(MARKER),
            "{} missing sign marker",
            artifact.display()
        );
    }

    // The marker-signed stub must be EMBEDDED, not just signed then dropped:
    // parse the overlay trailer (it sits just before the appended marker) and
    // check the signed_stub section is non-empty and ends with the marker.
    let bytes = fs::read(&setup_exe).unwrap();
    let trailer_end = bytes.len() - MARKER.len();
    let trailer = embala_setup_overlay::Trailer::parse(
        &bytes[trailer_end - embala_setup_overlay::TRAILER_LEN..trailer_end],
    )
    .expect("valid overlay trailer before the sign marker");
    let sec = trailer.signed_stub;
    assert!(sec.len > 0, "signed_stub section must be non-empty");
    let stub = &bytes[sec.offset as usize..(sec.offset + sec.len) as usize];
    assert!(
        stub.ends_with(MARKER),
        "embedded signed_stub must carry the sign marker"
    );

    // Order (spec R6): the work stub is signed BEFORE setup.exe is assembled,
    // and the finished setup.exe is signed after. The stub work file is removed,
    // but the log preserves the sequence.
    let log = fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    let stub_idx = lines
        .iter()
        .position(|l| l.contains(".embala-stub-"))
        .expect("stub was signed");
    let setup_idx = lines
        .iter()
        .position(|l| l.ends_with("setup.exe"))
        .expect("setup.exe was signed");
    assert!(
        stub_idx < setup_idx,
        "stub must be signed before setup.exe (log: {log:?})"
    );
    assert!(lines.iter().any(|l| l.ends_with(".msi")), "msi was signed");
}

#[test]
fn failing_sign_command_fails_the_build() {
    let dir = tmp("sign-fail");
    let config = write_fixture(&dir, "[sign.windows]\ncommand = \"false $f\"\n");
    let result = build(&config, &dir.join("dist"), "setup");
    assert!(
        !result.status.success(),
        "a failing sign command must fail the build"
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("sign command failed"), "stderr: {stderr}");
}

#[test]
fn no_sign_section_is_byte_reproducible() {
    let dir = tmp("sign-repro");
    let config = write_fixture(&dir, "");
    let out1 = dir.join("dist1");
    let out2 = dir.join("dist2");
    assert!(build(&config, &out1, "msi,setup").status.success());
    assert!(build(&config, &out2, "msi,setup").status.success());

    for name in ["hello-0.1.0-x86_64-setup.exe", "hello-0.1.0-x86_64.msi"] {
        assert_eq!(
            fs::read(out1.join(name)).unwrap(),
            fs::read(out2.join(name)).unwrap(),
            "{name} must be byte-identical without [sign]"
        );
    }
}
