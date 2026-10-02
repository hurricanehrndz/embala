//! WiX-less MSI builder.
//!
//! Writes the Windows Installer database (CFB container + relational tables +
//! embedded MSZIP cabinet) directly via the `msi` and `cab` crates, so MSIs
//! build on any OS with no WiX, Wine, or .NET.
//!
//! Scope for v1: install files to Program Files, Start-menu shortcut,
//! Add/Remove Programs entry, clean uninstall, MajorUpgrade, and optionally
//! the main executable as a Windows service. Nothing else.
//
// Portions ported from deno desktop (cli/tools/desktop.rs),
// Copyright 2018-2026 the Deno authors, MIT license.

mod cabinet;
mod guids;
mod resort;
mod summary;
mod tables;

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Windows Installer only supports 64-bit targets in embala.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsiArch {
    X86_64,
    Aarch64,
}

impl MsiArch {
    /// Summary-info `Template` architecture token.
    pub fn template(self) -> &'static str {
        match self {
            MsiArch::X86_64 => "x64",
            MsiArch::Aarch64 => "Arm64",
        }
    }

    /// Token used in artifact file names (`hello-0.1.0-x86_64.msi`).
    pub fn as_str(self) -> &'static str {
        match self {
            MsiArch::X86_64 => "x86_64",
            MsiArch::Aarch64 => "aarch64",
        }
    }
}

/// Service start type: `ServiceInstall.StartType` (SERVICE_AUTO_START,
/// SERVICE_DEMAND_START, SERVICE_DISABLED).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStart {
    Auto,
    Demand,
    Disabled,
}

/// Installs the main executable as an own-process service via the native
/// ServiceInstall/ServiceControl tables (no custom actions). Mirrors WiX
/// `<ServiceInstall Type="ownProcess" ErrorControl="normal">` plus
/// `<ServiceControl Start="install" Stop="both" Remove="uninstall" Wait="yes">`;
/// a `Disabled` service is not started (StartServices would fail on it).
#[derive(Debug, Clone)]
pub struct ServiceSpec {
    /// SCM service (key) name.
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub start: ServiceStart,
    /// Command-line arguments the SCM passes to the executable.
    pub arguments: Option<String>,
}

/// A payload file: `src` on disk (already resolved to a real path), `dest`
/// the `/`-separated install path relative to the install directory.
#[derive(Debug, Clone)]
pub struct FileSpec {
    pub src: PathBuf,
    pub dest: String,
}

