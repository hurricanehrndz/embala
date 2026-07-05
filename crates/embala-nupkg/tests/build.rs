//! End-to-end build assertions: exact OPC entry sets per style, nuspec
//! content (parsed as XML, escaping included), the generated tools/ files,
//! and byte-for-byte reproducibility.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use embala_nupkg::{FileSpec, NupkgSpec, Style, build};
use sha2::{Digest as _, Sha256};

const PAYLOAD_EXE: &[u8] = b"MZ fake windows executable payload";

fn tmp(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn base_spec(style: Style) -> NupkgSpec {
    NupkgSpec {
        name: "hello".to_string(),
        display_name: "Embala Hello".to_string(),
        version: "0.1.0".to_string(),
        identifier: "ca.hrndz.embala.hello".to_string(),
        publisher: "Carlos Hernandez".to_string(),
        description: "Embala end-to-end test fixture".to_string(),
        homepage: Some("https://github.com/hurricanehrndz/embala".to_string()),
        license: Some("MIT".to_string()),
        style,
    }
}

fn embedded_spec(payload_dir: &Path) -> NupkgSpec {
    fs::create_dir_all(payload_dir).unwrap();
    fs::write(payload_dir.join("hello.exe"), PAYLOAD_EXE).unwrap();
    base_spec(Style::Embedded {
        files: vec![FileSpec {
            src: payload_dir.join("hello.exe"),
            dest: "hello.exe".to_string(),
        }],
    })
}

fn download_spec() -> NupkgSpec {
    base_spec(Style::Download {
        url: "http://192.168.122.1:8000/hello.exe".to_string(),
        checksum: "a".repeat(64),
        dest: "hello.exe".to_string(),
    })
}

/// Entry name -> contents, for exact-set assertions.
fn read_entries(nupkg: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut archive = zip::ZipArchive::new(fs::File::open(nupkg).unwrap()).unwrap();
    let mut entries = BTreeMap::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).unwrap();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        entries.insert(entry.name().to_string(), bytes);
    }
    entries
}

// Determinism lock: UUIDv5 of identifier+version under the crate's fixed
// namespace. If this changes, published packages stop being byte-stable.
const PSMDCP: &str = "package/services/metadata/core-properties/\
                      ac049e95aefb527da84fffc6e16589c3.psmdcp";

/// Walk the nuspec with quick-xml and return element text by path, so the
/// assertion proves the document parses and unescapes correctly.
fn nuspec_texts(bytes: &[u8]) -> BTreeMap<String, String> {
    let mut reader = quick_xml::Reader::from_reader(bytes);
    let mut path: Vec<String> = Vec::new();
    let mut texts = BTreeMap::new();
    loop {
        match reader.read_event().unwrap() {
            quick_xml::events::Event::Start(e) => {
                path.push(String::from_utf8(e.name().as_ref().to_vec()).unwrap());
            }
            quick_xml::events::Event::End(_) => {
                path.pop();
            }
            quick_xml::events::Event::Text(t) => {
                texts
                    .entry(path.join("/"))
                    .or_insert_with(String::new)
                    .push_str(&t.decode().unwrap());
            }
            // quick-xml splits text at entity references; resolve the
            // predefined XML entities so escaping round-trips.
            quick_xml::events::Event::GeneralRef(r) => {
                let name = String::from_utf8(r.to_vec()).unwrap();
                let resolved = match name.as_str() {
                    "amp" => '&',
                    "lt" => '<',
                    "gt" => '>',
                    "quot" => '"',
                    "apos" => '\'',
                    other => panic!("unexpected entity reference: &{other};"),
                };
                texts
                    .entry(path.join("/"))
                    .or_insert_with(String::new)
                    .push(resolved);
            }
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }
    texts
}

#[test]
fn embedded_package_has_exact_entries_and_verification_checksum() {
    let spec = embedded_spec(&tmp("payload-embedded"));
    let out = tmp("embedded.nupkg");
    build(&spec, &out).unwrap();

    let entries = read_entries(&out);
    let names: Vec<&str> = entries.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        vec![
            "[Content_Types].xml",
            "_rels/.rels",
            "hello.nuspec",
            PSMDCP,
            "tools/LICENSE.txt",
            "tools/VERIFICATION.txt",
            "tools/hello.exe",
        ]
    );

    // Payload lands byte-identical under tools/.
    assert_eq!(entries["tools/hello.exe"], PAYLOAD_EXE);

    // VERIFICATION.txt carries the payload's sha256.
    let expected_sha = format!("{:x}", Sha256::digest(PAYLOAD_EXE));
    let verification = String::from_utf8(entries["tools/VERIFICATION.txt"].clone()).unwrap();
    assert!(
        verification.contains(&format!("hello.exe: {expected_sha}")),
        "VERIFICATION.txt must list the embedded file's sha256:\n{verification}"
    );

    // LICENSE.txt names the license.
    let license = String::from_utf8(entries["tools/LICENSE.txt"].clone()).unwrap();
    assert!(license.contains("MIT"), "LICENSE.txt: {license}");

    // Every extension in the zip has a content-type Default.
    let types = String::from_utf8(entries["[Content_Types].xml"].clone()).unwrap();
    for ext in ["rels", "psmdcp", "nuspec", "exe", "txt"] {
        assert!(
            types.contains(&format!("Extension=\"{ext}\"")),
            "missing content-type Default for {ext}: {types}"
        );
    }

    // .rels points at the nuspec and the psmdcp.
    let rels = String::from_utf8(entries["_rels/.rels"].clone()).unwrap();
    assert!(rels.contains("Target=\"/hello.nuspec\""), "rels: {rels}");
    assert!(
        rels.contains(&format!("Target=\"/{PSMDCP}\"")),
        "rels: {rels}"
    );
}

