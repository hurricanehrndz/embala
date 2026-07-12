//! Prebuilt stubs are build products, not committed (see `just stubs`). Fail
//! the build with an actionable message instead of a raw `include_bytes!` error.

fn main() {
    for stub in [
        "stubs/x86_64-pc-windows-gnu/setup-stub.exe",
        "stubs/aarch64-pc-windows-gnullvm/setup-stub.exe",
    ] {
        println!("cargo::rerun-if-changed={stub}");
        assert!(
            std::path::Path::new(stub).exists(),
            "prebuilt stubs are generated, not committed: run `just stubs` (dev shell) to create crates/embala-setup/stubs/**"
        );
    }
}
