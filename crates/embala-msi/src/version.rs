//! `File.Version` / `File.Language` from a payload PE's version resource.
//!
//! Windows Installer compares a component's key file by `File.Version`; a NULL
//! version means "unversioned", which always loses to an installed versioned
//! file. Leaving a versioned PE's version out therefore makes a MajorUpgrade
//! skip the component ("higher versioned keyfile exists"), and the early
//! RemoveExistingProducts then deletes the old copy. WiX fills both columns
//! from MsiGetFileVersion; this reads the same data straight from the file.
//!
//! Hand-rolled rather than editpe (already in the workspace): editpe's
//! `Image::parse` slices at header offsets without bounds checks and panics on
//! a truncated or fake PE. Every read here is checked, and anything that is not
//! a well-formed PE with a VS_FIXEDFILEINFO is simply unversioned.
//!
//! `embala-setup-overlay/src/pe.rs` walks the same PE headers (to the
//! security directory) over `Read + Seek`; the two are kept separate.

/// RT_VERSION resource type id.
const RT_VERSION: u32 = 16;
/// VS_FIXEDFILEINFO.dwSignature.
const FIXED_SIGNATURE: u32 = 0xFEEF_04BD;

/// What goes into the `File` table for one payload file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct FileVersion {
    /// `a.b.c.d` from dwFileVersionMS/LS.
    pub version: String,
    /// Decimal language ids from `VarFileInfo\Translation`, comma-separated
    /// (MsiGetFileVersion's format); `None` when the resource has none.
    pub language: Option<String>,
}

