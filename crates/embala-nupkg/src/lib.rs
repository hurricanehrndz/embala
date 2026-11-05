//! Chocolatey `.nupkg` builder.
//!
//! Writes the OPC package (a zip with a nuspec, OPC bookkeeping parts, and a
//! `tools/` payload) directly, so Chocolatey packages build on any OS with no
//! choco, NuGet, or .NET. Builds are byte-stable: fixed entry order, pinned
//! zip timestamps, and deterministic part names.
//!
//! Payloads are plain executables, either embedded in the package or
//! downloaded at install time. By default Chocolatey's automatic `tools/*.exe`
//! shimming puts them on PATH; an embedded or plain package can instead carry
//! its own install scripts.

use std::collections::BTreeSet;
use std::io::{Cursor, Write as _};
use std::path::{Path, PathBuf};

use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesText, Event};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;
use zip::write::SimpleFileOptions;

/// Fixed namespace for the deterministic psmdcp part name (UUIDv5 of
/// identifier + version). Changing it changes every published package's
/// bytes — never change it.
const NAMESPACE: Uuid = Uuid::from_u128(0x9d6c1e58_2f4b_4a03_b7e9_64d05c8a3f12);

/// Fixed zip entry mtime (2020-01-01T00:00:00Z) for reproducible output —
/// the same instant embala-msi pins for its summary stream.
const FIXED_DOS_TIME: (u16, u8, u8, u8, u8, u8) = (2020, 1, 1, 0, 0, 0);

const NUSPEC_XMLNS: &str = "http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd";
const CONTENT_TYPES_XMLNS: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const RELS_XMLNS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const CORE_PROPS_XMLNS: &str =
    "http://schemas.openxmlformats.org/package/2006/metadata/core-properties";
const DC_XMLNS: &str = "http://purl.org/dc/elements/1.1/";

/// A payload file: `src` on disk (already resolved to a real path), `dest`
/// the `/`-separated path relative to the package's `tools/` directory.
#[derive(Debug, Clone)]
pub struct FileSpec {
    pub src: PathBuf,
    pub dest: String,
    /// false: an embedded package adds an empty `tools/<dest>.ignore`, which
    /// stops Chocolatey from shimming the file. Plain packages ignore it.
    pub shim: bool,
}

/// The install scripts an embedded or plain package carries.
#[derive(Debug, Clone)]
pub enum Scripts {
    /// embala writes the install and uninstall scripts: they copy the payload
    /// to `<ProgramFiles>\<display-name> (nupkg)` and put that directory on
    /// the machine PATH, with no Chocolatey helpers.
    Generate,
    /// The user's own scripts (paths already resolved by the caller), packed
    /// byte-for-byte under the style's script names.
    Supplied {
        install: PathBuf,
        uninstall: Option<PathBuf>,
        before_modify: Option<PathBuf>,
    },
}

/// In-package names for install, uninstall and before-modify: Chocolatey's
/// for embedded packages, the plain set for plain ones.
const CHOCO_SCRIPTS: [&str; 3] = [
    "chocolateyInstall.ps1",
    "chocolateyUninstall.ps1",
    "chocolateyBeforeModify.ps1",
];
const PLAIN_SCRIPTS: [&str; 3] = ["install.ps1", "uninstall.ps1", "before-modify.ps1"];

/// How the payload reaches the machine.
#[derive(Debug, Clone)]
pub enum Style {
    /// Payload files ship inside the package under `tools/`. Chocolatey runs
    /// the scripts, if any, then shims every `tools/*.exe` that has no
    /// `.ignore` onto PATH.
    Embedded {
        files: Vec<FileSpec>,
        scripts: Option<Scripts>,
    },
    /// Not a Chocolatey package: the embedded payload, scripts under the
    /// plain names, and a guard `chocolateyInstall.ps1` that only throws, so
    /// `choco install` fails instead of half-installing.
    Plain {
        files: Vec<FileSpec>,
        scripts: Scripts,
    },
    /// The package carries only a `chocolateyinstall.ps1` that downloads the
    /// payload into `tools/` (checksum-verified) at install time.
    Download {
        url: String,
        /// sha256 of the payload, lowercase/uppercase hex, 64 chars.
        checksum: String,
        /// File name the download is saved as under `tools/`.
        dest: String,
    },
}

