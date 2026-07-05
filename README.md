# embala

> *embalar* (es/pt): to pack.

One config, multiple installers — `.msi`, `.nupkg`, `.app`, `.pkg` — built
natively from **any host OS**, no WiX/Wine/macOS. Package **any ecosystem's
binaries** (Go, Rust, Zig, C, .NET AOT, Deno/Bun, …).

**Status: v1 in progress.** Every backend is embala's own code on top of small
format-primitive crates — no packager frameworks underneath.

## Why

Every existing packager punts on the hard installer formats: cargo-packager and
electron-builder need WiX/Wine for MSI, fpm needs macOS `pkgbuild` for `.pkg`,
goreleaser shells out to `choco.exe` for nupkg. embala consumes prebuilt artifacts
plus one config file and writes these formats natively.

| Format | Approach |
|---|---|
| `.msi` | `embala-msi`: direct Windows Installer database writing (`msi` + `cab` crates) |
| `.app` | native bundle layout via `apple-bundles`, PNG→icns via `tauri-icns` |
| `.pkg` | `embala-pkg`: native flat package — `apple-xar` + `apple-bom` + cpio payload |
| `.nupkg` (Chocolatey) | `embala-nupkg`: native OPC zip writer (`zip` + `quick-xml`) |

The core knows nothing about any build system: input is files + metadata, never a
compilation step.

Every format is verified against its real target, not just unit-tested:
the `.msi` installs via `msiexec` and the `.nupkg` via `choco install` on
Windows 11, the `.app` launches via `open` and the `.pkg` installs via
`installer` on macOS 26. The pkg/BOM/cpio and MSI writers are additionally
differential-tested against Apple's `pkgbuild`/`productbuild`/`mkbom` and
against msitools respectively.

## Unsigned artifacts on Windows — SmartScreen

Like the `.app` bundles below, **embala's MSIs are unsigned** (signing is v2
scope). Windows SmartScreen shows "Windows protected your PC" for unsigned
MSIs downloaded via a browser; **More info → Run anyway** proceeds. Files
arriving via other channels (network share, `scp`, CI artifacts) install
without the prompt.

## macOS `.app` bundles and Gatekeeper — read this

**embala's `.app` bundles are unsigned and not notarized** (signing/notarization
is v2 scope). What that means in practice:

- Bundles **downloaded via a browser** get the `com.apple.quarantine` xattr and
  Gatekeeper refuses to launch them with a "cannot be opened" dialog.
  Escape hatches: right-click → **Open** (once per app), or
  `xattr -d com.apple.quarantine "/path/to/Your App.app"`.
- Bundles arriving via **scp, rsync, or a local copy** carry no quarantine
  flag and run normally.
- The bundle executable must be a **Mach-O binary**: as of macOS 26,
  LaunchServices refuses to launch a bundle whose `CFBundleExecutable` is a
  script (`open` fails with error -10669, even ad-hoc signed). Direct
  execution of `Contents/MacOS/<name>` still works either way.

## Workspace

- `crates/embala` — the CLI/binary and config schema
- `crates/embala-msi` — standalone WiX-less MSI builder (independently useful)
- `crates/embala-pkg` — standalone macOS flat-package builder (xar + BOM + cpio)
- `crates/embala-nupkg` — standalone Chocolatey package builder

## Development

Uses [devenv](https://devenv.sh) + direnv:

```sh
direnv allow   # or: devenv shell
cargo check
```

`msitools` (wixl, msiinfo) is included in the dev shell as the differential-testing
oracle for MSI table output.

## Attribution

embala is MIT-licensed (see [LICENSE](LICENSE)) and stands on prior work:

- **[deno](https://github.com/denoland/deno) desktop** (MIT, © 2018–2026 the
  Deno authors) — embala-msi's MSI authoring (summary info, table shapes,
  embedded cab, the table-stream sort fix) is ported and adapted from
  `cli/tools/desktop.rs`; each ported file carries a header notice.
- **[apple-bom](https://crates.io/crates/apple-bom)** (Apache-2.0 OR MIT,
  © Gregory Szorc) — embala-pkg's `bom_builder` ports the crate's builder
  with correctness fixes (used here under the MIT option); the module header
  catalogs every divergence. The rest of the `apple-*` suite (apple-xar,
  apple-bundles) is used as regular dependencies.
- **Format references** (no code taken): Apple's `pkgbuild`/`productbuild`/
  `mkbom`/`lsbom` output and [msitools](https://gitlab.gnome.org/GNOME/msitools)'
  wixl served as differential-testing oracles;
  [bomutils](https://github.com/hogliux/bomutils)' documentation of the BOM
  format aided the dissection.
