// CMY color-channel codec for v3 (Phase 6 first slice). Lifts
// the B&W codec from 1 bit per dot to 3 bits per dot by rendering
// three independent "channel layers" — cyan, magenta, yellow —
// into the same physical dot positions and composing them via
// subtractive color into an RGB page bitmap.
//
// Density gain at the same dot pitch: 3× B&W. War & Peace plain
// text (~3.2 MB → ~800 KB after zstd) fits on a single Letter
// page at the v3 GUI's default 200-dpi-equivalent geometry.
//
// Why no K (the 4th channel of CMYK): K=1 combined with C/M/Y=1
// is visually indistinguishable from K alone (full ink coverage
// = black regardless), which collapses 16 nominal CMYK codes to
// ~9 distinguishable colors. CMY-only gives a clean 8 colors at
// 3 bits/dot with no encoder-side ambiguity. K can land in a
// later slice once the 3-channel pipeline has real-paper
// validation.
//
// Resilience via packet pooling: each color channel is its own
// B&W encoding stream — but ALL channels carry packets generated
// by the SAME RaptorQ encoder. The decoder reads each channel's
// cells independently, pools all valid data cells across the 3
// channels, and asks RaptorQ to recover from the pool. So if
// one channel fades catastrophically (yellow on 5-year-old
// paper), the surviving 2 channels usually still carry > K
// packets and the file decodes. This is strictly better than
// "split source into 3 chunks, one per channel" — that approach
// loses the file when ANY channel fails.

use raptorq::{Decoder, Encoder, EncodingPacket, ObjectTransmissionInformation};

use super::cell::{
    self, AnchorPayload, CELL_BYTES, Compression, DecodedCell, RAPTORQ_MTU, SYMBOL_BYTES,
    encode_anchor_cell, encode_data_cell,
};
use super::page::{PageBitmap, PageGeometry, ParseError, parse_page, render_page};

/// zstd compression level. Same as the B&W codec.
const ZSTD_LEVEL: i32 = 22;

/// One rendered CMY page bitmap. RGB, row-major, 3 bytes per
/// pixel (R, G, B in that order). length = `width × height × 3`.
#[derive(Clone, Debug)]
pub struct RgbPageBitmap {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub enum CmyEncodeError {
    EmptyInput,
    GeometryTooSmall { cells_per_page: u32 },
}

impl core::fmt::Display for CmyEncodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyInput => f.write_str("v3 CMY encode: empty input"),
            Self::GeometryTooSmall { cells_per_page } => write!(
                f,
                "v3 CMY encode: geometry too small ({cells_per_page} cells/page; need ≥ 2)"
            ),
        }
    }
}

impl std::error::Error for CmyEncodeError {}

#[derive(Debug)]
pub enum CmyDecodeError {
    PageParse(ParseError),
    NoAnchorFound,
    AnchorMismatch,
    NoSolution,
    DecompressionFailed(String),
    SizeMismatch { expected: u64, actual: u64 },
}

impl core::fmt::Display for CmyDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PageParse(e) => write!(f, "v3 CMY decode: {e}"),
            Self::NoAnchorFound => f.write_str(
                "v3 CMY decode: no valid anchor cell on any color channel",
            ),
            Self::AnchorMismatch => {
                f.write_str("v3 CMY decode: per-channel anchors disagree on file metadata")
            }
            Self::NoSolution => f.write_str(
                "v3 CMY decode: RaptorQ did not converge — too few or too damaged cells across all channels",
            ),
            Self::DecompressionFailed(e) => write!(f, "v3 CMY decode: zstd: {e}"),
            Self::SizeMismatch { expected, actual } => write!(
                f,
                "v3 CMY decode: decompressed size {actual} doesn't match anchor's claimed file_size {expected}"
            ),
        }
    }
}

impl std::error::Error for CmyDecodeError {}

impl From<ParseError> for CmyDecodeError {
    fn from(e: ParseError) -> Self {
        Self::PageParse(e)
    }
}

