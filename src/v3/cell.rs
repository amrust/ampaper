// v3 cell layout (Phase 2). Each cell is a fixed 128-byte unit
// that round-trips through a 32×32 dot pattern: cell bytes are the
// dot pattern, packed MSB-first row-major (bit r*32 + c is at
// byte (r*32+c)/8, bit position 7 - (r*32+c)%8).
//
// Layout:
//   [0..2]   CRC-16/CCITT over bytes 2..128 (the per-cell integrity
//             check the scan layer uses to reject damaged cells)
//   [2]      Cell type — 0x00 = data, 0x01 = anchor
//   [3]      Reserved (must be zero in version 1)
//   [4..8]   Discriminator —
//              data cells:   RaptorQ payload ID (4 bytes per RFC 6330 §3.2)
//              anchor cells: magic b"ANCR"
//   [8..128] Payload —
//              data cells:   RaptorQ symbol (T = 120 bytes)
//              anchor cells: AnchorPayload struct (see below)
//
// Why 32×32 cells (matching PB 1.10's cell size for now): scan-side
// reuse. The cell-finding heuristics in `crate::scan` are tuned for
// 32-dot cells; reusing the size means Phase 2's scan path can be
// built on top of them. Phase 4+ will explore tighter packing once
// the page-level finder pattern lands.
//
// Why CRC-16/CCITT (vs CRC-32 or none): the per-cell payload is only
// 126 bytes — CRC-16 detects all single-, double-, triple-bit, and
// most four-bit errors at that length; the extra detection from
// CRC-32 doesn't justify the 2-byte overhead. The CRC also matches
// what `crate::crc::crc16` already provides, so we don't add a dep.
//
// Domain separation: ampaper's `crc16` is CRC-16/XMODEM (poly 0x1021,
// init 0). Over all-zero bytes this returns 0 — so a blank-paper
// cell (all-zero bytes) would otherwise pass CRC and surface as a
// bogus data packet with payload-ID [0,0,0,0]. We avoid this the
// same way v1 does (PB 1.10 XORs its block CRC with 0x55AA): we
// XOR cell CRCs with a v3-specific constant. Using a *different*
// constant from v1 also ensures a v1 cell can never accidentally
// pass a v3 CRC check or vice versa, even on otherwise-identical
// byte content.

use crate::crc::crc16;

/// XOR mask applied to the CRC-16 before it's stored in the cell.
/// 0x7633 = ASCII `"v3"`. Non-zero so blank-paper (all-zero) cells
/// fail the CRC check; distinct from v1's 0x55AA so v1 cells can
/// never accidentally satisfy a v3 reader and vice versa.
const CELL_CRC_XOR: u16 = 0x7633;

#[inline]
fn cell_crc(cell_body: &[u8]) -> u16 {
    crc16(cell_body) ^ CELL_CRC_XOR
}

/// Total bytes per cell on the wire and in the dot pattern.
pub const CELL_BYTES: usize = 128;

/// 32×32 dot grid per cell — same as PB 1.10's NDOT_CELL.
pub const CELL_DOTS: u32 = 32;

/// Bytes 0..2: CRC-16 over bytes 2..128.
pub const CELL_CRC_LEN: usize = 2;

/// Byte 2: cell type byte. 0x00 = data, 0x01 = anchor.
pub const CELL_TYPE_DATA: u8 = 0x00;
pub const CELL_TYPE_ANCHOR: u8 = 0x01;

/// Bytes 4..8: discriminator. For anchor cells, must equal this.
pub const ANCHOR_MAGIC: &[u8; 4] = b"ANCR";

/// Bytes 8..128: 120-byte payload. For data cells, this is exactly
/// the RaptorQ symbol — making `T = SYMBOL_BYTES = 120`.
pub const SYMBOL_BYTES: usize = 120;

/// MTU to pass to RaptorQ's `Encoder::with_defaults`. The wire
/// packet size is 4 (payload ID) + T (symbol). Setting MTU = 124
/// makes RaptorQ pick T = 120 (assuming default Al=8 alignment),
/// which matches our cell payload area exactly.
pub const RAPTORQ_MTU: u16 = 124;

