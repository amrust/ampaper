// ampaper v2 SuperBlock layout. Spec: docs/FORMAT-V2.md §2.
//
// v2 splits the SuperBlock across two cells on the wire so we don't
// have to stuff AES salt+IV into name[64] (the v1 hack catalogued in
// PAPERBAK-HACKS.md §2.1). The two cells are ordinary 128-byte blocks
// — same RS(128,96) + CRC-16 + dot grid + page geometry as v1 — but
// carry v2-specific `addr` sentinels:
//
//   addr == 0xFFFFFFFE  →  v2 SuperBlock cell 1 (file metadata)
//   addr == 0xFFFFFFFD  →  v2 SuperBlock cell 2 (crypto envelope)
//
// A v1 decoder reading these cells sees `is_super()=false` (because
// `is_super` checks for `addr == 0xFFFFFFFF`) and `ngroup()=15` (out
// of the 2..=10 valid recovery range), so it silently drops them.
// v2 input therefore neither corrupts nor confuses a v1 reader; it
// just produces a "no SuperBlock found" error since v2 has no v1
// SuperBlock to find.

use crate::block::{Block, ECC_BYTES, NDATA};

// --- Constants ---------------------------------------------------------------

/// `addr` sentinel for v2 SuperBlock cell 1 (file metadata). Chosen
/// to fall outside both v1's `is_super` (0xFFFFFFFF) and the valid
/// recovery `ngroup` range (1..=10).
pub const V2_SUPERBLOCK_ADDR_CELL1: u32 = 0xFFFF_FFFE;

/// `addr` sentinel for v2 SuperBlock cell 2 (crypto envelope).
pub const V2_SUPERBLOCK_ADDR_CELL2: u32 = 0xFFFF_FFFD;

/// Current v2 format version. Bumps to 3 reserved for incompatible
/// breaks (see FORMAT-V2.md §7).
pub const V2_FORMAT_VERSION: u8 = 2;

/// Feature-flag bit: AES-256-GCM envelope is active. Cell 1 byte 1.
pub const PBM_V2_ENCRYPTED: u8 = 0b0000_0001;

/// Feature-flag bit: payload was bzip2-compressed BEFORE encryption
/// (or just compressed, when encryption is off).
pub const PBM_V2_COMPRESSED: u8 = 0b0000_0010;

/// Mask of feature-flag bits this implementation understands. Bits
/// outside this mask are reserved for M12 (color, adaptive RS, dot
/// shape, hex packing); decoders MUST reject files setting unknown
/// bits rather than mis-handle them. See FORMAT-V2.md §2.3.
pub const V2_FEATURE_FLAGS_KNOWN: u8 = PBM_V2_ENCRYPTED | PBM_V2_COMPRESSED;

/// PBKDF2 salt length, bytes. 32 bytes of OS entropy per encode.
pub const V2_KDF_SALT_LEN: usize = 32;

/// AES-GCM nonce length, bytes. Standard 96-bit GCM IV.
pub const V2_GCM_IV_LEN: usize = 12;

/// AES-GCM tag length, bytes. Full 128-bit tag, no truncation.
pub const V2_GCM_TAG_LEN: usize = 16;

/// Reserved bytes inside cell 2 (for future M12 features). Always
/// zero in current v2 files; non-zero is a decoder warning, not an
/// error (forward-compat reserves bits in feature_flags AND bytes
/// here, see FORMAT-V2.md §7).
pub const V2_CELL2_RESERVED_LEN: usize = NDATA - V2_KDF_SALT_LEN - V2_GCM_IV_LEN;

// Layout consistency: cell 2 must exactly fill NDATA.
const _: () = assert!(V2_CELL2_RESERVED_LEN == 46);
const _: () = assert!(V2_KDF_SALT_LEN + V2_GCM_IV_LEN + V2_CELL2_RESERVED_LEN == NDATA);

// --- Cell 1: file metadata ---------------------------------------------------