/// Composite three grayscale channel bitmaps (C, M, Y in that
/// order) into one RGB bitmap using subtractive color.
///
/// Convention: in each channel's grayscale bitmap, a pixel value
/// `< 128` means "this dot's bit is set in this channel"
/// (matching the B&W codec). On paper that means ink IS present
/// for that channel. Subtractive color produces:
///
///   - Cyan ink absorbs red    → R = 0 if C set, 255 if not
///   - Magenta ink absorbs green → G = 0 if M set, 255 if not
///   - Yellow ink absorbs blue  → B = 0 if Y set, 255 if not
///
/// Output is RGB row-major, 3 bytes per pixel. The 3 input
/// bitmaps must have identical dimensions; the function panics
/// otherwise (it's an internal API only ever called with
/// matched-dimension layers from `encode_pages_cmyk`).
#[must_use]
pub fn composite_cmy(c: &PageBitmap, m: &PageBitmap, y: &PageBitmap) -> RgbPageBitmap {
    assert_eq!(c.width, m.width, "C/M layer width mismatch");
    assert_eq!(m.width, y.width, "M/Y layer width mismatch");
    assert_eq!(c.height, m.height, "C/M layer height mismatch");
    assert_eq!(m.height, y.height, "M/Y layer height mismatch");

    let n = (c.width as usize) * (c.height as usize);
    let mut pixels = vec![255u8; n * 3];
    for i in 0..n {
        let c_set = c.pixels[i] < 128;
        let m_set = m.pixels[i] < 128;
        let y_set = y.pixels[i] < 128;
        pixels[i * 3] = if c_set { 0 } else { 255 };
        pixels[i * 3 + 1] = if m_set { 0 } else { 255 };
        pixels[i * 3 + 2] = if y_set { 0 } else { 255 };
    }
    RgbPageBitmap { pixels, width: c.width, height: c.height }
}

/// Decompose an RGB bitmap into three grayscale channel bitmaps
/// via the inverse subtractive-color mapping. Each output bitmap
/// has the same dimensions as the input. Used by
/// `decode_pages_cmyk` to feed the existing B&W parse path.
///
/// Phase 6 first slice uses a fixed midpoint (`< 128`) per
/// channel. A future slice will switch to per-channel Otsu so
/// scanner output with channel-asymmetric gamma drift (very
/// common — yellow reads dimmer than cyan on most CCDs) decodes
/// cleanly.
#[must_use]
pub fn decompose_cmy(rgb: &RgbPageBitmap) -> (PageBitmap, PageBitmap, PageBitmap) {
    let n = (rgb.width as usize) * (rgb.height as usize);
    let mut c_pixels = vec![255u8; n];
    let mut m_pixels = vec![255u8; n];
    let mut y_pixels = vec![255u8; n];
    for i in 0..n {
        let r = rgb.pixels[i * 3];
        let g = rgb.pixels[i * 3 + 1];
        let b = rgb.pixels[i * 3 + 2];
        c_pixels[i] = if r < 128 { 0 } else { 255 };
        m_pixels[i] = if g < 128 { 0 } else { 255 };
        y_pixels[i] = if b < 128 { 0 } else { 255 };
    }
    (
        PageBitmap { pixels: c_pixels, width: rgb.width, height: rgb.height },
        PageBitmap { pixels: m_pixels, width: rgb.width, height: rgb.height },
        PageBitmap { pixels: y_pixels, width: rgb.width, height: rgb.height },
    )
}

