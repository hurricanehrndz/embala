//! Build-time PE resource branding for the setup stub (spec R7).
//!
//! NSIS's own architecture: the prebuilt stub carries a generic icon and no
//! version info; `embala build` patches its PE resources *before* the overlay is
//! appended — never compiling. Patching is a pure function of `(stub, Branding)`
//! so `setup.exe` stays byte-reproducible (spec R8). `uninstall.exe` is a prefix
//! copy of the running stub and inherits the branding for free.
//!
//! editpe 0.2.3's structured `set_version_info` computes VS_VERSIONINFO header
//! lengths from `str::len()` (UTF-8 bytes) while serializing strings as UTF-16,
//! so any non-ASCII value (e.g. a "©" copyright) desyncs the structure and
//! Windows drops the field. We take the plan's pre-authorized fallback: build
//! the VS_VERSIONINFO blob by hand with correct UTF-16 unit counts and set it as
//! a raw `RT_VERSION` id-1 resource. editpe still handles everything else.

use std::io::Cursor;

use editpe::constants::{LANGUAGE_ID_EN_US, RT_RCDATA, RT_VERSION};
use editpe::{Image, ResourceData, ResourceEntry, ResourceEntryName, ResourceTable};

use crate::{Error, Result};

/// Product identity + optional bitmaps patched into the stub. Self-contained so
/// the crate stays independent of the embala config types.
#[derive(Debug, Clone, Default)]
pub struct Branding {
    /// PNG or ICO icon bytes; `Some` sets the stub's main icon (spec R7).
    pub icon: Option<Vec<u8>>,
    /// BMP banner bytes; `Some` adds the named `EMBALA_BANNER` RCDATA resource.
    pub banner: Option<Vec<u8>>,
    pub display_name: String,
    pub publisher: String,
    pub description: String,
    /// Full version string (`ProductVersion`/`FileVersion` string values).
    pub version: String,
    /// `LegalCopyright`, emitted only when set.
    pub copyright: Option<String>,
}

/// Patch `stub`'s PE resources per `branding` and return the rebuilt bytes.
/// Always writes VERSIONINFO; sets the main icon / banner resource when supplied.
pub fn patch_stub(stub: &[u8], branding: &Branding) -> Result<Vec<u8>> {
    let mut image = Image::parse(stub).map_err(brand_err)?;
    let mut resources = image.resource_directory().cloned().unwrap_or_default();

    set_version_raw(&mut resources, &version_blob(branding));
    if let Some(icon) = &branding.icon {
        // set_main_icon_reader decodes PNG/ICO via the `images` feature and
        // resizes to the standard resolutions.
        resources
            .set_main_icon_reader(&mut Cursor::new(icon))
            .map_err(brand_err)?;
    }
    if let Some(banner) = &branding.banner {
        set_named_rcdata(&mut resources, "EMBALA_BANNER", banner);
    }

    image.set_resource_directory(resources).map_err(brand_err)?;
    let mut out = Vec::new();
    image.write_writer(&mut out).map_err(brand_err)?;
    Ok(out)
}

fn brand_err(e: impl std::fmt::Display) -> Error {
    Error::Brand(e.to_string())
}

/// Build the VS_VERSIONINFO blob from the branding (spec R7). String table under
/// the en-US/UTF-16 (`040904B0`) language, matching the `Translation` var. Every
/// `wLength`/`wValueLength` is an exact count of the UTF-16-encoded bytes/units,
/// which is the whole reason we assemble this by hand instead of via editpe.
fn version_blob(b: &Branding) -> Vec<u8> {
    let (major, minor, patch, build) = numeric_version(&b.version);
    let ver_ms = ((major as u32) << 16) | minor as u32;
    let ver_ls = ((patch as u32) << 16) | build as u32;

    // VS_FIXEDFILEINFO — 13 DWORDs, 52 bytes; this is the root node's value.
    let mut fixed = Vec::with_capacity(52);
    for dw in [
        0xFEEF_04BDu32, // dwSignature
        0x0001_0000,    // dwStrucVersion 1.0
        ver_ms,         // dwFileVersionMS
        ver_ls,         // dwFileVersionLS
        ver_ms,         // dwProductVersionMS
        ver_ls,         // dwProductVersionLS
        0x3F,           // dwFileFlagsMask
        0,              // dwFileFlags
        0x0004_0004,    // dwFileOS = VOS_NT_WINDOWS32
        1,              // dwFileType = VFT_APP
        0,              // dwFileSubtype
        0,              // dwFileDateMS
        0,              // dwFileDateLS
    ] {
        fixed.extend_from_slice(&dw.to_le_bytes());
    }

    // StringTable "040904B0" — same keys/order as before, copyright when set.
    let mut strings: Vec<(&str, &str)> = vec![
        ("ProductName", &b.display_name),
        ("CompanyName", &b.publisher),
        ("FileDescription", &b.description),
        ("FileVersion", &b.version),
        ("ProductVersion", &b.version),
    ];
    if let Some(copyright) = &b.copyright {
        strings.push(("LegalCopyright", copyright));
    }
    let string_nodes: Vec<Vec<u8>> = strings.iter().map(|(k, v)| string_entry(k, v)).collect();
    let string_table = node("040904B0", 0, 1, &concat_children(&string_nodes));
    let string_file_info = node("StringFileInfo", 0, 1, &string_table);

    // VarFileInfo / Translation = 0x0409 (en-US) + 0x04B0 (UTF-16 code page).
    let mut translation = Vec::with_capacity(4);
    translation.extend_from_slice(&0x0409u16.to_le_bytes());
    translation.extend_from_slice(&0x04B0u16.to_le_bytes());
    let var = node("Translation", translation.len() as u16, 0, &translation);
    let var_file_info = node("VarFileInfo", 0, 1, &var);

    let mut body = fixed;
    body.extend_from_slice(&string_file_info);
    pad4(&mut body);
    body.extend_from_slice(&var_file_info);
    node("VS_VERSION_INFO", 52, 0, &body)
}

