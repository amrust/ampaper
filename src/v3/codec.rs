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
    self, AnchorPayload, CELL_BYTES, Compression, DecodedCell, RAPTORQ_MTU, SYMBOL_BYTES,
    encode_anchor_cell, encode_data_cell,
};
use super::page::{PageBitmap, PageGeometry, ParseError, parse_page, render_page};

/// zstd compression level used by `encode_pages`. Level 22 is
/// `--ultra-22`, the densest setting; for paper-archive use the
/// encoder runs once per file so we don't care about encode speed.
const ZSTD_LEVEL: i32 = 22;

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
    /// Anchors disagree about the OTI / file size / total pages
    /// / compression flag. Most likely the user mixed pages from
    /// two different encode runs, or one anchor is corrupted in
    /// a way the CRC didn't catch (rare).
    AnchorMismatch,
    /// RaptorQ exhausted the surviving packets without converging.
    /// Too many cells were lost or damaged for the chosen repair
    /// budget.
    NoSolution,
    /// Zstd decompression failed on the post-RaptorQ recovered
    /// bytes. Most likely the source pages were corrupt in a way
    /// that survived per-cell CRC but mangled the compressed
    /// stream — extremely unlikely but worth distinguishing
    /// from a NoSolution failure.
    DecompressionFailed(String),
    /// Decompressed output's length doesn't match the
    /// `file_size` the anchor cell claimed. Indicates
    /// metadata-vs-data inconsistency, likely a corrupt anchor.
    SizeMismatch { expected: u64, actual: u64 },
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
            Self::DecompressionFailed(e) => write!(f, "v3 decode_pages: zstd decompression: {e}"),
            Self::SizeMismatch { expected, actual } => write!(
                f,
                "v3 decode_pages: decompressed size {actual} doesn't match anchor's claimed file_size {expected}"
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
/// `repair_overhead_percent` controls the RaptorQ repair budget as
/// a percentage of the source-symbol count. 25 means 25% extra
/// packets — total emitted = K · 1.25, so the receiver can lose
/// up to ~20% of cells and still decode.
///
/// The repair count is computed AFTER compression, so callers don't
/// need to know whether or how much zstd shrunk the input. (Earlier
/// API took an absolute packet count, which led to a bug where
/// 25%-of-raw-K turned into ~100%-of-compressed-K when zstd
/// quartered the input — emitting twice the necessary repair and
/// doubling the page count on text-like inputs.)
pub fn encode_pages(
    plaintext: &[u8],
    geometry: &PageGeometry,
    repair_overhead_percent: u32,
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

    // 1a. Try compressing with zstd. Use the result only when
    // it's actually smaller than the original — already-compressed
    // inputs (PDFs, JPEGs, ZIPs) typically come out the SAME size
    // or slightly bigger, in which case we ship raw bytes and set
    // the anchor's compression flag to None. Saves zstd's
    // ~14-byte frame overhead on incompressible inputs and skips
    // the decompress step at decode time.
    let compressed = zstd::encode_all(plaintext, ZSTD_LEVEL).ok();
    let (rq_input_owned, compression) = match compressed {
        Some(c) if c.len() < plaintext.len() => (c, Compression::Zstd),
        _ => (plaintext.to_vec(), Compression::None),
    };
    let rq_input: &[u8] = &rq_input_owned;

    // 1b. RaptorQ-encode the (compressed-or-not) bytes.
    let encoder = Encoder::with_defaults(rq_input, RAPTORQ_MTU);
    let oti_bytes = encoder.get_config().serialize();
    // Compute repair packet count from the ACTUAL post-compression
    // K. Floor of 5 ensures even tiny inputs get some loss
    // tolerance (a 1-cell payload with 0 repair would have to
    // round-trip every cell perfectly).
    let k = rq_input.len().div_ceil(SYMBOL_BYTES) as u32;
    let repair = ((k * repair_overhead_percent) / 100).max(5);
    let packets = encoder.get_encoded_packets(repair);

    // 2. How many pages do we need? cells/page - 1 data slots per
    // page (the first slot is the anchor).
    let data_slots_per_page = cells_per_page - 1;
    let total_packets = packets.len();
    let total_pages = total_packets.div_ceil(data_slots_per_page).max(1);

    // 3. Build pages.
    let mut bitmaps = Vec::with_capacity(total_pages);
    for page_idx in 0..total_pages {
        let mut cells: Vec<[u8; CELL_BYTES]> = Vec::with_capacity(cells_per_page);

        // Anchor cell first. `file_size` is the ORIGINAL input
        // length so the decoder can validate the post-decompress
        // output regardless of whether the bytes traveled
        // compressed or not.
        let anchor = AnchorPayload {
            oti: oti_bytes,
            file_size: plaintext.len() as u64,
            total_pages: total_pages as u32,
            page_index: page_idx as u32,
            compression,
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
    // agree on the OTI + file size + compression flag.
    let mut anchor: Option<AnchorPayload> = None;
    for cell_bytes in &all_cells {
        if let Ok(DecodedCell::Anchor(payload)) = cell::decode_cell(cell_bytes) {
            match anchor {
                None => anchor = Some(payload),
                Some(prev) => {
                    if prev.oti != payload.oti
                        || prev.file_size != payload.file_size
                        || prev.total_pages != payload.total_pages
                        || prev.compression != payload.compression
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
    let mut rq_recovered: Option<Vec<u8>> = None;
    for cell_bytes in &all_cells {
        let Ok(DecodedCell::Data { payload_id, symbol }) = cell::decode_cell(cell_bytes) else {
            continue;
        };
        packet_buf.clear();
        packet_buf.extend_from_slice(&payload_id);
        packet_buf.extend_from_slice(symbol);
        let packet = EncodingPacket::deserialize(&packet_buf);
        if let Some(out) = decoder.decode(packet) {
            rq_recovered = Some(out);
            break;
        }
    }
    let rq_recovered = rq_recovered.ok_or(PageDecodeError::NoSolution)?;

    // 4. Decompress if needed, then validate the recovered length
    // matches what the anchor claims.
    let plaintext = match anchor.compression {
        Compression::None => rq_recovered,
        Compression::Zstd => zstd::decode_all(rq_recovered.as_slice())
            .map_err(|e| PageDecodeError::DecompressionFailed(e.to_string()))?,
    };
    if plaintext.len() as u64 != anchor.file_size {
        return Err(PageDecodeError::SizeMismatch {
            expected: anchor.file_size,
            actual: plaintext.len() as u64,
        });
    }

    Ok(plaintext)
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

    /// Standard LCG (Numerical Recipes glibc parameters) producing
    /// high-entropy bytes — zstd can't compress this below ~100%
    /// of input size, so test page counts stay deterministic
    /// regardless of compression layer state.
    fn lcg_bytes(count: u32, seed: u32) -> Vec<u8> {
        let mut x = seed;
        (0..count)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (x >> 16) as u8
            })
            .collect()
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
        // 8 KB high-entropy payload — should span at least 3 pages
        // at 5×5 cells. Uses the standard LCG (period 2^32) so
        // zstd can't compress the input below the multi-page
        // threshold; tests with low-entropy inputs (e.g. cyclic
        // mod-256 patterns) collapse to one page once compression
        // is on.
        let plaintext = lcg_bytes(8192, 0xCAFE_BABE);
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
        // High-entropy LCG so compression doesn't shrink it to a
        // single page.
        let geom = medium_geometry();
        let plaintext = lcg_bytes(16_000, 0xF00D_F00D);
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
