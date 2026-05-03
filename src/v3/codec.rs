// v3 page-level codec (Phase 2 first slice). Glues the cell layer
// + page-render layer + RaptorQ together:
//
//   encode_pages(bytes, geometry, repair) →
//      RaptorQ packets → data cells (one per packet) →
//      anchor cell per page (file metadata) →
//      grid → render bitmap → repeat per page
//
//   decode_pages(bitmaps, geometry) →
//      parse each bitmap → cells → drop bad-CRC cells →
//      first valid anchor gives OTI + file size →
//      feed remaining data cells' (payload_id, symbol) pairs back
//      into RaptorQ as EncodingPackets → recover bytes
//
// Phase 2 first slice limitations (lifted in subsequent slices):
//   - Decoder requires the caller to supply the same `PageGeometry`
//     used at encode time. No auto-detection. (Phase 2.5: corner
//     fiducials let the decoder figure out geometry from the
//     scanned bitmap.)
//   - No registration / rotation / noise tolerance. Pixel-perfect
//     bitmaps only — fine for synthetic round-trip, not yet for
//     real scanner output.
//   - No multi-page packet distribution strategy beyond
//     "consecutive packets fill consecutive pages." Future work
//     could interleave for better partial-recovery resilience.

use raptorq::{Decoder, Encoder, EncodingPacket, ObjectTransmissionInformation};

use super::cell::{
    self, AnchorPayload, CELL_BYTES, DecodedCell, RAPTORQ_MTU, SYMBOL_BYTES,
    encode_anchor_cell, encode_data_cell,
};
use super::page::{PageBitmap, PageGeometry, ParseError, parse_page, render_page};

#[derive(Debug)]
pub enum PageEncodeError {
    /// Empty input — RaptorQ rejects 0-byte source objects.
    EmptyInput,
    /// `geometry.cells_per_page()` is too small to fit the
    /// per-page anchor + at least one data cell. Caller should
    /// pick `nx` × `ny` ≥ 2.
    GeometryTooSmall { cells_per_page: u32 },
}

impl core::fmt::Display for PageEncodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyInput => f.write_str("v3 encode_pages: empty input"),
            Self::GeometryTooSmall { cells_per_page } => write!(
                f,
                "v3 encode_pages: geometry too small ({cells_per_page} cells/page; need ≥ 2)"
            ),
        }
    }
}

impl std::error::Error for PageEncodeError {}

#[derive(Debug)]
pub enum PageDecodeError {
    /// A page bitmap couldn't be parsed (wrong dimensions for the
    /// supplied geometry).
    PageParse(ParseError),
    /// No anchor cell with a valid CRC was found anywhere in the
    /// supplied pages — the decoder has no OTI and can't initialise
    /// RaptorQ. Indicates either total page loss or the wrong
    /// geometry was supplied.
    NoAnchorFound,
    /// Anchors disagree about the OTI / file size / total pages.
    /// Most likely the user mixed pages from two different encode
    /// runs, or one anchor is corrupted in a way the CRC didn't
    /// catch (rare).
    AnchorMismatch,
    /// RaptorQ exhausted the surviving packets without converging.
    /// Too many cells were lost or damaged for the chosen repair
    /// budget.
    NoSolution,
}

impl core::fmt::Display for PageDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PageParse(e) => write!(f, "v3 decode_pages: {e}"),
            Self::NoAnchorFound => f.write_str(
                "v3 decode_pages: no valid anchor cell on any page — \
                 check geometry, or the file was mis-scanned",
            ),
            Self::AnchorMismatch => f.write_str(
                "v3 decode_pages: anchor cells disagree — pages from different encode runs?",
            ),
            Self::NoSolution => f.write_str(
                "v3 decode_pages: RaptorQ did not converge — too few or too damaged cells",
            ),
        }
    }
}

impl std::error::Error for PageDecodeError {}

impl From<ParseError> for PageDecodeError {
    fn from(e: ParseError) -> Self {
        Self::PageParse(e)
    }
}