/// v2 SuperBlock cell 1 — the file metadata cell. Wire `addr` is
/// [`V2_SUPERBLOCK_ADDR_CELL1`]; the 90-byte data field carries the
/// fields below. Layout per FORMAT-V2.md §2.1.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct V2SuperBlockCell1 {
    /// Format version. = [`V2_FORMAT_VERSION`] for current spec.
    pub format_version: u8,
    /// Bitfield of [`PBM_V2_ENCRYPTED`] | [`PBM_V2_COMPRESSED`] |
    /// reserved bits. Reserved bits MUST be 0 on emit; decoders MUST
    /// reject when reserved bits are set.
    pub feature_flags: u8,
    /// 1-based page index.
    pub page: u16,
    /// Total pages in this encode.
    pub page_count: u16,
    /// Ciphertext + 16-byte GCM tag length. When unencrypted, equal
    /// to the (compressed) plaintext length.
    pub datasize: u32,
    /// Plaintext byte count, before compression and encryption.
    pub origsize: u32,
    /// Bytes of (compressed) data carried by THIS page (not the
    /// whole encode). The encoder uses this for capacity bookkeeping.
    pub pagesize: u32,
    /// Win32 FILETIME — 100ns ticks since 1601-01-01 UTC.
    pub modified: u64,
    /// UTF-8 filename, NUL-terminated. Bytes 32..64 of v1's name[64]
    /// are NOT reused for AES salt/IV in v2 — that hack lives only in
    /// v1 (PAPERBAK-HACKS.md §2.1). v2 has cell 2 for crypto material.
    pub name: [u8; 64],
}

impl V2SuperBlockCell1 {
    /// Build a fully-formed [`Block`] (with `addr`, CRC, ECC) for this
    /// cell. Caller passes the result through the normal cell-placement
    /// pipeline; on the wire it's indistinguishable from any other
    /// 128-byte block until the decoder inspects `addr`.
    #[must_use]
    pub fn to_block(&self) -> Block {
        let mut block = Block {
            addr: V2_SUPERBLOCK_ADDR_CELL1,
            data: self.to_data_bytes(),
            crc: 0,
            ecc: [0; ECC_BYTES],
        };
        block.crc = block.compute_crc();
        block.ecc = block.compute_ecc();
        block
    }

    /// Serialize the 90-byte data field per FORMAT-V2.md §2.1. All
    /// multi-byte fields are little-endian, matching v1.
    #[must_use]
    pub fn to_data_bytes(&self) -> [u8; NDATA] {
        let mut out = [0u8; NDATA];
        out[0] = self.format_version;
        out[1] = self.feature_flags;
        out[2..4].copy_from_slice(&self.page.to_le_bytes());
        out[4..6].copy_from_slice(&self.page_count.to_le_bytes());
        out[6..10].copy_from_slice(&self.datasize.to_le_bytes());
        out[10..14].copy_from_slice(&self.origsize.to_le_bytes());
        out[14..18].copy_from_slice(&self.pagesize.to_le_bytes());
        out[18..26].copy_from_slice(&self.modified.to_le_bytes());
        out[26..90].copy_from_slice(&self.name);
        out
    }

    /// Parse from a 90-byte data field. The caller is responsible for
    /// verifying the enclosing [`Block`]'s `addr` is
    /// [`V2_SUPERBLOCK_ADDR_CELL1`] before calling this.
    #[must_use]
    pub fn from_data_bytes(data: &[u8; NDATA]) -> Self {
        let mut name = [0u8; 64];
        name.copy_from_slice(&data[26..90]);
        Self {
            format_version: data[0],
            feature_flags: data[1],
            page: u16::from_le_bytes(data[2..4].try_into().unwrap()),
            page_count: u16::from_le_bytes(data[4..6].try_into().unwrap()),
            datasize: u32::from_le_bytes(data[6..10].try_into().unwrap()),
            origsize: u32::from_le_bytes(data[10..14].try_into().unwrap()),
            pagesize: u32::from_le_bytes(data[14..18].try_into().unwrap()),
            modified: u64::from_le_bytes(data[18..26].try_into().unwrap()),
            name,
        }
    }
}

// --- Cell 2: crypto envelope -------------------------------------------------

/// v2 SuperBlock cell 2 — the crypto envelope cell. Wire `addr` is
/// [`V2_SUPERBLOCK_ADDR_CELL2`]; carries the per-encode KDF salt and
/// GCM IV plus reserved space for future M12 features. Layout per
/// FORMAT-V2.md §2.2.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct V2SuperBlockCell2 {
    /// PBKDF2-HMAC-SHA-256 salt, 32 bytes of OS entropy per encode.
    /// All zeros when [`PBM_V2_ENCRYPTED`] is unset (the field is on
    /// the wire either way; only its content varies).
    pub kdf_salt: [u8; V2_KDF_SALT_LEN],
    /// AES-256-GCM nonce, 12 bytes of OS entropy per encode. All zeros
    /// when [`PBM_V2_ENCRYPTED`] is unset.
    pub gcm_iv: [u8; V2_GCM_IV_LEN],
    /// Reserved for future M12 features. MUST be zero on emit. A v2
    /// decoder receiving non-zero reserved bytes proceeds without
    /// error — they're a forward-compat bridge, not a hard rejection.
    pub reserved: [u8; V2_CELL2_RESERVED_LEN],
}