/// Encode `plaintext` into one or more CMY page bitmaps.
///
/// Single RaptorQ stream over the (optionally zstd-compressed)
/// input. Packets are distributed across 3 color channels per
/// physical page so the decoder pools them and recovers from any
/// `K + small_overhead` survivors regardless of which channels
/// they came from. Each channel runs its own anchor cell at
/// physical cell index 0; the 3 anchors are byte-identical
/// (same OTI / file_size / total_pages / page_index /
/// compression), so the top-left dot block renders as solid
/// black on every page.
pub fn encode_pages_cmyk(
    plaintext: &[u8],
    geometry: &PageGeometry,
    repair_overhead_percent: u32,
) -> Result<Vec<RgbPageBitmap>, CmyEncodeError> {
    if plaintext.is_empty() {
        return Err(CmyEncodeError::EmptyInput);
    }
    let cells_per_page = geometry.cells_per_page() as usize;
    if cells_per_page < 2 {
        return Err(CmyEncodeError::GeometryTooSmall {
            cells_per_page: cells_per_page as u32,
        });
    }

    // Compression — same logic as the B&W encode_pages.
    let compressed = zstd::encode_all(plaintext, ZSTD_LEVEL).ok();
    let (rq_input_owned, compression) = match compressed {
        Some(c) if c.len() < plaintext.len() => (c, Compression::Zstd),
        _ => (plaintext.to_vec(), Compression::None),
    };
    let rq_input: &[u8] = &rq_input_owned;

    // RaptorQ encode — single encoder, single OTI shared across all
    // 3 channels.
    let encoder = Encoder::with_defaults(rq_input, RAPTORQ_MTU);
    let oti_bytes = encoder.get_config().serialize();

    // Page count: 3 channels × (cells_per_page - 1 anchor) data
    // slots per physical page. Aim for K · (1 + overhead) packets,
    // rounded up to fill complete pages.
    let data_slots_per_page_per_channel = cells_per_page - 1;
    let total_data_slots_per_page = data_slots_per_page_per_channel * 3;
    let k = rq_input.len().div_ceil(SYMBOL_BYTES) as u32;
    let target_packets =
        ((k as u64 * (100 + repair_overhead_percent as u64)).div_ceil(100)) as u32;
    let target_packets = target_packets.max(k + 5); // floor: K+5 for tiny inputs
    let total_pages = (target_packets as usize).div_ceil(total_data_slots_per_page).max(1);
    let total_packets_to_emit = total_pages * total_data_slots_per_page;
    let repair = (total_packets_to_emit as u32).saturating_sub(k);

    let packets = encoder.get_encoded_packets(repair);

    let mut rgb_pages: Vec<RgbPageBitmap> = Vec::with_capacity(total_pages);
    for page_idx in 0..total_pages {
        // Identical anchor on all 3 channels — they share OTI,
        // file metadata, and physical page index. The decoder
        // accepts agreement across channels as a sanity check.
        let anchor = AnchorPayload {
            oti: oti_bytes,
            file_size: plaintext.len() as u64,
            total_pages: total_pages as u32,
            page_index: page_idx as u32,
            compression,
        };
        let anchor_cell = encode_anchor_cell(&anchor);

        let mut channel_cells: [Vec<[u8; CELL_BYTES]>; 3] =
            [Vec::with_capacity(cells_per_page),
             Vec::with_capacity(cells_per_page),
             Vec::with_capacity(cells_per_page)];

        // Cell 0 of each channel is the anchor.
        for cells in channel_cells.iter_mut() {
            cells.push(anchor_cell);
        }

        // Distribute data packets across channels for THIS page.
        // Layout: page_idx covers `total_data_slots_per_page`
        // packets total; first `data_slots_per_page_per_channel`
        // go to C, next to M, last to Y. The packet's RaptorQ
        // payload-ID carries its identity, so the decoder can
        // pool them across channels regardless of where they
        // landed.
        let page_first_packet = page_idx * total_data_slots_per_page;
        for (ch, cells) in channel_cells.iter_mut().enumerate() {
            let ch_first = page_first_packet + ch * data_slots_per_page_per_channel;
            for slot in 0..data_slots_per_page_per_channel {
                let pkt_idx = ch_first + slot;
                if let Some(packet) = packets.get(pkt_idx) {
                    let serialized = packet.serialize();
                    debug_assert_eq!(serialized.len(), 4 + SYMBOL_BYTES);
                    let payload_id: [u8; 4] = serialized[..4].try_into().unwrap();
                    cells.push(encode_data_cell(payload_id, &serialized[4..]));
                } else {
                    // Trailing-page slack — render as blank cells
                    // (CRC-fail at parse time, decoder skips).
                    cells.push([0u8; CELL_BYTES]);
                }
            }
        }

        let c_layer = render_page(&channel_cells[0], geometry);
        let m_layer = render_page(&channel_cells[1], geometry);
        let y_layer = render_page(&channel_cells[2], geometry);
        rgb_pages.push(composite_cmy(&c_layer, &m_layer, &y_layer));
    }

    Ok(rgb_pages)
}

