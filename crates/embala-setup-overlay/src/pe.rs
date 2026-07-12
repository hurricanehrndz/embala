//! Locate the overlay end relative to the PE, not the raw file EOF.
//!
//! Authenticode signing appends a certificate table *past* the last PE section,
//! so `EOF − TRAILER_LEN` no longer points at our trailer once a `setup.exe` is
//! signed. The trailer instead sits at the *unsigned* end: the byte where an
//! appended cert table starts. That byte is recorded in the PE certificate-table
//! data-directory entry (security directory, index 4). When present and nonzero
//! the file is signed and that offset is the overlay end; otherwise the file is
//! unsigned and the stream length is the overlay end.
//!
//! We NEVER scan backwards from EOF for the trailer magic: cert-table bytes are
//! excluded from the Authenticode hash, so a backward scan is a signature-bypass
//! injection vector (an attacker could append a second forged trailer without
//! invalidating the signature). Any malformed / truncated / non-PE input degrades
//! silently to the stream length — exactly the pre-signing behavior — and never
//! errors on structure; only a genuine reader I/O error propagates.

use std::io::{self, Read, Seek, SeekFrom};

/// File offset where the overlay ends: the PE certificate-table offset when the
/// file is signed, otherwise the stream length. Structural problems (non-PE,
/// truncated headers, no security directory) degrade to the stream length; only
/// a real reader I/O error propagates.
pub fn overlay_end<R: Read + Seek>(r: &mut R) -> io::Result<u64> {
    let len = r.seek(SeekFrom::End(0))?;
    Ok(security_dir_offset(r, len)?.unwrap_or(len))
}

/// The nonzero certificate-table file offset, or `None` for any unsigned or
/// unparseable input. Every read is bounds-checked against `len` first, so a
/// truncated header returns `None` rather than an `UnexpectedEof` — leaving real
/// I/O errors to propagate.
fn security_dir_offset<R: Read + Seek>(r: &mut R, len: u64) -> io::Result<Option<u64>> {
    let mut b2 = [0u8; 2];
    let mut b4 = [0u8; 4];

    // DOS header: "MZ" magic, then e_lfanew (PE header offset) at 0x3C.
    if !read_at(r, 0, &mut b2, len)? || &b2 != b"MZ" {
        return Ok(None);
    }
    if !read_at(r, 0x3C, &mut b4, len)? {
        return Ok(None);
    }
    let e_lfanew = u32::from_le_bytes(b4) as u64;

    // "PE\0\0" signature, then skip the 20-byte COFF header to the optional header.
    if !read_at(r, e_lfanew, &mut b4, len)? || &b4 != b"PE\0\0" {
        return Ok(None);
    }
    let opt = e_lfanew + 4 + 20;

    // Optional-header magic selects the offsets of NumberOfRvaAndSizes and the
    // data-directory array (PE32 vs PE32+).
    if !read_at(r, opt, &mut b2, len)? {
        return Ok(None);
    }
    let (num_rvas_off, cert_dir_off) = match u16::from_le_bytes(b2) {
        0x10b => (92u64, 128u64),  // PE32
        0x20b => (108u64, 144u64), // PE32+
        _ => return Ok(None),
    };

    // The certificate directory is entry index 4; absent when there are ≤4 entries.
    if !read_at(r, opt + num_rvas_off, &mut b4, len)? {
        return Ok(None);
    }
    if u32::from_le_bytes(b4) <= 4 {
        return Ok(None);
    }

    // Certificate data-directory entry: u32 file offset (+ u32 size, unused).
    if !read_at(r, opt + cert_dir_off, &mut b4, len)? {
        return Ok(None);
    }
    match u32::from_le_bytes(b4) as u64 {
        0 => Ok(None),
        // An offset past EOF is malformed — notably a prefix copy of a signed
        // exe (uninstall.exe) whose header still records the parent's cert
        // offset. Degrade to stream length like every other structural miss.
        off if off > len => Ok(None),
        off => Ok(Some(off)),
    }
}