#[test]
fn nuspec_carries_the_package_metadata() {
    let spec = embedded_spec(&tmp("payload-nuspec"));
    let out = tmp("nuspec.nupkg");
    build(&spec, &out).unwrap();

    let texts = nuspec_texts(&read_entries(&out)["hello.nuspec"]);
    let get = |path: &str| texts.get(path).map(String::as_str);
    assert_eq!(get("package/metadata/id"), Some("hello"));
    assert_eq!(get("package/metadata/version"), Some("0.1.0"));
    assert_eq!(get("package/metadata/title"), Some("Embala Hello"));
    assert_eq!(get("package/metadata/authors"), Some("Carlos Hernandez"));
    assert_eq!(
        get("package/metadata/description"),
        Some("Embala end-to-end test fixture")
    );
    assert_eq!(
        get("package/metadata/projectUrl"),
        Some("https://github.com/hurricanehrndz/embala")
    );
}

#[test]
fn nuspec_escapes_xml_metacharacters() {
    // nuspec is UTF-8 XML: non-ASCII is fine, but & and < must be escaped.
    let mut spec = embedded_spec(&tmp("payload-escape"));
    spec.description = "søren & friends <3".to_string();
    let out = tmp("escape.nupkg");
    build(&spec, &out).unwrap();

    let nuspec = read_entries(&out)["hello.nuspec"].clone();
    let raw = String::from_utf8(nuspec.clone()).unwrap();
    assert!(
        raw.contains("søren &amp; friends &lt;3"),
        "raw nuspec must escape metacharacters: {raw}"
    );
    // And the parsed document round-trips to the original text.
    assert_eq!(
        nuspec_texts(&nuspec)
            .get("package/metadata/description")
            .map(String::as_str),
        Some("søren & friends <3")
    );
}

#[test]
fn download_package_has_only_the_install_script() {
    let out = tmp("download.nupkg");
    build(&download_spec(), &out).unwrap();

    let entries = read_entries(&out);
    let names: Vec<&str> = entries.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        vec![
            "[Content_Types].xml",
            "_rels/.rels",
            "hello.nuspec",
            PSMDCP,
            "tools/chocolateyinstall.ps1",
        ]
    );

    // The script downloads with the configured url + checksum into tools/.
    let ps1 = String::from_utf8(entries["tools/chocolateyinstall.ps1"].clone()).unwrap();
    assert!(ps1.contains("Get-ChocolateyWebFile"), "ps1: {ps1}");
    assert!(
        ps1.contains("-Url 'http://192.168.122.1:8000/hello.exe'"),
        "ps1: {ps1}"
    );
    assert!(
        ps1.contains(&format!("-Checksum '{}'", "a".repeat(64))),
        "ps1: {ps1}"
    );
    assert!(
        ps1.contains("Join-Path $toolsDir 'hello.exe'"),
        "ps1: {ps1}"
    );
}

#[test]
fn builds_are_reproducible() {
    for (label, spec) in [
        ("embedded", embedded_spec(&tmp("payload-repro"))),
        ("download", download_spec()),
    ] {
        let out1 = tmp(&format!("repro-{label}-1.nupkg"));
        let out2 = tmp(&format!("repro-{label}-2.nupkg"));
        build(&spec, &out1).unwrap();
        build(&spec, &out2).unwrap();
        assert_eq!(
            fs::read(&out1).unwrap(),
            fs::read(&out2).unwrap(),
            "two consecutive {label} builds must be byte-identical"
        );
    }
}

#[test]
fn invalid_specs_are_rejected() {
    let payload = tmp("payload-invalid");
    fs::create_dir_all(&payload).unwrap();
    fs::write(payload.join("hello.exe"), PAYLOAD_EXE).unwrap();
    let file = |dest: &str| FileSpec {
        src: payload.join("hello.exe"),
        dest: dest.to_string(),
    };

    let fail = |style: Style| build(&base_spec(style), &tmp("invalid.nupkg")).unwrap_err();

    let err = fail(Style::Embedded { files: vec![] });
    assert!(matches!(err, embala_nupkg::Error::NoFiles), "{err}");

    let err = fail(Style::Embedded {
        files: vec![file("../evil.exe")],
    });
    assert!(matches!(err, embala_nupkg::Error::InvalidDest(_)), "{err}");

    let err = fail(Style::Embedded {
        files: vec![file("hello.exe"), file("hello.exe")],
    });
    assert!(
        matches!(err, embala_nupkg::Error::DuplicateDest(_)),
        "{err}"
    );

    // Would silently clobber the generated file on a case-insensitive extract.
    let err = fail(Style::Embedded {
        files: vec![file("verification.txt")],
    });
    assert!(matches!(err, embala_nupkg::Error::ReservedDest(_)), "{err}");

    let err = fail(Style::Download {
        url: "http://example.com/x.exe".to_string(),
        checksum: "abc123".to_string(), // not 64 hex chars
        dest: "x.exe".to_string(),
    });
    assert!(
        matches!(err, embala_nupkg::Error::InvalidChecksum(_)),
        "{err}"
    );
}
