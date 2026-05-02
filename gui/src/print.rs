// Windows GDI printing — M9.
//
// Three-step flow per page:
//   1. PrintDlgExW (or its older sibling PrintDlgW) → user picks a
//      printer + paper + tray; we get back an HDC for the printer.
//   2. StartDocW + StartPage / EndPage / EndDoc bracket the job.
//   3. StretchDIBits pushes our 8-bpp grayscale bitmap onto the
//      printer's DC at the printer's native pixel resolution. We
//      ask for SRCCOPY: no scaling, no halftoning interpolation —
//      we want the dot pattern preserved exactly.
//
// The bitmap layer (`image::codecs::bmp::BmpEncoder`) we use for
// "save to file" wraps headers + a palette around the raw pixels.
// For GDI we hand-build a BITMAPINFO with a 256-entry grayscale
// palette and pass the raw pixel rows directly. Rows must be
// 4-byte aligned (Windows DIB convention); for grayscale 8-bpp
// that means we pad each row to the next multiple of 4.
//
// This module compiles on every platform (the API surface is
// `Result<(), PrintError>`); on non-Windows the `print_pages` entry
// returns `Err(PrintError::PlatformUnsupported)` and the GUI greys
// the button. Windows-specific code lives behind `cfg(windows)`.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Quality preset that drives the auto-picked dot density. Replaces
/// the old hand-tuned `blocks_per_inch` setting — ampaper picks the
/// largest dots (lowest density) within the preset's range that
/// still fits the input payload in one page. For payloads that
/// don't fit in one page at the preset's MAX density, multi-page
/// output is produced at the max density.
///
///   - **Safe**: 20–50 dot/inch — biggest dots, decodes cleanly off
///     a casual phone-camera scan or a low-quality scanner. Fits
///     less per page; use for files you really, really need to
///     recover. Within the range, ampaper picks the lowest density
///     that fits the payload — small files end up at the floor (20)
///     with cells filling the page generously.
///   - **Normal**: 40–100 dot/inch — PaperBack 1.10's calibration
///     point at the upper end. Reliable on a flatbed scan; some
///     errors recover via the per-group XOR redundancy. Default.
///   - **Compact**: 80–200 dot/inch — pack as much as possible per
///     page. The high end pushes scan_decode's grid finder near
///     its limits; reserve for inputs you've test-scanned at home.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum QualityPreset {
    Safe,
    #[default]
    Normal,
    Compact,
}

impl QualityPreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::Safe => "Safe — biggest dots, always decodes",
            Self::Normal => "Normal — PaperBack 1.10 default",
            Self::Compact => "Compact — pack densely, scan carefully",
        }
    }

    /// Allowed dot-density range, in data dots per inch. Auto-fit
    /// picks the lowest value in this range that fits the payload
    /// in one page; if it can't fit at the high end, multi-page
    /// output is produced at the high end.
    ///
    /// Floors are calibrated to two constraints:
    /// - At a 600-DPI printer, the encoder needs nx ≥ redundancy+1
    ///   cells across an 8.5" page; cell width is 35 × (ppix/dpi)
    ///   px, so dpi ≥ ~30 keeps redundancy=5 fitting on Letter.
    /// - scan_decode's grid finder is calibrated for ~3 device
    ///   pixels per dot at 300-DPI render; very-low-density
    ///   bitmaps (<25 dot/in) blow that up beyond what
    ///   find_peaks's bin range comfortably handles.
    ///
    /// 30 dot/in is the safe floor.
    pub fn density_range(self) -> (u32, u32) {
        match self {
            Self::Safe => (30, 60),
            Self::Normal => (60, 100),
            Self::Compact => (100, 200),
        }
    }
}

/// Estimate the encoded payload size for density-picking. When
/// `compress` is true, actually run bzip2 to get an accurate size
/// (typically 30–70% of raw); otherwise use the raw file size.
/// Returns 0 on stat failure — the density picker treats that as
/// "tiny payload" and returns the preset's floor density.
#[must_use]
pub fn estimate_payload_size(path: &Path, compress: bool) -> usize {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return 0,
    };
    if compress {
        ampaper::bz::compress(&bytes, ampaper::bz::BlockSize::Max).len()
    } else {
        bytes.len()
    }
}