/// Read `buf.len()` bytes at absolute `off`, or return `false` if that range
/// falls outside `[0, len)`. The bounds check turns a truncated/oversized header
/// field into a structural miss instead of an `UnexpectedEof` error.
fn read_at<R: Read + Seek>(r: &mut R, off: u64, buf: &mut [u8], len: u64) -> io::Result<bool> {
    match off.checked_add(buf.len() as u64) {
        Some(end) if end <= len => {
            r.seek(SeekFrom::Start(off))?;
            r.read_exact(buf)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a minimal PE header (no sections) with the given optional-header
    /// magic, NumberOfRvaAndSizes, and certificate-directory file offset.
    fn pe(magic: u16, num_rvas: u32, cert_off: u32) -> Vec<u8> {
        let e_lfanew = 0x40usize;
        let opt = e_lfanew + 4 + 20;
        let (num_rvas_off, cert_dir_off) = if magic == 0x10b {
            (92usize, 128usize)
        } else {
            (108usize, 144usize)
        };
        let mut v = vec![0u8; opt + cert_dir_off + 8];
        v[0..2].copy_from_slice(b"MZ");
        v[0x3C..0x40].copy_from_slice(&(e_lfanew as u32).to_le_bytes());
        v[e_lfanew..e_lfanew + 4].copy_from_slice(b"PE\0\0");
        v[opt..opt + 2].copy_from_slice(&magic.to_le_bytes());
        v[opt + num_rvas_off..opt + num_rvas_off + 4].copy_from_slice(&num_rvas.to_le_bytes());
        v[opt + cert_dir_off..opt + cert_dir_off + 4].copy_from_slice(&cert_off.to_le_bytes());
        v
    }

    fn end(bytes: Vec<u8>) -> u64 {
        overlay_end(&mut Cursor::new(bytes)).unwrap()
    }

    #[test]
    fn unsigned_pe_returns_stream_length() {
        // Why: an unsigned build (cert offset 0) must place the trailer at raw
        // EOF, exactly as before signing existed.
        let bytes = pe(0x20b, 16, 0);
        let len = bytes.len() as u64;
        assert_eq!(end(bytes), len);
    }

    #[test]
    fn missing_cert_directory_returns_length() {
        // Why: with ≤4 data directories there is no security entry at all; must
        // fall back to stream length, not read a phantom entry.
        let bytes = pe(0x20b, 4, 0);
        let len = bytes.len() as u64;
        assert_eq!(end(bytes), len);
    }

    #[test]
    fn signed_pe32plus_returns_cert_offset() {
        // Why: a signed file's trailer ends where the appended cert table starts
        // (the security-directory offset), not at raw EOF.
        let mut bytes = pe(0x20b, 16, 0x1000);
        bytes.resize(0x1000 + 200, 0); // pretend a cert table was appended
        assert_eq!(end(bytes), 0x1000);
    }

    #[test]
    fn signed_pe32_returns_cert_offset() {
        // Why: the PE32 (0x10b) optional-header offsets differ from PE32+; both
        // must resolve the security directory.
        let mut bytes = pe(0x10b, 16, 0x800);
        bytes.resize(0x800 + 200, 0); // pretend a cert table was appended
        assert_eq!(end(bytes), 0x800);
    }

    #[test]
    fn cert_offset_beyond_eof_returns_length() {
        // Why: uninstall.exe is a prefix copy of the signed setup.exe; its PE
        // header still records the parent's cert offset, which lies past the
        // copy's own EOF. That must degrade to stream length, not seek past EOF.
        let bytes = pe(0x20b, 16, 0x1000); // file is far shorter than 0x1000
        let len = bytes.len() as u64;
        assert!(len < 0x1000);
        assert_eq!(end(bytes), len);
    }

    #[test]
    fn garbage_returns_stream_length() {
        // Why: a non-PE input must never error; it degrades to stream length.
        let bytes = b"this is not a PE file, just some bytes".to_vec();
        let len = bytes.len() as u64;
        assert_eq!(end(bytes), len);
    }

    #[test]
    fn truncated_after_mz_returns_length() {
        // Why: a truncated header (bounds-check miss) is structural, not an I/O
        // error — return stream length.
        assert_eq!(end(b"MZ".to_vec()), 2);
    }
}
