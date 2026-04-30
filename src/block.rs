// v1 block-on-paper layout. See docs/FORMAT-V1.md §2 (Block) and §3
// (SuperBlock) for the citation-by-citation derivation. This module
// is the wire layer only — no CRC computation, no Reed-Solomon, no
// page-level layout. Those land in their own modules at M2..M4.
//
// Both Block and SuperBlock occupy 128 bytes on the wire, share the
// same wire size, and are discriminated by the four-byte `addr`
// field: addr == SUPERBLOCK_ADDR marks a superblock, otherwise the
// high nibble is the recovery-group tag (0 = data, 1..=15 = recovery)
// and the low 28 bits are the byte offset into the encoded buffer.

// --- Constants ---------------------------------------------------------------
// Citations are paperbak.h:line-number in reference/paperbak-1.10.src/.

/// Block side length in dots. paperbak.h:50 (`NDOT`).
pub const NDOT: usize = 32;

/// Useful data bytes per block. paperbak.h:51 (`NDATA`).
pub const NDATA: usize = 90;

/// Reed-Solomon parity bytes per block. CCSDS (255,223,33) shortened
/// to (128,96); the 32-byte parity tail survives the shortening.
/// See FORMAT-V1.md §2.3.
pub const ECC_BYTES: usize = 32;

/// Wire size of a serialized block, bytes. Two derivations:
/// - dot grid: NDOT * NDOT / 8 bits per byte = 128
/// - field layout: 4 (addr) + NDATA + 2 (crc) + ECC_BYTES = 128
///
/// Both must agree. The compile-time asserts below enforce this.
pub const BLOCK_BYTES: usize = 128;
const _: () = assert!(BLOCK_BYTES == NDOT * NDOT / 8);
const _: () = assert!(BLOCK_BYTES == 4 + NDATA + 2 + ECC_BYTES);

/// Sentinel `addr` value that marks a SuperBlock. paperbak.h:53.
pub const SUPERBLOCK_ADDR: u32 = 0xFFFF_FFFF;

/// Maximum input file size in bytes (~256 MB minus 128). paperbak.h:52.
/// The high nibble of `addr` is reserved for the recovery-group tag,
/// so byte offsets are limited to 28 bits.
pub const MAXSIZE: u32 = 0x0FFF_FF80;

/// Default redundancy / group size. paperbak.h:55 (`NGROUP`).
pub const NGROUP_DEFAULT: u8 = 5;
/// Minimum redundancy (paperbak.h:56, `NGROUPMIN`).
pub const NGROUP_MIN: u8 = 2;
/// Maximum redundancy (paperbak.h:57, `NGROUPMAX`).
pub const NGROUP_MAX: u8 = 10;

/// SuperBlock mode bit: data is bzip2-compressed. paperbak.h:70.
pub const PBM_COMPRESSED: u8 = 0x01;
/// SuperBlock mode bit: data is AES-192-CBC encrypted. paperbak.h:71.
pub const PBM_ENCRYPTED: u8 = 0x02;

/// Number of bytes covered by the per-block CRC: addr (4) + the
/// 90-byte payload region. The CRC and ECC fields are excluded.
/// Same coverage applies to SuperBlock — see FORMAT-V1.md §2.2 / §3.
pub const CRC_COVERAGE_BYTES: usize = 4 + NDATA;
const _: () = assert!(CRC_COVERAGE_BYTES == 94);

/// XOR mask applied to the raw CRC-16 before storing it on the wire.
/// `Printer.cpp:174` (encode) and `Decoder.cpp:235` (verify) — present
/// to break the trivial all-zero-block / CRC-of-zero false-positive.
pub const CRC_FINAL_XOR: u16 = 0x55AA;

// --- Block -------------------------------------------------------------------