/// Offsets into a cell's 128 bytes.
const OFF_CRC: usize = 0;
const OFF_TYPE: usize = 2;
const OFF_RESERVED: usize = 3;
const OFF_DISCRIMINATOR: usize = 4;
const OFF_PAYLOAD: usize = 8;

/// Decoded view of a cell's contents. `Data` borrows the symbol
/// slice from the underlying cell bytes — saves a 120-byte copy
/// per cell on the decode hot path.
#[derive(Clone, Copy, Debug)]
pub enum DecodedCell<'a> {
    Data {
        payload_id: [u8; 4],
        symbol: &'a [u8],
    },
    Anchor(AnchorPayload),
}

/// Compression algorithm applied to the source bytes before
/// RaptorQ encoding. The decoder uses this to pick the right
/// decompressor on the post-RaptorQ output. New variants get
/// new byte values and refuse to decode on old readers — that's
/// fine; v3 is greenfield and the legacy v1/v2 paths are
/// untouched, so backward compatibility burdens stay zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// Raw bytes, no compression. Used when zstd's output isn't
    /// smaller than the original (already-compressed inputs like
    /// PDFs and JPEGs typically fall in this bucket).
    None = 0,
    /// Zstandard (RFC 8478). Phase 3 default.
    Zstd = 1,
}

impl Compression {
    fn from_byte(b: u8) -> Result<Self, CellError> {
        match b {
            0 => Ok(Self::None),
            1 => Ok(Self::Zstd),
            other => Err(CellError::UnknownCompression { byte: other }),
        }
    }
}

/// Anchor cell payload — file-level metadata that lets a decoder
/// verify it has all the pages and reconstruct the RaptorQ
/// configuration. Fits inside the 120-byte cell payload area.
///
/// Layout (relative to the start of the cell payload, byte 8 of
/// the cell):
///
/// | Offset | Bytes | Field                                       |
/// |--------|-------|---------------------------------------------|
/// | 0      | 12    | RaptorQ OTI                                 |
/// | 12     | 8     | File size (LE) — ORIGINAL uncompressed size |
/// | 20     | 4     | Total pages (LE)                            |
/// | 24     | 4     | Page index (LE)                             |
/// | 28     | 1     | Compression algorithm byte (see [`Compression`]) |
/// | 29     | 91    | Reserved (zero in version 1; future filename + mtime + attrs) |
///
/// `file_size` is the size of the ORIGINAL input; the post-
/// RaptorQ recovered byte stream may be smaller (when compressed)
/// or equal (when raw). The decoder uses `file_size` to validate
/// the post-decompression output length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnchorPayload {
    pub oti: [u8; 12],
    pub file_size: u64,
    pub total_pages: u32,
    pub page_index: u32,
    pub compression: Compression,
}

/// Cell decode failure. The scan path drops failed cells and asks
/// the rateless ECC to recover the file from the survivors, so
/// these errors are non-fatal at the file level — the decoder
/// just skips and keeps reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellError {
    BadCrc,
    UnknownType { type_byte: u8 },
    NonZeroReserved,
    BadAnchorMagic,
    AnchorReservedNonZero,
    /// Anchor cell's compression-algorithm byte names a value
    /// this decoder doesn't know how to handle. Most likely the
    /// file was produced by a future ampaper version that added
    /// a new compression algorithm (e.g. xz, brotli).
    UnknownCompression { byte: u8 },
}

impl core::fmt::Display for CellError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadCrc => f.write_str("cell CRC mismatch"),
            Self::UnknownType { type_byte } => {
                write!(f, "unknown cell type 0x{type_byte:02x}")
            }
            Self::NonZeroReserved => f.write_str("cell reserved byte must be zero"),
            Self::BadAnchorMagic => f.write_str("anchor cell magic mismatch (expected ANCR)"),
            Self::AnchorReservedNonZero => {
                f.write_str("anchor cell reserved bytes must be zero in version 1")
            }
            Self::UnknownCompression { byte } => write!(
                f,
                "unknown anchor compression byte 0x{byte:02x} — file was produced by a newer ampaper version?"
            ),
        }
    }
}

impl std::error::Error for CellError {}

