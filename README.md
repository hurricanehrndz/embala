# embala

> *embalar* (es/pt): to pack.

One config, multiple installers — `.msi`, `.nupkg`, `.app`, `.pkg` — built
natively from **any host OS**, no WiX/Wine/macOS. Package **any ecosystem's
binaries** (Go, Rust, Zig, C, .NET AOT, Deno/Bun, …).

**Status: v1 in progress.** Every backend is embala's own code on top of small
format-primitive crates — no packager frameworks underneath.

## Install

Prebuilt binaries are attached to each
[release](https://github.com/hurricanehrndz/embala/releases), alongside a
`SHA256SUMS`:

| Platform | Archive |
|---|---|
| Linux x86_64 | `embala-<version>-x86_64-unknown-linux-musl.tar.gz` |
| Linux aarch64 | `embala-<version>-aarch64-unknown-linux-musl.tar.gz` |
| macOS Apple silicon | `embala-<version>-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `embala-<version>-x86_64-pc-windows-gnu.zip` |
| Windows aarch64 | `embala-<version>-aarch64-pc-windows-gnullvm.zip` |

The Linux builds are static musl — no glibc version to match, no runtime
dependencies at all. The Windows builds import only OS DLLs. The macOS build
links `libSystem` and friends, as every Mach-O binary must.

```sh
tar -xzf embala-0.9.1-x86_64-unknown-linux-musl.tar.gz
./embala-0.9.1-x86_64-unknown-linux-musl/embala --version
```

Building from source needs the dev shell once, to cross-compile the embedded
`setup.exe` stubs — see [Development](#development):

```sh
just stubs && cargo install --path crates/embala
```

## Why

Every existing packager punts on the hard installer formats: cargo-packager and
electron-builder need WiX/Wine for MSI, fpm needs macOS `pkgbuild` for `.pkg`,
goreleaser shells out to `choco.exe` for nupkg. embala consumes prebuilt artifacts
plus one config file and writes these formats natively.

| Format | Approach |
|---|---|
| `.msi` | `embala-msi`: direct Windows Installer database writing (`msi` + `cab` crates); optional `service` via native ServiceInstall/ServiceControl tables (on `main-executable` or a separate `executable`); optional `add-to-path` via the native Environment table; optional HKLM `registry` values via the native Registry table |
| `.app` | native bundle layout via `apple-bundles`, PNG→icns via `tauri-icns` |
| `.pkg` | `embala-pkg`: native flat package — `apple-xar` + `apple-bom` + cpio payload; optional `scripts` dir packed like `pkgbuild --scripts` |
| `.nupkg` (Chocolatey) | `embala-nupkg`: native OPC zip writer (`zip` + `quick-xml`) |

The core knows nothing about any build system: input is files + metadata, never a
compilation step.

Every format is verified against its real target, not just unit-tested:
the `.msi` installs via `msiexec` on Windows 11, and `just nupkg-e2e` runs
the `.nupkg` on Windows 11: `choco install`, upgrade, a locked upgrade and
uninstall, a `display-name` rename and uninstall of a `scripts = "generate"`
package, the same scripts run as SYSTEM without Chocolatey, an embedded
package still shimmed, and a plain package refused by its guard. The `.app`
launches via `open` and the `.pkg` installs via
`installer` on macOS 26. The pkg/BOM/cpio and MSI writers are additionally
differential-tested against Apple's `pkgbuild`/`productbuild`/`mkbom` and
against msitools respectively.

## Versions

`[package].version` (or `--version`) is numeric `x.y.z`, optionally followed by
a build number: `x.y.z.build`, with the build no higher than 65535. Every format
carries the full string, except that `.app` puts only `x.y.z` in
`CFBundleShortVersionString`. Windows Installer ignores the build field when it
compares versions, so the `.msi` replaces any install with the same `x.y.z`.
That means a build-only bump upgrades, but a build-only downgrade isn't blocked.

Inside the `.msi`, each payload file that is a PE with a version resource
gets that file version in `File.Version`, as WiX does. On an upgrade, a
component whose installed file version is higher than the incoming one is
skipped by costing and then removed with the old product, so a file's version
must never go down between releases, and a binary must not lose its version
resource. Files that never had a version resource are unversioned and always
replaced on a major upgrade.

## MSI registry values

`[msi].registry` writes registry values on install and removes them on
uninstall:

```toml
[msi]
registry = [
  { root = "HKLM", key = 'Software\Microsoft\Windows\CurrentVersion\Run', name = "AI Usage Monitor", value = '"[INSTALLDIR]AI Usage Monitor.exe"' },
  { root = "HKLM", key = 'Software\Example\App', name = "Level", value = 1 },
]
```

- `root`: only `"HKLM"`. The MSI is per-machine and is often installed as
  SYSTEM (Intune, SCCM), where HKCU is SYSTEM's own hive, not the user's.
  Values go to the 64-bit view (not `WOW6432Node`).
- `key`: the path below the root, `\`-separated, with no leading, trailing,
  or doubled `\`. TOML literal strings (`'...'`) avoid escaping backslashes.
- `name`: the value name. Leave it out to set the key's default value.
- `value`: a string is written as REG_SZ, an integer as REG_DWORD (32-bit
  signed). Strings are MSI
  [formatted strings](https://learn.microsoft.com/en-us/windows/win32/msi/formatted)
  (`[INSTALLDIR]` expands; a literal `[` is `[\[]`). `[~]` (REG_MULTI_SZ)
  and empty strings are rejected.

All values share one component with a GUID derived from the identifier, so
they live and die with the product: uninstall removes each value.

## nupkg install scripts and shims

By default an embedded `.nupkg` ships its files under `tools/` with no install
script, and Chocolatey shims each exe into `C:\ProgramData\chocolatey\bin`.
Three opt-in keys change that. A config that uses none of them builds the same
bytes as before they existed.

```toml
# Generated scripts: work under choco and under a script-only installer.
[nupkg]
style = "embedded"
scripts = "generate"
files = [{ src = "dist/hello.exe", dest = "hello.exe" }]
```

- `scripts = "generate"` packs a generated install and uninstall script
  (Windows PowerShell 5.1, no Chocolatey cmdlets). The install copies the
  payload to `<ProgramFiles>\<display-name> (nupkg)`, for example
  `C:\Program Files\Embala Hello (nupkg)`, and appends that directory to the
  machine PATH. It records the directory under
  `HKLM\SOFTWARE\embala\nupkg\<identifier>`, so a later version with a
  different `display-name` removes the old directory. An upgrade replaces the
  directory as a whole. If a file in it is in use, install and uninstall fail
  with `<display-name> is in use; close it and retry` and change nothing. In
  an embedded package every `.exe` also gets an `.ignore` file, so Chocolatey
  does not shim on top of the PATH entry. `package.display-name` must be a
  valid Windows directory name.
- `scripts = { install = "...", uninstall = "...", before-modify = "..." }`
  packs your own scripts, from any path relative to the config, under
  `chocolateyInstall.ps1`, `chocolateyUninstall.ps1` and
  `chocolateyBeforeModify.ps1`. `install` is required. Without `uninstall`
  the build prints `nupkg: warning: scripts has no uninstall, ...`.
- `shim = false` on a file adds `tools/<dest>.ignore`, so Chocolatey does not
  shim that exe.
- `style = "plain"` builds a package that is not a Chocolatey package: the
  scripts are packed as `tools/install.ps1`, `uninstall.ps1` and
  `before-modify.ps1`, and a guard `chocolateyInstall.ps1` makes
  `choco install` fail with `<display-name> is not a Chocolatey package`.

```toml
# Own scripts, packed under Chocolatey's names. shim = false because the
# script puts hello.exe on PATH itself.
[nupkg]
style = "embedded"
scripts = { install = "scripts/install.ps1", uninstall = "scripts/uninstall.ps1" }
files = [{ src = "dist/hello.exe", dest = "hello.exe", shim = false }]

# Keep choco's shim for hello.exe, hide a helper exe (choco only).
[nupkg]
style = "embedded"
files = [
  { src = "dist/hello.exe",  dest = "hello.exe" },
  { src = "dist/helper.exe", dest = "helper.exe", shim = false },
]

# Not a Chocolatey package. scripts is required; "generate" or a table.
[nupkg]
style = "plain"
scripts = "generate"
files = [{ src = "dist/hello.exe", dest = "hello.exe" }]
```

`embala build` rejects these configs before writing anything:

| Config | Error starts with |
|---|---|
| `scripts` or `shim` with `style = "download"` | `nupkg: scripts and shim are not valid with style "download"` |
| `style = "plain"` without `scripts` | `nupkg: style "plain" requires scripts` |
| `shim = false` with `style = "plain"` | `nupkg: shim = false has no effect with style "plain"` |
| a `scripts` table without `install` | `nupkg: a scripts table requires install` |
| with `scripts` or `style = "plain"`, a dest named like any script of either set (case-insensitive) | `nupkg: dest "..." collides with an install script name` |
| a dest equal to an `.ignore` file embala emits | `nupkg: dest "..." collides with the .ignore file generated for "..."` |
| `shim` on a `[msi]` or `[setup]` file | a parse error: `` unknown field `shim`  `` |
| `scripts = "generate"` with a `display-name` that is not a valid directory name | `nupkg: scripts = "generate" installs to "<display-name> (nupkg)"` |
| `scripts = "generate"` with a dest containing `\` or `:` | `nupkg: scripts = "generate" copies dest "..." into the install directory` |

Install either the `.msi` or the generated-script `.nupkg` on a machine, not
both. They use different directories (`<display-name>` and
`<display-name> (nupkg)`) and each removes only its own PATH entry, but with
the MSI's `add-to-path` both directories end up on PATH and the first one
wins, so uninstalling it silently hands the command to the other copy.

## Signing — SmartScreen & Gatekeeper

embala artifacts are **signable**: an optional `[sign]` section in `embala.toml`
runs your own signing tool (osslsigncode, signtool, jsign, rcodesign) on each
built `.msi` / `setup.exe` / `.app` / `.pkg`, including the embedded uninstaller.
embala never implements a signature format — it only shells out. Full guide:
**[Code Signing](https://github.com/hurricanehrndz/embala/wiki/Code-Signing)**.

```toml
[sign.windows]
command = "jsign --keystore cert.pfx --storepass pw --storetype PKCS12 $f"

[sign.macos]
command = "rcodesign sign --p12-file devid.p12 $f"
```

Read this honestly: signing removes the *unknown-publisher* prompt only for a
certificate the target machine already trusts. **A self-signed cert changes
neither SmartScreen nor Gatekeeper** for outside users. What clears those
prompts is earned reputation (SignPath's OSS program or an EV cert on Windows)
and an Apple Developer ID + notarization on macOS.

If you ship **unsigned** (or self-signed to outside users), the escape hatches
still apply:

Windows SmartScreen shows "Windows protected your PC" for MSIs downloaded via a
browser; **More info → Run anyway** proceeds. Files arriving via other channels
(network share, `scp`, CI artifacts) install without the prompt.

macOS `.app` bundles that aren't signed with a Developer ID and notarized:

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
just stubs     # once per fresh clone: cross-compiles the setup.exe stubs
cargo check
```

The Windows `setup.exe` runtime stubs are build products, not committed. A fresh
clone must run `just stubs` (dev shell) once before `cargo build` / `just test`.

`msitools` (wixl, msiinfo) is included in the dev shell as the differential-testing
oracle for MSI table output.

`just nupkg-e2e` (dev shell) builds test nupkgs and checks them on a Windows 11
VM driven by the windows-test-rig `rig` script, printing a `PASS` or `FAIL` line
per check. It needs `rig` at `RIG` (default
`~/.claude/skills/windows-test-rig/scripts/rig`), a VM config named by
`RIG_E2E_VM` (default `win11-test`) whose `clean` snapshot has no Chocolatey,
and guest internet: the run installs Chocolatey from community.chocolatey.org.
It resets the VM first and resets and shuts it down at the end, and takes a
few minutes.

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
