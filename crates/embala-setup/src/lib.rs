//! Windows `setup.exe` builder (prebuilt stub + PE overlay).
//!
//! Honors embala's byte-writer invariant: `embala build` never compiles. The
//! Win32 runtime stub (`embala-setup-runtime`) is cross-compiled once by
//! `just stubs` and committed under `stubs/<target>/setup-stub.exe`; this crate
//! `include_bytes!`-embeds it and appends `payload.zip + install.lua +
//! uninstall.lua + manifest + trailer` as a PE overlay — pure byte-writing, no
//! compile, no network. Output is byte-reproducible: the payload zip pins entry
//! mtimes and sorts entries, so the whole `setup.exe` is a pure function of
//! (stub, scripts, manifest, payload).
//!
//! Scope grows per phase. Phase 3 carries the real manifest (product identity +
//! resolved install mode, spec R8) and ships the caller's `install.lua` /
//! `uninstall.lua` bytes verbatim (spec R7) — the runtime's mlua host runs them.
//! When no `install.lua` is supplied the Phase-2 extraction-only placeholder
//! still ships. The wizard arrives with Phase 5.

use std::io::{Cursor, Write as _};
use std::path::{Path, PathBuf};

use embala_setup_overlay::{FORMAT_VERSION, Manifest, Package, Section, Trailer};
use zip::write::SimpleFileOptions;

/// The committed runtime stubs, embedded at this crate's compile time (spec R2).
/// The aarch64 GNU-family triple is `-gnullvm` (there is no aarch64 mingw).
const STUB_X86_64: &[u8] = include_bytes!("../stubs/x86_64-pc-windows-gnu/setup-stub.exe");
const STUB_AARCH64: &[u8] = include_bytes!("../stubs/aarch64-pc-windows-gnullvm/setup-stub.exe");

/// Fixed zip entry mtime (2020-01-01T00:00:00Z) for reproducible output — the
/// same instant embala-nupkg/embala-msi pin.
const FIXED_DOS_TIME: (u16, u8, u8, u8, u8, u8) = (2020, 1, 1, 0, 0, 0);

/// Phase-2 `install.lua`: the runtime extracts natively and does not yet run
/// Lua, so this is a plaintext placeholder (spec R7: scripts ship verbatim).
const INSTALL_LUA_PLACEHOLDER: &[u8] = b"-- embala phase-2 placeholder: extraction is native\n";

/// Windows arch selecting the embedded stub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupArch {
    X86_64,
    Aarch64,
}

impl SetupArch {
    /// Token used in artifact file names (`hello-0.1.0-x86_64-setup.exe`).
    pub fn as_str(self) -> &'static str {
        match self {
            SetupArch::X86_64 => "x86_64",
            SetupArch::Aarch64 => "aarch64",
        }
    }

    /// The committed stub bytes for this arch.
    fn stub(self) -> &'static [u8] {
        match self {
            SetupArch::X86_64 => STUB_X86_64,
            SetupArch::Aarch64 => STUB_AARCH64,
        }
    }
}

/// Default install mode baked into the manifest (spec R13). Config-independent,
/// mirroring the config's own enum; a `/mode=` CLI flag overrides it at install
/// time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMode {
    PerUser,
    PerMachine,
    UserChoice,
}

impl InstallMode {
    /// The manifest token the runtime parses (matches the spec's CLI vocabulary).
    fn as_str(self) -> &'static str {
        match self {
            InstallMode::PerUser => "per-user",
            InstallMode::PerMachine => "per-machine",
            InstallMode::UserChoice => "user-choice",
        }
    }
}

/// Product identity carried into the manifest and, at install time, the
/// `embala.package` Lua table + the ARP entry. Mirrors `[package]`.
#[derive(Debug, Clone)]
pub struct ProductInfo {
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub identifier: String,
    pub publisher: String,
    pub description: String,
    pub homepage: Option<String>,
    pub license: Option<String>,
}

/// A payload file: `src` on disk (already resolved to a real path), `dest` the
/// `/`-separated install path relative to the install directory.
#[derive(Debug, Clone)]
pub struct FileSpec {
    pub src: PathBuf,
    pub dest: String,
}

