//! End-to-end build assertions: exact OPC entry sets per style, nuspec
//! content (parsed as XML, escaping included), the generated tools/ files,
//! and byte-for-byte reproducibility.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use embala_nupkg::{FileSpec, NupkgSpec, Scripts, Style, build, validate};
use sha2::{Digest as _, Sha256};

const PAYLOAD_EXE: &[u8] = b"MZ fake windows executable payload";
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
const JPEG_MAGIC: &[u8] = b"\xFF\xD8\xFF";

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
        copyright: Some("© 2026 Carlos Hernandez".to_string()),
        tags: vec!["cli".to_string(), "packaging".to_string()],
        release_notes: Some("First public release.".to_string()),
        require_license_acceptance: true,
        project_source_url: Some("https://github.com/hurricanehrndz/embala".to_string()),
        package_source_url: Some("https://github.com/hurricanehrndz/embala-packages".to_string()),
        license_url: Some("https://github.com/hurricanehrndz/embala/blob/main/LICENSE".to_string()),
        // Kept None so the exact-entry-set assertions are unaffected; the
        // icon-packing tests set it explicitly.
        icon: None,
        icon_url: Some("https://github.com/hurricanehrndz/embala/raw/main/icon.png".to_string()),
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
            shim: true,
        }],
        scripts: None,
    })
}

/// An embedded spec whose icon points at a file made of `magic` + `tail`,
/// written into `payload_dir` as `filename`.
fn embedded_spec_with_icon(
    payload_dir: &Path,
    filename: &str,
    magic: &[u8],
    tail: &[u8],
) -> (NupkgSpec, Vec<u8>) {
    let mut spec = embedded_spec(payload_dir);
    let icon_path = payload_dir.join(filename);
    let mut bytes = magic.to_vec();
    bytes.extend_from_slice(tail);
    fs::write(&icon_path, &bytes).unwrap();
    spec.icon = Some(icon_path);
    (spec, bytes)
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

    // LICENSE.txt names the license and points at the license URL (preferred
    // over the homepage when set).
    let license = String::from_utf8(entries["tools/LICENSE.txt"].clone()).unwrap();
    assert!(license.contains("MIT"), "LICENSE.txt: {license}");
    assert!(
        license.contains("https://github.com/hurricanehrndz/embala/blob/main/LICENSE"),
        "LICENSE.txt must prefer the license URL: {license}"
    );

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
    assert_eq!(get("package/metadata/license"), Some("MIT"));
    assert_eq!(
        get("package/metadata/licenseUrl"),
        Some("https://github.com/hurricanehrndz/embala/blob/main/LICENSE")
    );
    assert_eq!(
        get("package/metadata/projectSourceUrl"),
        Some("https://github.com/hurricanehrndz/embala")
    );
    assert_eq!(
        get("package/metadata/packageSourceUrl"),
        Some("https://github.com/hurricanehrndz/embala-packages")
    );
    assert_eq!(
        get("package/metadata/iconUrl"),
        Some("https://github.com/hurricanehrndz/embala/raw/main/icon.png")
    );
    assert_eq!(
        get("package/metadata/requireLicenseAcceptance"),
        Some("true")
    );
    assert_eq!(
        get("package/metadata/releaseNotes"),
        Some("First public release.")
    );
    assert_eq!(
        get("package/metadata/copyright"),
        Some("© 2026 Carlos Hernandez")
    );
    // Tags are joined with a single space.
    assert_eq!(get("package/metadata/tags"), Some("cli packaging"));
}

#[test]
fn nuspec_license_element_carries_the_type_attribute() {
    let spec = embedded_spec(&tmp("payload-license-attr"));
    let out = tmp("license-attr.nupkg");
    build(&spec, &out).unwrap();

    let raw = String::from_utf8(read_entries(&out)["hello.nuspec"].clone()).unwrap();
    assert!(
        raw.contains("<license type=\"expression\">MIT</license>"),
        "raw nuspec must carry the SPDX license element: {raw}"
    );
}