/// Everything needed to author one nupkg. Deliberately independent of the
/// embala config types so the crate is usable on its own.
#[derive(Debug, Clone)]
pub struct NupkgSpec {
    /// Package id (nuspec `<id>`, artifact file name).
    pub name: String,
    /// Human name: nuspec `<title>`.
    pub display_name: String,
    pub version: String,
    /// Reverse-DNS identity; seeds the deterministic psmdcp part name.
    pub identifier: String,
    /// nuspec `<authors>`.
    pub publisher: String,
    pub description: String,
    /// nuspec `<projectUrl>`.
    pub homepage: Option<String>,
    /// License identifier ("MIT", …): emits nuspec `<license type="expression">`
    /// and names the license in the generated `tools/LICENSE.txt` (embedded
    /// style only).
    pub license: Option<String>,
    /// nuspec `<copyright>`.
    pub copyright: Option<String>,
    /// nuspec `<tags>` (space-joined; empty = omit).
    pub tags: Vec<String>,
    /// nuspec `<releaseNotes>`.
    pub release_notes: Option<String>,
    /// nuspec `<requireLicenseAcceptance>` (emitted only when true).
    pub require_license_acceptance: bool,
    /// nuspec `<projectSourceUrl>`.
    pub project_source_url: Option<String>,
    /// nuspec `<packageSourceUrl>`.
    pub package_source_url: Option<String>,
    /// nuspec `<licenseUrl>`.
    pub license_url: Option<String>,
    /// Source image path (already resolved by the caller); packed at the zip
    /// root as `icon.png`/`icon.jpg`, named by the detected format.
    pub icon: Option<PathBuf>,
    /// nuspec `<iconUrl>`.
    pub icon_url: Option<String>,
    pub style: Style,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("nupkg: no files to package")]
    NoFiles,
    #[error("nupkg dest {0:?} must be a relative /-separated path without empty or '..' segments")]
    InvalidDest(String),
    #[error("nupkg: two files share the dest {0:?}")]
    DuplicateDest(String),
    #[error("nupkg dest {0:?} collides with a generated tools/ file")]
    ReservedDest(String),
    #[error(
        "nupkg: dest {0:?} collides with an install script name (with scripts set or style \
         \"plain\", no dest may be a Chocolatey or plain script name)"
    )]
    ScriptDest(String),
    #[error("nupkg: dest {0:?} collides with the .ignore file generated for {1:?}")]
    IgnoreDest(String, String),
    #[error(
        "nupkg: scripts = \"generate\" installs to \"<display-name> (nupkg)\", so \
         package.display-name must be a valid Windows directory name: non-empty, no \
         <>:\"/\\|?* or control characters, not ending in a space or dot (got {0:?})"
    )]
    InvalidDisplayName(String),
    #[error(
        "nupkg: scripts = \"generate\" copies dest {0:?} into the install directory as \
         SYSTEM, so it may not contain \\ or :"
    )]
    GeneratedDest(String),
    #[error("nupkg checksum must be 64 hex chars (sha256), got {0:?}")]
    InvalidChecksum(String),
    #[error("nupkg icon {0:?} must be a PNG or JPEG (magic-byte check)")]
    InvalidIconFormat(PathBuf),
    #[error("nupkg icon {0:?} is {1} bytes; NuGet caps embedded icons at 1 MiB")]
    IconTooLarge(PathBuf, u64),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Build the `.nupkg` described by `spec` at `out` (parent dirs are created).
pub fn build(spec: &NupkgSpec, out: &Path) -> Result<()> {
    validate(spec)?;

    // `tools/`-relative name -> content, assembled per style then sorted so
    // the zip entry order is deterministic.
    let mut tools: Vec<(String, Vec<u8>)> = match &spec.style {
        Style::Embedded { files, scripts } => payload_tools(spec, files, scripts.as_ref(), false)?,
        Style::Plain { files, scripts } => payload_tools(spec, files, Some(scripts), true)?,
        Style::Download {
            url,
            checksum,
            dest,
        } => vec![(
            "chocolateyinstall.ps1".to_string(),
            install_ps1(&spec.name, url, checksum, dest),
        )],
    };
    tools.sort_by(|a, b| a.0.cmp(&b.0));

    // Icon: read bytes, sniff PNG/JPEG magic for the in-zip entry name, and
    // size-gate. Read here (not in the IO-free validate()) since the bytes
    // are needed anyway, and share the sniffed name with the nuspec below.
    let icon: Option<(&'static str, Vec<u8>)> = match &spec.icon {
        Some(path) => {
            let bytes = std::fs::read(path)?;
            let name =
                icon_entry_name(&bytes).ok_or_else(|| Error::InvalidIconFormat(path.clone()))?;
            let len = bytes.len() as u64;
            if len > 1_048_576 {
                return Err(Error::IconTooLarge(path.clone(), len));
            }
            Some((name, bytes))
        }
        None => None,
    };

    let nuspec_name = format!("{}.nuspec", spec.name);
    let psmdcp_name = format!(
        "package/services/metadata/core-properties/{}.psmdcp",
        Uuid::new_v5(
            &NAMESPACE,
            format!("{}{}", spec.identifier, spec.version).as_bytes()
        )
        .simple()
    );

    // [Content_Types].xml needs a Default per extension present in the zip.
    // (Extension-less tools files get none; Chocolatey's extractor does not
    // consult content types, so that is cosmetic, not fatal.)
    let mut extensions: BTreeSet<String> = ["rels", "psmdcp", "nuspec"].map(String::from).into();
    for (name, _) in &tools {
        if let Some((_, ext)) = name.rsplit_once('.') {
            if !ext.is_empty() && !ext.contains('/') {
                extensions.insert(ext.to_ascii_lowercase());
            }
        }
    }
    // The root icon entry lives outside tools/, so add its extension by hand.
    if let Some((name, _)) = &icon {
        if let Some((_, ext)) = name.rsplit_once('.') {
            extensions.insert(ext.to_string());
        }
    }

    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let (year, month, day, hour, minute, second) = FIXED_DOS_TIME;
    let mtime = zip::DateTime::from_date_and_time(year, month, day, hour, minute, second)
        .expect("fixed timestamp is a valid DOS datetime");
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(mtime);

    let mut writer = zip::ZipWriter::new(std::fs::File::create(out)?);
    let put =
        |writer: &mut zip::ZipWriter<std::fs::File>, name: &str, bytes: &[u8]| -> Result<()> {
            writer.start_file(name, options)?;
            writer.write_all(bytes)?;
            Ok(())
        };
    put(
        &mut writer,
        "[Content_Types].xml",
        &content_types_xml(&extensions)?,
    )?;
    put(
        &mut writer,
        "_rels/.rels",
        &rels_xml(&nuspec_name, &psmdcp_name)?,
    )?;
    let icon_entry = icon.as_ref().map(|(name, _)| *name);
    put(&mut writer, &nuspec_name, &nuspec_xml(spec, icon_entry)?)?;
    put(&mut writer, &psmdcp_name, &psmdcp_xml(spec)?)?;
    // Fixed position: after psmdcp, before the tools/ loop, so builds stay
    // byte-reproducible.
    if let Some((name, bytes)) = &icon {
        put(&mut writer, name, bytes)?;
    }
    for (name, bytes) in &tools {
        put(&mut writer, &format!("tools/{name}"), bytes)?;
    }
    writer.finish()?.sync_all()?;
    Ok(())
}