/// Everything needed to author one `setup.exe`. Deliberately independent of the
/// embala config types so the crate is usable on its own.
#[derive(Debug, Clone)]
pub struct SetupSpec {
    pub arch: SetupArch,
    pub install_mode: InstallMode,
    pub product: ProductInfo,
    pub files: Vec<FileSpec>,
    /// Raw `install.lua` bytes shipped verbatim (spec R7). `None` ships the
    /// extraction-only Phase-2 placeholder.
    pub install_lua: Option<Vec<u8>>,
    /// Raw `uninstall.lua` bytes shipped verbatim. `None` ships an empty
    /// section; the runtime then falls back to the log-driven uninstaller.
    pub uninstall_lua: Option<Vec<u8>>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("setup: no files to package")]
    NoFiles,
    #[error(
        "setup dest {0:?} must be a relative /-separated path without empty, '..', or drive segments"
    )]
    InvalidDest(String),
    #[error("setup: two files share the dest {0:?}")]
    DuplicateDest(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Build the `setup.exe` described by `spec` at `out` (parent dirs are created).
pub fn build(spec: &SetupSpec, out: &Path) -> Result<()> {
    if spec.files.is_empty() {
        return Err(Error::NoFiles);
    }

    // dest -> file bytes, normalized to forward slashes and traversal-checked,
    // then sorted so the zip entry order (and thus the whole exe) is stable.
    let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(spec.files.len());
    let mut seen = std::collections::BTreeSet::new();
    for file in &spec.files {
        let dest = validate_dest(&file.dest)?;
        if !seen.insert(dest.clone()) {
            return Err(Error::DuplicateDest(dest));
        }
        entries.push((dest, std::fs::read(&file.src)?));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let payload = build_payload_zip(&entries)?;
    let stub = spec.arch.stub();
    // Ship the caller's script bytes verbatim (spec R7); fall back to the
    // Phase-2 extraction-only placeholder when no install.lua was supplied.
    let install: &[u8] = spec
        .install_lua
        .as_deref()
        .unwrap_or(INSTALL_LUA_PLACEHOLDER);
    let uninstall: &[u8] = spec.uninstall_lua.as_deref().unwrap_or(b"");
    let manifest = manifest_bytes(spec);

    // Section offsets are cumulative; order matches the trailer's field
    // semantics: [stub][payload.zip][install.lua][uninstall.lua][manifest].
    let stub_sec = Section {
        offset: 0,
        len: stub.len() as u64,
    };
    let payload_sec = after(&stub_sec, payload.len());
    let install_sec = after(&payload_sec, install.len());
    let uninstall_sec = after(&install_sec, uninstall.len());
    let manifest_sec = after(&uninstall_sec, manifest.len());
    let trailer = Trailer {
        version: FORMAT_VERSION,
        stub: stub_sec,
        payload_zip: payload_sec,
        install_lua: install_sec,
        uninstall_lua: uninstall_sec,
        manifest: manifest_sec,
    };

    let mut bytes = Vec::with_capacity(
        manifest_sec.offset as usize + manifest.len() + trailer.to_bytes().len(),
    );
    bytes.extend_from_slice(stub);
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(install);
    bytes.extend_from_slice(uninstall);
    bytes.extend_from_slice(&manifest);
    bytes.extend_from_slice(&trailer.to_bytes());

    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(out, bytes)?;
    Ok(())
}

/// A section starting immediately after `prev`.
fn after(prev: &Section, len: usize) -> Section {
    Section {
        offset: prev.offset + prev.len,
        len: len as u64,
    }
}

/// Normalize `\` to `/` and reject anything that could write outside the target
/// dir at extract time (absolute, drive-relative, `..`, or empty segments).
fn validate_dest(dest: &str) -> Result<String> {
    let normalized = dest.replace('\\', "/");
    // A drive segment (`C:`) or a bare drive-relative path escapes the target.
    if normalized.contains(':') {
        return Err(Error::InvalidDest(dest.to_string()));
    }
    if normalized.split('/').any(|s| s.is_empty() || s == "..") {
        return Err(Error::InvalidDest(dest.to_string()));
    }
    Ok(normalized)
}

/// Build the deterministic payload zip (mirrors embala-nupkg's option style).
fn build_payload_zip(entries: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    let (year, month, day, hour, minute, second) = FIXED_DOS_TIME;
    let mtime = zip::DateTime::from_date_and_time(year, month, day, hour, minute, second)
        .expect("fixed timestamp is a valid DOS datetime");
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(mtime);

    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, data) in entries {
        writer.start_file(name, options)?;
        writer.write_all(data)?;
    }
    Ok(writer.finish()?.into_inner())
}

/// Build the deterministic manifest (spec R8) via the shared overlay type: the
/// runtime deserializes the very same struct. Stable field order, no timestamps.
fn manifest_bytes(spec: &SetupSpec) -> Vec<u8> {
    let p = &spec.product;
    Manifest {
        format_version: FORMAT_VERSION,
        arch: spec.arch.as_str().to_string(),
        install_mode: spec.install_mode.as_str().to_string(),
        package: Package {
            name: p.name.clone(),
            display_name: p.display_name.clone(),
            version: p.version.clone(),
            identifier: p.identifier.clone(),
            publisher: p.publisher.clone(),
            description: p.description.clone(),
            homepage: p.homepage.clone(),
            license: p.license.clone(),
        },
    }
    .to_bytes()
}