/// UTF-16LE bytes of `s` with a nul terminator.
fn utf16z(s: &str) -> Vec<u8> {
    let mut out: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
    out.extend_from_slice(&[0, 0]);
    out
}

/// Pad `v` up to the next 4-byte boundary (offsets are DWORD-aligned relative to
/// the blob start, which is itself DWORD-aligned).
fn pad4(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
}

/// A version node: `wLength`, `wValueLength`, `wType`, UTF-16 `szKey` + nul,
/// pad to 4, then `body`. `wLength` counts the node through `body`, excluding any
/// trailing pad that aligns a following sibling (the parent adds that).
fn node(key: &str, value_len: u16, wtype: u16, body: &[u8]) -> Vec<u8> {
    let mut inner = utf16z(key);
    while !(6 + inner.len()).is_multiple_of(4) {
        inner.push(0); // Padding1: DWORD-align the value/children after szKey.
    }
    inner.extend_from_slice(body);
    let length = 6 + inner.len();
    let mut out = Vec::with_capacity(length);
    out.extend_from_slice(&(length as u16).to_le_bytes());
    out.extend_from_slice(&value_len.to_le_bytes());
    out.extend_from_slice(&wtype.to_le_bytes());
    out.extend_from_slice(&inner);
    out
}

/// One `String` entry. `wValueLength` is the value's UTF-16 unit count including
/// the nul terminator — the count editpe gets wrong for non-ASCII values.
fn string_entry(name: &str, value: &str) -> Vec<u8> {
    let value_units = value.encode_utf16().count() as u16 + 1;
    node(name, value_units, 1, &utf16z(value))
}

/// Concatenate child nodes, DWORD-aligning each after the previous.
fn concat_children(nodes: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        out.extend_from_slice(n);
        if i + 1 < nodes.len() {
            pad4(&mut out);
        }
    }
    out
}

/// Numeric `a.b.c.d` VERSIONINFO tuple from a version string: the leading
/// `major[.minor[.patch]]` components, the fourth always zero, and any
/// unparseable component (or the whole string) collapsing to zero (spec R7).
fn numeric_version(version: &str) -> (u16, u16, u16, u16) {
    let mut parts = version.split('.');
    let mut next = || {
        parts
            .next()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0)
    };
    (next(), next(), next(), 0)
}

/// Set `blob` as the raw `RT_VERSION` resource id 1 under en-US, replacing any
/// version info the prebuilt stub carried (root → RT_VERSION → ID(1) → en-US).
fn set_version_raw(resources: &mut editpe::ResourceDirectory, blob: &[u8]) {
    let mut data = ResourceData::default();
    data.set_data(blob.to_vec());
    let mut lang = ResourceTable::default();
    lang.insert(
        ResourceEntryName::ID(LANGUAGE_ID_EN_US as u32),
        ResourceEntry::Data(data),
    );
    let mut version = ResourceTable::default();
    version.insert(ResourceEntryName::ID(1), ResourceEntry::Table(lang));
    resources.root_mut().insert(
        ResourceEntryName::ID(RT_VERSION as u32),
        ResourceEntry::Table(version),
    );
}