impl V2SuperBlockCell2 {
    /// Build a fully-formed [`Block`] for this cell with `addr` set
    /// to [`V2_SUPERBLOCK_ADDR_CELL2`] and CRC/ECC computed.
    #[must_use]
    pub fn to_block(&self) -> Block {
        let mut block = Block {
            addr: V2_SUPERBLOCK_ADDR_CELL2,
            data: self.to_data_bytes(),
            crc: 0,
            ecc: [0; ECC_BYTES],
        };
        block.crc = block.compute_crc();
        block.ecc = block.compute_ecc();
        block
    }

    #[must_use]
    pub fn to_data_bytes(&self) -> [u8; NDATA] {
        let mut out = [0u8; NDATA];
        out[0..V2_KDF_SALT_LEN].copy_from_slice(&self.kdf_salt);
        let iv_off = V2_KDF_SALT_LEN;
        out[iv_off..iv_off + V2_GCM_IV_LEN].copy_from_slice(&self.gcm_iv);
        let rsv_off = iv_off + V2_GCM_IV_LEN;
        out[rsv_off..rsv_off + V2_CELL2_RESERVED_LEN].copy_from_slice(&self.reserved);
        out
    }

    #[must_use]
    pub fn from_data_bytes(data: &[u8; NDATA]) -> Self {
        let mut kdf_salt = [0u8; V2_KDF_SALT_LEN];
        kdf_salt.copy_from_slice(&data[0..V2_KDF_SALT_LEN]);
        let iv_off = V2_KDF_SALT_LEN;
        let mut gcm_iv = [0u8; V2_GCM_IV_LEN];
        gcm_iv.copy_from_slice(&data[iv_off..iv_off + V2_GCM_IV_LEN]);
        let rsv_off = iv_off + V2_GCM_IV_LEN;
        let mut reserved = [0u8; V2_CELL2_RESERVED_LEN];
        reserved.copy_from_slice(&data[rsv_off..rsv_off + V2_CELL2_RESERVED_LEN]);
        Self {
            kdf_salt,
            gcm_iv,
            reserved,
        }
    }
}