/// A 128-byte block as it appears on the wire. Carries either a
/// data payload (NDATA bytes), a recovery checksum, or — when
/// `addr == SUPERBLOCK_ADDR` — the bytes of a [`SuperBlock`].
///
/// Use [`Block::is_super`], [`Block::is_recovery`], [`Block::is_data`]
/// to discriminate. The CRC and ECC fields are stored verbatim — they
/// are computed/verified by callers in higher layers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Block {
    /// Overloaded address. For data blocks: byte offset into the
    /// encoded buffer (must be a multiple of NDATA). For recovery
    /// blocks: `offset ^ (ngroup << 28)`. For superblocks: SUPERBLOCK_ADDR.
    pub addr: u32,
    pub data: [u8; NDATA],
    /// CRC-16/CCITT of (addr || data) XOR'd with 0x55AA. See
    /// FORMAT-V1.md §2.2. Stored as written by the encoder; this
    /// module does not verify or compute it.
    pub crc: u16,
    /// Reed-Solomon parity. Computed/verified by the ecc module (M2).
    pub ecc: [u8; ECC_BYTES],
}

impl Block {
    /// Serialize to the 128-byte wire form. All multi-byte integers
    /// are little-endian per the format spec (the original C source
    /// aliases pointers freely on x86, locking the wire to LE).
    #[must_use]
    pub fn to_bytes(&self) -> [u8; BLOCK_BYTES] {
        let mut out = [0u8; BLOCK_BYTES];
        out[0..4].copy_from_slice(&self.addr.to_le_bytes());
        out[4..4 + NDATA].copy_from_slice(&self.data);
        out[4 + NDATA..4 + NDATA + 2].copy_from_slice(&self.crc.to_le_bytes());
        out[4 + NDATA + 2..].copy_from_slice(&self.ecc);
        out
    }

    /// Deserialize from the 128-byte wire form. Always succeeds —
    /// every 128-byte sequence is a syntactically valid Block; only
    /// CRC/ECC checks at higher layers can reject it.
    #[must_use]
    pub fn from_bytes(buf: &[u8; BLOCK_BYTES]) -> Self {
        let addr = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let mut data = [0u8; NDATA];
        data.copy_from_slice(&buf[4..4 + NDATA]);
        let crc = u16::from_le_bytes(buf[4 + NDATA..4 + NDATA + 2].try_into().unwrap());
        let mut ecc = [0u8; ECC_BYTES];
        ecc.copy_from_slice(&buf[4 + NDATA + 2..]);
        Self {
            addr,
            data,
            crc,
            ecc,
        }
    }

    /// Group index encoded in the high nibble of `addr` (bits 28..32).
    /// 0 for a data block; 1..=15 for a recovery (XOR-checksum) block.
    /// Meaningless when [`Self::is_super`] is true.
    #[must_use]
    pub fn ngroup(&self) -> u8 {
        ((self.addr >> 28) & 0x0F) as u8
    }

    /// Byte offset into the encoded buffer (low 28 bits of `addr`).
    /// Always a multiple of NDATA for valid blocks. Meaningless when
    /// [`Self::is_super`] is true.
    #[must_use]
    pub fn offset(&self) -> u32 {
        self.addr & 0x0FFF_FFFF
    }

    /// True when this block's bytes encode a [`SuperBlock`].
    #[must_use]
    pub fn is_super(&self) -> bool {
        self.addr == SUPERBLOCK_ADDR
    }

    /// True when this block is a recovery (XOR-checksum) block.
    #[must_use]
    pub fn is_recovery(&self) -> bool {
        !self.is_super() && self.ngroup() != 0
    }

    /// True when this block is an ordinary data block (`ngroup == 0`).
    #[must_use]
    pub fn is_data(&self) -> bool {
        !self.is_super() && self.ngroup() == 0
    }

    /// Compute the CRC the way the encoder does it: CRC-16/CCITT
    /// over (addr || data) — the first [`CRC_COVERAGE_BYTES`] bytes
    /// of the wire form — then XOR with [`CRC_FINAL_XOR`]. See
    /// FORMAT-V1.md §2.2; mirrors `Printer.cpp:174`.
    ///
    /// Does not read or mutate `self.crc`; caller compares the
    /// returned value or assigns it via `block.crc = block.compute_crc()`.
    #[must_use]
    pub fn compute_crc(&self) -> u16 {
        crate::crc::crc16(&self.to_bytes()[..CRC_COVERAGE_BYTES]) ^ CRC_FINAL_XOR
    }