/// The `tools/` entries of an embedded or plain package: the payload, the
/// scripts under the style's names, a plain package's guard, the `.ignore`
/// files, and the generated `LICENSE.txt` and `VERIFICATION.txt`.
fn payload_tools(
    spec: &NupkgSpec,
    files: &[FileSpec],
    scripts: Option<&Scripts>,
    plain: bool,
) -> Result<Vec<(String, Vec<u8>)>> {
    let mut tools = Vec::new();
    let mut listed = Vec::new();
    for file in files {
        let bytes = std::fs::read(&file.src)?;
        listed.push((file.dest.clone(), sha256_hex(&bytes)));
        tools.push((file.dest.clone(), bytes));
    }
    let names = if plain { PLAIN_SCRIPTS } else { CHOCO_SCRIPTS };
    match scripts {
        None => {}
        // No before-modify: the generated install replaces the old version
        // itself.
        Some(Scripts::Generate) => {
            tools.push((names[0].to_string(), generated_install_ps1(spec, files)));
            tools.push((names[1].to_string(), generated_uninstall_ps1(spec)));
        }
        Some(Scripts::Supplied {
            install,
            uninstall,
            before_modify,
        }) => {
            for (name, src) in
                names
                    .into_iter()
                    .zip([Some(install), uninstall.as_ref(), before_modify.as_ref()])
            {
                if let Some(src) = src {
                    tools.push((name.to_string(), std::fs::read(src)?));
                }
            }
        }
    }
    if plain {
        tools.push((CHOCO_SCRIPTS[0].to_string(), guard_ps1(&spec.display_name)));
    }
    for dest in ignored(&spec.style) {
        tools.push((format!("{dest}.ignore"), Vec::new()));
    }
    if let Some(license) = license_txt(spec) {
        tools.push(("LICENSE.txt".to_string(), license));
    }
    tools.push((
        "VERIFICATION.txt".to_string(),
        verification_txt(spec, &listed),
    ));
    Ok(tools)
}

/// Payload dests that get an empty `tools/<dest>.ignore`, the only way to
/// stop Chocolatey's shim. In an embedded package: every `shim = false` file,
/// plus every `.exe` when the scripts are generated (they put the install
/// directory on PATH themselves). One entry per file, so each `.ignore` is
/// emitted once. None in a plain package: its guard fails a choco install
/// before shimming.
fn ignored(style: &Style) -> Vec<&str> {
    let Style::Embedded { files, scripts } = style else {
        return Vec::new();
    };
    let generate = matches!(scripts, Some(Scripts::Generate));
    files
        .iter()
        .filter(|f| !f.shim || (generate && f.dest.to_ascii_lowercase().ends_with(".exe")))
        .map(|f| f.dest.as_str())
        .collect()
}

/// Layout rules, no filesystem access: dest shape, duplicates, collisions
/// with generated `tools/` files, and the display name generated scripts
/// install under. [`build`] runs it first; callers can run it early to fail
/// before building anything.
pub fn validate(spec: &NupkgSpec) -> Result<()> {
    match &spec.style {
        Style::Embedded { files, scripts } => {
            validate_payload(spec, files, scripts.as_ref())?;
        }
        Style::Plain { files, scripts } => validate_payload(spec, files, Some(scripts))?,
        Style::Download { checksum, dest, .. } => {
            validate_dest(dest)?;
            if checksum.len() != 64 || !checksum.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(Error::InvalidChecksum(checksum.clone()));
            }
        }
    }
    Ok(())
}

