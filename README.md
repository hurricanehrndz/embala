# embala

> *embalar* (es/pt): to pack.

One config, every installer — build `.msi`, `.app`, `.pkg`, and Chocolatey
`.nupkg` for **any ecosystem's binaries** (Go, Rust, Zig, C, .NET AOT, Deno/Bun, …),
from **any host OS**. No WiX, no Wine, no macOS required.

**Status: v1 in progress.** Every backend is embala's own code on top of small
format-primitive crates — no packager frameworks underneath.

## Why

Every existing packager punts on the hard installer formats: cargo-packager and
electron-builder need WiX/Wine for MSI, fpm needs macOS `pkgbuild` for `.pkg`,
goreleaser shells out to `choco.exe` for nupkg. embala consumes prebuilt artifacts
plus one config file and writes every format natively.

| Format | Approach |
|---|---|
| `.msi` | `embala-msi`: direct Windows Installer database writing (`msi` + `cab` crates) |
| `.app` | native bundle layout via `apple-bundles`, PNG→icns via `tauri-icns` |
| `.pkg` | `embala-pkg`: native flat package — `apple-xar` + `apple-bom` + cpio payload |
| `.nupkg` (Chocolatey) | `embala-nupkg`: native OPC zip writer (`zip` + `quick-xml`) |

The core knows nothing about any build system: input is files + metadata, never a
compilation step.

## Workspace

- `crates/embala` — the CLI/binary and config schema
- `crates/embala-msi` — standalone WiX-less MSI builder (independently useful)

## Development

Uses [devenv](https://devenv.sh) + direnv:

```sh
direnv allow   # or: devenv shell
cargo check
```

`msitools` (wixl, msiinfo) is included in the dev shell as the differential-testing
oracle for MSI table output.