    /// Returns true when [`Self::crc`] matches the expected value
    /// computed from the rest of the block. Mirrors `Decoder.cpp:235-236`.
    #[must_use]
    pub fn verify_crc(&self) -> bool {
        self.compute_crc() == self.crc
    }
}

// --- SuperBlock --------------------------------------------------------------

/// The page-and-file identification block. Same 128-byte wire size as
/// [`Block`]; `addr` is implicitly SUPERBLOCK_ADDR and not stored as
/// a struct field. Layout per FORMAT-V1.md §3.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SuperBlock {
    /// Size of (compressed-and-padded, optionally-encrypted) data, bytes.
    pub datasize: u32,
    /// Bytes of (compressed) data carried by this page.
    pub pagesize: u32,
    /// Original uncompressed file size, bytes.
    pub origsize: u32,
    /// Bitmask of [`PBM_COMPRESSED`] | [`PBM_ENCRYPTED`].
    pub mode: u8,
    /// Win32 file-attribute subset (READONLY|HIDDEN|SYSTEM|ARCHIVE|NORMAL).
    pub attributes: u8,
    /// 1-based page index.
    pub page: u16,
    /// Win32 FILETIME — 100ns ticks since 1601-01-01 UTC, as a u64.
    /// (FILETIME on the wire is `dwLowDateTime` then `dwHighDateTime`,
    /// each LE u32; concatenated they are the LE u64 value of `ticks`.)
    pub modified: u64,
    /// CRC-16/CCITT of the compressed-but-unencrypted data. Used by
    /// the decoder to verify the password (FORMAT-V1.md §6.3.3).
    pub filecrc: u16,
    /// Filename + crypto material per FORMAT-V1.md §3.2:
    /// - bytes 0..32: filename (NUL-terminated, capped at 31 chars)
    /// - bytes 32..48: AES PBKDF2 salt (only when `mode & PBM_ENCRYPTED`)
    /// - bytes 48..64: AES-CBC IV (only when `mode & PBM_ENCRYPTED`)
    pub name: [u8; 64],
    /// Same CRC scheme as [`Block::crc`] — covers the 94 bytes preceding it.
    pub crc: u16,
    /// Reed-Solomon parity over the same 96-byte payload as [`Block::ecc`].
    pub ecc: [u8; ECC_BYTES],
}