/// Build a data cell holding one RaptorQ encoded packet.
#[must_use]
pub fn encode_data_cell(payload_id: [u8; 4], symbol: &[u8]) -> [u8; CELL_BYTES] {
    debug_assert_eq!(symbol.len(), SYMBOL_BYTES, "symbol size must equal SYMBOL_BYTES");

    let mut cell = [0u8; CELL_BYTES];
    cell[OFF_TYPE] = CELL_TYPE_DATA;
    cell[OFF_RESERVED] = 0;
    cell[OFF_DISCRIMINATOR..OFF_DISCRIMINATOR + 4].copy_from_slice(&payload_id);
    cell[OFF_PAYLOAD..OFF_PAYLOAD + SYMBOL_BYTES].copy_from_slice(symbol);
    let crc = cell_crc(&cell[OFF_TYPE..]);
    cell[OFF_CRC..OFF_CRC + 2].copy_from_slice(&crc.to_le_bytes());
    cell
}

/// Build an anchor cell holding file-level metadata.
#[must_use]
pub fn encode_anchor_cell(anchor: &AnchorPayload) -> [u8; CELL_BYTES] {
    let mut cell = [0u8; CELL_BYTES];
    cell[OFF_TYPE] = CELL_TYPE_ANCHOR;
    cell[OFF_RESERVED] = 0;
    cell[OFF_DISCRIMINATOR..OFF_DISCRIMINATOR + 4].copy_from_slice(ANCHOR_MAGIC);
    // Anchor payload starts at OFF_PAYLOAD = 8.
    cell[OFF_PAYLOAD..OFF_PAYLOAD + 12].copy_from_slice(&anchor.oti);
    cell[OFF_PAYLOAD + 12..OFF_PAYLOAD + 20]
        .copy_from_slice(&anchor.file_size.to_le_bytes());
    cell[OFF_PAYLOAD + 20..OFF_PAYLOAD + 24]
        .copy_from_slice(&anchor.total_pages.to_le_bytes());
    cell[OFF_PAYLOAD + 24..OFF_PAYLOAD + 28]
        .copy_from_slice(&anchor.page_index.to_le_bytes());
    cell[OFF_PAYLOAD + 28] = anchor.compression as u8;
    // Bytes OFF_PAYLOAD+29..128 are reserved, already zero.
    let crc = cell_crc(&cell[OFF_TYPE..]);
    cell[OFF_CRC..OFF_CRC + 2].copy_from_slice(&crc.to_le_bytes());
    cell
}

