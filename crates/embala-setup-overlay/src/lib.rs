//! Fixed-size PE-overlay trailer (spec R5).
//!
//! `setup.exe` is `[stub][payload.zip][install.lua][uninstall.lua][manifest]`
//! followed by this fixed-size trailer. The runtime seeks to `EOF −
//! TRAILER_LEN`, reads these bytes, validates [`MAGIC`], and then locates every
//! other section from the offset/length table.
//!
//! Byte layout (little-endian, [`TRAILER_LEN`] = 92 bytes):
//!
//! ```text
//! offset  size  field
//!   0       8   magic                = b"EMBALASU"
//!   8       4   format version (u32) = FORMAT_VERSION
//!  12    5×16   section table: (offset u64, len u64) for
//!               stub, payload.zip, install.lua, uninstall.lua, manifest
//! ```
//!
//! This is the single source of truth shared by both sides of the overlay: the
//! package-time writer (`embala-setup`, which [`Trailer::to_bytes`]) and the
//! install-time runtime (`embala-setup-runtime`, which [`Trailer::parse`]). It
//! is its own crate — with no dependency on the stubs — so the two never form a
//! cycle (the runtime cannot depend on `embala-setup`, which `include_bytes!`s
//! the runtime).

/// Trailer magic: "EMBALA SetUp".
pub const MAGIC: [u8; 8] = *b"EMBALASU";

/// Overlay format version. Bumped on any incompatible layout change.
pub const FORMAT_VERSION: u32 = 1;

/// Sections described by the table, in serialized order.
pub const SECTION_COUNT: usize = 5;

/// Total trailer size: magic (8) + version (4) + 5 × (offset u64 + len u64).
pub const TRAILER_LEN: usize = 8 + 4 + SECTION_COUNT * 16;

/// A byte range of one overlay section within the `setup.exe` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    /// Absolute offset from the start of the file.
    pub offset: u64,
    /// Section length in bytes.
    pub len: u64,
}

/// Parsed overlay trailer. `stub.len` is the overlay start (spec R5); the
/// uninstaller copies `[0..stub.len)` as its engine prefix (Phase 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trailer {
    pub version: u32,
    pub stub: Section,
    pub payload_zip: Section,
    pub install_lua: Section,
    pub uninstall_lua: Section,
    pub manifest: Section,
}

/// Why a trailer failed to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrailerError {
    /// The last [`TRAILER_LEN`] bytes did not carry [`MAGIC`] — no overlay.
    BadMagic,
    /// Fewer than [`TRAILER_LEN`] bytes were supplied.
    TooShort,
    /// The magic matched but the version is one this stub cannot read.
    UnsupportedVersion(u32),
}

impl Trailer {
    /// Parse the trailing [`TRAILER_LEN`] bytes of a `setup.exe`.
    ///
    /// `bytes` must be exactly the last [`TRAILER_LEN`] bytes of the file.
    pub fn parse(bytes: &[u8]) -> Result<Trailer, TrailerError> {
        if bytes.len() != TRAILER_LEN {
            return Err(TrailerError::TooShort);
        }
        if bytes[0..8] != MAGIC {
            return Err(TrailerError::BadMagic);
        }
        let version = read_u32(&bytes[8..12]);
        if version != FORMAT_VERSION {
            return Err(TrailerError::UnsupportedVersion(version));
        }
        let mut off = 12;
        let mut section = || {
            let s = Section {
                offset: read_u64(&bytes[off..off + 8]),
                len: read_u64(&bytes[off + 8..off + 16]),
            };
            off += 16;
            s
        };
        Ok(Trailer {
            version,
            stub: section(),
            payload_zip: section(),
            install_lua: section(),
            uninstall_lua: section(),
            manifest: section(),
        })
    }

    /// Serialize to the fixed-size on-disk form. The writer appends this.
    pub fn to_bytes(self) -> [u8; TRAILER_LEN] {
        let mut out = [0u8; TRAILER_LEN];
        out[0..8].copy_from_slice(&MAGIC);
        out[8..12].copy_from_slice(&self.version.to_le_bytes());
        let mut off = 12;
        for s in [
            self.stub,
            self.payload_zip,
            self.install_lua,
            self.uninstall_lua,
            self.manifest,
        ] {
            out[off..off + 8].copy_from_slice(&s.offset.to_le_bytes());
            out[off + 8..off + 16].copy_from_slice(&s.len.to_le_bytes());
            off += 16;
        }
        out
    }
}

fn read_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn read_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Trailer {
        Trailer {
            version: FORMAT_VERSION,
            stub: Section {
                offset: 0,
                len: 1024,
            },
            payload_zip: Section {
                offset: 1024,
                len: 42,
            },
            install_lua: Section {
                offset: 1066,
                len: 7,
            },
            uninstall_lua: Section {
                offset: 1073,
                len: 0,
            },
            manifest: Section {
                offset: 1073,
                len: 99,
            },
        }
    }

    #[test]
    fn trailer_len_is_fixed() {
        assert_eq!(TRAILER_LEN, 92);
        assert_eq!(sample().to_bytes().len(), TRAILER_LEN);
    }

    #[test]
    fn round_trips_every_section() {
        // Why: the runtime locates payload/scripts/manifest solely from this
        // table; any offset/len that does not survive serialize→parse would
        // make the self-extractor read the wrong bytes.
        let t = sample();
        let parsed = Trailer::parse(&t.to_bytes()).expect("valid trailer");
        assert_eq!(parsed, t);
    }

    #[test]
    fn absent_magic_reports_no_overlay() {
        // Why: a bare stub (uninstall.exe / an un-appended build) must be
        // detected as "no overlay", not misparsed into garbage sections.
        let mut bytes = sample().to_bytes();
        bytes[0] ^= 0xFF;
        assert_eq!(Trailer::parse(&bytes), Err(TrailerError::BadMagic));
    }

    #[test]
    fn wrong_length_is_rejected() {
        assert_eq!(Trailer::parse(&[0u8; 8]), Err(TrailerError::TooShort));
    }

    #[test]
    fn unknown_version_is_surfaced() {
        // Why: a future overlay format must fail loudly on an old stub rather
        // than silently misreading a changed layout.
        let mut bytes = sample().to_bytes();
        bytes[8..12].copy_from_slice(&99u32.to_le_bytes());
        assert_eq!(
            Trailer::parse(&bytes),
            Err(TrailerError::UnsupportedVersion(99))
        );
    }
}