/// The file version of PE `bytes`, or `None` for anything else.
pub(crate) fn read(bytes: &[u8]) -> Option<FileVersion> {
    let blob = version_resource(bytes)?;
    parse_version_info(blob)
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// The raw VS_VERSIONINFO bytes: RT_VERSION id 1 (else the first name), first
/// language. Windows picks the language by the user's UI language with
/// fallbacks; a PE normally has one, so the first is close enough.
fn version_resource(b: &[u8]) -> Option<&[u8]> {
    if b.get(0..2)? != b"MZ" {
        return None;
    }
    let pe = u32_at(b, 0x3C)? as usize;
    if b.get(pe..pe.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    let coff = pe + 4;
    let sections = u16_at(b, coff + 2)? as usize;
    let opt_size = u16_at(b, coff + 16)? as usize;
    let opt = coff + 20;
    // NumberOfRvaAndSizes and the data-directory array, PE32 vs PE32+.
    let (num_rvas, dirs) = match u16_at(b, opt)? {
        0x10b => (opt + 92, opt + 96),
        0x20b => (opt + 108, opt + 112),
        _ => return None,
    };
    if u32_at(b, num_rvas)? <= 2 {
        return None;
    }
    let rsrc_rva = u32_at(b, dirs + 2 * 8)?; // entry 2: resource table
    if rsrc_rva == 0 {
        return None;
    }

    // Map an RVA to a file offset through the section table.
    let table = opt + opt_size;
    let to_offset = |rva: u32| -> Option<usize> {
        (0..sections).find_map(|i| {
            let s = table + i * 40;
            let virtual_size = u32_at(b, s + 8)?;
            let virtual_address = u32_at(b, s + 12)?;
            let raw_size = u32_at(b, s + 16)?;
            let raw_pointer = u32_at(b, s + 20)?;
            let delta = rva.checked_sub(virtual_address)?;
            if delta >= virtual_size.max(raw_size) {
                return None;
            }
            (raw_pointer as usize).checked_add(delta as usize)
        })
    };
    let root = to_offset(rsrc_rva)?;

    // IMAGE_RESOURCE_DIRECTORY: named entries, then id entries, 8 bytes
    // each; the high bit of OffsetToData marks a subdirectory (offsets are
    // relative to the resource root).
    let entries = |dir: usize| -> Option<Vec<(u32, u32)>> {
        let count = u16_at(b, dir + 12)? as usize + u16_at(b, dir + 14)? as usize;
        (0..count)
            .map(|i| Some((u32_at(b, dir + 16 + i * 8)?, u32_at(b, dir + 20 + i * 8)?)))
            .collect()
    };
    let at = |off: u32| root.checked_add(off as usize).filter(|&o| o < b.len());
    let subdir = |off: u32| at(off & 0x7FFF_FFFF).filter(|_| off & 0x8000_0000 != 0);

    let (_, types) = entries(root)?
        .into_iter()
        .find(|(name, _)| *name == RT_VERSION)?;
    let names = subdir(types)?;
    // Named entries sort before id entries; only the high bit marks a name.
    let names = entries(names)?;
    let (_, langs) = *names.iter().find(|(id, _)| *id == 1).or(names.first())?;
    let langs = subdir(langs)?;
    let (_, data) = *entries(langs)?.first()?;
    if data & 0x8000_0000 != 0 {
        return None;
    }
    // IMAGE_RESOURCE_DATA_ENTRY: OffsetToData is an RVA, then Size.
    let data = at(data)?;
    let start = to_offset(u32_at(b, data)?)?;
    let size = u32_at(b, data + 4)? as usize;
    b.get(start..start.checked_add(size)?)
}

fn align4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

/// One version-info node: `(key, value offset, end)`, offsets into `b`.
/// Layout: wLength, wValueLength, wType, then a NUL-terminated UTF-16 key,
/// padding to a DWORD, the value, and DWORD-aligned children up to wLength.
/// `None` unless the node, key included, fits within its parent's `limit`.
fn node(b: &[u8], off: usize, limit: usize) -> Option<(String, usize, usize)> {
    let len = u16_at(b, off)? as usize;
    let end = off.checked_add(len)?;
    if len < 6 || end > limit {
        return None;
    }
    let b = b.get(..end)?;
    let mut units = Vec::new();
    let mut p = off + 6;
    loop {
        match u16_at(b, p)? {
            0 => break,
            u => units.push(u),
        }
        p += 2;
    }
    Some((String::from_utf16_lossy(&units), align4(p + 2), end))
}

fn parse_version_info(b: &[u8]) -> Option<FileVersion> {
    let (key, value, end) = node(b, 0, b.len())?;
    // The 52-byte VS_FIXEDFILEINFO must fit inside the root node.
    if key != "VS_VERSION_INFO"
        || u16_at(b, 2)? < 52
        || value.checked_add(52)? > end
        || u32_at(b, value)? != FIXED_SIGNATURE
    {
        return None;
    }
    let ms = u32_at(b, value + 8)?;
    let ls = u32_at(b, value + 12)?;
    let version = format!("{}.{}.{}.{}", ms >> 16, ms & 0xFFFF, ls >> 16, ls & 0xFFFF);

    // Children: StringFileInfo / VarFileInfo; VarFileInfo\Translation holds
    // (language, codepage) u16 pairs.
    let mut languages = Vec::<u16>::new();
    let mut child = align4(value + 52);
    while child < end {
        let Some((key, first, child_end)) = node(b, child, end) else {
            break;
        };
        if key == "VarFileInfo" {
            let mut var = first;
            while var < child_end {
                let Some((key, data, var_end)) = node(b, var, child_end) else {
                    break;
                };
                if key == "Translation" {
                    let value_len = u16_at(b, var + 2)? as usize;
                    let mut p = data;
                    while p + 4 <= (data + value_len).min(var_end) {
                        let lang = u16_at(b, p)?;
                        if !languages.contains(&lang) {
                            languages.push(lang);
                        }
                        p += 4;
                    }
                }
                var = align4(var_end.max(var + 1));
            }
        }
        child = align4(child_end.max(child + 1));
    }
    let language = languages
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    // `File.Language` is a 20-character column (five ids don't fit); NULL
    // there only drops msiexec's language tie-break, the version still counts.
    let language = (!language.is_empty() && language.len() <= 20).then_some(language);
    Some(FileVersion { version, language })
}

// The synthetic PE builder is shared with `tests/build.rs`, which can't see
// this private module, and there is no committed PE fixture to read instead.
#[cfg(test)]
#[path = "../tests/support/pe.rs"]
mod test_pe;

#[cfg(test)]
mod tests {
    use super::test_pe::{pe_with_versions, version_info, versioned_pe};
    use super::*;

    #[test]
    fn reads_file_version_and_languages() {
        // Why: this is the value msiexec compares against the installed key
        // file; the fields must come out in MS/LS order, not swapped.
        assert_eq!(
            read(&versioned_pe([1, 2, 3, 4], &[1033])),
            Some(FileVersion {
                version: "1.2.3.4".to_string(),
                language: Some("1033".to_string()),
            })
        );
        // Language-neutral (0) and multi-language resources, as MsiGetFileVersion.
        assert_eq!(
            read(&versioned_pe([10, 0, 26100, 1], &[0, 1033, 0]))
                .unwrap()
                .language
                .as_deref(),
            Some("0,1033")
        );
        assert_eq!(
            read(&versioned_pe([1, 0, 0, 0], &[])).unwrap().language,
            None
        );
    }

    #[test]
    fn non_pe_and_resourceless_files_are_unversioned() {
        // Why: these must stay NULL (unversioned), and odd input must never
        // panic or fail the build.
        assert_eq!(read(b""), None);
        assert_eq!(read(b"MZ fake windows executable payload"), None);
        assert_eq!(read(b"readme contents\n"), None);
        assert_eq!(read(&pe_with_versions(&[])), None);
        let full = versioned_pe([1, 2, 3, 4], &[1033]);
        for cut in [0x3E, 0x60, 0x200, full.len() - 10] {
            assert_eq!(read(&full[..cut]), None, "truncated at {cut:#x}");
        }
    }

    #[test]
    fn nodes_must_fit_their_parent() {
        // Why: a declared length is the only bound on a node; reading past it
        // would take the version or languages from unrelated bytes.
        let read_blob = |blob: Vec<u8>| read(&pe_with_versions(&[(1, blob)]));
        for root_len in [6u16, 60] {
            // 6: the key runs past the node; 60: the fixed info does.
            let mut blob = version_info([1, 2, 3, 4], &[1033]);
            blob[0..2].copy_from_slice(&root_len.to_le_bytes());
            assert_eq!(read_blob(blob), None, "root wLength {root_len}");
        }
        // VarFileInfo (the first child, after the 40-byte header and the
        // 52-byte fixed info) claims more than the root holds: it is ignored,
        // the root's version still counts.
        let mut blob = version_info([1, 2, 3, 4], &[1033]);
        let too_long = (blob.len() - 92 + 4) as u16;
        blob[92..94].copy_from_slice(&too_long.to_le_bytes());
        assert_eq!(
            read_blob(blob),
            Some(FileVersion {
                version: "1.2.3.4".to_string(),
                language: None,
            })
        );
    }

    #[test]
    fn version_resource_id_1_wins_over_a_named_entry() {
        // Why: named entries sort first, and the wrong resource could
        // advertise a lower version and recreate the upgrade loss.
        let pe = pe_with_versions(&[
            (0x8000_0000, version_info([9, 9, 9, 9], &[])),
            (1, version_info([1, 2, 3, 4], &[])),
        ]);
        assert_eq!(read(&pe).unwrap().version, "1.2.3.4");
    }
}