/// `scripts` is always set for a plain package.
fn validate_payload(spec: &NupkgSpec, files: &[FileSpec], scripts: Option<&Scripts>) -> Result<()> {
    if files.is_empty() {
        return Err(Error::NoFiles);
    }
    let mut seen = BTreeSet::new();
    for file in files {
        validate_dest(&file.dest)?;
        // Windows extraction is case-insensitive; block collisions
        // with the files we generate alongside the payload.
        let lowered = file.dest.to_ascii_lowercase();
        if lowered == "license.txt" || lowered == "verification.txt" {
            return Err(Error::ReservedDest(file.dest.clone()));
        }
        // One source per script: with scripts (or a plain guard) in the
        // package, no dest may take a script name of either set.
        if scripts.is_some()
            && CHOCO_SCRIPTS
                .iter()
                .chain(&PLAIN_SCRIPTS)
                .any(|name| name.eq_ignore_ascii_case(&file.dest))
        {
            return Err(Error::ScriptDest(file.dest.clone()));
        }
        if !seen.insert(lowered) {
            return Err(Error::DuplicateDest(file.dest.clone()));
        }
    }
    for shimless in ignored(&spec.style) {
        let ignore = format!("{shimless}.ignore");
        if let Some(file) = files.iter().find(|f| f.dest.eq_ignore_ascii_case(&ignore)) {
            return Err(Error::IgnoreDest(file.dest.clone(), shimless.to_string()));
        }
    }
    if matches!(scripts, Some(Scripts::Generate)) {
        if !is_dir_name(&spec.display_name) {
            return Err(Error::InvalidDisplayName(spec.display_name.clone()));
        }
        // A \ or : would let Path.Combine in the generated install leave the
        // install directory. Configs without generate keep the older rules.
        if let Some(file) = files.iter().find(|f| f.dest.contains(['\\', ':'])) {
            return Err(Error::GeneratedDest(file.dest.clone()));
        }
    }
    Ok(())
}

/// Whether `name` is a valid Windows directory name.
fn is_dir_name(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        && !name.ends_with([' ', '.'])
}

fn validate_dest(dest: &str) -> Result<()> {
    if dest.split('/').any(|s| s.is_empty() || s == "..") {
        return Err(Error::InvalidDest(dest.to_string()));
    }
    Ok(())
}

/// In-zip entry name for an icon, sniffed from its magic bytes: PNG →
/// `icon.png`, JPEG → `icon.jpg`, anything else → None. The name is shared
/// between the zip entry and the nuspec `<icon>` text so they never diverge.
fn icon_entry_name(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("icon.png")
    } else if bytes.starts_with(b"\xFF\xD8\xFF") {
        Some("icon.jpg")
    } else {
        None
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

// ---------------------------------------------------------------------------
// Generated tools/ files

fn license_txt(spec: &NupkgSpec) -> Option<Vec<u8>> {
    let license = spec.license.as_ref()?;
    let mut text = format!(
        "{} is distributed under the {} license by {}.\n",
        spec.display_name, license, spec.publisher
    );
    if let Some(url) = spec.license_url.as_ref().or(spec.homepage.as_ref()) {
        text.push_str(&format!("Full license text: {url}\n"));
    }
    Some(text.into_bytes())
}

/// Chocolatey requires VERIFICATION.txt whenever binaries ship inside the
/// package: say where the binaries come from and how to check them.
fn verification_txt(spec: &NupkgSpec, files: &[(String, String)]) -> Vec<u8> {
    let mut text = String::from("VERIFICATION\n\n");
    text.push_str(&format!(
        "This package is published by {publisher} and contains binaries built\n\
         by the publisher from their own release artifacts (packaged with embala).\n\n\
         Embedded files and their SHA256 checksums:\n\n",
        publisher = spec.publisher
    ));
    for (dest, sha) in files {
        text.push_str(&format!("  {dest}: {sha}\n"));
    }
    text.push_str("\nVerify with: checksum -t sha256 <file>\n");
    text.into_bytes()
}

fn ps_quote(value: &str) -> String {
    // PowerShell single-quoted literal: ' and its typographic forms
    // (U+2018 to U+201B) all delimit it, and doubling any one of them yields
    // that one character.
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for c in value.chars() {
        if matches!(c, '\'' | '\u{2018}'..='\u{201b}') {
            quoted.push(c);
        }
        quoted.push(c);
    }
    quoted.push('\'');
    quoted
}

/// A plain package's `chocolateyInstall.ps1`: fails `choco install` before
/// Chocolatey shims anything. UTF-8 with a byte-order mark, so Windows
/// PowerShell 5.1 decodes a non-ASCII display name.
fn guard_ps1(display_name: &str) -> Vec<u8> {
    format!(
        "\u{feff}throw {}\n",
        ps_quote(&format!("{display_name} is not a Chocolatey package"))
    )
    .into_bytes()
}

fn install_ps1(name: &str, url: &str, checksum: &str, dest: &str) -> Vec<u8> {
    // Get-ChocolateyWebFile (not Install-ChocolateyPackage: that runs an
    // installer) downloads the raw exe into tools/, where Chocolatey's shim
    // machinery picks it up like an embedded payload.
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         $toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition\n\
         Get-ChocolateyWebFile -PackageName {name} `\n\
         \x20 -FileFullPath (Join-Path $toolsDir {dest}) `\n\
         \x20 -Url {url} `\n\
         \x20 -Checksum {checksum} `\n\
         \x20 -ChecksumType 'sha256'\n",
        name = ps_quote(name),
        dest = ps_quote(&dest.replace('/', "\\")),
        url = ps_quote(url),
        checksum = ps_quote(checksum),
    )
    .into_bytes()
}

