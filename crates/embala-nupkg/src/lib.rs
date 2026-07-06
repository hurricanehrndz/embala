//! Chocolatey `.nupkg` builder.
//!
//! Writes the OPC package (a zip with a nuspec, OPC bookkeeping parts, and a
//! `tools/` payload) directly, so Chocolatey packages build on any OS with no
//! choco, NuGet, or .NET. Builds are byte-stable: fixed entry order, pinned
//! zip timestamps, and deterministic part names.
//!
//! Scope for v1: plain executables put on PATH via Chocolatey's automatic
//! `tools/*.exe` shimming — either embedded in the package or downloaded at
//! install time. Nothing else.

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
}

/// How the payload reaches the machine.
#[derive(Debug, Clone)]
pub enum Style {
    /// Payload files ship inside the package under `tools/`; Chocolatey
    /// shims every `tools/*.exe` onto PATH with no install script.
    Embedded { files: Vec<FileSpec> },
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
    #[error("nupkg checksum must be 64 hex chars (sha256), got {0:?}")]
    InvalidChecksum(String),
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
    let mut tools: Vec<(String, Vec<u8>)> = Vec::new();
    match &spec.style {
        Style::Embedded { files } => {
            let mut listed = Vec::new();
            for file in files {
                let bytes = std::fs::read(&file.src)?;
                listed.push((file.dest.clone(), sha256_hex(&bytes)));
                tools.push((file.dest.clone(), bytes));
            }
            if let Some(license) = license_txt(spec) {
                tools.push(("LICENSE.txt".to_string(), license));
            }
            tools.push((
                "VERIFICATION.txt".to_string(),
                verification_txt(spec, &listed),
            ));
        }
        Style::Download {
            url,
            checksum,
            dest,
        } => {
            tools.push((
                "chocolateyinstall.ps1".to_string(),
                install_ps1(&spec.name, url, checksum, dest),
            ));
        }
    }
    tools.sort_by(|a, b| a.0.cmp(&b.0));

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
    put(&mut writer, &nuspec_name, &nuspec_xml(spec)?)?;
    put(&mut writer, &psmdcp_name, &psmdcp_xml(spec)?)?;
    for (name, bytes) in &tools {
        put(&mut writer, &format!("tools/{name}"), bytes)?;
    }
    writer.finish()?.sync_all()?;
    Ok(())
}

fn validate(spec: &NupkgSpec) -> Result<()> {
    match &spec.style {
        Style::Embedded { files } => {
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
                if !seen.insert(lowered) {
                    return Err(Error::DuplicateDest(file.dest.clone()));
                }
            }
        }
        Style::Download { checksum, dest, .. } => {
            validate_dest(dest)?;
            if checksum.len() != 64 || !checksum.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(Error::InvalidChecksum(checksum.clone()));
            }
        }
    }
    Ok(())
}

fn validate_dest(dest: &str) -> Result<()> {
    if dest.split('/').any(|s| s.is_empty() || s == "..") {
        return Err(Error::InvalidDest(dest.to_string()));
    }
    Ok(())
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
    // PowerShell single-quoted literal: only ' needs escaping (doubled).
    format!("'{}'", value.replace('\'', "''"))
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

fn nuspec_xml(spec: &NupkgSpec) -> XmlResult {
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
                // icon/iconUrl slot — reserved for a later phase.
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