impl SuperBlock {
    /// Serialize to the 128-byte wire form. Writes [`SUPERBLOCK_ADDR`]
    /// in the first four bytes — this struct does not store `addr`
    /// as a field because its value is fixed by the format.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; BLOCK_BYTES] {
        let mut out = [0u8; BLOCK_BYTES];
        out[0..4].copy_from_slice(&SUPERBLOCK_ADDR.to_le_bytes());
        out[4..8].copy_from_slice(&self.datasize.to_le_bytes());
        out[8..12].copy_from_slice(&self.pagesize.to_le_bytes());
        out[12..16].copy_from_slice(&self.origsize.to_le_bytes());
        out[16] = self.mode;
        out[17] = self.attributes;
        out[18..20].copy_from_slice(&self.page.to_le_bytes());
        out[20..28].copy_from_slice(&self.modified.to_le_bytes());
        out[28..30].copy_from_slice(&self.filecrc.to_le_bytes());
        out[30..94].copy_from_slice(&self.name);
        out[94..96].copy_from_slice(&self.crc.to_le_bytes());
        out[96..128].copy_from_slice(&self.ecc);
        out
    }

    /// Deserialize from the 128-byte wire form. Returns
    /// [`FormatError::NotASuperBlock`] if the leading `addr` field
    /// is not [`SUPERBLOCK_ADDR`].
    pub fn from_bytes(buf: &[u8; BLOCK_BYTES]) -> Result<Self, FormatError> {
        let addr = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if addr != SUPERBLOCK_ADDR {
            return Err(FormatError::NotASuperBlock { addr });
        }
        let mut name = [0u8; 64];
        name.copy_from_slice(&buf[30..94]);
        let mut ecc = [0u8; ECC_BYTES];
        ecc.copy_from_slice(&buf[96..128]);
        Ok(Self {
            datasize: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            pagesize: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            origsize: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            mode: buf[16],
            attributes: buf[17],
            page: u16::from_le_bytes(buf[18..20].try_into().unwrap()),
            modified: u64::from_le_bytes(buf[20..28].try_into().unwrap()),
            filecrc: u16::from_le_bytes(buf[28..30].try_into().unwrap()),
            name,
            crc: u16::from_le_bytes(buf[94..96].try_into().unwrap()),
            ecc,
        })
    }

    /// Same convention as [`Block::compute_crc`]: CRC-16/CCITT over
    /// the first [`CRC_COVERAGE_BYTES`] of the wire form, XOR'd with
    /// [`CRC_FINAL_XOR`]. The encoder treats Block and SuperBlock as
    /// the same 128 bytes for CRC purposes; this just keeps that
    /// symmetry on the Rust side.
    #[must_use]
    pub fn compute_crc(&self) -> u16 {
        crate::crc::crc16(&self.to_bytes()[..CRC_COVERAGE_BYTES]) ^ CRC_FINAL_XOR
    }

    /// Returns true when [`Self::crc`] matches the expected value.
    #[must_use]
    pub fn verify_crc(&self) -> bool {
        self.compute_crc() == self.crc
    }
}

// --- Errors ------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum FormatError {
    /// `SuperBlock::from_bytes` was called on a buffer whose leading
    /// `addr` field is not [`SUPERBLOCK_ADDR`].
    NotASuperBlock { addr: u32 },
}

impl core::fmt::Display for FormatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotASuperBlock { addr } => write!(
                f,
                "block has addr {addr:#010x}, expected superblock addr {SUPERBLOCK_ADDR:#010x}"
            ),
        }
    }
}

impl std::error::Error for FormatError {}