/// `scripts = "generate"`'s install: copies exactly the payload into the
/// install directory, swapping it in whole, then puts it on the machine PATH
/// and records it in the marker. See [`PS_INSTALL`] for the sequence.
fn generated_install_ps1(spec: &NupkgSpec, files: &[FileSpec]) -> Vec<u8> {
    let mut text = generated_header(spec, "installs");
    text.push_str("# The payload, relative to this script and to the install directory.\n");
    text.push_str("$payload = @(\n");
    for file in files {
        text.push_str(&format!(
            "    {}\n",
            ps_quote(&file.dest.replace('/', "\\"))
        ));
    }
    text.push_str(")\n");
    text.push_str(PS_HELPERS);
    text.push_str(PS_INSTALL);
    text.into_bytes()
}

/// `scripts = "generate"`'s uninstall: removes what the generated install
/// put down. See [`PS_UNINSTALL`].
fn generated_uninstall_ps1(spec: &NupkgSpec) -> Vec<u8> {
    let mut text = generated_header(spec, "uninstalls");
    text.push_str(PS_HELPERS);
    text.push_str(PS_UNINSTALL);
    text.into_bytes()
}

/// UTF-8 byte-order mark (so Windows PowerShell 5.1 decodes a non-ASCII
/// display name), strict errors, and the package's literals. Every embedded
/// value goes through [`ps_quote`].
fn generated_header(spec: &NupkgSpec, verb: &str) -> String {
    format!(
        "\u{feff}# Generated by embala: {verb} the package payload in\n\
         # <ProgramFiles>\\<display-name> (nupkg) and keeps that directory on the machine\n\
         # PATH. Windows PowerShell 5.1, run as an administrator or SYSTEM.\n\
         $ErrorActionPreference = 'Stop'\n\
         \n\
         $displayName = {display_name}\n\
         $identifier = {identifier}\n",
        display_name = ps_quote(&spec.display_name),
        identifier = ps_quote(&spec.identifier),
    )
}

/// Shared by the generated install and uninstall. Filesystem work goes
/// through .NET rather than cmdlets, so no path is ever read as a wildcard
/// (display names and dests may contain `[` or `]`), and every path is
/// absolute. Registry work uses the 64-bit view, so a 32-bit host reads and
/// writes the same PATH and marker.
const PS_HELPERS: &str = r##"
$inUse = $displayName + ' is in use; close it and retry'
$programFiles = $env:ProgramW6432
if (-not $programFiles) { $programFiles = $env:ProgramFiles }
if (-not $programFiles) { throw 'neither ProgramW6432 nor ProgramFiles is set; cannot locate Program Files' }
$dir = [System.IO.Path]::Combine($programFiles, $displayName + ' (nupkg)')
$markerKey = 'SOFTWARE\embala\nupkg\' + $identifier
$environmentKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
$hklm = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64)

function Test-Dir([string]$path) {
    [System.IO.Directory]::Exists($path)
}

# The lock probe: throws if any file under the directory is open elsewhere.
# Each file is opened exclusively and closed at once; a running exe fails
# this even though renaming its directory would succeed. A read-only file
# counts as in use: a read-only open cannot detect a running exe, so the probe
# fails safe. The install clears ReadOnly on what it copies, so only a file
# marked read-only by hand trips it.
function Assert-DirNotInUse([string]$path) {
    foreach ($file in [System.IO.Directory]::GetFiles($path, '*', [System.IO.SearchOption]::AllDirectories)) {
        [System.IO.File]::Open($file, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None).Close()
    }
}

# Every probing rename follows Assert-DirNotInUse. The rename is the atomic
# swap and a second probe: it fails while a process's working directory is
# inside.
function Move-Dir([string]$from, [string]$to) {
    [System.IO.Directory]::Move($from, $to)
}

function Remove-Dir([string]$path) {
    if (-not (Test-Dir $path)) { return }
    foreach ($file in [System.IO.Directory]::GetFiles($path, '*', [System.IO.SearchOption]::AllDirectories)) {
        [System.IO.File]::SetAttributes($file, [System.IO.FileAttributes]::Normal)
    }
    [System.IO.Directory]::Delete($path, $true)
}

