//! Post-write fixups on the finished compound file.
//!
//! The `msi` crate emits table rows sorted by their key *text*, but Windows
//! Installer requires every table's rows sorted ascending by primary key in
//! *string-pool-id* order — and the pool assigns ids in first-reference
//! order, so the two disagree. Real `msiexec` then rejects the database at
//! open time with error 2219 "Invalid Installer database format", even
//! though the file round-trips through the `msi` crate's own reader. Fix it
//! up in place by re-sorting every persistent table by its primary-key
//! columns in string-id order. Also pins the CFB root-storage timestamps
//! (which the cfb crate stamps with wall-clock time) for reproducibility.
//
// Portions ported from deno desktop (cli/tools/desktop.rs),
// Copyright 2018-2026 the Deno authors, MIT license.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

use crate::Result;
use crate::summary::FIXED_TIMESTAMP_SECS;

fn malformed(what: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("malformed .msi: {what}"),
    )
}

/// Decode a Windows Installer stream name back to its logical table name.
///
/// MSI stores each table in a compound-file stream whose name is encoded with
/// a custom base-64 scheme: code points in `0x3800..0x4800` carry two base-64
/// digits and `0x4800..0x4840` carry one, over the alphabet
/// `0-9 A-Z a-z . _`. Table-data streams are additionally prefixed with the
/// sentinel `0x4840`, which falls outside both ranges; we return that prefix
/// as a leading NUL so callers can tell a real table stream apart from the
/// special `\u{5}SummaryInformation` stream.
fn demangle_stream_name(name: &str) -> String {
    const B64: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz._";
    let mut out = String::new();
    for c in name.chars() {
        let v = c as u32;
        if (0x3800..0x4800).contains(&v) {
            let n = v - 0x3800;
            out.push(B64[(n & 0x3f) as usize] as char);
            out.push(B64[((n >> 6) & 0x3f) as usize] as char);
        } else if (0x4800..0x4840).contains(&v) {
            let n = v - 0x4800;
            out.push(B64[(n & 0x3f) as usize] as char);
        } else if v == 0x4840 {
            out.push('\0');
        } else {
            out.push(c);
        }
    }
    out
}

/// Re-sort the rows of a column-major MSI table stream by the given key
/// columns.
///
/// `widths` is the byte width of every column (a stream is laid out column by
/// column: all of column 0's values, then all of column 1's, …). `keys` lists
/// the column indices to sort by, in priority order. Stored values are
/// compared as little-endian unsigned integers, which matches MSI's ordering
/// for both integer columns and string columns (whose stored value is the
/// string-pool id).
fn resort_stream(data: &[u8], widths: &[usize], keys: &[usize]) -> Vec<u8> {
    let row_width: usize = widths.iter().sum();
    if row_width == 0 {
        return data.to_vec();
    }
    let rows = data.len() / row_width;
    if rows <= 1 {
        return data.to_vec();
    }
    // Byte offset where each column's run of values begins.
    let mut col_off = vec![0usize; widths.len()];
    let mut acc = 0;
    for (c, &w) in widths.iter().enumerate() {
        col_off[c] = acc;
        acc += rows * w;
    }
    let read = |row: usize, col: usize| -> u64 {
        let base = col_off[col] + row * widths[col];
        let mut v = 0u64;
        for k in 0..widths[col] {
            v |= (data[base + k] as u64) << (8 * k);
        }
        v
    };
    let mut order: Vec<usize> = (0..rows).collect();
    order.sort_by(|&a, &b| {
        for &k in keys {
            match read(a, k).cmp(&read(b, k)) {
                std::cmp::Ordering::Equal => continue,
                ord => return ord,
            }
        }
        a.cmp(&b)
    });
    let mut out = vec![0u8; data.len()];
    for (c, &w) in widths.iter().enumerate() {
        for (new_row, &old_row) in order.iter().enumerate() {
            let src = col_off[c] + old_row * w;
            let dst = col_off[c] + new_row * w;
            out[dst..dst + w].copy_from_slice(&data[src..src + w]);
        }
    }
    out
}

