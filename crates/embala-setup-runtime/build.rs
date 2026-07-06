//! Compiles the installer resources (baked icon + application manifest) into
//! the stub, for Windows targets only.
//!
//! Uses `zig rc` (resinator) rather than the `embed-resource`/`winresource`
//! crates because those shell out to `windres`/`llvm-rc`, and the dev shell has
//! only an x86_64 `windres` — no aarch64 windres and no llvm-rc. `zig rc`
//! compiles the same `.rc` for BOTH windows arches; the resulting `.res` is
//! handed to the cargo-zigbuild (`zig cc`) linker as an input file. On this
//! Linux host the build script is a no-op, so `cargo build` needs no zig.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Only windows targets carry resources; the host build links nothing.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let assets = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let res = out_dir.join("setup.res");

    for f in ["setup.rc", "setup.ico", "setup.manifest"] {
        println!("cargo:rerun-if-changed={}", assets.join(f).display());
    }

    // Resolve `setup.ico` / `setup.manifest` relative to the assets dir.
    let status = Command::new("zig")
        .arg("rc")
        .arg("--")
        .arg("setup.rc")
        .arg(&res)
        .current_dir(&assets)
        .status()
        .expect("run `zig rc` — the setup stub cross-build runs inside the devenv (`just stubs`)");
    assert!(status.success(), "zig rc failed to compile setup.rc");

    // Feed the compiled resource to the zig-cc linker as an input file.
    println!("cargo:rustc-link-arg={}", res.display());
}