/// Pick the lowest data-dot density within the preset's range that
/// fits `payload_size_bytes` in a single page of `page_width_inches`
/// × `page_height_inches`. Falls back to the preset's max density
/// when nothing in the range fits a single page.
///
/// `is_v2` adds the GCM tag (16 bytes) and accounts for the v2
/// SuperBlock's two-cell-per-string overhead vs v1's one.
#[must_use]
pub fn auto_blocks_per_inch(
    preset: QualityPreset,
    payload_size_bytes: usize,
    redundancy: u8,
    is_v2: bool,
    page_width_inches: f32,
    page_height_inches: f32,
) -> u32 {
    use ampaper::block::NDATA;
    let datasize = if is_v2 {
        payload_size_bytes + 16
    } else {
        (payload_size_bytes + 15) & !15
    };
    let n_data = datasize.div_ceil(NDATA) as u32;
    let r = redundancy as u32;
    let nstring = n_data.div_ceil(r.max(1));
    let cells_per_string = if is_v2 { nstring + 2 } else { nstring + 1 };
    let strings = r + 1;
    let cells_needed = strings * cells_per_string;

    let page_area = page_width_inches * page_height_inches;
    let (floor, target) = preset.density_range();

    // Sweep from floor → target in 5-dot/inch steps. The first
    // density that fits the payload single-page wins. Lower density
    // = bigger dots = more reliable scan recovery, so we want the
    // smallest one that works.
    let mut chosen = target;
    let mut d = floor;
    while d <= target {
        // Cells per page at density d. Each cell occupies (35 dots)²
        // area; the inverse gives us cells per square inch as
        // (d/35)², and total cells = page_area * (d/35)².
        let cells_per_page = (page_area * (d as f32).powi(2) / (35.0 * 35.0)) as u32;
        if cells_per_page >= cells_needed {
            chosen = d;
            break;
        }
        d += 5;
    }
    chosen
}

/// One page's worth of bitmap to print.
#[derive(Clone)]
pub struct PrintPage {
    /// 8-bit grayscale, row-major, length = `width * height`.
    pub bitmap: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Metadata for the PaperBack-1.10-style header line drawn at the top
/// of each PDF page. The header reads:
///
///   `<filename> [<yyyy-mm-dd hh:mm:ss>, <N> bytes] Page X of Y`
///
/// `modified_unix_secs == None` skips the date/time.
#[derive(Clone, Debug)]
pub struct PdfHeader {
    pub filename: String,
    pub modified_unix_secs: Option<u64>,
    pub origsize: u64,
}

#[derive(Debug)]
pub enum PrintError {
    /// User clicked Cancel in the print dialog. Not really an error;
    /// the GUI surfaces this as a status message rather than a modal.
    UserCancelled,
    /// We're not on Windows; this build can't drive a printer
    /// directly. Linux/macOS users save BMPs and print however they
    /// like (per memory/cross_platform_goal.md).
    #[cfg_attr(windows, allow(dead_code))]
    PlatformUnsupported,
    /// Something went wrong calling into Win32 GDI / printing —
    /// includes the API name and the HRESULT or last-error.
    Win32 { api: &'static str, message: String },
    /// Failed to read or decode an input file before sending to the
    /// printer. Carries the path + underlying error.
    Io { path: String, message: String },
}

impl core::fmt::Display for PrintError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UserCancelled => f.write_str("Print cancelled"),
            Self::PlatformUnsupported => {
                f.write_str("Printing is Windows-only in this build")
            }
            Self::Win32 { api, message } => write!(f, "{api}: {message}"),
            Self::Io { path, message } => write!(f, "{path}: {message}"),
        }
    }
}

impl std::error::Error for PrintError {}