/// Add `data` as a named `RCDATA` resource (mirrors editpe's `set_manifest`
/// nesting: root → RT_RCDATA table → named table → data under en-US).
fn set_named_rcdata(resources: &mut editpe::ResourceDirectory, name: &str, data: &[u8]) {
    let root = resources.root_mut();
    if root.get(ResourceEntryName::ID(RT_RCDATA as u32)).is_none() {
        root.insert(
            ResourceEntryName::ID(RT_RCDATA as u32),
            ResourceEntry::Table(ResourceTable::default()),
        );
    }
    let table = match root
        .get_mut(ResourceEntryName::ID(RT_RCDATA as u32))
        .unwrap()
    {
        ResourceEntry::Table(t) => t,
        ResourceEntry::Data(_) => unreachable!("RT_RCDATA is always a table"),
    };

    let mut inner = ResourceTable::default();
    let mut entry = ResourceData::default();
    entry.set_data(data.to_vec());
    inner.insert(
        ResourceEntryName::ID(LANGUAGE_ID_EN_US as u32),
        ResourceEntry::Data(entry),
    );
    table.insert_at(
        ResourceEntryName::from_string(name),
        ResourceEntry::Table(inner),
        0,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SetupArch, stub_bytes};

    const ICON_PNG: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/hello/icon.png"
    ));
    const BANNER_BMP: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/hello/banner.bmp"
    ));

    fn branding() -> Branding {
        Branding {
            icon: Some(ICON_PNG.to_vec()),
            banner: Some(BANNER_BMP.to_vec()),
            display_name: "Embala Hello".to_string(),
            publisher: "Carlos Hernandez".to_string(),
            description: "test fixture".to_string(),
            version: "1.2.3".to_string(),
            copyright: Some("© 2026 Carlos Hernandez".to_string()),
        }
    }

    /// Fetch the raw RT_VERSION / id-1 / en-US resource bytes.
    fn version_data(resources: &editpe::ResourceDirectory) -> Vec<u8> {
        let table = match resources
            .root()
            .get(ResourceEntryName::ID(RT_VERSION as u32))
            .expect("RT_VERSION present")
        {
            ResourceEntry::Table(t) => t,
            _ => panic!("RT_VERSION is a table"),
        };
        let lang = match table.get(ResourceEntryName::ID(1)).expect("id 1 present") {
            ResourceEntry::Table(t) => t,
            _ => panic!("id 1 is a table"),
        };
        match lang
            .get(ResourceEntryName::ID(LANGUAGE_ID_EN_US as u32))
            .expect("en-US present")
        {
            ResourceEntry::Data(d) => d.data().to_vec(),
            _ => panic!("en-US is data"),
        }
    }

    fn rd_u16(b: &[u8], o: usize) -> usize {
        u16::from_le_bytes([b[o], b[o + 1]]) as usize
    }
    fn align4(n: usize) -> usize {
        n.div_ceil(4) * 4
    }
    /// Read a UTF-16 nul-terminated string; returns (value, offset past the nul).
    fn read_sz(b: &[u8], mut o: usize) -> (String, usize) {
        let mut units = Vec::new();
        while rd_u16(b, o) != 0 {
            units.push(rd_u16(b, o) as u16);
            o += 2;
        }
        (String::from_utf16_lossy(&units), o + 2)
    }

    /// Walk the hand-built blob, asserting each `String`'s declared `wLength` /
    /// `wValueLength` match the bytes actually written, and return name→value.
    /// A byte-vs-unit desync (the editpe bug we route around) trips an assert.
    fn parse_and_check(blob: &[u8]) -> std::collections::BTreeMap<String, String> {
        assert_eq!(rd_u16(blob, 0), blob.len(), "root wLength spans the blob");
        let (rkey, o) = read_sz(blob, 6);
        assert_eq!(rkey, "VS_VERSION_INFO");
        let mut o = align4(o) + 52; // skip Padding1 + VS_FIXEDFILEINFO.

        let mut map = std::collections::BTreeMap::new();
        let root_len = rd_u16(blob, 0);
        while o < root_len {
            let blen = rd_u16(blob, o);
            let (bkey, p) = read_sz(blob, o + 6);
            if bkey == "StringFileInfo" {
                let mut p = align4(p);
                while p < o + blen {
                    let stlen = rd_u16(blob, p);
                    let (_st, q) = read_sz(blob, p + 6);
                    let mut q = align4(q);
                    while q < p + stlen {
                        let slen = rd_u16(blob, q);
                        let vlen = rd_u16(blob, q + 2); // UTF-16 units incl. nul.
                        let (name, after) = read_sz(blob, q + 6);
                        let vo = align4(after);
                        let (value, vend) = read_sz(blob, vo);
                        let units = value.encode_utf16().count() + 1;
                        assert_eq!(vlen, units, "wValueLength desync for {name}");
                        assert_eq!(slen, vend - q, "wLength desync for {name}");
                        map.insert(name, value);
                        q = align4(q + slen);
                    }
                    p = align4(p + stlen);
                }
            }
            o = align4(o + blen);
        }
        map
    }

    #[test]
    fn version_blob_lengths_match_bytes() {
        // Why: the raison d'être of the hand-built blob — non-ASCII values must
        // declare UTF-16 *unit* counts, not UTF-8 byte counts (the editpe bug).
        let b = Branding {
            display_name: "Ünïcödé Ürünü".to_string(),
            publisher: "Håkon &  Cie ©".to_string(),
            description: "描述".to_string(),
            version: "3.4.5".to_string(),
            copyright: Some("© 2026 Carlos Hernández".to_string()),
            ..Default::default()
        };
        let strings = parse_and_check(&version_blob(&b)); // asserts internally.
        assert_eq!(
            strings.get("CompanyName").map(String::as_str),
            Some("Håkon &  Cie ©")
        );
        assert_eq!(
            strings.get("LegalCopyright").map(String::as_str),
            Some("© 2026 Carlos Hernández")
        );
    }

    #[test]
    fn numeric_version_cases() {
        assert_eq!(numeric_version("1.2.3"), (1, 2, 3, 0));
        assert_eq!(numeric_version("0.1.0-rc1"), (0, 1, 0, 0));
        assert_eq!(numeric_version("garbage"), (0, 0, 0, 0));
    }

    #[test]
    fn patching_is_deterministic() {
        // Why: byte-reproducibility (spec R8) requires a pure patch — a hash-map
        // iteration order or timestamp leak would break signing/caching.
        let stub = stub_bytes(SetupArch::X86_64);
        let a = patch_stub(stub, &branding()).unwrap();
        let b = patch_stub(stub, &branding()).unwrap();
        assert_eq!(a, b, "same inputs must patch to identical bytes");
    }

    #[test]
    fn patched_stub_round_trips() {
        // Why: the patched stub must re-parse as a valid PE carrying the icon,
        // VERSIONINFO, and EMBALA_BANNER, with pre-existing resources (the stub's
        // application manifest) left intact.
        let patched = patch_stub(stub_bytes(SetupArch::X86_64), &branding()).unwrap();
        let image = Image::parse(patched.as_slice()).expect("patched stub is a valid PE");
        let resources = image
            .resource_directory()
            .expect("resource directory present");

        // Parse our own raw RT_VERSION/id-1 blob (editpe's get_version_info shares
        // the byte-vs-unit bug, so we can't trust it as an oracle).
        let strings = parse_and_check(&version_data(resources));
        assert_eq!(
            strings.get("ProductName").map(String::as_str),
            Some("Embala Hello")
        );
        assert_eq!(
            strings.get("FileVersion").map(String::as_str),
            Some("1.2.3")
        );
        assert_eq!(
            strings.get("LegalCopyright").map(String::as_str),
            Some("© 2026 Carlos Hernandez"),
            "non-ASCII copyright must survive intact"
        );

        assert!(
            resources.get_main_icon().unwrap().is_some(),
            "main icon set"
        );
        // The stub ships an application manifest (setup.rc); patching must not
        // drop it.
        assert!(
            resources.get_manifest().unwrap().is_some(),
            "pre-existing manifest intact"
        );

        // EMBALA_BANNER lives under RT_RCDATA as a named entry.
        let rcdata = resources
            .root()
            .get(ResourceEntryName::ID(RT_RCDATA as u32))
            .and_then(|e| match e {
                ResourceEntry::Table(t) => Some(t),
                _ => None,
            })
            .expect("RCDATA table present");
        assert!(
            rcdata
                .get(ResourceEntryName::from_string("EMBALA_BANNER"))
                .is_some(),
            "EMBALA_BANNER resource present"
        );
    }

    #[test]
    fn version_info_without_icon_or_banner() {
        // Why: VERSIONINFO is always set even when no bitmaps are configured
        // (spec R7); icon/banner stay absent.
        let b = Branding {
            version: "2.5.0".to_string(),
            display_name: "Bare".to_string(),
            ..Default::default()
        };
        let patched = patch_stub(stub_bytes(SetupArch::X86_64), &b).unwrap();
        let image = Image::parse(patched.as_slice()).unwrap();
        let resources = image.resource_directory().unwrap();
        assert!(
            resources
                .root()
                .get(ResourceEntryName::ID(RT_VERSION as u32))
                .is_some(),
            "VERSIONINFO always set"
        );
        assert!(
            resources
                .root()
                .get(ResourceEntryName::ID(RT_RCDATA as u32))
                .is_none(),
            "no banner when unset"
        );
    }
}