/// Sort every persistent table by primary key in string-pool-id order and pin
/// the root-storage timestamps. Operates in place on the compound file; the
/// embedded cabinet stream is left untouched.
pub(crate) fn finalize(msi_path: &Path) -> Result<()> {
    let mut comp = cfb::open_rw(msi_path)?;
    let names: Vec<String> = comp
        .read_storage("/")?
        .map(|e| e.name().to_string())
        .collect();
    let read_stream = |comp: &mut cfb::CompoundFile<std::fs::File>, raw: &str| -> Result<Vec<u8>> {
        let mut s = comp.open_stream(format!("/{raw}"))?;
        let mut b = Vec::new();
        s.read_to_end(&mut b)?;
        Ok(b)
    };
    let write_stream =
        |comp: &mut cfb::CompoundFile<std::fs::File>, raw: &str, bytes: &[u8]| -> Result<()> {
            let mut s = comp.open_stream(format!("/{raw}"))?;
            s.write_all(bytes)?;
            Ok(())
        };
    let find_raw = |suffix: &str| -> Option<String> {
        names
            .iter()
            .find(|n| demangle_stream_name(n).ends_with(suffix))
            .cloned()
    };

    // The string-pool header's top bit selects 3-byte string references; tiny
    // databases use 2-byte refs, but honor the flag to stay correct at any
    // size.
    let pool_raw = find_raw("_StringPool").ok_or_else(|| malformed("no _StringPool"))?;
    let pool = read_stream(&mut comp, &pool_raw)?;
    let str_w: usize = if pool.len() >= 4 && (pool[3] & 0x80) != 0 {
        3
    } else {
        2
    };

    // Map every string-pool id to its text so data-table streams (named by
    // table text) can be matched to their column schema (keyed by table
    // string id).
    let data_raw = find_raw("_StringData").ok_or_else(|| malformed("no _StringData"))?;
    let str_data = read_stream(&mut comp, &data_raw)?;
    let mut strings = vec![String::new()]; // 1-based; index 0 is unused.
    {
        let mut off = 0usize;
        let mut i = 4;
        while i + 4 <= pool.len() {
            let len = u16::from_le_bytes([pool[i], pool[i + 1]]) as usize;
            let end = (off + len).min(str_data.len());
            strings.push(String::from_utf8_lossy(&str_data[off..end]).into_owned());
            off += len;
            i += 4;
        }
    }
    let id_of =
        |text: &str| -> Option<u64> { strings.iter().position(|s| s == text).map(|i| i as u64) };

    // System tables have a fixed schema not described in `_Columns`; their
    // string-typed columns use the same `str_w` reference width.
    let system_tables: &[(&str, Vec<usize>, Vec<usize>)] = &[
        ("_Tables", vec![str_w], vec![0]),
        ("_Columns", vec![str_w, 2, str_w, 2], vec![0, 1]),
        (
            "_Validation",
            vec![str_w, str_w, str_w, 4, 4, str_w, 2, str_w, str_w, str_w],
            vec![0, 1],
        ),
    ];
    for (name, widths, keys) in system_tables {
        if let Some(raw) = find_raw(name) {
            let bytes = read_stream(&mut comp, &raw)?;
            let sorted = resort_stream(&bytes, widths, keys);
            write_stream(&mut comp, &raw, &sorted)?;
        }
    }

    // Parse the (now sorted) `_Columns` table to recover every persistent
    // table's column widths and which columns are primary keys.
    let columns_raw = find_raw("_Columns").ok_or_else(|| malformed("no _Columns"))?;
    let columns = read_stream(&mut comp, &columns_raw)?;
    let col_row_w = str_w + 2 + str_w + 2; // Table, Number, Name, Type
    let ncol = columns.len() / col_row_w;
    let read_col = |arr_off: usize, row: usize, w: usize| -> u64 {
        let base = arr_off + row * w;
        let mut v = 0u64;
        for k in 0..w {
            v |= (columns[base + k] as u64) << (8 * k);
        }
        v
    };
    let off_table = 0usize;
    let off_number = ncol * str_w;
    let off_type = off_number + ncol * 2 + ncol * str_w;
    // A column Type word: 0x8000 marks the stored value present (always set
    // here); 0x0800 marks a string type; 0x2000 marks a primary-key column;
    // for integer columns the low byte is the storage size (2 or 4 bytes).
    let width_of = |ty: u64| -> usize {
        let t = (ty ^ 0x8000) & 0xffff;
        if (t & 0x0800) != 0 {
            str_w
        } else if (t & 0xff) == 4 {
            4
        } else {
            2
        }
    };
    let is_key = |ty: u64| -> bool { ((ty ^ 0x8000) & 0x2000) != 0 };
    let mut table_columns: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new();
    for r in 0..ncol {
        let table_id = read_col(off_table, r, str_w);
        let number = read_col(off_number, r, 2) ^ 0x8000;
        let ty = read_col(off_type, r, 2);
        table_columns
            .entry(table_id)
            .or_default()
            .push((number, ty));
    }
    for cols in table_columns.values_mut() {
        cols.sort_by_key(|&(number, _)| number);
    }

    // Re-sort each persistent data-table stream by its primary-key columns.
    for raw in &names {
        let demangled = demangle_stream_name(raw);
        // Table-data streams carry the 0x4840 sentinel (decoded as a leading
        // NUL); skip the summary-information stream and the already-handled
        // system tables.
        let Some(table_name) = demangled.strip_prefix('\0') else {
            continue;
        };
        if table_name.starts_with('_') {
            continue;
        }
        let Some(table_id) = id_of(table_name) else {
            continue;
        };
        let Some(cols) = table_columns.get(&table_id) else {
            continue;
        };
        let widths: Vec<usize> = cols.iter().map(|&(_, ty)| width_of(ty)).collect();
        let keys: Vec<usize> = cols
            .iter()
            .enumerate()
            .filter(|&(_, &(_, ty))| is_key(ty))
            .map(|(i, _)| i)
            .collect();
        if keys.is_empty() {
            continue;
        }
        let bytes = read_stream(&mut comp, raw)?;
        let sorted = resort_stream(&bytes, &widths, &keys);
        write_stream(&mut comp, raw, &sorted)?;
    }

    // Reproducibility: the cfb crate stamps the root storage with the current
    // wall-clock time at creation; pin it to the fixed build timestamp.
    let fixed = std::time::UNIX_EPOCH + std::time::Duration::from_secs(FIXED_TIMESTAMP_SECS);
    comp.set_created_time("/", fixed)?;
    comp.set_modified_time("/", fixed)?;

    comp.flush()?;
    Ok(())
}