# For leftovers once the outcome is settled: warn, and the next install's
# step 1 deletes them.
function Remove-DirOrWarn([string]$path) {
    try {
        Remove-Dir $path
    } catch {
        Write-Warning ('could not remove ' + $path + ': ' + $_.Exception.Message)
    }
}

# PATH entries and directories compare case-insensitively, ignoring a
# trailing backslash.
function Test-SamePath([string]$a, [string]$b) {
    [string]::Equals($a.TrimEnd('\'), $b.TrimEnd('\'), [System.StringComparison]::OrdinalIgnoreCase)
}

# The directory the last install recorded, or $null. Only a drive-qualified
# absolute "<name> (nupkg)" path counts (IsPathRooted alone accepts C:foo and
# \foo), so a damaged marker can never point these
# scripts at an unrelated directory.
function Get-MarkerDir {
    $key = $hklm.OpenSubKey($markerKey)
    if ($null -eq $key) { return $null }
    try {
        $value = [string]$key.GetValue('InstallDir')
    } finally {
        $key.Close()
    }
    $value = $value.TrimEnd('\')
    if ($value -notmatch '^[A-Za-z]:\\') { return $null }
    if (-not $value.EndsWith(' (nupkg)', [System.StringComparison]::OrdinalIgnoreCase)) { return $null }
    $value
}

# Machine PATH: read unexpanded and written back as REG_EXPAND_SZ, so
# entries such as %SystemRoot%\system32 keep their text. (The .NET
# environment-variable API would expand them and write REG_SZ.)
function Update-MachinePath([string[]]$add, [string[]]$remove) {
    $key = $hklm.OpenSubKey($environmentKey, $true)
    try {
        $before = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $entries = New-Object System.Collections.Generic.List[string]
        foreach ($entry in ($before -split ';')) {
            if ($entry -eq '') { continue }
            $drop = $false
            foreach ($gone in $remove) {
                if (Test-SamePath $entry $gone) { $drop = $true }
            }
            if (-not $drop) { $entries.Add($entry) }
        }
        foreach ($wanted in $add) {
            $present = $false
            foreach ($entry in $entries) {
                if (Test-SamePath $entry $wanted) { $present = $true }
            }
            if (-not $present) { $entries.Add($wanted) }
        }
        $after = $entries -join ';'
        if ($after -ceq $before) { return }
        $key.SetValue('Path', $after, [Microsoft.Win32.RegistryValueKind]::ExpandString)
    } finally {
        $key.Close()
    }
    Send-SettingChange
}

# Tell running programs that the environment changed. Best effort: a
# failure only warns.
function Send-SettingChange {
    try {
        if (-not ('EmbalaNupkg.Native' -as [type])) {
            Add-Type -Namespace EmbalaNupkg -Name Native -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint msg, UIntPtr wParam, string lParam, uint flags, uint timeout, out UIntPtr result);
'@
        }
        $result = [UIntPtr]::Zero
        # HWND_BROADCAST, WM_SETTINGCHANGE, SMTO_ABORTIFHUNG, 5 seconds.
        $sent = [EmbalaNupkg.Native]::SendMessageTimeout([IntPtr]0xffff, 0x1a, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result)
        if ($sent -eq [IntPtr]::Zero) { throw 'SendMessageTimeout failed or timed out' }
    } catch {
        Write-Warning ('could not broadcast the PATH change: ' + $_.Exception.Message)
    }
}
"##;

/// The generated install's sequence (research Decision 10). Steps 1 to 4
/// touch only `<dir>.new` and leftovers from earlier runs until their lock
/// probes pass, and every failure there undoes what came before, so `<dir>`,
/// `<prev>`, PATH and the marker stay as they were.
const PS_INSTALL: &str = r##"
$scriptDir = $PSScriptRoot
if (-not $scriptDir) { $scriptDir = [System.IO.Path]::GetDirectoryName($MyInvocation.MyCommand.Path) }
$new = $dir + '.new'
$old = $dir + '.old'

# Step 1: recover from an interrupted run. <dir> missing next to <dir>.old
# means a run stopped between steps 4 and 5: put the old version back. Then
# delete leftovers; the installed version, PATH and the marker are untouched.
if (-not (Test-Dir $dir) -and (Test-Dir $old)) { Move-Dir $old $dir }
try {
    Remove-Dir $new
    Remove-Dir $old
    Remove-Dir ($dir + '.removing')
} catch {
    throw $inUse
}

# Step 2: copy exactly the payload into <dir>.new, clearing ReadOnly on each
# copy so the lock probe can open it on a later upgrade or uninstall.
try {
    foreach ($dest in $payload) {
        $target = [System.IO.Path]::Combine($new, $dest)
        [void][System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($target))
        [System.IO.File]::Copy([System.IO.Path]::Combine($scriptDir, $dest), $target)
        if (([System.IO.File]::GetAttributes($target) -band [System.IO.FileAttributes]::ReadOnly) -ne 0) {
            [System.IO.File]::SetAttributes($target, [System.IO.FileAttributes]::Normal)
        }
    }
} catch {
    Remove-DirOrWarn $new
    throw
}

# Step 3: a marker naming another directory <prev> means display-name changed.
# Delete a <prev>.removing left by an interrupted run (the marker moves on
# below, which would orphan it), then probe <prev> and rename it aside, as
# in step 4. On failure only <dir>.new has changed: delete it.
$prev = Get-MarkerDir
if ($prev -and (Test-SamePath $prev $dir)) { $prev = $null }
$prevRemoving = $null
if ($prev) {
    try {
        Remove-Dir ($prev + '.removing')
        if (Test-Dir $prev) {
            Assert-DirNotInUse $prev
            Move-Dir $prev ($prev + '.removing')
            $prevRemoving = $prev + '.removing'
        }
    } catch {
        Remove-DirOrWarn $new
        throw $inUse
    }
}

# Step 4: probe the installed version and rename <dir> aside. On failure
# undo step 3 and delete <dir>.new.
$movedOld = $false
if (Test-Dir $dir) {
    try {
        Assert-DirNotInUse $dir
        Move-Dir $dir $old
    } catch {
        if ($prevRemoving) { Move-Dir $prevRemoving $prev }
        Remove-DirOrWarn $new
        throw $inUse
    }
    $movedOld = $true
}

# Step 5: swap the new version in. Should the rename fail, undo steps 3 and 4.
try {
    Move-Dir $new $dir
} catch {
    if ($movedOld) { Move-Dir $old $dir }
    if ($prevRemoving) { Move-Dir $prevRemoving $prev }
    Remove-DirOrWarn $new
    throw
}

# Step 6: put <dir> on PATH, drop <prev>'s entries, record <dir> in the
# marker, then delete what was renamed aside (a failure there only warns).
$remove = @()
if ($prev) { $remove = @($prev) }
Update-MachinePath -add $dir -remove $remove
$marker = $hklm.CreateSubKey($markerKey)
try {
    $marker.SetValue('InstallDir', $dir, [Microsoft.Win32.RegistryValueKind]::String)
} finally {
    $marker.Close()
}
Remove-DirOrWarn $old
if ($prevRemoving) { Remove-DirOrWarn $prevRemoving }
Write-Host ($displayName + ' installed to ' + $dir)
"##;

/// The generated uninstall: probe first, then remove, so a failure changes
/// nothing and PATH never points at a missing directory.
const PS_UNINSTALL: &str = r##"
$target = Get-MarkerDir
if (-not $target) { $target = $dir }
$removing = $target + '.removing'

# Leftovers of interrupted runs go first, so a later install can never
# restore a version this uninstall removed. PATH and the marker are untouched.
Remove-Dir ($target + '.new')
Remove-Dir ($target + '.old')
Remove-Dir $removing

# Lock probe: check for open files, then rename the directory aside, before
# touching PATH or the marker. Nothing installed: nothing to probe.
$probed = $false
if (Test-Dir $target) {
    try {
        Assert-DirNotInUse $target
        Move-Dir $target $removing
    } catch {
        throw $inUse
    }
    $probed = $true
}

Update-MachinePath -remove $target
$hklm.DeleteSubKey($markerKey, $false)
if ($probed) { Remove-DirOrWarn $removing }
Write-Host ($displayName + ' uninstalled')
"##;

// ---------------------------------------------------------------------------
// XML parts (quick-xml writer: correct escaping, deterministic output)

type XmlResult = std::result::Result<Vec<u8>, std::io::Error>;

fn xml_writer() -> std::result::Result<Writer<Cursor<Vec<u8>>>, std::io::Error> {
    let mut writer = Writer::new(Cursor::new(Vec::new()));
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("utf-8"), None)))?;
    Ok(writer)
}

