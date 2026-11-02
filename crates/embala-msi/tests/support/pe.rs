//! Synthetic PE images for version-resource tests, shared by the unit tests
//! (`src/version.rs`) and `tests/build.rs`. Just enough structure for
//! embala-msi's reader, not loadable images.

#![allow(dead_code)]

const RT_VERSION: u32 = 16;
const FIXED_SIGNATURE: u32 = 0xFEEF_04BD;

fn utf16z(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
    v.extend_from_slice(&[0, 0]);
    v
}

fn pad4(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
}

fn vnode(key: &str, value_len: u16, value: &[u8], children: &[u8]) -> Vec<u8> {
    let mut v = vec![0, 0];
    v.extend_from_slice(&value_len.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&utf16z(key));
    pad4(&mut v);
    v.extend_from_slice(value);
    pad4(&mut v);
    v.extend_from_slice(children);
    let len = v.len() as u16;
    v[0..2].copy_from_slice(&len.to_le_bytes());
    v
}

/// A VS_VERSIONINFO for `version` with the given Translation languages.
pub fn version_info(version: [u16; 4], languages: &[u16]) -> Vec<u8> {
    let mut fixed = Vec::new();
    for dw in [
        FIXED_SIGNATURE,
        0x0001_0000,
        (version[0] as u32) << 16 | version[1] as u32,
        (version[2] as u32) << 16 | version[3] as u32,
        0,
        0,
        0,
        0,
        0x0004_0004,
        1,
        0,
        0,
        0,
    ] {
        fixed.extend_from_slice(&dw.to_le_bytes());
    }
    let mut translation = Vec::new();
    for lang in languages {
        translation.extend_from_slice(&lang.to_le_bytes());
        translation.extend_from_slice(&0x04B0u16.to_le_bytes());
    }
    let var = vnode("Translation", translation.len() as u16, &translation, &[]);
    let var_file_info = vnode("VarFileInfo", 0, &[], &var);
    vnode("VS_VERSION_INFO", 52, &fixed, &var_file_info)
}

/// A minimal PE32+ whose only section is `.rsrc`, holding one RT_VERSION
/// resource per `(name, blob)` in that order (`name` is the raw directory
/// entry field: an id, or high bit set for a named entry), or no resource
/// directory when empty. Enough structure for `read`, not a loadable image.
pub fn pe_with_versions(versions: &[(u32, Vec<u8>)]) -> Vec<u8> {
    const PE: usize = 0x40;
    const OPT: usize = PE + 24;
    const OPT_SIZE: usize = 240;
    const SECTION: usize = OPT + OPT_SIZE;
    const RAW: usize = 0x200;
    const RVA: u32 = 0x1000;
    let mut b = vec![0u8; RAW];
    b[0..2].copy_from_slice(b"MZ");
    b[0x3C..0x40].copy_from_slice(&(PE as u32).to_le_bytes());
    b[PE..PE + 4].copy_from_slice(b"PE\0\0");
    b[PE + 4..PE + 6].copy_from_slice(&0x8664u16.to_le_bytes());
    b[PE + 6..PE + 8].copy_from_slice(&1u16.to_le_bytes()); // sections
    b[PE + 20..PE + 22].copy_from_slice(&(OPT_SIZE as u16).to_le_bytes());
    b[OPT..OPT + 2].copy_from_slice(&0x20bu16.to_le_bytes());
    b[OPT + 108..OPT + 112].copy_from_slice(&16u32.to_le_bytes());

    let mut rsrc = Vec::new();
    if !versions.is_empty() {
        // Three directory levels (type, name, language), then one data
        // entry per blob, then the blobs.
        let header = |named: usize, ids: usize| {
            let mut d = vec![0u8; 16];
            d[12..14].copy_from_slice(&(named as u16).to_le_bytes());
            d[14..16].copy_from_slice(&(ids as u16).to_le_bytes());
            d
        };
        let entry = |d: &mut Vec<u8>, id: u32, target: u32| {
            d.extend_from_slice(&id.to_le_bytes());
            d.extend_from_slice(&target.to_le_bytes());
        };
        let n = versions.len();
        let names_dir = 24;
        let langs_dir = names_dir + 16 + 8 * n;
        let data_entries = langs_dir + 24 * n;
        let mut blob_at = data_entries + 16 * n;

        rsrc.extend(header(0, 1));
        entry(&mut rsrc, RT_VERSION, 0x8000_0000 | names_dir as u32);
        let named = versions.iter().filter(|(name, _)| name & 0x8000_0000 != 0);
        rsrc.extend(header(named.count(), n));
        for (i, (name, _)) in versions.iter().enumerate() {
            entry(&mut rsrc, *name, 0x8000_0000 | (langs_dir + 24 * i) as u32);
        }
        for i in 0..n {
            rsrc.extend(header(0, 1));
            entry(&mut rsrc, 1033, (data_entries + 16 * i) as u32);
        }
        for (_, blob) in versions {
            rsrc.extend_from_slice(&(RVA + blob_at as u32).to_le_bytes());
            rsrc.extend_from_slice(&(blob.len() as u32).to_le_bytes());
            rsrc.extend_from_slice(&[0; 8]);
            blob_at += blob.len().next_multiple_of(4);
        }
        for (_, blob) in versions {
            rsrc.extend_from_slice(blob);
            pad4(&mut rsrc);
        }
        let dirs = OPT + 112;
        b[dirs + 16..dirs + 20].copy_from_slice(&RVA.to_le_bytes());
        b[dirs + 20..dirs + 24].copy_from_slice(&(rsrc.len() as u32).to_le_bytes());
    }
    b[SECTION..SECTION + 5].copy_from_slice(b".rsrc");
    let len = (rsrc.len() as u32).max(1);
    b[SECTION + 8..SECTION + 12].copy_from_slice(&len.to_le_bytes());
    b[SECTION + 12..SECTION + 16].copy_from_slice(&RVA.to_le_bytes());
    b[SECTION + 16..SECTION + 20].copy_from_slice(&len.to_le_bytes());
    b[SECTION + 20..SECTION + 24].copy_from_slice(&(RAW as u32).to_le_bytes());
    b.extend_from_slice(&rsrc);
    b
}

pub fn versioned_pe(version: [u16; 4], languages: &[u16]) -> Vec<u8> {
    pe_with_versions(&[(1, version_info(version, languages))])
}
