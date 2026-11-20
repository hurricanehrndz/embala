//! `embala info`: report the identity of a PE or MSI (VERSIONINFO or the
//! Property table) for recipes that need a file's version. Read-only, and fed
//! untrusted vendor installers, so the PE side walks only the headers and the
//! one resource path it needs with bounded reads instead of loading the image.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const CFB_MAGIC: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
const RT_VERSION: u32 = 16;
/// VERSIONINFO is a few KiB; anything bigger is not one we want to buffer.
const MAX_VERSION_BLOB: u32 = 1 << 20;
/// Real resource directories list a few dozen entries per level.
const MAX_DIR_ENTRIES: u32 = 4096;

pub fn run(path: &Path, json: bool) -> Result<()> {
    let info = read(path).with_context(|| format!("info: {}", path.display()))?;
    if json {
        println!("{info}");
        return Ok(());
    }
    // Human output: the same data as `key: value` lines, `type` first and
    // nested keys prefixed (`strings.ProductName`) so they can't collide.
    println!("type: {}", info["type"].as_str().unwrap_or_default());
    for (key, value) in info.as_object().into_iter().flatten() {
        match value {
            Value::Object(map) => {
                for (name, value) in map {
                    println!("{key}.{name}: {}", value.as_str().unwrap_or_default());
                }
            }
            _ if key == "type" => {}
            Value::String(s) => println!("{key}: {s}"),
            other => println!("{key}: {other}"),
        }
    }
    Ok(())
}

fn read(path: &Path) -> Result<Value> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 8];
    let len = read_up_to(&mut file, &mut magic)?;
    if len >= 2 && magic[..2] == *b"MZ" {
        read_pe(path, &mut file)
    } else if len == magic.len() && magic == CFB_MAGIC {
        read_msi(path)
    } else {
        bail!("not a PE or MSI file")
    }
}

/// `read` until `buf` is full or EOF, so short files are a clean mismatch.
fn read_up_to(file: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut len = 0;
    while len < buf.len() {
        match file.read(&mut buf[len..])? {
            0 => break,
            n => len += n,
        }
    }
    Ok(len)
}

fn read_msi(path: &Path) -> Result<Value> {
    // The msi crate unwraps on some malformed table data and can panic on a
    // crafted package; we don't guard against that here. A fix belongs in the
    // msi crate (or a catch_unwind here if it ever bites a recipe).
    let mut package = msi::open(path).context("not an MSI package")?;
    if !package.has_table("Property") {
        bail!("MSI package has no Property table");
    }
    let properties: BTreeMap<String, String> = package
        .select_rows(msi::Select::table("Property"))?
        .filter_map(|row| Some((row[0].as_str()?.to_string(), row[1].as_str()?.to_string())))
        .collect();
    Ok(json!({ "type": "msi", "properties": properties }))
}

fn read_pe(path: &Path, file: &mut File) -> Result<Value> {
    let mut info = json!({
        "type": "pe",
        "file_version": null,
        "product_version": null,
        "strings": {},
    });
    let Some(blob) = version_blob(file)? else {
        return Ok(info);
    };
    // editpe's parser is stricter than Windows: it drops strings whose wType
    // isn't 1 (text) and rejects a string whose value length counts bytes
    // instead of characters. Such files error here (or lose those strings)
    // even though Explorer shows them.
    let version = editpe::VersionInfo::parse(&blob).context("invalid VERSIONINFO resource")?;
    // FixedFileInfo is packed: copy the dwords out by value.
    let fixed = version.info;
    let (file_version, product_version) = (fixed.file_version, fixed.product_version);
    info["file_version"] = four_part(file_version.major, file_version.minor).into();
    info["product_version"] = four_part(product_version.major, product_version.minor).into();
    // Windows version queries (and sofcat) read the table named by the first
    // Translation entry: `<lang><codepage>` as 8 hex digits. Without a match,
    // fall back to the first table and say so.
    let translation = version
        .vars
        .first()
        .map(|&var| format!("{:04x}{:04x}", { var.major }, { var.minor }));
    let matching = translation.as_ref().and_then(|t| {
        version
            .strings
            .iter()
            .find(|table| table.key.eq_ignore_ascii_case(t))
    });
    let table = matching.or_else(|| {
        let first = version.strings.first()?;
        let why = match &translation {
            Some(t) => format!("does not match the first translation {t}"),
            None => "has no translation".to_string(),
        };
        eprintln!(
            "warning: {}: string table key {} {why}, so Windows version queries may not find these strings",
            path.display(),
            first.key
        );
        Some(first)
    });
    if let Some(table) = table {
        let strings: BTreeMap<&str, &str> = table
            .strings
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        info["strings"] = json!(strings);
    }
    Ok(info)
}

fn four_part(ms: u32, ls: u32) -> String {
    format!("{}.{}.{}.{}", ms >> 16, ms & 0xffff, ls >> 16, ls & 0xffff)
}

fn read_at(file: &mut File, offset: u64, buf: &mut [u8]) -> Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buf)
        .with_context(|| format!("truncated PE: {} bytes at {offset:#x}", buf.len()))
}