fn finish(writer: Writer<Cursor<Vec<u8>>>) -> Vec<u8> {
    writer.into_inner().into_inner()
}

fn content_types_xml(extensions: &BTreeSet<String>) -> XmlResult {
    let mut writer = xml_writer()?;
    writer
        .create_element("Types")
        .with_attribute(("xmlns", CONTENT_TYPES_XMLNS))
        .write_inner_content(|w| {
            for ext in extensions {
                let content_type = match ext.as_str() {
                    "rels" => "application/vnd.openxmlformats-package.relationships+xml",
                    "psmdcp" => "application/vnd.openxmlformats-package.core-properties+xml",
                    _ => "application/octet-stream",
                };
                w.create_element("Default")
                    .with_attribute(("Extension", ext.as_str()))
                    .with_attribute(("ContentType", content_type))
                    .write_empty()?;
            }
            Ok::<(), std::io::Error>(())
        })?;
    Ok(finish(writer))
}

fn rels_xml(nuspec_name: &str, psmdcp_name: &str) -> XmlResult {
    let mut writer = xml_writer()?;
    writer
        .create_element("Relationships")
        .with_attribute(("xmlns", RELS_XMLNS))
        .write_inner_content(|w| {
            w.create_element("Relationship")
                .with_attribute((
                    "Type",
                    "http://schemas.microsoft.com/packaging/2010/07/manifest",
                ))
                .with_attribute(("Target", format!("/{nuspec_name}").as_str()))
                .with_attribute(("Id", "R1"))
                .write_empty()?;
            w.create_element("Relationship")
                .with_attribute((
                    "Type",
                    "http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties",
                ))
                .with_attribute(("Target", format!("/{psmdcp_name}").as_str()))
                .with_attribute(("Id", "R2"))
                .write_empty()?;
            Ok::<(), std::io::Error>(())
        })?;
    Ok(finish(writer))
}