/// Encode `plaintext` into one or more page bitmaps.
///
/// Each page lays out cells in `geometry.nx × geometry.ny` row-major
/// order. Cell 0 of every page is the anchor (file metadata + OTI);
/// cells 1.. are RaptorQ data packets. Pages are filled in
/// sequence: page 0 carries anchor + the first `cells_per_page - 1`
/// packets, page 1 carries anchor + the next slice, etc. The last
/// page may have trailing blank cells (rendered as all-white) when
/// the packet stream runs out.
///
/// `repair_packets` is the number of RaptorQ repair packets above
/// the source K. RaptorQ's recovery probability is `1 - 1/256^(h+1)`
/// at receive overhead h, so even modest values give effectively
/// perfect recovery in the absence of pathological cell loss.
pub fn encode_pages(
    plaintext: &[u8],
    geometry: &PageGeometry,
    repair_packets: u32,
) -> Result<Vec<PageBitmap>, PageEncodeError> {
    if plaintext.is_empty() {
        return Err(PageEncodeError::EmptyInput);
    }
    let cells_per_page = geometry.cells_per_page() as usize;
    if cells_per_page < 2 {
        return Err(PageEncodeError::GeometryTooSmall {
            cells_per_page: cells_per_page as u32,
        });
    }

    // 1. RaptorQ-encode the plaintext at our cell-fixed MTU.
    let encoder = Encoder::with_defaults(plaintext, RAPTORQ_MTU);
    let oti_bytes = encoder.get_config().serialize();
    let packets = encoder.get_encoded_packets(repair_packets);

    // 2. How many pages do we need? cells/page - 1 data slots per
    // page (the first slot is the anchor).
    let data_slots_per_page = cells_per_page - 1;
    let total_packets = packets.len();
    let total_pages = total_packets.div_ceil(data_slots_per_page).max(1);

    // 3. Build pages.
    let mut bitmaps = Vec::with_capacity(total_pages);
    for page_idx in 0..total_pages {
        let mut cells: Vec<[u8; CELL_BYTES]> = Vec::with_capacity(cells_per_page);

        // Anchor cell first.
        let anchor = AnchorPayload {
            oti: oti_bytes,
            file_size: plaintext.len() as u64,
            total_pages: total_pages as u32,
            page_index: page_idx as u32,
        };
        cells.push(encode_anchor_cell(&anchor));

        // Data cells.
        let start = page_idx * data_slots_per_page;
        let end = (start + data_slots_per_page).min(total_packets);
        for packet in &packets[start..end] {
            let serialized = packet.serialize();
            // serialized = 4-byte payload ID + T-byte symbol.
            // We expect T = SYMBOL_BYTES = 120, set by RAPTORQ_MTU.
            debug_assert_eq!(
                serialized.len(),
                4 + SYMBOL_BYTES,
                "RaptorQ symbol size doesn't match cell SYMBOL_BYTES; \
                 RAPTORQ_MTU may need adjustment"
            );
            let payload_id: [u8; 4] = serialized[..4].try_into().unwrap();
            cells.push(encode_data_cell(payload_id, &serialized[4..]));
        }

        // Pad the trailing cells on the last page with all-zero
        // cells — they fail CRC at parse time and are skipped.
        // Rendering them as blank also keeps the bitmap aesthetics
        // consistent with PB-1.10's pad_to_full_page=false path.
        while cells.len() < cells_per_page {
            cells.push([0u8; CELL_BYTES]);
        }

        bitmaps.push(render_page(&cells, geometry));
    }

    Ok(bitmaps)
}