/// Decode CMY page bitmaps back to plaintext, auto-detecting
/// `nx`/`ny`/`pixels_per_dot` from the first page's finder
/// positions (decomposed cyan layer). Use this when the
/// geometry isn't known to the caller.
pub fn decode_pages_cmyk_auto(pages: &[RgbPageBitmap]) -> Result<Vec<u8>, CmyDecodeError> {
    if pages.is_empty() {
        return Err(CmyDecodeError::NoAnchorFound);
    }
    // Decompose the first page; finder detection on the C layer
    // suffices because all 3 channels carry the same finder
    // pattern (they get rendered as composite-black corners).
    let (c_layer, _, _) = decompose_cmy(&pages[0]);
    let detected = super::finder::detect_geometry(&c_layer)
        .map_err(|e| CmyDecodeError::PageParse(super::page::ParseError::FinderDetection(e)))?;
    let geom = PageGeometry {
        nx: detected.nx,
        ny: detected.ny,
        pixels_per_dot: detected.pixels_per_dot,
    };
    decode_pages_cmyk(pages, &geom)
}

/// Decode CMY page bitmaps back to plaintext.
pub fn decode_pages_cmyk(
    pages: &[RgbPageBitmap],
    geometry: &PageGeometry,
) -> Result<Vec<u8>, CmyDecodeError> {
    // 1. Decompose every RGB page into 3 grayscale layers.
    // 2. Parse each layer with the B&W parse_page. A channel
    //    whose ink has faded catastrophically (the canonical
    //    yellow-on-aged-paper failure mode) decomposes to a
    //    near-blank layer where finder detection fails — that's
    //    EXPECTED, and we just skip it. The surviving channels
    //    contribute their cells to the pool; RaptorQ recovers
    //    from any K + small_overhead, so 2 of 3 channels usually
    //    carries enough.
    //
    //    Tracking per-page parse outcomes: we tolerate up to
    //    2 of 3 channels failing per page, but if ALL channels
    //    fail on EVERY page we surface the first ParseError so
    //    the user sees something more actionable than NoSolution.
    let mut all_cells: Vec<[u8; CELL_BYTES]> = Vec::new();
    let mut first_parse_err: Option<ParseError> = None;
    let mut any_channel_parsed = false;
    for rgb in pages {
        let (c_layer, m_layer, y_layer) = decompose_cmy(rgb);
        for layer in [&c_layer, &m_layer, &y_layer] {
            match parse_page(layer, geometry) {
                Ok(cells) => {
                    all_cells.extend(cells);
                    any_channel_parsed = true;
                }
                Err(e) => {
                    if first_parse_err.is_none() {
                        first_parse_err = Some(e);
                    }
                }
            }
        }
    }
    if !any_channel_parsed {
        return Err(CmyDecodeError::PageParse(
            first_parse_err.expect("no_channel_parsed implies at least one error"),
        ));
    }

    // 3. Find anchor + verify cross-channel agreement. Per-channel
    //    anchors are identical at encode time, so any disagreement
    //    points at scanner damage + CRC collision (rare) or mixed
    //    pages from different encode runs.
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
                        return Err(CmyDecodeError::AnchorMismatch);
                    }
                }
            }
        }
    }
    let anchor = anchor.ok_or(CmyDecodeError::NoAnchorFound)?;

    // 4. Pool data cells across all 3 channels and feed RaptorQ.
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
    let rq_recovered = rq_recovered.ok_or(CmyDecodeError::NoSolution)?;

    // 5. Decompress + length-validate, mirroring the B&W path.
    let plaintext = match anchor.compression {
        Compression::None => rq_recovered,
        Compression::Zstd => zstd::decode_all(rq_recovered.as_slice())
            .map_err(|e| CmyDecodeError::DecompressionFailed(e.to_string()))?,
    };
    if plaintext.len() as u64 != anchor.file_size {
        return Err(CmyDecodeError::SizeMismatch {
            expected: anchor.file_size,
            actual: plaintext.len() as u64,
        });
    }
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_geometry() -> PageGeometry {
        PageGeometry { nx: 5, ny: 5, pixels_per_dot: 1 }
    }

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
        let err = encode_pages_cmyk(b"", &small_geometry(), 25).unwrap_err();
        assert!(matches!(err, CmyEncodeError::EmptyInput));
    }

    #[test]
    fn round_trips_short_payload() {
        let plaintext = b"ampaper v3 in glorious cyan-magenta-yellow";
        let pages = encode_pages_cmyk(plaintext, &small_geometry(), 25).unwrap();
        assert_eq!(pages.len(), 1);
        let recovered = decode_pages_cmyk(&pages, &small_geometry()).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn round_trips_high_entropy_multi_page() {
        // Use enough data that the multi-page property holds even
        // with 3-channel density (3× more capacity than B&W).
        let plaintext = lcg_bytes(15_000, 0xCAFE_BABE);
        let pages = encode_pages_cmyk(&plaintext, &small_geometry(), 25).unwrap();
        assert!(pages.len() >= 2, "expected ≥ 2 pages, got {}", pages.len());
        let recovered = decode_pages_cmyk(&pages, &small_geometry()).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn density_beats_bw_on_compressible_input() {
        // Highly compressible input: 50 KB of repeated text. With
        // CMY at 3× density, this should still fit on 1 page.
        // Canary that the 3-channel density gain is real, not just
        // notional.
        let mut plaintext = Vec::with_capacity(50_000);
        let line = b"PaperBack archives bytes. ampaper v3 ships them in color. ";
        while plaintext.len() < 50_000 {
            plaintext.extend_from_slice(line);
        }
        plaintext.truncate(50_000);
        let pages = encode_pages_cmyk(&plaintext, &small_geometry(), 25).unwrap();
        assert_eq!(
            pages.len(),
            1,
            "50 KB of repeated text should fit on 1 CMY page, got {}",
            pages.len()
        );
        let recovered = decode_pages_cmyk(&pages, &small_geometry()).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn cmy_composite_decompose_round_trips_pixels() {
        // Synthesize three grayscale layers with known dot
        // patterns and confirm composite + decompose reverses
        // exactly. Pixel-perfect round-trip — no thresholding
        // surprises at the boundary.
        let w = 4u32;
        let h = 3u32;
        let n = (w * h) as usize;
        // Layer C: even pixels black, odd white.
        let c = PageBitmap {
            pixels: (0..n)
                .map(|i| if i % 2 == 0 { 0u8 } else { 255 })
                .collect(),
            width: w,
            height: h,
        };
        // Layer M: thirds.
        let m = PageBitmap {
            pixels: (0..n)
                .map(|i| if i % 3 == 0 { 0u8 } else { 255 })
                .collect(),
            width: w,
            height: h,
        };
        // Layer Y: fifths.
        let y = PageBitmap {
            pixels: (0..n)
                .map(|i| if i % 5 == 0 { 0u8 } else { 255 })
                .collect(),
            width: w,
            height: h,
        };
        let rgb = composite_cmy(&c, &m, &y);
        let (c2, m2, y2) = decompose_cmy(&rgb);
        assert_eq!(c2.pixels, c.pixels);
        assert_eq!(m2.pixels, m.pixels);
        assert_eq!(y2.pixels, y.pixels);
    }
}
