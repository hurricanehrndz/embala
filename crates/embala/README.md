# embala

One config, multiple installers: `.msi`, `setup.exe`, `.app`, `.pkg` and
Chocolatey `.nupkg`, written natively from any host OS. No WiX, no Wine, no
macOS required. embala packages prebuilt artifacts; it never compiles.

```sh
embala build                      # every format with a section in embala.toml
embala build --formats msi,nupkg  # a subset, into dist/
```

A minimal `embala.toml`:

```toml
[package]
name = "hello"
display-name = "Hello"
version = "1.0.0"
identifier = "com.example.hello"
publisher = "Example"
description = "Example app"

[msi]
arch = "x86_64"
main-executable = "hello.exe"
files = [{ src = "dist/hello.exe", dest = "hello.exe" }]
# Optional: register a payload file as a Windows service.
# service = { name = "hello", start = "auto", arguments = "--service", executable = "agent.exe" }
```

Prebuilt binaries, the full config reference and signing notes are in the
[repository](https://github.com/hurricanehrndz/embala). The format writers are
separate crates: `embala-msi`, `embala-pkg`, `embala-nupkg`, `embala-setup`.