#[test]
fn nuspec_omits_absent_optional_metadata() {
    // Pre-Phase-2 shape: only the always-present fields, everything optional
    // cleared. None of the conditional elements may appear.
    let mut spec = embedded_spec(&tmp("payload-omit"));
    spec.license = None;
    spec.copyright = None;
    spec.tags = Vec::new();
    spec.release_notes = None;
    spec.require_license_acceptance = false;
    spec.project_source_url = None;
    spec.package_source_url = None;
    spec.license_url = None;
    spec.icon = None;
    spec.icon_url = None;
    let out = tmp("omit.nupkg");
    build(&spec, &out).unwrap();

    let raw = String::from_utf8(read_entries(&out)["hello.nuspec"].clone()).unwrap();
    for absent in [
        "<requireLicenseAcceptance",
        "<licenseUrl",
        "<projectSourceUrl",
        "<packageSourceUrl",
        // "<icon>" with the closing bracket: "<icon" alone is a prefix of
        // "<iconUrl". Both must be absent here.
        "<icon>",
        "<iconUrl",
        "<releaseNotes",
        "<copyright",
        "<tags",
        // license is None, so no <license…> at all (guards the
        // <license>/<licenseUrl> prefix collision: neither is present).
        "<license",
    ] {
        assert!(!raw.contains(absent), "must omit {absent}: {raw}");
    }
}