/// Validate and dispatch on cell type. Returns `Err` for any cell
/// the integrity check rejects — the scan layer treats these as
/// "lost cells" and lets the rateless ECC fill them in.
pub fn decode_cell(cell: &[u8; CELL_BYTES]) -> Result<DecodedCell<'_>, CellError> {
    let stored_crc = u16::from_le_bytes([cell[0], cell[1]]);
    let computed_crc = cell_crc(&cell[OFF_TYPE..]);
    if stored_crc != computed_crc {
        return Err(CellError::BadCrc);
    }
    if cell[OFF_RESERVED] != 0 {
        return Err(CellError::NonZeroReserved);
    }
    match cell[OFF_TYPE] {
        CELL_TYPE_DATA => {
            let mut payload_id = [0u8; 4];
            payload_id.copy_from_slice(&cell[OFF_DISCRIMINATOR..OFF_DISCRIMINATOR + 4]);
            Ok(DecodedCell::Data {
                payload_id,
                symbol: &cell[OFF_PAYLOAD..OFF_PAYLOAD + SYMBOL_BYTES],
            })
        }
        CELL_TYPE_ANCHOR => {
            if &cell[OFF_DISCRIMINATOR..OFF_DISCRIMINATOR + 4] != ANCHOR_MAGIC {
                return Err(CellError::BadAnchorMagic);
            }
            let mut oti = [0u8; 12];
            oti.copy_from_slice(&cell[OFF_PAYLOAD..OFF_PAYLOAD + 12]);
            let mut file_size_b = [0u8; 8];
            file_size_b.copy_from_slice(&cell[OFF_PAYLOAD + 12..OFF_PAYLOAD + 20]);
            let mut total_b = [0u8; 4];
            total_b.copy_from_slice(&cell[OFF_PAYLOAD + 20..OFF_PAYLOAD + 24]);
            let mut idx_b = [0u8; 4];
            idx_b.copy_from_slice(&cell[OFF_PAYLOAD + 24..OFF_PAYLOAD + 28]);
            let compression = Compression::from_byte(cell[OFF_PAYLOAD + 28])?;
            // Reject anchor cells whose reserved tail isn't zero —
            // future versions will use it, current version mustn't.
            if cell[OFF_PAYLOAD + 29..].iter().any(|&b| b != 0) {
                return Err(CellError::AnchorReservedNonZero);
            }
            Ok(DecodedCell::Anchor(AnchorPayload {
                oti,
                file_size: u64::from_le_bytes(file_size_b),
                total_pages: u32::from_le_bytes(total_b),
                page_index: u32::from_le_bytes(idx_b),
                compression,
            }))
        }
        other => Err(CellError::UnknownType { type_byte: other }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_cell_round_trips() {
        let payload_id = [0x12, 0x34, 0x56, 0x78];
        let symbol: Vec<u8> = (0..SYMBOL_BYTES).map(|i| i as u8).collect();
        let cell = encode_data_cell(payload_id, &symbol);
        let decoded = decode_cell(&cell).unwrap();
        match decoded {
            DecodedCell::Data { payload_id: pid, symbol: sym } => {
                assert_eq!(pid, payload_id);
                assert_eq!(sym, symbol.as_slice());
            }
            DecodedCell::Anchor(_) => panic!("expected Data, got Anchor"),
        }
    }

    #[test]
    fn anchor_cell_round_trips() {
        let anchor = AnchorPayload {
            oti: [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C],
            file_size: 0xDEAD_BEEF_1234,
            total_pages: 42,
            page_index: 7,
            compression: Compression::Zstd,
        };
        let cell = encode_anchor_cell(&anchor);
        let decoded = decode_cell(&cell).unwrap();
        match decoded {
            DecodedCell::Anchor(got) => assert_eq!(got, anchor),
            DecodedCell::Data { .. } => panic!("expected Anchor, got Data"),
        }
    }

    #[test]
    fn flipped_bit_is_caught_by_crc() {
        let payload_id = [0u8; 4];
        let symbol = [0x55u8; SYMBOL_BYTES];
        let mut cell = encode_data_cell(payload_id, &symbol);
        // Flip a single bit deep inside the payload.
        cell[60] ^= 0x01;
        let err = decode_cell(&cell).unwrap_err();
        assert!(matches!(err, CellError::BadCrc));
    }

    #[test]
    fn unknown_type_byte_rejected() {
        let mut cell = [0u8; CELL_BYTES];
        cell[OFF_TYPE] = 0xFF;
        let crc = cell_crc(&cell[OFF_TYPE..]);
        cell[..2].copy_from_slice(&crc.to_le_bytes());
        let err = decode_cell(&cell).unwrap_err();
        assert!(matches!(err, CellError::UnknownType { type_byte: 0xFF }));
    }

    #[test]
    fn anchor_reserved_byte_must_be_zero() {
        let anchor = AnchorPayload {
            oti: [0u8; 12],
            file_size: 0,
            total_pages: 1,
            page_index: 0,
            compression: Compression::None,
        };
        let mut cell = encode_anchor_cell(&anchor);
        // Set a reserved byte to 1 and re-CRC.
        cell[OFF_PAYLOAD + 60] = 1;
        let crc = cell_crc(&cell[OFF_TYPE..]);
        cell[..2].copy_from_slice(&crc.to_le_bytes());
        let err = decode_cell(&cell).unwrap_err();
        assert!(matches!(err, CellError::AnchorReservedNonZero));
    }

    #[test]
    fn all_zero_cell_fails_crc() {
        // An entirely blank cell (e.g., trailing padding cell on the
        // last page). The CRC over zeros is a known constant; an
        // all-zero cell stores 0 there, which won't match.
        let cell = [0u8; CELL_BYTES];
        let err = decode_cell(&cell).unwrap_err();
        assert!(matches!(err, CellError::BadCrc));
    }
}
