# embala-msi

WiX-less MSI builder. Writes the Windows Installer database directly with the
`msi` and `cab` crates, so an `.msi` builds on Linux, macOS or Windows with no
WiX, Wine or .NET. Output is byte-for-byte reproducible.

Scope: per-machine install into `Program Files`, a Start-menu shortcut,
Add/Remove Programs entry, clean uninstall, major upgrades, and optionally one
payload file registered as a Windows service (`ServiceSpec`, defaulting to the
main executable).

```rust
use embala_msi::{build, FileSpec, MsiArch, MsiSpec};
use std::path::Path;

let spec = MsiSpec {
    name: "hello".into(),
    display_name: "Hello".into(),
    version: "1.0.0".into(),
    identifier: "com.example.hello".into(),
    publisher: "Example".into(),
    description: "Example app".into(),
    homepage: None,
    arch: MsiArch::X86_64,
    main_executable: "hello.exe".into(),
    files: vec![FileSpec { src: "dist/hello.exe".into(), dest: "hello.exe".into() }],
    service: None,
};
build(&spec, Path::new("hello-1.0.0-x86_64.msi"))?;
# Ok::<(), embala_msi::Error>(())
```

Part of [embala](https://github.com/hurricanehrndz/embala), usable on its own.