#[test]
fn nuspec_emits_metadata_in_canonical_order() {
    let spec = embedded_spec(&tmp("payload-order"));
    let out = tmp("order.nupkg");
    build(&spec, &out).unwrap();

    let raw = String::from_utf8(read_entries(&out)["hello.nuspec"].clone()).unwrap();
    // Every element the fully-populated base_spec emits, in canonical order.
    // "<license " (trailing space) distinguishes it from "<licenseUrl>".
    let order = [
        "<id>",
        "<version>",
        "<title>",
        "<authors>",
        "<license ",
        "<licenseUrl>",
        "<projectUrl>",
        "<projectSourceUrl>",
        "<packageSourceUrl>",
        "<iconUrl>",
        "<requireLicenseAcceptance>",
        "<description>",
        "<releaseNotes>",
        "<copyright>",
        "<tags>",
    ];
    let mut last = 0;
    for tag in order {
        let at = raw
            .find(tag)
            .unwrap_or_else(|| panic!("missing {tag}: {raw}"));
        assert!(
            at >= last,
            "tag {tag} at {at} is out of canonical order (previous ended at {last}): {raw}"
        );
        last = at;
    }
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
    let (icon_spec, _) =
        embedded_spec_with_icon(&tmp("payload-repro-icon"), "icon.png", PNG_MAGIC, b"pixels");
    for (label, spec) in [
        ("embedded", embedded_spec(&tmp("payload-repro"))),
        ("download", download_spec()),
        ("icon", icon_spec),
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
fn png_icon_is_packed_at_the_zip_root() {
    let (spec, icon_bytes) = embedded_spec_with_icon(
        &tmp("payload-icon-png"),
        "app-icon.png",
        PNG_MAGIC,
        b"some pixels",
    );
    let out = tmp("icon-png.nupkg");
    build(&spec, &out).unwrap();

    let entries = read_entries(&out);
    // Icon lives at the ZIP ROOT (not under tools/), byte-identical.
    assert!(
        entries.contains_key("icon.png"),
        "root-level icon.png missing: {:?}",
        entries.keys().collect::<Vec<_>>()
    );
    assert_eq!(entries["icon.png"], icon_bytes);

    // Its extension gets a content-type Default.
    let types = String::from_utf8(entries["[Content_Types].xml"].clone()).unwrap();
    assert!(types.contains("Extension=\"png\""), "{types}");

    // nuspec <icon> names the in-zip entry.
    let texts = nuspec_texts(&entries["hello.nuspec"]);
    assert_eq!(
        texts.get("package/metadata/icon").map(String::as_str),
        Some("icon.png")
    );
}

#[test]
fn jpeg_icon_is_packed_as_icon_jpg() {
    let (spec, _) = embedded_spec_with_icon(
        &tmp("payload-icon-jpg"),
        "app-icon.jpg",
        JPEG_MAGIC,
        b"jpeg body",
    );
    let out = tmp("icon-jpg.nupkg");
    build(&spec, &out).unwrap();

    let entries = read_entries(&out);
    assert!(
        entries.contains_key("icon.jpg"),
        "root-level icon.jpg missing: {:?}",
        entries.keys().collect::<Vec<_>>()
    );
    // The nuspec text must match the JPEG entry name.
    let texts = nuspec_texts(&entries["hello.nuspec"]);
    assert_eq!(
        texts.get("package/metadata/icon").map(String::as_str),
        Some("icon.jpg")
    );
}

#[test]
fn icon_with_unknown_magic_is_rejected() {
    let (spec, _) = embedded_spec_with_icon(
        &tmp("payload-icon-bad"),
        "app-icon.png",
        b"GIF89a",
        b" not a supported format",
    );
    let err = build(&spec, &tmp("icon-bad.nupkg")).unwrap_err();
    assert!(
        matches!(err, embala_nupkg::Error::InvalidIconFormat(_)),
        "{err}"
    );
}

#[test]
fn icon_size_gate_accepts_1_mib_and_rejects_larger() {
    let payload = tmp("payload-icon-size");
    // Exactly 1 MiB (magic + zero padding) is accepted.
    let (mut at_cap, _) = embedded_spec_with_icon(&payload, "cap.png", PNG_MAGIC, b"");
    let cap_path = payload.join("cap.png");
    let mut cap_bytes = PNG_MAGIC.to_vec();
    cap_bytes.resize(1_048_576, 0);
    fs::write(&cap_path, &cap_bytes).unwrap();
    at_cap.icon = Some(cap_path);
    build(&at_cap, &tmp("icon-cap.nupkg")).expect("exactly 1 MiB is accepted");

    // One byte over the cap is rejected.
    let mut over = at_cap;
    let over_path = payload.join("over.png");
    let mut over_bytes = PNG_MAGIC.to_vec();
    over_bytes.resize(1_048_577, 0);
    fs::write(&over_path, &over_bytes).unwrap();
    over.icon = Some(over_path);
    let err = build(&over, &tmp("icon-over.nupkg")).unwrap_err();
    assert!(
        matches!(err, embala_nupkg::Error::IconTooLarge(_, 1_048_577)),
        "{err}"
    );
}

#[test]
fn invalid_specs_are_rejected() {
    let payload = tmp("payload-invalid");
    fs::create_dir_all(&payload).unwrap();
    fs::write(payload.join("hello.exe"), PAYLOAD_EXE).unwrap();
    let file = |dest: &str| FileSpec {
        src: payload.join("hello.exe"),
        dest: dest.to_string(),
        shim: true,
    };

    let fail = |style: Style| build(&base_spec(style), &tmp("invalid.nupkg")).unwrap_err();

    let err = fail(Style::Embedded {
        files: vec![],
        scripts: None,
    });
    assert!(matches!(err, embala_nupkg::Error::NoFiles), "{err}");

    let err = fail(Style::Embedded {
        files: vec![file("../evil.exe")],
        scripts: None,
    });
    assert!(matches!(err, embala_nupkg::Error::InvalidDest(_)), "{err}");

    let err = fail(Style::Embedded {
        files: vec![file("hello.exe"), file("hello.exe")],
        scripts: None,
    });
    assert!(
        matches!(err, embala_nupkg::Error::DuplicateDest(_)),
        "{err}"
    );

    // Would silently clobber the generated file on a case-insensitive extract.
    let err = fail(Style::Embedded {
        files: vec![file("verification.txt")],
        scripts: None,
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

#[test]
fn packages_without_new_keys_keep_their_bytes() {
    // Pinned at 496883a, before nupkg scripts and shim control existed.
    // Deployed machines call embedded exes through Chocolatey's shims in
    // C:\ProgramData\chocolatey\bin; a package built from an unchanged
    // config must stay byte-identical so upgrading embala never changes
    // what those machines get. Update the hashes only for a deliberate
    // format change.
    let (embedded, _) =
        embedded_spec_with_icon(&tmp("payload-pin"), "icon.png", PNG_MAGIC, b"pinned pixels");
    for (label, spec, expected) in [
        (
            "embedded",
            embedded,
            "b089ff74aa6b549eee5c22094420daa901677ec97a6c16b6c7e54e28e59b2631",
        ),
        (
            "download",
            download_spec(),
            "44b7fe3a6f60734ea77edc132d309dc53cf3a3fa08a9eabc016f602c8eb6bbd3",
        ),
    ] {
        let out = tmp(&format!("pin-{label}.nupkg"));
        build(&spec, &out).unwrap();
        let actual = format!("{:x}", Sha256::digest(fs::read(&out).unwrap()));
        assert_eq!(actual, expected, "{label} nupkg bytes changed");
    }
}

/// The payload exe written into `dir`, packed at `dest`.
fn payload_file(dir: &Path, dest: &str, shim: bool) -> FileSpec {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("hello.exe"), PAYLOAD_EXE).unwrap();
    FileSpec {
        src: dir.join("hello.exe"),
        dest: dest.to_string(),
        shim,
    }
}

/// All three supplied scripts, written into `dir` under names unlike any
/// in-package name, each with its own content.
fn supplied_scripts(dir: &Path) -> Scripts {
    fs::create_dir_all(dir).unwrap();
    for name in ["setup.ps1", "remove.ps1", "pre.ps1"] {
        fs::write(dir.join(name), format!("# {name}\n")).unwrap();
    }
    Scripts::Supplied {
        install: dir.join("setup.ps1"),
        uninstall: Some(dir.join("remove.ps1")),
        before_modify: Some(dir.join("pre.ps1")),
    }
}

fn entry_names(entries: &BTreeMap<String, Vec<u8>>) -> Vec<&str> {
    entries.keys().map(String::as_str).collect()
}

#[test]
fn embedded_supplied_scripts_pack_under_chocolatey_names() {
    let dir = tmp("payload-embedded-scripts");
    let spec = base_spec(Style::Embedded {
        files: vec![payload_file(&dir, "hello.exe", true)],
        scripts: Some(supplied_scripts(&dir)),
    });
    let out = tmp("embedded-scripts.nupkg");
    build(&spec, &out).unwrap();

    let entries = read_entries(&out);
    assert_eq!(
        entry_names(&entries),
        vec![
            "[Content_Types].xml",
            "_rels/.rels",
            "hello.nuspec",
            PSMDCP,
            "tools/LICENSE.txt",
            "tools/VERIFICATION.txt",
            "tools/chocolateyBeforeModify.ps1",
            "tools/chocolateyInstall.ps1",
            "tools/chocolateyUninstall.ps1",
            "tools/hello.exe",
        ]
    );
    // Byte-for-byte, whatever the source file was called.
    assert_eq!(entries["tools/chocolateyInstall.ps1"], b"# setup.ps1\n");
    assert_eq!(entries["tools/chocolateyUninstall.ps1"], b"# remove.ps1\n");
    assert_eq!(entries["tools/chocolateyBeforeModify.ps1"], b"# pre.ps1\n");
    // VERIFICATION.txt vouches for the payload, not the scripts.
    let verification = String::from_utf8(entries["tools/VERIFICATION.txt"].clone()).unwrap();
    assert!(!verification.contains(".ps1"), "{verification}");
}

#[test]
fn shim_false_adds_an_ignore_in_the_configured_case() {
    // Why: Chocolatey skips shimming an exe only when <exe>.ignore sits next
    // to it, so the name must match the dest exactly.
    let dir = tmp("payload-shim");
    let spec = base_spec(Style::Embedded {
        files: vec![
            payload_file(&dir, "hello.exe", true),
            payload_file(&dir, "bin/Helper.exe", false),
        ],
        scripts: None,
    });
    let out = tmp("shim.nupkg");
    build(&spec, &out).unwrap();

    let entries = read_entries(&out);
    assert_eq!(
        entry_names(&entries),
        vec![
            "[Content_Types].xml",
            "_rels/.rels",
            "hello.nuspec",
            PSMDCP,
            "tools/LICENSE.txt",
            "tools/VERIFICATION.txt",
            "tools/bin/Helper.exe",
            "tools/bin/Helper.exe.ignore",
            "tools/hello.exe",
        ]
    );
    assert!(entries["tools/bin/Helper.exe.ignore"].is_empty());
}

#[test]
fn plain_package_has_plain_scripts_and_a_guard() {
    let dir = tmp("payload-plain");
    let mut spec = base_spec(Style::Plain {
        // shim = false is rejected by config validation for plain; here it
        // proves a plain package never emits .ignore.
        files: vec![payload_file(&dir, "hello.exe", false)],
        scripts: supplied_scripts(&dir),
    });
    spec.display_name = "O'Hello Ünï".to_string();
    let out = tmp("plain.nupkg");
    build(&spec, &out).unwrap();

    let entries = read_entries(&out);
    assert_eq!(
        entry_names(&entries),
        vec![
            "[Content_Types].xml",
            "_rels/.rels",
            "hello.nuspec",
            PSMDCP,
            "tools/LICENSE.txt",
            "tools/VERIFICATION.txt",
            "tools/before-modify.ps1",
            "tools/chocolateyInstall.ps1",
            "tools/hello.exe",
            "tools/install.ps1",
            "tools/uninstall.ps1",
        ]
    );
    assert_eq!(entries["tools/install.ps1"], b"# setup.ps1\n");
    assert_eq!(entries["tools/uninstall.ps1"], b"# remove.ps1\n");
    assert_eq!(entries["tools/before-modify.ps1"], b"# pre.ps1\n");
    // The guard only throws, with the display name as a quoted literal. The
    // BOM keeps a non-ASCII name intact under Windows PowerShell 5.1.
    assert_eq!(
        String::from_utf8(entries["tools/chocolateyInstall.ps1"].clone()).unwrap(),
        "\u{feff}throw 'O''Hello Ünï is not a Chocolatey package'\n"
    );
}

#[test]
fn choco_script_dest_without_scripts_is_a_plain_payload_file() {
    // R7: configs that predate `scripts` may already ship this dest.
    let dir = tmp("payload-legacy-script");
    let spec = base_spec(Style::Embedded {
        files: vec![
            payload_file(&dir, "hello.exe", true),
            payload_file(&dir, "chocolateyInstall.ps1", true),
        ],
        scripts: None,
    });
    let out = tmp("legacy-script.nupkg");
    build(&spec, &out).unwrap();
    assert_eq!(
        read_entries(&out)["tools/chocolateyInstall.ps1"],
        PAYLOAD_EXE
    );
}

#[test]
fn script_and_ignore_collisions_are_rejected() {
    let dir = tmp("payload-collide");
    let scripts = supplied_scripts(&dir);
    let check = |style: Style| validate(&base_spec(style)).unwrap_err();

    // Either set, any case, once scripts are present or the style is plain.
    for dest in ["CHOCOLATEYINSTALL.PS1", "before-modify.ps1"] {
        let err = check(Style::Embedded {
            files: vec![payload_file(&dir, dest, true)],
            scripts: Some(scripts.clone()),
        });
        assert!(
            matches!(err, embala_nupkg::Error::ScriptDest(_)),
            "{dest}: {err}"
        );
        let err = check(Style::Plain {
            files: vec![payload_file(&dir, dest, true)],
            scripts: scripts.clone(),
        });
        assert!(
            matches!(err, embala_nupkg::Error::ScriptDest(_)),
            "{dest}: {err}"
        );
    }

    let err = check(Style::Embedded {
        files: vec![
            payload_file(&dir, "Hello.exe", false),
            payload_file(&dir, "HELLO.EXE.ignore", true),
        ],
        scripts: None,
    });
    assert!(
        matches!(err, embala_nupkg::Error::IgnoreDest(ref d, ref f)
            if d == "HELLO.EXE.ignore" && f == "Hello.exe"),
        "{err}"
    );
    // With generated scripts every .exe gets an .ignore, shim key or not.
    let err = check(Style::Embedded {
        files: vec![
            payload_file(&dir, "hello.exe", true),
            payload_file(&dir, "hello.exe.ignore", true),
        ],
        scripts: Some(Scripts::Generate),
    });
    assert!(matches!(err, embala_nupkg::Error::IgnoreDest(..)), "{err}");
}

#[test]
fn generated_scripts_need_a_valid_directory_display_name() {
    // Why: the generated install creates "<display-name> (nupkg)" under
    // Program Files, so the name must be a legal Windows directory name.
    let dir = tmp("payload-display-name");
    let generate = || Style::Embedded {
        files: vec![payload_file(&dir, "hello.exe", true)],
        scripts: Some(Scripts::Generate),
    };
    for bad in ["", "Hello: World", "a/b", "tab\there", "Hello.", "Hello "] {
        let mut spec = base_spec(generate());
        spec.display_name = bad.to_string();
        let err = validate(&spec).unwrap_err();
        assert!(
            matches!(err, embala_nupkg::Error::InvalidDisplayName(_)),
            "{bad:?}: {err}"
        );
        // Supplied scripts never use the name as a directory.
        spec.style = Style::Embedded {
            files: vec![payload_file(&dir, "hello.exe", true)],
            scripts: Some(supplied_scripts(&dir)),
        };
        validate(&spec).unwrap_or_else(|e| panic!("{bad:?}: {e}"));
    }
    validate(&base_spec(generate())).expect("a normal display name passes");
}

/// A display name that needs `ps_quote` escaping and the BOM.
const QUOTED_NAME: &str = "O'Hello Ünï";

fn text(entries: &BTreeMap<String, Vec<u8>>, name: &str) -> String {
    String::from_utf8(entries[name].clone()).unwrap()
}

/// The single-quoted literals of the script's `$payload = @( ... )` list.
fn payload_list(script: &str) -> Vec<&str> {
    let start = script.find("$payload = @(\n").expect("a payload list") + 14;
    let end = start + script[start..].find("\n)\n").unwrap();
    script[start..end].lines().map(str::trim).collect()
}

/// What every generated script must hold, whatever its job.
fn assert_generated_script(name: &str, script: &str) {
    // Windows PowerShell 5.1 reads a BOM-less file as ANSI and mangles a
    // non-ASCII display name.
    assert!(script.starts_with('\u{feff}'), "{name}: no BOM");
    // A ' in the name would end the literal early and break the script.
    assert!(script.contains("'O''Hello Ünï'"), "{name}");
    assert!(!script.contains("O'Hello"), "{name}: unescaped name");
    // Its own directory, never the MSI's <ProgramFiles>\<display-name>.
    assert!(script.contains("' (nupkg)'"), "{name}");
    // The same script must run under a script-only installer that has no
    // Chocolatey helpers or variables.
    let lowered = script.to_lowercase();
    for choco in [
        "chocolatey",
        "install-binfile",
        "uninstall-binfile",
        "update-sessionenvironment",
        "get-toolslocation",
        "get-packageparameters",
        "$env:choco",
    ] {
        assert!(!lowered.contains(choco), "{name} uses {choco}");
    }
    // Expands %SystemRoot% entries and writes REG_SZ.
    assert!(!script.contains("SetEnvironmentVariable"), "{name}");
}

/// Windows lets a directory be renamed while an exe inside it runs, so the
/// rename alone misses a running app: the exclusive-open probe of the same
/// directory must come right before every probing rename.
fn assert_probe_precedes_renames(name: &str, script: &str, dirs: &[&str]) {
    for dir in dirs {
        let rename = format!("Move-Dir {dir} ");
        let at = script
            .find(&rename)
            .unwrap_or_else(|| panic!("{name}: no rename of {dir}"));
        let before = script[..at].trim_end();
        assert!(
            before.ends_with(&format!("Assert-DirNotInUse {dir}")),
            "{name}: rename of {dir} is not preceded by its open probe"
        );
    }
}

#[test]
fn embedded_generate_packs_scripts_and_hides_every_exe_from_the_shim() {
    let dir = tmp("payload-embedded-generate");
    let mut spec = base_spec(Style::Embedded {
        files: vec![
            payload_file(&dir, "hello.exe", true),
            // shim = false and .exe both ask for an .ignore: emitted once.
            payload_file(&dir, "bin/Tool.EXE", false),
            payload_file(&dir, "bin/[data].txt", true),
        ],
        scripts: Some(Scripts::Generate),
    });
    spec.display_name = QUOTED_NAME.to_string();
    let out = tmp("embedded-generate.nupkg");
    build(&spec, &out).unwrap();

    let entries = read_entries(&out);
    assert_eq!(
        entry_names(&entries),
        vec![
            "[Content_Types].xml",
            "_rels/.rels",
            "hello.nuspec",
            PSMDCP,
            "tools/LICENSE.txt",
            "tools/VERIFICATION.txt",
            "tools/bin/Tool.EXE",
            "tools/bin/Tool.EXE.ignore",
            "tools/bin/[data].txt",
            "tools/chocolateyInstall.ps1",
            "tools/chocolateyUninstall.ps1",
            "tools/hello.exe",
            "tools/hello.exe.ignore",
        ]
    );
    let install = text(&entries, "tools/chocolateyInstall.ps1");
    let uninstall = text(&entries, "tools/chocolateyUninstall.ps1");
    assert_generated_script("install", &install);
    assert_generated_script("uninstall", &uninstall);
    assert_probe_precedes_renames("install", &install, &["$prev", "$dir"]);
    assert_probe_precedes_renames("uninstall", &uninstall, &["$target"]);
    // Exactly the payload, as Windows paths: no scripts, .ignore files,
    // LICENSE.txt or VERIFICATION.txt land in the install directory.
    assert_eq!(
        payload_list(&install),
        vec!["'hello.exe'", "'bin\\Tool.EXE'", "'bin\\[data].txt'"]
    );
    // The lock probe reports a read-only file as in use, so every copy must
    // drop ReadOnly or the next upgrade or uninstall refuses to run.
    let copy = install.find("[System.IO.File]::Copy(").unwrap();
    assert!(
        install[copy..]
            .find("SetAttributes($target, [System.IO.FileAttributes]::Normal)")
            .is_some_and(|at| at < 400),
        "install does not clear ReadOnly on its copies"
    );
    // The uninstall removes a directory; it copies nothing.
    assert!(!uninstall.contains("$payload"));

    // Reproducible, scripts included.
    let again = tmp("embedded-generate-again.nupkg");
    build(&spec, &again).unwrap();
    assert_eq!(fs::read(&out).unwrap(), fs::read(&again).unwrap());
}

#[test]
fn plain_generate_packs_plain_scripts_and_the_guard_only() {
    let dir = tmp("payload-plain-generate");
    let mut spec = base_spec(Style::Plain {
        files: vec![payload_file(&dir, "hello.exe", true)],
        scripts: Scripts::Generate,
    });
    spec.display_name = QUOTED_NAME.to_string();
    let out = tmp("plain-generate.nupkg");
    build(&spec, &out).unwrap();

    let entries = read_entries(&out);
    // No .ignore (the guard fails choco first) and no before-modify.
    assert_eq!(
        entry_names(&entries),
        vec![
            "[Content_Types].xml",
            "_rels/.rels",
            "hello.nuspec",
            PSMDCP,
            "tools/LICENSE.txt",
            "tools/VERIFICATION.txt",
            "tools/chocolateyInstall.ps1",
            "tools/hello.exe",
            "tools/install.ps1",
            "tools/uninstall.ps1",
        ]
    );
    let install = text(&entries, "tools/install.ps1");
    assert_generated_script("install", &install);
    assert_generated_script("uninstall", &text(&entries, "tools/uninstall.ps1"));
    assert_eq!(payload_list(&install), vec!["'hello.exe'"]);
    assert!(text(&entries, "tools/chocolateyInstall.ps1").contains("is not a Chocolatey package"));
}

#[test]
fn typographic_quotes_in_the_display_name_are_doubled() {
    // Why: PowerShell ends a single-quoted literal at ’ just as at ', so an
    // undoubled one breaks the generated script.
    let dir = tmp("payload-typographic");
    let mut spec = base_spec(Style::Embedded {
        files: vec![payload_file(&dir, "hello.exe", true)],
        scripts: Some(Scripts::Generate),
    });
    spec.display_name = "Carlos’ Hello".to_string();
    let out = tmp("typographic.nupkg");
    build(&spec, &out).unwrap();

    let entries = read_entries(&out);
    for name in [
        "tools/chocolateyInstall.ps1",
        "tools/chocolateyUninstall.ps1",
    ] {
        let script = text(&entries, name);
        assert!(
            script.contains("$displayName = 'Carlos’’ Hello'\n"),
            "{name}"
        );
    }
}

#[test]
fn generated_scripts_reject_dests_that_could_leave_the_install_dir() {
    // Why: the generated install joins each dest onto the install directory
    // with Path.Combine and runs as SYSTEM; a \ or : could escape it.
    let dir = tmp("payload-generated-dest");
    for bad in [
        "..\\evil.exe",
        "C:\\Windows\\evil.exe",
        "C:evil.exe",
        "bin\\x.exe",
    ] {
        for style in [
            Style::Embedded {
                files: vec![payload_file(&dir, bad, true)],
                scripts: Some(Scripts::Generate),
            },
            Style::Plain {
                files: vec![payload_file(&dir, bad, true)],
                scripts: Scripts::Generate,
            },
        ] {
            let err = validate(&base_spec(style)).unwrap_err();
            assert!(
                matches!(err, embala_nupkg::Error::GeneratedDest(ref d) if d == bad),
                "{bad:?}: {err}"
            );
            assert!(err.to_string().starts_with("nupkg:"), "{err}");
        }
        // Without generate the older rules stand, so existing builds keep working.
        validate(&base_spec(Style::Embedded {
            files: vec![payload_file(&dir, bad, true)],
            scripts: None,
        }))
        .unwrap_or_else(|e| panic!("{bad:?}: {e}"));
    }
}