fn nuspec_xml(spec: &NupkgSpec, icon_entry: Option<&str>) -> XmlResult {
    let mut writer = xml_writer()?;
    writer
        .create_element("package")
        .with_attribute(("xmlns", NUSPEC_XMLNS))
        .write_inner_content(|w| {
            w.create_element("metadata").write_inner_content(|w| {
                // Free fn (not a closure): the license element borrows `w`
                // directly between text() calls, which a capturing closure
                // would forbid.
                fn text(
                    w: &mut Writer<Cursor<Vec<u8>>>,
                    tag: &str,
                    value: &str,
                ) -> std::result::Result<(), std::io::Error> {
                    w.create_element(tag)
                        .write_text_content(BytesText::new(value))
                        .map(|_| ())
                }
                text(w, "id", &spec.name)?;
                text(w, "version", &spec.version)?;
                text(w, "title", &spec.display_name)?;
                text(w, "authors", &spec.publisher)?;
                if let Some(license) = &spec.license {
                    // SPDX identifier; the element carries both the type
                    // attribute and the id as text content.
                    w.create_element("license")
                        .with_attribute(("type", "expression"))
                        .write_text_content(BytesText::new(license))?;
                }
                if let Some(license_url) = &spec.license_url {
                    text(w, "licenseUrl", license_url)?;
                }
                if let Some(homepage) = &spec.homepage {
                    text(w, "projectUrl", homepage)?;
                }
                if let Some(project_source_url) = &spec.project_source_url {
                    text(w, "projectSourceUrl", project_source_url)?;
                }
                if let Some(package_source_url) = &spec.package_source_url {
                    text(w, "packageSourceUrl", package_source_url)?;
                }
                // <icon> names the in-zip entry (icon.png/icon.jpg), matching
                // the sniffed name build() packed; <iconUrl> is the fallback URL.
                if let Some(icon_entry) = icon_entry {
                    text(w, "icon", icon_entry)?;
                }
                if let Some(icon_url) = &spec.icon_url {
                    text(w, "iconUrl", icon_url)?;
                }
                if spec.require_license_acceptance {
                    // false is the nuspec default, so we only emit true.
                    text(w, "requireLicenseAcceptance", "true")?;
                }
                text(w, "description", &spec.description)?;
                if let Some(release_notes) = &spec.release_notes {
                    text(w, "releaseNotes", release_notes)?;
                }
                if let Some(copyright) = &spec.copyright {
                    text(w, "copyright", copyright)?;
                }
                if !spec.tags.is_empty() {
                    text(w, "tags", &spec.tags.join(" "))?;
                }
                Ok::<(), std::io::Error>(())
            })?;
            Ok::<(), std::io::Error>(())
        })?;
    Ok(finish(writer))
}

fn psmdcp_xml(spec: &NupkgSpec) -> XmlResult {
    let mut writer = xml_writer()?;
    writer
        .create_element("coreProperties")
        .with_attribute(("xmlns", CORE_PROPS_XMLNS))
        .with_attribute(("xmlns:dc", DC_XMLNS))
        .write_inner_content(|w| {
            w.create_element("dc:creator")
                .write_text_content(BytesText::new(&spec.publisher))?;
            w.create_element("dc:identifier")
                .write_text_content(BytesText::new(&spec.name))?;
            w.create_element("version")
                .write_text_content(BytesText::new(&spec.version))?;
            Ok::<(), std::io::Error>(())
        })?;
    Ok(finish(writer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_ignore_is_emitted_once() {
        // A shim = false exe with generated scripts matches both .ignore
        // rules, but a zip cannot hold the same entry twice.
        let file = |dest: &str, shim| FileSpec {
            src: PathBuf::new(),
            dest: dest.to_string(),
            shim,
        };
        let style = Style::Embedded {
            files: vec![
                file("hello.exe", false),
                file("bin/Tool.EXE", true),
                file("readme.txt", true),
                file("notes.txt", false),
            ],
            scripts: Some(Scripts::Generate),
        };
        assert_eq!(
            ignored(&style),
            vec!["hello.exe", "bin/Tool.EXE", "notes.txt"]
        );
    }
}