// --- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cell1() -> V2SuperBlockCell1 {
        V2SuperBlockCell1 {
            format_version: V2_FORMAT_VERSION,
            feature_flags: PBM_V2_ENCRYPTED | PBM_V2_COMPRESSED,
            page: 7,
            page_count: 42,
            datasize: 0x1234_5678,
            origsize: 0x0BAD_F00D,
            pagesize: 0xAABB_CCDD,
            modified: 0x01D5_C0FF_EE12_3456,
            name: core::array::from_fn(|i| (i + 1) as u8),
        }
    }

    fn sample_cell2() -> V2SuperBlockCell2 {
        V2SuperBlockCell2 {
            kdf_salt: core::array::from_fn(|i| (i * 3) as u8),
            gcm_iv: [0x42; V2_GCM_IV_LEN],
            reserved: [0; V2_CELL2_RESERVED_LEN],
        }
    }

    /// Cell 1 round-trips through to_data_bytes / from_data_bytes
    /// preserving every field. Pins the byte layout against drift.
    #[test]
    fn cell1_data_round_trip_preserves_fields() {
        let c = sample_cell1();
        let bytes = c.to_data_bytes();
        assert_eq!(V2SuperBlockCell1::from_data_bytes(&bytes), c);
    }

    /// Cell 2 round-trips through to_data_bytes / from_data_bytes.
    #[test]
    fn cell2_data_round_trip_preserves_fields() {
        let c = sample_cell2();
        let bytes = c.to_data_bytes();
        assert_eq!(V2SuperBlockCell2::from_data_bytes(&bytes), c);
    }

    /// Cell 1 byte offsets must match FORMAT-V2.md §2.1 exactly. If
    /// any of these assertions trips, the doc and impl have drifted.
    #[test]
    fn cell1_field_offsets_match_spec() {
        let c = V2SuperBlockCell1 {
            format_version: 0x11,
            feature_flags: 0x22,
            page: 0x3344,
            page_count: 0x5566,
            datasize: 0x7788_99AA,
            origsize: 0xBBCC_DDEE,
            pagesize: 0x1122_3344,
            modified: 0x5566_7788_99AA_BBCC,
            name: [0xFF; 64],
        };
        let b = c.to_data_bytes();
        // format_version at 0
        assert_eq!(b[0], 0x11);
        // feature_flags at 1
        assert_eq!(b[1], 0x22);
        // page at 2..4 (LE)
        assert_eq!(&b[2..4], &[0x44, 0x33]);
        // page_count at 4..6 (LE)
        assert_eq!(&b[4..6], &[0x66, 0x55]);
        // datasize at 6..10 (LE)
        assert_eq!(&b[6..10], &[0xAA, 0x99, 0x88, 0x77]);
        // origsize at 10..14 (LE)
        assert_eq!(&b[10..14], &[0xEE, 0xDD, 0xCC, 0xBB]);
        // pagesize at 14..18 (LE)
        assert_eq!(&b[14..18], &[0x44, 0x33, 0x22, 0x11]);
        // modified at 18..26 (LE u64)
        assert_eq!(
            &b[18..26],
            &[0xCC, 0xBB, 0xAA, 0x99, 0x88, 0x77, 0x66, 0x55]
        );
        // name at 26..90 (full 64 bytes)
        assert!(b[26..90].iter().all(|&x| x == 0xFF));
    }

    /// Cell 2 byte offsets must match FORMAT-V2.md §2.2 exactly.
    #[test]
    fn cell2_field_offsets_match_spec() {
        let c = V2SuperBlockCell2 {
            kdf_salt: [0xAA; V2_KDF_SALT_LEN],
            gcm_iv: [0xBB; V2_GCM_IV_LEN],
            reserved: [0xCC; V2_CELL2_RESERVED_LEN],
        };
        let b = c.to_data_bytes();
        // kdf_salt at 0..32
        assert!(b[0..32].iter().all(|&x| x == 0xAA));
        // gcm_iv at 32..44
        assert!(b[32..44].iter().all(|&x| x == 0xBB));
        // reserved at 44..90
        assert!(b[44..90].iter().all(|&x| x == 0xCC));
    }

    /// to_block() emits a syntactically valid Block whose addr is
    /// the v2 cell-1 sentinel and whose CRC and ECC verify.
    #[test]
    fn cell1_to_block_emits_valid_block() {
        let c = sample_cell1();
        let block = c.to_block();
        assert_eq!(block.addr, V2_SUPERBLOCK_ADDR_CELL1);
        assert!(block.verify_crc());
        assert!(block.verify_ecc());
        // Recover the original cell from the block.
        let recovered = V2SuperBlockCell1::from_data_bytes(&block.data);
        assert_eq!(recovered, c);
    }

    #[test]
    fn cell2_to_block_emits_valid_block() {
        let c = sample_cell2();
        let block = c.to_block();
        assert_eq!(block.addr, V2_SUPERBLOCK_ADDR_CELL2);
        assert!(block.verify_crc());
        assert!(block.verify_ecc());
        let recovered = V2SuperBlockCell2::from_data_bytes(&block.data);
        assert_eq!(recovered, c);
    }

    /// v1 decoder discrimination: v2 cells must NOT look like v1
    /// SuperBlocks (addr 0xFFFFFFFF), and they must classify as
    /// recovery blocks with ngroup outside the valid 2..=10 range so
    /// v1 silently drops them.
    #[test]
    fn v2_cells_do_not_collide_with_v1_super_or_valid_recovery() {
        use crate::block::{NGROUP_MAX, NGROUP_MIN, SUPERBLOCK_ADDR};

        let cell1_block = sample_cell1().to_block();
        let cell2_block = sample_cell2().to_block();

        for block in [cell1_block, cell2_block] {
            assert!(!block.is_super(), "v2 cell looks like v1 SuperBlock");
            assert_ne!(block.addr, SUPERBLOCK_ADDR);
            // v1 sees these as recovery blocks (ngroup != 0), but
            // ngroup is 15 (out of range) so v1 silently drops them.
            assert!(block.is_recovery());
            let ng = block.ngroup();
            assert_eq!(ng, 15);
            assert!(
                !(NGROUP_MIN..=NGROUP_MAX).contains(&ng),
                "v2 ngroup={ng} is in v1's valid recovery range — v1 would mis-decode"
            );
        }
    }
}