/// Re-derive a `PageGeometry` whose `nx`/`ny` are just big enough
/// for the cells the data actually needs. The encoder then produces
/// a tight bitmap with the sync raster wrapping the data block area
/// (instead of wrapping a full Letter page of mostly-blank cells).
///
/// Used by the PDF save path on raw inputs — runs a quick "trial
/// encode" to learn how many cells the data takes, computes the
/// required bitmap dimensions, and returns a geometry the real
/// encode pass uses. Belt-and-suspenders: we still feed
/// pad_to_full_page=false so the trial encode places only the
/// cells it needs.
fn shrink_geometry_to_data(
    geometry: &ampaper::page::PageGeometry,
    data_len_bytes: usize,
    redundancy: u8,
    is_v2: bool,
) -> ampaper::page::PageGeometry {
    use ampaper::block::NDATA;
    use ampaper::page::CELL_SIZE_DOTS;

    // Number of data blocks the payload needs. v2 adds a 16-byte
    // GCM tag so the ciphertext is 16 bytes longer than the
    // plaintext input.
    let datasize = if is_v2 {
        data_len_bytes + 16
    } else {
        // v1 zero-pads to 16-byte alignment before splitting into
        // 90-byte blocks (Printer.cpp:417).
        (data_len_bytes + 15) & !15
    };
    let n_data = datasize.div_ceil(NDATA);
    let r = redundancy as usize;
    let nstring = n_data.div_ceil(r.max(1));
    let cells_per_string = if is_v2 { nstring + 2 } else { nstring + 1 };
    let strings = r + 1;
    let total_cells = strings * cells_per_string;

    // Lay the cells out at most `nx_max` columns wide so they form
    // a short, page-shaped rectangle, and respect the encoder's
    // minimum-page-size guards (redundancy+1 cols, 3 rows,
    // 2*redundancy+2 cells). Pick nx = redundancy+1 — the smallest
    // valid width — and ny = max(3, ceil(total / nx)) so the
    // bitmap is roughly square-ish for small inputs and grows in
    // height as the payload gets bigger.
    let nx_max = geometry.nx().max(1) as usize;
    let nx_min = r + 1;
    let nx = nx_min.min(nx_max).max(nx_min);
    let ny_min_for_cells = total_cells.div_ceil(nx);
    let ny = ny_min_for_cells.max(3);

    let dx = geometry.dx();
    let dy = geometry.dy();
    let cell_pitch_x = CELL_SIZE_DOTS as u32 * dx;
    let cell_pitch_y = CELL_SIZE_DOTS as u32 * dy;
    let target_width = nx as u32 * cell_pitch_x + geometry.px() + 2 * geometry.border();
    let target_height = ny as u32 * cell_pitch_y + geometry.py() + 2 * geometry.border();

    ampaper::page::PageGeometry {
        width: target_width,
        height: target_height,
        ..*geometry
    }
}

/// Format the header line drawn at the top of every PDF page. Shape:
///   `"<filename> [<yyyy-mm-dd hh:mm:ss>, <N> bytes] Page X of Y"`
/// Skips the date/time block when modified_unix_secs is None.
fn format_header_text(h: &PdfHeader, page_idx: usize, page_count: usize) -> String {
    let mut bracket = String::new();
    if let Some(secs) = h.modified_unix_secs {
        bracket.push_str(&format_unix_secs_iso(secs));
        bracket.push_str(", ");
    }
    bracket.push_str(&format_byte_count(h.origsize));
    bracket.push_str(" bytes");
    format!(
        "{} [{}] Page {} of {}",
        h.filename,
        bracket,
        page_idx + 1,
        page_count
    )
}

/// Convert seconds-since-1970 to `YYYY-MM-DD HH:MM:SS` (UTC) without
/// pulling in chrono / time. Civil-date math from Howard Hinnant's
/// "days_from_civil" derivation.
fn format_unix_secs_iso(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    let (y, m, d) = days_to_civil(days + 719_468);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
}