// --- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty block helper for terse construction in tests.
    fn zero_block() -> Block {
        Block {
            addr: 0,
            data: [0; NDATA],
            crc: 0,
            ecc: [0; ECC_BYTES],
        }
    }

    #[test]
    fn block_size_is_128() {
        assert_eq!(BLOCK_BYTES, 128);
        assert_eq!(BLOCK_BYTES, NDOT * NDOT / 8);
        assert_eq!(BLOCK_BYTES, 4 + NDATA + 2 + ECC_BYTES);
    }

    #[test]
    fn block_round_trip_preserves_all_fields() {
        let b = Block {
            addr: 0x1234_5678,
            data: core::array::from_fn(|i| i as u8),
            crc: 0xAABB,
            ecc: core::array::from_fn(|i| (i + 100) as u8),
        };
        let bytes = b.to_bytes();
        assert_eq!(bytes.len(), BLOCK_BYTES);
        assert_eq!(Block::from_bytes(&bytes), b);
    }

    #[test]
    fn block_addr_is_little_endian_on_wire() {
        // 0x12345678 LE = 0x78 0x56 0x34 0x12.
        let b = Block {
            addr: 0x1234_5678,
            ..zero_block()
        };
        let bytes = b.to_bytes();
        assert_eq!(&bytes[0..4], &[0x78, 0x56, 0x34, 0x12]);
    }

    #[test]
    fn block_crc_is_little_endian_on_wire() {
        let b = Block {
            crc: 0xAABB,
            ..zero_block()
        };
        let bytes = b.to_bytes();
        // crc lives at offset 4 + NDATA = 94.
        assert_eq!(&bytes[94..96], &[0xBB, 0xAA]);
    }

    #[test]
    fn block_data_block_kind() {
        let data = Block {
            addr: 90 * 7,
            ..zero_block()
        };
        assert!(data.is_data());
        assert!(!data.is_recovery());
        assert!(!data.is_super());
        assert_eq!(data.ngroup(), 0);
        assert_eq!(data.offset(), 90 * 7);
    }

    #[test]
    fn block_recovery_block_kind() {
        // ngroup=5, offset=900 — Printer.cpp:881 form.
        let recovery = Block {
            addr: 900 ^ (5u32 << 28),
            ..zero_block()
        };
        assert!(recovery.is_recovery());
        assert!(!recovery.is_data());
        assert!(!recovery.is_super());
        assert_eq!(recovery.ngroup(), 5);
        assert_eq!(recovery.offset(), 900);
    }

    #[test]
    fn block_super_block_kind() {
        let sup = Block {
            addr: SUPERBLOCK_ADDR,
            ..zero_block()
        };
        assert!(sup.is_super());
        assert!(!sup.is_data());
        assert!(!sup.is_recovery());
    }

    #[test]
    fn super_block_round_trip_preserves_all_fields() {
        let s = SuperBlock {
            datasize: 12_345,
            pagesize: 67_890,
            origsize: 11_111,
            mode: PBM_COMPRESSED | PBM_ENCRYPTED,
            attributes: 0x80,
            page: 3,
            modified: 0x01D5_C0FF_EE12_3456,
            filecrc: 0xBEEF,
            name: core::array::from_fn(|i| (i + 1) as u8),
            crc: 0xDEAD,
            ecc: core::array::from_fn(|i| (i + 200) as u8),
        };
        let bytes = s.to_bytes();
        assert_eq!(bytes.len(), BLOCK_BYTES);
        assert_eq!(SuperBlock::from_bytes(&bytes).unwrap(), s);
    }

    #[test]
    fn super_block_writes_sentinel_addr() {
        let s = SuperBlock {
            datasize: 0,
            pagesize: 0,
            origsize: 0,
            mode: 0,
            attributes: 0,
            page: 0,
            modified: 0,
            filecrc: 0,
            name: [0; 64],
            crc: 0,
            ecc: [0; ECC_BYTES],
        };
        let bytes = s.to_bytes();
        assert_eq!(&bytes[0..4], &[0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn super_block_rejects_non_super_addr() {
        let mut bytes = [0u8; BLOCK_BYTES];
        bytes[0..4].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        let err = SuperBlock::from_bytes(&bytes).unwrap_err();
        assert_eq!(err, FormatError::NotASuperBlock { addr: 0x1234_5678 });
    }

    /// CRC of an all-zero (addr, data) is the bare CRC-CCITT init
    /// value (0x0000), XOR'd with the format's 0x55AA mask.
    /// This pins the constant 0x55AA so a typo (e.g. swapping with
    /// 0xAA55) trips loudly without needing a real 1.10 vector.
    #[test]
    fn block_crc_of_all_zero_payload_is_xor_mask() {
        let b = zero_block();
        assert_eq!(b.compute_crc(), CRC_FINAL_XOR);
    }

    #[test]
    fn block_compute_then_verify_round_trip() {
        let mut b = Block {
            addr: 0x1234_5678,
            data: core::array::from_fn(|i| (i * 3) as u8),
            crc: 0,
            ecc: [0; ECC_BYTES],
        };
        b.crc = b.compute_crc();
        assert!(b.verify_crc());
    }

    #[test]
    fn block_verify_crc_rejects_data_mutation() {
        let mut b = Block {
            addr: 1234,
            data: core::array::from_fn(|i| i as u8),
            crc: 0,
            ecc: [0; ECC_BYTES],
        };
        b.crc = b.compute_crc();
        assert!(b.verify_crc());
        // Flip a single bit anywhere in the covered range; CRC must reject.
        b.data[42] ^= 1;
        assert!(!b.verify_crc());
    }

    #[test]
    fn block_verify_crc_rejects_addr_mutation() {
        let mut b = Block {
            addr: 1234,
            data: [0xAA; NDATA],
            crc: 0,
            ecc: [0; ECC_BYTES],
        };
        b.crc = b.compute_crc();
        assert!(b.verify_crc());
        // ECC sits past the CRC coverage and isn't covered, but addr is.
        b.addr ^= 0x1000;
        assert!(!b.verify_crc());
    }

    #[test]
    fn block_verify_crc_ignores_ecc_mutation() {
        // ECC is excluded from CRC coverage by construction (it's the
        // outer redundancy layer over the entire block including the
        // CRC). Mutating ECC must not affect verify_crc's verdict.
        let mut b = Block {
            addr: 99 * NDATA as u32,
            data: [0x33; NDATA],
            crc: 0,
            ecc: [0; ECC_BYTES],
        };
        b.crc = b.compute_crc();
        assert!(b.verify_crc());
        b.ecc[0] = 0xFF;
        b.ecc[31] = 0xFF;
        assert!(b.verify_crc(), "ecc field must not affect block CRC");
    }

    #[test]
    fn super_block_compute_then_verify_round_trip() {
        let mut s = SuperBlock {
            datasize: 12_345,
            pagesize: 67_890,
            origsize: 11_111,
            mode: PBM_COMPRESSED,
            attributes: 0x80,
            page: 7,
            modified: 0x01D5_C0FF_EE12_3456,
            filecrc: 0xBEEF,
            name: core::array::from_fn(|i| (i + 1) as u8),
            crc: 0,
            ecc: [0; ECC_BYTES],
        };
        s.crc = s.compute_crc();
        assert!(s.verify_crc());
    }

    #[test]
    fn super_block_verify_crc_rejects_field_mutation() {
        let mut s = SuperBlock {
            datasize: 1,
            pagesize: 1,
            origsize: 1,
            mode: 0,
            attributes: 0,
            page: 1,
            modified: 0,
            filecrc: 0,
            name: [0; 64],
            crc: 0,
            ecc: [0; ECC_BYTES],
        };
        s.crc = s.compute_crc();
        assert!(s.verify_crc());
        s.page = 2;
        assert!(!s.verify_crc());
    }

    /// Field offsets must match FORMAT-V1.md §3 byte by byte. If any
    /// of these assertions trip, the doc and the impl have drifted.
    #[test]
    fn super_block_field_offsets_match_spec() {
        let s = SuperBlock {
            datasize: 0x1111_1111,
            pagesize: 0x2222_2222,
            origsize: 0x3333_3333,
            mode: 0x44,
            attributes: 0x55,
            page: 0x6666,
            modified: 0x7777_7777_8888_8888,
            filecrc: 0x9999,
            name: [0xAA; 64],
            crc: 0xBBBB,
            ecc: [0xCC; ECC_BYTES],
        };
        let b = s.to_bytes();
        // addr at 0..4 (= SUPERBLOCK_ADDR)
        assert_eq!(&b[0..4], &[0xFF, 0xFF, 0xFF, 0xFF]);
        // datasize at 4..8 (LE)
        assert_eq!(&b[4..8], &[0x11, 0x11, 0x11, 0x11]);
        // pagesize at 8..12
        assert_eq!(&b[8..12], &[0x22, 0x22, 0x22, 0x22]);
        // origsize at 12..16
        assert_eq!(&b[12..16], &[0x33, 0x33, 0x33, 0x33]);
        // mode at 16
        assert_eq!(b[16], 0x44);
        // attributes at 17
        assert_eq!(b[17], 0x55);
        // page at 18..20 (LE)
        assert_eq!(&b[18..20], &[0x66, 0x66]);
        // modified at 20..28 (LE u64)
        assert_eq!(
            &b[20..28],
            &[0x88, 0x88, 0x88, 0x88, 0x77, 0x77, 0x77, 0x77]
        );
        // filecrc at 28..30 (LE)
        assert_eq!(&b[28..30], &[0x99, 0x99]);
        // name at 30..94
        assert!(b[30..94].iter().all(|&x| x == 0xAA));
        // crc at 94..96 (LE)
        assert_eq!(&b[94..96], &[0xBB, 0xBB]);
        // ecc at 96..128
        assert!(b[96..128].iter().all(|&x| x == 0xCC));
    }
}