/// Decode a stream of page bitmaps back to plaintext.
///
/// Walks each page, extracts CRC-valid cells, takes the first
/// anchor it finds for the OTI, then feeds every data cell's
/// (payload ID, symbol) pair to RaptorQ as an EncodingPacket
/// until the decoder converges.
///
/// Anchor disagreement (different OTI / file size on different
/// pages) is treated as a hard error — likely the user combined
/// pages from different encode runs.
pub fn decode_pages(
    pages: &[PageBitmap],
    geometry: &PageGeometry,
) -> Result<Vec<u8>, PageDecodeError> {
    // 1. Collect all valid cells across all pages.
    let mut all_cells: Vec<[u8; CELL_BYTES]> = Vec::new();
    for page in pages {
        all_cells.extend(parse_page(page, geometry)?);
    }

    // 2. First valid anchor wins; all subsequent anchors must
    // agree on the OTI + file size.
    let mut anchor: Option<AnchorPayload> = None;
    for cell_bytes in &all_cells {
        if let Ok(DecodedCell::Anchor(payload)) = cell::decode_cell(cell_bytes) {
            match anchor {
                None => anchor = Some(payload),
                Some(prev) => {
                    if prev.oti != payload.oti
                        || prev.file_size != payload.file_size
                        || prev.total_pages != payload.total_pages
                    {
                        return Err(PageDecodeError::AnchorMismatch);
                    }
                }
            }
        }
    }
    let anchor = anchor.ok_or(PageDecodeError::NoAnchorFound)?;

    // 3. Hand every data cell's packet to RaptorQ.
    let oti = ObjectTransmissionInformation::deserialize(&anchor.oti);
    let mut decoder = Decoder::new(oti);
    let mut packet_buf = Vec::with_capacity(4 + SYMBOL_BYTES);
    for cell_bytes in &all_cells {
        let Ok(DecodedCell::Data { payload_id, symbol }) = cell::decode_cell(cell_bytes) else {
            continue;
        };
        packet_buf.clear();
        packet_buf.extend_from_slice(&payload_id);
        packet_buf.extend_from_slice(symbol);
        let packet = EncodingPacket::deserialize(&packet_buf);
        if let Some(plaintext) = decoder.decode(packet) {
            return Ok(plaintext);
        }
    }

    Err(PageDecodeError::NoSolution)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn medium_geometry() -> PageGeometry {
        // 5×5 cells = 25 cells/page = 24 data slots per page.
        // At RAPTORQ_MTU=124 → T=120 → ~120 bytes/cell, so one
        // page carries ~2.8 KB of source data.
        PageGeometry { nx: 5, ny: 5, pixels_per_dot: 1 }
    }

    #[test]
    fn empty_input_rejected() {
        let err = encode_pages(b"", &medium_geometry(), 5).unwrap_err();
        assert!(matches!(err, PageEncodeError::EmptyInput));
    }

    #[test]
    fn rejects_too_small_geometry() {
        let geom = PageGeometry { nx: 1, ny: 1, pixels_per_dot: 1 };
        let err = encode_pages(b"data", &geom, 0).unwrap_err();
        assert!(matches!(err, PageEncodeError::GeometryTooSmall { .. }));
    }

    #[test]
    fn round_trips_single_page_payload() {
        let geom = medium_geometry();
        // Small enough to fit on one page.
        let plaintext = b"the quick brown fox jumps over the lazy dog".to_vec();
        let pages = encode_pages(&plaintext, &geom, 5).unwrap();
        assert_eq!(pages.len(), 1, "small input should fit on one page");
        let recovered = decode_pages(&pages, &geom).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn round_trips_multi_page_payload() {
        let geom = medium_geometry();
        // 8 KB payload — should span at least 3 pages at 5×5 cells.
        let plaintext: Vec<u8> = (0u32..8192)
            .map(|i| (i.wrapping_mul(13).wrapping_add(7) & 0xFF) as u8)
            .collect();
        let pages = encode_pages(&plaintext, &geom, 10).unwrap();
        assert!(
            pages.len() >= 3,
            "8 KB at 24 data cells/page should need ≥ 3 pages, got {}",
            pages.len()
        );
        let recovered = decode_pages(&pages, &geom).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn anchor_present_on_every_page() {
        let geom = medium_geometry();
        let plaintext: Vec<u8> = (0u32..6000)
            .map(|i| (i.wrapping_mul(7) & 0xFF) as u8)
            .collect();
        let pages = encode_pages(&plaintext, &geom, 10).unwrap();
        for (page_idx, page) in pages.iter().enumerate() {
            let cells = parse_page(page, &geom).unwrap();
            // Cell 0 should always be a valid anchor.
            let decoded = cell::decode_cell(&cells[0]).expect("cell 0 must be valid");
            match decoded {
                DecodedCell::Anchor(payload) => {
                    assert_eq!(payload.page_index as usize, page_idx);
                    assert_eq!(payload.total_pages as usize, pages.len());
                    assert_eq!(payload.file_size, plaintext.len() as u64);
                }
                DecodedCell::Data { .. } => panic!(
                    "expected anchor at cell 0 of page {page_idx}, got Data"
                ),
            }
        }
    }

    #[test]
    fn missing_pages_break_decode_when_too_few_packets_survive() {
        // Drop all but the first page from a multi-page encode.
        // With only ~24 packets surviving for a payload that needs
        // far more, decode should fail with NoSolution (not panic).
        let geom = medium_geometry();
        let plaintext: Vec<u8> = (0u32..16_000)
            .map(|i| (i.wrapping_mul(31) & 0xFF) as u8)
            .collect();
        let pages = encode_pages(&plaintext, &geom, 5).unwrap();
        assert!(pages.len() > 2, "test setup: needs multi-page encode");
        let one_page = &pages[..1];
        let err = decode_pages(one_page, &geom).unwrap_err();
        assert!(
            matches!(err, PageDecodeError::NoSolution),
            "expected NoSolution, got {err:?}"
        );
    }
}