fn u16_at(b: &[u8], at: usize) -> Result<u16> {
    match b.get(at..at + 2) {
        Some(s) => Ok(u16::from_le_bytes([s[0], s[1]])),
        None => bail!("truncated PE header"),
    }
}

fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    match b.get(at..at + 4) {
        Some(s) => Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]])),
        None => bail!("truncated PE header"),
    }
}

/// Section headers reduced to what RVA → file offset needs.
struct Section {
    rva: u32,
    raw_size: u32,
    raw_offset: u32,
}

fn rva_to_offset(sections: &[Section], rva: u32) -> Result<u64> {
    for s in sections {
        match rva.checked_sub(s.rva) {
            Some(delta) if delta < s.raw_size => {
                return Ok(u64::from(s.raw_offset) + u64::from(delta));
            }
            _ => {}
        }
    }
    bail!("resource RVA {rva:#x} is outside every section")
}

/// Raw RT_VERSION blob: root → type 16 → first name → first language → data.
/// `None` when the PE has no resource directory or no RT_VERSION entry.
/// File offsets are u64 sums of u32 fields, so they cannot overflow.
fn version_blob(file: &mut File) -> Result<Option<Vec<u8>>> {
    let mut dos = [0u8; 64];
    read_at(file, 0, &mut dos)?;
    let pe = u64::from(u32_at(&dos, 0x3c)?);

    let mut coff = [0u8; 24];
    read_at(file, pe, &mut coff)?;
    if coff[..4] != *b"PE\0\0" {
        bail!("missing PE signature");
    }
    let section_count = u16_at(&coff, 6)?;
    let optional_size = u16_at(&coff, 20)?;

    let mut optional = vec![0u8; usize::from(optional_size)];
    read_at(file, pe + 24, &mut optional)?;
    let (count_at, dirs_at) = match u16_at(&optional, 0)? {
        0x10b => (92, 96),
        0x20b => (108, 112),
        magic => bail!("unknown optional header magic {magic:#x}"),
    };
    if u32_at(&optional, count_at)? <= 2 {
        return Ok(None);
    }
    let resource_rva = u32_at(&optional, dirs_at + 16)?;
    if resource_rva == 0 {
        return Ok(None);
    }

    let mut table = vec![0u8; usize::from(section_count) * 40];
    read_at(file, pe + 24 + u64::from(optional_size), &mut table)?;
    let sections = table
        .as_chunks::<40>()
        .0
        .iter()
        .map(|h| {
            Ok(Section {
                rva: u32_at(h, 12)?,
                raw_size: u32_at(h, 16)?,
                raw_offset: u32_at(h, 20)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let root = rva_to_offset(&sections, resource_rva)?;

    // Fixed depth 3, no recursion. Subdirectory offsets are relative to the
    // resource root; the high bit marks a subdirectory.
    let Some(by_type) = dir_entry(file, root, Some(RT_VERSION))? else {
        return Ok(None);
    };
    let by_name = subdir(root, by_type)?;
    let Some(by_lang) = dir_entry(file, by_name, None)? else {
        return Ok(None);
    };
    let by_lang = subdir(root, by_lang)?;
    let Some(data) = dir_entry(file, by_lang, None)? else {
        return Ok(None);
    };
    if data & 0x8000_0000 != 0 {
        bail!("RT_VERSION resource nests deeper than three levels");
    }

    let mut entry = [0u8; 16];
    read_at(file, root + u64::from(data), &mut entry)?;
    let (data_rva, size) = (u32_at(&entry, 0)?, u32_at(&entry, 4)?);
    if size > MAX_VERSION_BLOB {
        bail!("VERSIONINFO resource is {size} bytes, over the {MAX_VERSION_BLOB} limit");
    }
    let mut blob = vec![0u8; size as usize];
    read_at(file, rva_to_offset(&sections, data_rva)?, &mut blob)?;
    Ok(Some(blob))
}

fn subdir(root: u64, entry: u32) -> Result<u64> {
    if entry & 0x8000_0000 == 0 {
        bail!("resource entry should be a subdirectory");
    }
    Ok(root + u64::from(entry & 0x7fff_ffff))
}

/// Offset field of the entry with ID `id` (or the first entry when `None`)
/// in the resource directory at `dir`.
fn dir_entry(file: &mut File, dir: u64, id: Option<u32>) -> Result<Option<u32>> {
    let mut header = [0u8; 16];
    read_at(file, dir, &mut header)?;
    let named = u32::from(u16_at(&header, 12)?);
    let ids = u32::from(u16_at(&header, 14)?);
    let count = named + ids;
    if count > MAX_DIR_ENTRIES {
        bail!("resource directory has {count} entries, over the {MAX_DIR_ENTRIES} limit");
    }
    let mut entries = vec![0u8; count as usize * 8];
    read_at(file, dir + 16, &mut entries)?;
    let mut entries = entries.as_chunks::<8>().0.iter();
    let found = match id {
        None => entries.next(),
        // Named entries come first; IDs follow with the high bit clear.
        Some(id) => entries
            .skip(named as usize)
            .find(|e| e[..4] == id.to_le_bytes()),
    };
    found.map(|e| u32_at(e, 4)).transpose()
}