/// Everything needed to author one MSI. Deliberately independent of the
/// embala config types so the crate is usable (and signable) on its own.
#[derive(Debug, Clone)]
pub struct MsiSpec {
    /// Short machine name (registry key, artifact file name).
    pub name: String,
    /// Human name: ProductName, install folder, shortcut label.
    pub display_name: String,
    /// Numeric x.y.z, as MSI requires.
    pub version: String,
    /// Reverse-DNS identity; the GUID derivation seed.
    pub identifier: String,
    /// Manufacturer in ARP.
    pub publisher: String,
    /// ARP comments.
    pub description: String,
    /// ARP "more information" link.
    pub homepage: Option<String>,
    pub arch: MsiArch,
    /// Dest of the file the Start-menu shortcut targets.
    pub main_executable: String,
    pub files: Vec<FileSpec>,
    /// Register `main_executable` as a Windows service.
    pub service: Option<ServiceSpec>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // The MSI is authored in Windows-1252; non-ASCII would silently mangle.
    #[error("msi {field} must be ASCII (MSI databases are Windows-1252): {value:?}")]
    NonAscii { field: String, value: String },
    #[error("msi: no files to package")]
    NoFiles,
    #[error("msi dest {0:?} must be a relative /-separated path without empty or '..' segments")]
    InvalidDest(String),
    #[error("msi main-executable {0:?} does not match any file dest")]
    MainExecutableNotFound(String),
    #[error("msi: two files share the dest {0:?}")]
    DuplicateDest(String),
    #[error(
        "msi service name {0:?} must be 1-256 characters without '/' or '\\' \
         (Windows SCM rules)"
    )]
    InvalidServiceName(String),
    #[error("msi payload too large (File.FileSize is a 32-bit int): {0}")]
    FileTooLarge(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Build the `.msi` described by `spec` at `out` (parent dirs are created).
pub fn build(spec: &MsiSpec, out: &Path) -> Result<()> {
    validate(spec)?;
    let staged = tables::stage(spec)?;
    let cab_bytes = cabinet::build(&staged.files)?;

    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    let mut package = msi::Package::create(msi::PackageType::Installer, &mut cursor)?;
    // The `msi` crate defaults the database string pool to UTF-8 (65001),
    // which msiexec rejects outright ("This installation package could not
    // be opened"): an MSI codepage must be a valid ANSI codepage. All our
    // strings are ASCII, so Windows-1252 encodes them identically.
    package.set_database_codepage(msi::CodePage::Windows1252);
    summary::write(&mut package, spec);
    tables::write(&mut package, spec, &staged)?;
    // Embed the cabinet as the stream Media.Cabinet points at (the "#"
    // prefix in the table means "internal stream of this name").
    package
        .write_stream(tables::CAB_STREAM)?
        .write_all(&cab_bytes)?;
    package.flush()?;
    drop(package);

    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(out, cursor.into_inner())?;
    // Re-sort table streams into the order msiexec demands and pin the CFB
    // wall-clock timestamps for reproducible output.
    resort::finalize(out)?;
    Ok(())
}

fn ensure_ascii(field: &str, value: &str) -> Result<()> {
    if value.is_ascii() && !value.chars().any(|c| c.is_ascii_control()) {
        Ok(())
    } else {
        Err(Error::NonAscii {
            field: field.to_string(),
            value: value.to_string(),
        })
    }
}

fn validate(spec: &MsiSpec) -> Result<()> {
    ensure_ascii("name", &spec.name)?;
    ensure_ascii("display-name", &spec.display_name)?;
    ensure_ascii("version", &spec.version)?;
    ensure_ascii("identifier", &spec.identifier)?;
    ensure_ascii("publisher", &spec.publisher)?;
    ensure_ascii("description", &spec.description)?;
    if let Some(homepage) = &spec.homepage {
        ensure_ascii("homepage", homepage)?;
    }
    ensure_ascii("main-executable", &spec.main_executable)?;
    if spec.files.is_empty() {
        return Err(Error::NoFiles);
    }
    let mut seen = std::collections::BTreeSet::new();
    for file in &spec.files {
        ensure_ascii("file dest", &file.dest)?;
        let segments: Vec<&str> = file.dest.split('/').collect();
        if segments.iter().any(|s| s.is_empty() || *s == "..") {
            return Err(Error::InvalidDest(file.dest.clone()));
        }
        if !seen.insert(&file.dest) {
            return Err(Error::DuplicateDest(file.dest.clone()));
        }
    }
    if !spec.files.iter().any(|f| f.dest == spec.main_executable) {
        return Err(Error::MainExecutableNotFound(spec.main_executable.clone()));
    }
    if let Some(service) = &spec.service {
        ensure_ascii("service name", &service.name)?;
        // CreateService: at most 256 chars, no slashes. Brackets would be
        // expanded as properties (ServiceInstall.Name is Formatted).
        if service.name.is_empty()
            || service.name.len() > 256
            || service.name.contains(['/', '\\', '[', ']'])
        {
            return Err(Error::InvalidServiceName(service.name.clone()));
        }
        for (field, value) in [
            ("service display-name", &service.display_name),
            ("service description", &service.description),
            ("service arguments", &service.arguments),
        ] {
            if let Some(value) = value {
                ensure_ascii(field, value)?;
            }
        }
    }
    Ok(())
}