fn days_to_civil(z: i64) -> (i64, u32, u32) {
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Render a byte count with thousands separators.
fn format_byte_count(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

/// PaperBack 1.10's drop-any-file UX: each input is either a
/// pre-rendered bitmap (BMP/PNG/JPG — pass straight through) or
/// anything else (raw file, including PDFs → encode through the
/// codec using the supplied settings).
///
/// Returns the bitmaps in the order the user dropped them; raw
/// inputs that span multiple pages produce multiple bitmaps in
/// place. The caller hands the result to [`print_pages`] or
/// [`save_pages_as_pdf`] without caring which path each came from.
pub fn prepare_print_pages(
    paths: &[impl AsRef<Path>],
    encode_options: &ampaper::encoder::EncodeOptions,
    quality: QualityPreset,
    v2_password: Option<&str>,
) -> Result<Vec<PrintPage>, PrintError> {
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let p = path.as_ref();
        let kind = sniff_kind(p)?;
        match kind {
            InputKind::Image => {
                let img = image::open(p).map_err(|e| PrintError::Io {
                    path: p.display().to_string(),
                    message: e.to_string(),
                })?;
                let luma = img.to_luma8();
                let (w, h) = luma.dimensions();
                out.push(PrintPage {
                    bitmap: luma.into_raw(),
                    width: w,
                    height: h,
                });
            }
            InputKind::Raw => {
                // Encode this file through the codec. We shrink the
                // PageGeometry to just the size the data needs —
                // the sync-raster border (print_border:true) then
                // wraps the actual data area, not a full Letter
                // page of mostly-blank cells. Matches PB 1.10's
                // print appearance.
                let bytes = std::fs::read(p).map_err(|e| PrintError::Io {
                    path: p.display().to_string(),
                    message: e.to_string(),
                })?;
                let name = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("input.bin");
                let meta = ampaper::encoder::FileMeta {
                    name,
                    modified: 0,
                    attributes: 0x80,
                };
                let is_v2 = v2_password.is_some_and(|pw| !pw.is_empty());
                let payload_len = if encode_options.compress {
                    ampaper::bz::compress(&bytes, ampaper::bz::BlockSize::Max).len()
                } else {
                    bytes.len()
                };
                // Auto-pick dot density: lowest density (biggest
                // dots) within the quality preset's range that
                // still fits the payload in one page. For a small
                // file like a 446-byte text doc, this picks the
                // preset's floor density, making the cells nice
                // and big in the resulting print.
                let (in_w, in_h) = (
                    encode_options.geometry.width as f32 / encode_options.geometry.ppix as f32,
                    encode_options.geometry.height as f32 / encode_options.geometry.ppiy as f32,
                );
                let auto_dpi = auto_blocks_per_inch(
                    quality,
                    payload_len,
                    encode_options.redundancy,
                    is_v2,
                    in_w,
                    in_h,
                );
                let geometry_with_auto_dpi = ampaper::page::PageGeometry {
                    dpi: auto_dpi,
                    ..encode_options.geometry
                };
                let shrunk_geometry = shrink_geometry_to_data(
                    &geometry_with_auto_dpi,
                    payload_len,
                    encode_options.redundancy,
                    is_v2,
                );
                let shrunk_options = ampaper::encoder::EncodeOptions {
                    geometry: shrunk_geometry,
                    ..*encode_options
                };
                let encoded = match v2_password {
                    Some(pw) if !pw.is_empty() => {
                        ampaper::encoder::encode_v2(
                            &bytes,
                            &shrunk_options,
                            &meta,
                            pw.as_bytes(),
                        )
                        .map_err(|e| PrintError::Io {
                            path: p.display().to_string(),
                            message: format!("encode_v2: {e}"),
                        })?
                    }
                    _ => ampaper::encoder::encode(&bytes, &shrunk_options, &meta)
                        .map_err(|e| PrintError::Io {
                            path: p.display().to_string(),
                            message: format!("encode: {e}"),
                        })?,
                };
                for page in encoded {
                    out.push(PrintPage {
                        bitmap: page.bitmap,
                        width: page.width,
                        height: page.height,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// What the first few bytes of a file say about its format. Only
/// pre-rendered bitmap formats get pass-through treatment — every
/// other format (including PDF) is treated as raw bytes to encode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputKind {
    Image,
    Raw,
}

fn sniff_kind(path: &Path) -> Result<InputKind, PrintError> {
    use std::io::Read;
    let mut sniff = [0u8; 8];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut sniff))
        .map_err(|e| PrintError::Io {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
    let head = &sniff[..n];
    // BMP "BM", PNG \x89PNG, JPEG \xFF\xD8\xFF. PDFs deliberately
    // *not* matched here: most PDFs dropped on the Print tab are
    // user data the user wants encoded, not ampaper-rendered output
    // to be re-printed verbatim.
    if head.starts_with(b"BM")
        || head.starts_with(b"\x89PNG")
        || head.starts_with(&[0xFFu8, 0xD8, 0xFF])
    {
        return Ok(InputKind::Image);
    }
    Ok(InputKind::Raw)
}

#[cfg(windows)]
pub fn print_pages(pages: &[PrintPage], doc_name: &str) -> Result<(), PrintError> {
    win32::print_pages(pages, doc_name)
}

#[cfg(not(windows))]
pub fn print_pages(_pages: &[PrintPage], _doc_name: &str) -> Result<(), PrintError> {
    Err(PrintError::PlatformUnsupported)
}

/// Write the pages out as a multi-page PDF at `path`. Cross-platform
/// — pure Rust via the `printpdf` crate (MIT). Each PDF page is
/// sized at `(width / dpi)` × `(height / dpi)` inches so 1 device
/// pixel = 1/dpi inch on paper, matching what a direct print at
/// the same DPI would produce.
///
/// Note: the source BMPs don't carry DPI metadata reliably (PB 1.10
/// BMPs do, but the `image` crate's BmpEncoder we use on the encode
/// side doesn't set it), so the caller passes `dpi` explicitly. The
/// natural value is whatever was used at encode time — typically
/// 600 DPI for consumer laser printers (the EncodeView default).
pub fn save_pages_as_pdf(
    pages: &[PrintPage],
    dpi: u32,
    header: Option<&PdfHeader>,
    doc_name: &str,
    path: &Path,
) -> Result<(), PrintError> {
    use printpdf::{
        BuiltinFont, ImageCompression, ImageOptimizationOptions, Mm, Op, PdfDocument,
        PdfFontHandle, PdfPage, PdfSaveOptions, Point, Pt, RawImage, RawImageData,
        RawImageFormat, TextItem, XObjectTransform,
    };

    if pages.is_empty() {
        return Err(PrintError::Io {
            path: path.display().to_string(),
            message: "no pages to write".into(),
        });
    }
    if dpi == 0 {
        return Err(PrintError::Io {
            path: path.display().to_string(),
            message: "DPI must be > 0".into(),
        });
    }

    let mut doc = PdfDocument::new(doc_name);

    // Page layout — Letter portrait, with PB-1.10-style margins and
    // a header line at the top.
    //
    //   ┌──────────────────────────────────┐ ← y = LETTER_HEIGHT_PT
    //   │  HEADER (Helvetica, ~10pt)       │   ↑ ~0.5" margin
    //   │----------------------------------│
    //   │                                  │
    //   │       [centered bitmap]          │
    //   │                                  │
    //   └──────────────────────────────────┘ ← y = 0
    //
    // PDF coordinates are bottom-left origin, so larger y = higher
    // on the page. The bitmap's `translate_y` is its bottom edge.
    const LETTER_WIDTH_PT: f32 = 612.0; // 8.5 in × 72
    const LETTER_HEIGHT_PT: f32 = 792.0; // 11 in × 72
    const PAGE_MARGIN_PT: f32 = 36.0; // 0.5 inch
    const HEADER_FONT_SIZE_PT: f32 = 10.0;
    const HEADER_GAP_PT: f32 = 6.0; // gap between header text baseline and bitmap top
    let mut pdf_pages = Vec::with_capacity(pages.len());

    let helvetica = PdfFontHandle::Builtin(BuiltinFont::Helvetica);

    for (i, page) in pages.iter().enumerate() {
        let raw = RawImage {
            width: page.width as usize,
            height: page.height as usize,
            data_format: RawImageFormat::R8,
            pixels: RawImageData::U8(page.bitmap.clone()),
            tag: Vec::new(),
        };
        let image_id = doc.add_image(&raw);

        let img_width_pt = page.width as f32 * 72.0 / dpi as f32;
        let img_height_pt = page.height as f32 * 72.0 / dpi as f32;

        // Pages: Letter unless header is None AND bitmap is bigger
        // than Letter (caller knows what they want — keep the
        // tight-bitmap fallback for testing). With header on, always
        // Letter.
        let use_letter_page = header.is_some()
            || (img_width_pt <= LETTER_WIDTH_PT && img_height_pt <= LETTER_HEIGHT_PT);
        let (page_width_pt, page_height_pt) = if use_letter_page {
            (LETTER_WIDTH_PT, LETTER_HEIGHT_PT)
        } else {
            (img_width_pt, img_height_pt)
        };

        // Bitmap bottom-left coordinate. Centered horizontally.
        // Vertically: top edge sits PAGE_MARGIN + header_h + gap
        // below the page top when header is on; just PAGE_MARGIN
        // below the top otherwise.
        let header_band_pt = if header.is_some() {
            HEADER_FONT_SIZE_PT + HEADER_GAP_PT
        } else {
            0.0
        };
        let img_x_pt = ((page_width_pt - img_width_pt) / 2.0).max(0.0);
        let img_top_pt = page_height_pt - PAGE_MARGIN_PT - header_band_pt;
        let img_bottom_pt = img_top_pt - img_height_pt;
        let img_y_pt = img_bottom_pt.max(0.0);

        let mut ops = Vec::new();

        if let Some(h) = header {
            let header_text = format_header_text(h, i, pages.len());
            // Header baseline sits PAGE_MARGIN below top edge
            // (in PDF coords: y = page_height - PAGE_MARGIN).
            let header_baseline_y_pt = page_height_pt - PAGE_MARGIN_PT;
            let header_x_pt = PAGE_MARGIN_PT;
            ops.extend([
                Op::StartTextSection,
                Op::SetFont {
                    font: helvetica.clone(),
                    size: Pt(HEADER_FONT_SIZE_PT),
                },
                Op::SetTextCursor {
                    pos: Point {
                        x: Pt(header_x_pt),
                        y: Pt(header_baseline_y_pt),
                    },
                },
                Op::ShowText {
                    items: vec![TextItem::Text(header_text)],
                },
                Op::EndTextSection,
            ]);
        }

        ops.push(Op::UseXobject {
            id: image_id,
            transform: XObjectTransform {
                translate_x: Some(Pt(img_x_pt)),
                translate_y: Some(Pt(img_y_pt)),
                dpi: Some(dpi as f32),
                ..Default::default()
            },
        });

        let width_mm = page_width_pt * 25.4 / 72.0;
        let height_mm = page_height_pt * 25.4 / 72.0;
        pdf_pages.push(PdfPage::new(Mm(width_mm), Mm(height_mm), ops));
    }

    doc.with_pages(pdf_pages);

    // printpdf defaults to NO compression on image streams, which
    // makes a 5100×6600 grayscale page balloon the PDF to ~33 MB.
    // Switch on Flate (lossless) compression — ampaper bitmaps are
    // bimodal black/white with large white runs (especially with
    // pad_to_full_page = false), so deflate squeezes them down by
    // ~50–100×. Quality / max_image_size / dither only matter for
    // lossy paths; we set format = Flate to force the codec.
    let opts = PdfSaveOptions {
        image_optimization: Some(ImageOptimizationOptions {
            format: Some(ImageCompression::Flate),
            // No re-encoding to a lossy codec, no resize, no dither —
            // we want the original pixels through, just deflate'd.
            quality: None,
            max_image_size: None,
            dither_greyscale: None,
            convert_to_greyscale: None,
            auto_optimize: Some(false),
        }),
        ..Default::default()
    };
    let mut warnings = Vec::new();
    let bytes = doc.save(&opts, &mut warnings);
    std::fs::write(path, bytes).map_err(|e| PrintError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    Ok(())
}

#[cfg(windows)]
mod win32 {
    use super::{PrintError, PrintPage};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteDC, RGBQUAD, SRCCOPY,
        StretchDIBits,
    };
    use windows::Win32::Storage::Xps::{DOCINFOW, EndDoc, EndPage, StartDocW, StartPage};
    use windows::Win32::UI::Controls::Dialogs::{
        PD_ALLPAGES, PD_RESULT_CANCEL, PD_RETURNDC, PRINTDLGEX_FLAGS, PRINTDLGEXW, PrintDlgExW,
        START_PAGE_GENERAL,
    };

    /// PrintDlgEx flags we care about:
    ///   - PD_RETURNDC — get back a printer-ready HDC, not just the
    ///     printer name; saves us from a separate CreateDC call and
    ///     gets the user's tray / paper / orientation choices baked
    ///     in for free.
    ///   - PD_ALLPAGES (no per-page selection — the user picked these
    ///     pages by dropping them in; we always print all of them).
    fn printdlg_flags() -> PRINTDLGEX_FLAGS {
        PRINTDLGEX_FLAGS(PD_RETURNDC.0 | PD_ALLPAGES.0)
    }

    pub fn print_pages(pages: &[PrintPage], doc_name: &str) -> Result<(), PrintError> {
        if pages.is_empty() {
            return Err(PrintError::Win32 {
                api: "print_pages",
                message: "no pages to print".into(),
            });
        }

        // 1. Show the print dialog. PRINTDLGEXW is heap-allocated and
        // partly opaque; zeroing it out + setting the few fields we
        // care about is the documented pattern.
        let mut pdex: PRINTDLGEXW = unsafe { core::mem::zeroed() };
        pdex.lStructSize = core::mem::size_of::<PRINTDLGEXW>() as u32;
        pdex.hwndOwner = HWND::default();
        pdex.Flags = printdlg_flags();
        // Open on the "General" tab — the printer picker. Other tabs
        // (Layout, Paper) are wizard-specific and not relevant for
        // dropping a pre-rendered bitmap on a printer.
        pdex.nStartPage = START_PAGE_GENERAL;
        pdex.nCopies = 1;

        // PrintDlgExW returns HRESULT, NOT a bool. S_OK == success
        // (with Flags & PD_RETURNDC giving us pdex.hDC).
        let hr = unsafe { PrintDlgExW(&mut pdex) };
        if hr.is_err() {
            return Err(PrintError::Win32 {
                api: "PrintDlgExW",
                message: format!("HRESULT {hr:?}"),
            });
        }
        if pdex.dwResultAction == PD_RESULT_CANCEL {
            return Err(PrintError::UserCancelled);
        }
        let hdc = pdex.hDC;
        if hdc.is_invalid() {
            return Err(PrintError::Win32 {
                api: "PrintDlgExW",
                message: "no HDC returned even with PD_RETURNDC".into(),
            });
        }

        // RAII guard so the HDC is freed even on early-return.
        struct DcGuard(windows::Win32::Graphics::Gdi::HDC);
        impl Drop for DcGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = DeleteDC(self.0);
                }
            }
        }
        let _dc = DcGuard(hdc);

        // 2. StartDoc → StartPage / draw / EndPage * N → EndDoc.
        let doc_name_w: Vec<u16> = doc_name.encode_utf16().chain(std::iter::once(0)).collect();
        let docinfo = DOCINFOW {
            cbSize: core::mem::size_of::<DOCINFOW>() as i32,
            lpszDocName: windows::core::PCWSTR(doc_name_w.as_ptr()),
            ..unsafe { core::mem::zeroed() }
        };
        let job_id = unsafe { StartDocW(hdc, &docinfo) };
        if job_id <= 0 {
            return Err(PrintError::Win32 {
                api: "StartDocW",
                message: format!("returned {job_id}"),
            });
        }

        for page in pages {
            if unsafe { StartPage(hdc) } <= 0 {
                let _ = unsafe { EndDoc(hdc) };
                return Err(PrintError::Win32 {
                    api: "StartPage",
                    message: "non-positive return".into(),
                });
            }

            // 3. StretchDIBits with a hand-built grayscale BITMAPINFO.
            // Win32 DIBs are bottom-up by default (positive height
            // means "top is at the bottom of the data"), so we
            // negate height to feed top-down rows — which is what
            // our codec produces.
            //
            // Byte layout of BITMAPINFO for 8 bpp = BITMAPINFOHEADER
            // followed by 256 RGBQUAD entries. Box the whole thing
            // so we can reference it with a stable pointer.
            let info_buf: Box<BitmapInfo256> = Box::new(BitmapInfo256::grayscale(
                page.width as i32,
                page.height as i32,
            ));
            // Pixel rows must be 4-byte aligned. Build a padded copy
            // when the row already isn't; pass through otherwise.
            let stride = page.width as usize;
            let padded_stride = stride.div_ceil(4) * 4;
            let pixels: Vec<u8> = if padded_stride == stride {
                page.bitmap.clone()
            } else {
                let mut out = vec![0u8; padded_stride * page.height as usize];
                for y in 0..page.height as usize {
                    let src = y * stride;
                    let dst = y * padded_stride;
                    out[dst..dst + stride]
                        .copy_from_slice(&page.bitmap[src..src + stride]);
                }
                out
            };

            // dest size = source size at 1:1 device pixels. The user's
            // printer DPI equals what they configured in the Encode
            // tab, so this is the bytes-as-printed mapping.
            //
            // We cast `&BitmapInfo256` to `*const BITMAPINFO`. The
            // layout matches: BITMAPINFOHEADER is the first field
            // followed by 256 RGBQUAD entries — exactly what
            // BITMAPINFO's flexible-array tail expects for an 8-bpp
            // DIB with `biClrUsed = 256`. `#[repr(C)]` on
            // BitmapInfo256 + the palette being the immediately-
            // following field guarantees the cast is sound.
            let result = unsafe {
                StretchDIBits(
                    hdc,
                    0,
                    0,
                    page.width as i32,
                    page.height as i32,
                    0,
                    0,
                    page.width as i32,
                    page.height as i32,
                    Some(pixels.as_ptr() as *const _),
                    &*info_buf as *const BitmapInfo256 as *const BITMAPINFO,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                )
            };
            if result == 0 {
                let _ = unsafe { EndPage(hdc) };
                let _ = unsafe { EndDoc(hdc) };
                return Err(PrintError::Win32 {
                    api: "StretchDIBits",
                    message: "returned 0 (failed to push DIB)".into(),
                });
            }

            if unsafe { EndPage(hdc) } <= 0 {
                let _ = unsafe { EndDoc(hdc) };
                return Err(PrintError::Win32 {
                    api: "EndPage",
                    message: "non-positive return".into(),
                });
            }
        }

        if unsafe { EndDoc(hdc) } <= 0 {
            return Err(PrintError::Win32 {
                api: "EndDoc",
                message: "non-positive return".into(),
            });
        }
        Ok(())
    }

    /// BITMAPINFO with a fixed 256-entry palette area. Mirrors the
    /// idiomatic C `struct { BITMAPINFOHEADER; RGBQUAD[256]; }`.
    #[repr(C)]
    struct BitmapInfo256 {
        header: BITMAPINFOHEADER,
        palette: [RGBQUAD; 256],
    }

    impl BitmapInfo256 {
        fn grayscale(width: i32, height: i32) -> Self {
            let mut palette = [RGBQUAD::default(); 256];
            for (i, entry) in palette.iter_mut().enumerate() {
                let v = i as u8;
                entry.rgbRed = v;
                entry.rgbGreen = v;
                entry.rgbBlue = v;
                entry.rgbReserved = 0;
            }
            BitmapInfo256 {
                header: BITMAPINFOHEADER {
                    biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height, // negative = top-down rows
                    biPlanes: 1,
                    biBitCount: 8,
                    biCompression: BI_RGB.0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 256,
                    biClrImportant: 0,
                },
                palette,
            }
        }

    }
}
