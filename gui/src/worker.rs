// Worker-thread plumbing for long-running codec jobs.
//
// The encoder and decoder are CPU-bound; running them inline on
// egui's update loop would block the UI for hundreds of milliseconds
// per page. Instead we spawn a `std::thread` and post progress +
// results back via a `mpsc::Receiver` the UI drains every frame.
//
// `egui::Context::request_repaint` is the bridge: when the worker
// has news, it pings the context, which schedules a repaint, which
// fires `App::ui` again, which drains the channel. No polling, no
// timer, no surprise stalls.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use ampaper::block::{Block, BLOCK_BYTES, MAXSIZE, SUPERBLOCK_ADDR, SuperBlock};
use ampaper::encoder::{
    EncodeError, EncodeOptions, EncodedPage, FileMeta, encode, encode_v2,
};
use ampaper::format_v2::{
    V2_SUPERBLOCK_ADDR_CELL1, V2_SUPERBLOCK_ADDR_CELL2, V2SuperBlockCell1,
};
use ampaper::scan::{ScanGeometry, detect_geometry, scan_decode, scan_extract};
use pdfium_render::prelude::{PdfRenderConfig, Pdfium, PdfiumError};

/// Snapshot of an encode request, populated from the UI when the
/// user clicks "Encode."
#[derive(Clone)]
pub struct EncodeRequest {
    pub input_path: PathBuf,
    pub output_dir: PathBuf,
    pub output_stem: String,
    pub options: EncodeOptions,
    /// `None` → emit v1 (legacy PaperBack 1.10-compatible). `Some` →
    /// emit v2 (AES-256-GCM, encrypted by definition).
    pub v2_password: Option<String>,
}

/// Messages the worker posts back to the UI thread.
pub enum EncodeMessage {
    /// Worker started; UI flips to "encoding..." state.
    Started,
    /// Worker finished a step the user might want to see in the
    /// status bar (e.g. "compressing", "encrypting", "rendering page
    /// 3/12"). Optional — emit when meaningful, ignore when not.
    Status(String),
    /// Worker finished successfully and wrote N files.
    Done { files: Vec<PathBuf> },
    /// Worker failed. The UI shows this in the status bar /
    /// (later) a modal.
    Failed(String),
}

/// Handle the UI keeps while a job is in flight. Drained per frame.
pub struct EncodeJob {
    pub rx: mpsc::Receiver<EncodeMessage>,
}

impl EncodeJob {
    /// Spawn a worker that runs `encode` (or `encode_v2`) on `req`,
    /// writes one BMP per page under `req.output_dir`, and posts
    /// progress via the returned [`EncodeJob`].
    ///
    /// `repaint` is captured by the worker and called whenever a
    /// message goes onto the channel — egui then schedules a
    /// repaint and the UI's next `update` drains the messages.
    pub fn spawn(req: EncodeRequest, repaint: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let send = |msg: EncodeMessage| {
                let _ = tx.send(msg);
                repaint();
            };
            send(EncodeMessage::Started);
            match run_encode(&req, &send) {
                Ok(files) => send(EncodeMessage::Done { files }),
                Err(e) => send(EncodeMessage::Failed(e)),
            }
        });
        EncodeJob { rx }
    }
}

fn run_encode(
    req: &EncodeRequest,
    send: &impl Fn(EncodeMessage),
) -> Result<Vec<PathBuf>, String> {
    send(EncodeMessage::Status("Reading input...".into()));
    let input = std::fs::read(&req.input_path)
        .map_err(|e| format!("read {}: {e}", req.input_path.display()))?;

    let meta = FileMeta {
        name: req
            .input_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("input.bin"),
        modified: 0,
        attributes: 0x80,
    };

    let pages = match req.v2_password.as_ref() {
        None => {
            send(EncodeMessage::Status("Encoding (v1)...".into()));
            encode(&input, &req.options, &meta).map_err(format_encode_error)?
        }
        Some(pw) => {
            send(EncodeMessage::Status("Encoding (v2 / AES-256-GCM)...".into()));
            encode_v2(&input, &req.options, &meta, pw.as_bytes())
                .map_err(format_encode_error)?
        }
    };

    if !req.output_dir.exists() {
        std::fs::create_dir_all(&req.output_dir)
            .map_err(|e| format!("create output dir: {e}"))?;
    }

    let mut written = Vec::with_capacity(pages.len());
    for (i, page) in pages.iter().enumerate() {
        send(EncodeMessage::Status(format!(
            "Writing page {}/{}",
            i + 1,
            pages.len()
        )));
        let path = req
            .output_dir
            .join(format!("{}-page-{:03}.bmp", req.output_stem, i + 1));
        write_grayscale_bmp(&path, page)?;
        written.push(path);
    }
    Ok(written)
}

fn format_encode_error(e: EncodeError) -> String {
    format!("{e}")
}

fn write_grayscale_bmp(path: &std::path::Path, page: &EncodedPage) -> Result<(), String> {
    use image::ImageEncoder;
    let mut buf = std::fs::File::create(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    let encoder = image::codecs::bmp::BmpEncoder::new(&mut buf);
    encoder
        .write_image(
            &page.bitmap,
            page.width,
            page.height,
            image::ExtendedColorType::L8,
        )
        .map_err(|e| format!("encode {}: {e}", path.display()))
}

// ====================================================================
// Decode side
// ====================================================================
//
// Same shape as the encode worker, but the I/O direction is inverted:
// the user supplies one or more bitmap files, the worker runs
// `scan_decode` (auto-detected geometry) and `scan_extract` (for the
// per-cell visualisation), and posts a DecodeMessage with the
// recovered bytes plus a per-page classification grid.
//
// The classification is computed in the GUI rather than the lib: we
// run scan_extract once per page, parse each cell into a Block,
// CRC-check it, and bucket it. PaperBack 1.10's "good / bad" grid
// gave users a clear signal about scan quality without exposing
// per-dot detail; we mirror that posture.

/// One page worth of input on the decode side.
#[derive(Clone)]
pub struct DecodePage {
    /// Source path (for display in the result list).
    pub source: PathBuf,
    /// Grayscale 8-bit bitmap, row-major. `luma.len() == width * height`.
    /// Used by the legacy v1/v2 decoder and the v3 B&W decoder.
    pub luma: Vec<u8>,
    /// RGB 8-bit bitmap, row-major, 3 bytes per pixel (R, G, B).
    /// `rgb.len() == width * height * 3`. Used by the v3 CMY decoder.
    /// Always populated for PDF-rendered pages; image-loaded inputs
    /// derive RGB from luma (R = G = B = luma) so non-color sources
    /// route to the grayscale paths cleanly.
    pub rgb: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Snapshot of a decode request from the UI.
#[derive(Clone)]
pub struct DecodeRequest {
    pub pages: Vec<DecodePage>,
    /// `None` = unencrypted v1 OR v2 with password (we'll error
    /// cleanly if the file claims encryption but no password was
    /// supplied).
    pub password: Option<String>,
}

/// Per-cell classification for the visual grid. Mirrors the
/// PaperBack 1.10 "Read" dialog colour code, simplified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellStatus {
    /// CRC verified; cell carries valid data.
    DataOk,
    /// CRC verified; cell is a v1 SuperBlock or a v2 cell 1 / cell 2.
    Super,
    /// CRC verified; cell is a recovery (XOR-checksum) block.
    Recovery,
    /// CRC failed even after the scan extractor's RS sweep — the
    /// cell is unreadable on this scan. Group recovery may still
    /// pull the underlying data block back if no more than one data
    /// cell of the group is in this state.
    Damaged,
    /// Cell falls outside the data area: blank paper, page header,
    /// or margin region picked up by the auto-detected grid bounds.
    /// Distinct from `Damaged` — empty cells are expected on real
    /// scans with PB-1.10-style headers / borders, not a sign of
    /// scan corruption. Renders neutrally in the UI.
    Empty,
}

/// Per-page classification — mirrors the auto-detected scan grid so
/// the GUI can render an `nx` × `ny` grid where each rectangle is
/// coloured per [`CellStatus`].
#[derive(Clone, Debug)]
pub struct PageReport {
    pub source: PathBuf,
    pub nx: u32,
    pub ny: u32,
    /// Length `nx * ny`, row-major.
    pub cells: Vec<CellStatus>,
    /// Filename the encoder stored in the SuperBlock, if a SuperBlock
    /// (v1 or v2 cell 1) survived CRC on this page. PB 1.10 caps at
    /// 31 chars + NUL; v2 cell 1 carries the full 63-char + NUL slot.
    /// Empty / NUL-only names yield `None`.
    pub original_filename: Option<String>,
}

/// Messages the decode worker posts back to the UI thread.
pub enum DecodeMessage {
    Started,
    Status(String),
    /// The classification grid for one input page is ready. Sent
    /// before `Done` / `Failed` so the UI can render the grid even
    /// if the final reassembly + decrypt step errors.
    PageClassified(PageReport),
    /// Worker finished successfully and recovered `plaintext`.
    Done { plaintext: Vec<u8> },
    /// Worker failed. Pages already classified via PageClassified
    /// remain valid for display; this carries only the error string.
    Failed(String),
}

pub struct DecodeJob {
    pub rx: mpsc::Receiver<DecodeMessage>,
}

impl DecodeJob {
    pub fn spawn(req: DecodeRequest, repaint: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let send = |msg: DecodeMessage| {
                let _ = tx.send(msg);
                repaint();
            };
            send(DecodeMessage::Started);
            match run_decode(&req, &send) {
                Ok(plaintext) => send(DecodeMessage::Done { plaintext }),
                Err(e) => send(DecodeMessage::Failed(e)),
            }
        });
        DecodeJob { rx }
    }
}

fn run_decode(req: &DecodeRequest, send: &impl Fn(DecodeMessage)) -> Result<Vec<u8>, String> {
    if req.pages.is_empty() {
        return Err("no input pages".into());
    }

    // Dispatch by codec, in order:
    //   1. v3 CMY (color) — sniff for color content in the rendered
    //      RGB. Routes to ampaper::v3::decode_pages_cmyk.
    //   2. v3 B&W — sniff for QR-style corner finders. Routes to
    //      ampaper::v3::decode_pages.
    //   3. Legacy v1/v2 — fallback. Existing scan_decode path.
    // Legacy classification is skipped for v3 pages (it'd show
    // all-red because v3 cells aren't PB-1.10 cells); a v3-
    // specific cell classification is a future polish item.
    //
    // For v3 inputs that came from a PDF: re-render the source at
    // V3_PDF_RENDER_DPI (600) before decoding. The default 300-DPI
    // pdf-to-bitmap render leaves the v3 codec sampling on
    // fractional pixel boundaries (ppd=1.5 when encoder_ppd=3 and
    // print_dpi=600 — the GUI's defaults), which is too phase-
    // sensitive: small image-origin offsets within the bitmap (eg
    // an extra header band shifting where the data area starts)
    // shift sampled pixels by a half-dot and corrupt enough cells
    // that RaptorQ can't converge on dense pages. 600 DPI render
    // gives integer pixels-per-dot for the GUI's encode defaults
    // and decodes cleanly regardless of phase. We keep 300 DPI as
    // the default first render because it's lighter weight and
    // works for the legacy scan_decode path's calibration range
    // (FORMAT-V1.md §5: scan_decode tuned for ~3 px/dot at 300
    // DPI render, breaks at higher).
    let initial_color = has_color(&req.pages[0]);
    let initial_v3_bw = sniff_v3(&req.pages[0]);
    let v3_detected = initial_color || initial_v3_bw;
    eprintln!(
        "[v3] dispatch sniff: input page 1 is {}x{} pixels, has_color={}, v3_bw_finders={}, v3_detected={}",
        req.pages[0].width, req.pages[0].height, initial_color, initial_v3_bw, v3_detected
    );
    let pages_owned: Vec<DecodePage>;
    let pages_for_decode: &[DecodePage] = if v3_detected {
        match rerender_pdf_pages_at(req.pages.as_slice(), V3_PDF_RENDER_DPI) {
            Ok(p) => {
                eprintln!(
                    "[v3] re-rendered {} page(s) from PDF source at {} DPI for v3 cell sampling (was {} DPI)",
                    p.len(),
                    V3_PDF_RENDER_DPI,
                    DEFAULT_PDF_RENDER_DPI
                );
                pages_owned = p;
                send(DecodeMessage::Status(format!(
                    "Re-rendered PDF input at {V3_PDF_RENDER_DPI} DPI for v3 cell sampling"
                )));
                pages_owned.as_slice()
            }
            Err(e) => {
                // Re-render failure (e.g. source is an image, not a
                // PDF) is fine — fall back to the original render.
                eprintln!(
                    "[v3] re-render at {} DPI unavailable ({}); using original-DPI bitmap",
                    V3_PDF_RENDER_DPI, e
                );
                send(DecodeMessage::Status(format!(
                    "Using original-DPI bitmap (re-render unavailable: {e})"
                )));
                req.pages.as_slice()
            }
        }
    } else {
        req.pages.as_slice()
    };
    let rerendered_req = DecodeRequest {
        pages: pages_for_decode.to_vec(),
        password: req.password.clone(),
    };
    if has_color(&rerendered_req.pages[0]) {
        eprintln!("[v3] routing to v3 CMY decoder");
        send(DecodeMessage::Status(
            "v3 CMY codec detected — decoding via RaptorQ + 3-channel pool".into(),
        ));
        return run_v3_cmy_decode(&rerendered_req);
    }
    if sniff_v3(&rerendered_req.pages[0]) {
        eprintln!("[v3] routing to v3 B&W decoder");
        send(DecodeMessage::Status(
            "v3 B&W codec detected — decoding via RaptorQ".into(),
        ));
        return run_v3_decode(&rerendered_req);
    }
    eprintln!("[v3] no v3 codec detected on re-rendered bitmap; falling through to legacy path");

    // Legacy path. First pass: classify each page's cells. We do
    // this even if the eventual decode fails — the user still
    // wants to see WHY (e.g., almost everything red = the scan is
    // too noisy / the grid wasn't recovered).
    for (i, page) in req.pages.iter().enumerate() {
        send(DecodeMessage::Status(format!(
            "Analyzing page {}/{}",
            i + 1,
            req.pages.len()
        )));
        if let Some(report) = classify_page(page) {
            send(DecodeMessage::PageClassified(report));
        } else {
            // detect_geometry returned None — the bitmap doesn't have
            // a recognisable PaperBack grid. Surface a 1x1 "all
            // damaged" report so the UI shows SOMETHING instead of
            // an empty grid.
            send(DecodeMessage::PageClassified(PageReport {
                source: page.source.clone(),
                nx: 1,
                ny: 1,
                cells: vec![CellStatus::Damaged],
                original_filename: None,
            }));
        }
    }

    // Second pass: full scan_decode for the actual recovered bytes.
    // scan_decode runs scan_extract again internally; this is some
    // duplicated work, but it keeps the lib API simple and the
    // overhead is negligible compared to the human-time scale of
    // dragging a file in.
    send(DecodeMessage::Status("Decoding...".into()));
    let pages: Vec<(&[u8], u32, u32)> = req
        .pages
        .iter()
        .map(|p| (p.luma.as_slice(), p.width, p.height))
        .collect();
    let password = req.password.as_deref().map(str::as_bytes);
    scan_decode(&pages, password).map_err(|e| format!("{e}"))
}

/// Try to detect v3 corner finders on a page. Returns true if
/// `detect_geometry` succeeds — i.e. three corner finders were
/// found AND the L-shape's dot distances round to a valid
/// `nx`/`ny` cell count. False positives on legacy v1/v2 PDFs
/// are very unlikely because the legacy codec doesn't render
/// QR-style corner finder patterns.
fn sniff_v3(page: &DecodePage) -> bool {
    use ampaper::v3::finder::detect_geometry;
    use ampaper::v3::page::PageBitmap;

    let bm = PageBitmap {
        pixels: page.luma.clone(),
        width: page.width,
        height: page.height,
    };
    detect_geometry(&bm).is_ok()
}

/// Decode v3 B&W pages with stage-by-stage diagnostic logging to
/// stderr. Captured by swaplog when the GUI is launched via the
/// VS Code build/rebuild task — gives the user visibility into
/// where decode succeeded or failed during a real print-and-scan
/// test, since the v3 codec doesn't have the per-cell colored
/// classification grid the legacy path emits.
///
/// Stages logged: page dimensions → finder geometry detection →
/// per-page cell-extraction count → CRC-validity breakdown
/// (anchors / data / bad-CRC) → first-anchor metadata → RaptorQ
/// convergence → decompression → final size match.
fn run_v3_decode(req: &DecodeRequest) -> Result<Vec<u8>, String> {
    use ampaper::v3::cell::{
        AnchorPayload, CELL_BYTES, Compression, DecodedCell, SYMBOL_BYTES, decode_cell,
    };
    use ampaper::v3::finder::detect_geometry;
    use ampaper::v3::page::{PageBitmap, PageGeometry, parse_page};
    use raptorq::{Decoder, EncodingPacket, ObjectTransmissionInformation};

    eprintln!("[v3 BW] decode start: {} page(s)", req.pages.len());
    for (i, p) in req.pages.iter().enumerate() {
        eprintln!(
            "[v3 BW]   page {}: bitmap {}x{}",
            i + 1,
            p.width,
            p.height
        );
    }
    if req.pages.is_empty() {
        return Err("v3 decode: no input pages".into());
    }

    let pages: Vec<PageBitmap> = req
        .pages
        .iter()
        .map(|p| PageBitmap {
            pixels: p.luma.clone(),
            width: p.width,
            height: p.height,
        })
        .collect();

    // Stage 1: detect geometry from first page's finders.
    log_luma_stats("v3 BW", &pages[0].pixels);
    save_diag_luma(
        "bw-luma.png",
        &pages[0].pixels,
        pages[0].width,
        pages[0].height,
    );
    let detected = match detect_geometry(&pages[0]) {
        Ok(g) => {
            eprintln!(
                "[v3 BW] detect_geometry: nx={} ny={} ppd={} (page_dots={}x{})",
                g.nx,
                g.ny,
                g.pixels_per_dot,
                g.nx * 32 + 16,
                g.ny * 32 + 16
            );
            for (label, hit) in [
                ("TL", &g.finders[0]),
                ("TR", &g.finders[1]),
                ("BL", &g.finders[2]),
            ] {
                eprintln!(
                    "[v3 BW]   {} center=({:.1},{:.1}) unit={:.3}",
                    label, hit.center_x, hit.center_y, hit.unit
                );
            }
            g
        }
        Err(e) => {
            eprintln!("[v3 BW] detect_geometry FAILED: {e}");
            return Err(format!("v3 decode: {e}"));
        }
    };
    let geom = PageGeometry {
        nx: detected.nx,
        ny: detected.ny,
        pixels_per_dot: detected.pixels_per_dot,
    };
    let cells_expected = geom.cells_per_page();

    // Stage 2: parse cells per page; classify each cell.
    let mut all_cells: Vec<[u8; CELL_BYTES]> = Vec::new();
    let mut anchor: Option<AnchorPayload> = None;
    let mut anchor_disagreement = false;
    let mut total_data_pkts = 0u32;

    for (page_idx, page) in pages.iter().enumerate() {
        match parse_page(page, &geom) {
            Ok(cells) => {
                let extracted = cells.len();
                let mut anchors = 0u32;
                let mut data = 0u32;
                let mut invalid = 0u32;
                for cell in &cells {
                    match decode_cell(cell) {
                        Ok(DecodedCell::Anchor(p)) => {
                            anchors += 1;
                            match anchor {
                                None => anchor = Some(p),
                                Some(prev) => {
                                    if prev.oti != p.oti
                                        || prev.file_size != p.file_size
                                        || prev.total_pages != p.total_pages
                                        || prev.compression != p.compression
                                    {
                                        anchor_disagreement = true;
                                    }
                                }
                            }
                        }
                        Ok(DecodedCell::Data { .. }) => data += 1,
                        Err(_) => invalid += 1,
                    }
                }
                eprintln!(
                    "[v3 BW] page {}/{} parse: {}/{} cells extracted — {} anchor + {} data + {} bad-CRC",
                    page_idx + 1,
                    pages.len(),
                    extracted,
                    cells_expected,
                    anchors,
                    data,
                    invalid
                );
                total_data_pkts += data;
                all_cells.extend(cells);
            }
            Err(e) => {
                eprintln!(
                    "[v3 BW] page {}/{} parse FAILED: {e}",
                    page_idx + 1,
                    pages.len()
                );
                return Err(format!("v3 decode: {e}"));
            }
        }
    }
    if anchor_disagreement {
        eprintln!("[v3 BW] anchor metadata disagreement across pages");
        return Err("v3 decode: pages from different encode runs (anchor mismatch)".into());
    }
    let anchor = match anchor {
        Some(a) => {
            eprintln!(
                "[v3 BW] anchor: file_size={} total_pages={} page_index={} compression={:?}",
                a.file_size, a.total_pages, a.page_index, a.compression
            );
            a
        }
        None => {
            eprintln!("[v3 BW] no valid anchor cell on any page");
            return Err("v3 decode: no valid anchor cell".into());
        }
    };

    // Stage 3: RaptorQ.
    let oti = ObjectTransmissionInformation::deserialize(&anchor.oti);
    let k_min = (anchor.file_size as usize).div_ceil(SYMBOL_BYTES);
    eprintln!(
        "[v3 BW] RaptorQ: feeding {} data packets, K_source≥{} (need K + small overhead to converge)",
        total_data_pkts, k_min
    );
    let mut decoder = Decoder::new(oti);
    let mut packet_buf = Vec::with_capacity(4 + SYMBOL_BYTES);
    let mut rq_recovered: Option<Vec<u8>> = None;
    let mut packets_fed = 0u32;
    for cell_bytes in &all_cells {
        let Ok(DecodedCell::Data { payload_id, symbol }) = decode_cell(cell_bytes) else {
            continue;
        };
        packet_buf.clear();
        packet_buf.extend_from_slice(&payload_id);
        packet_buf.extend_from_slice(symbol);
        let packet = EncodingPacket::deserialize(&packet_buf);
        packets_fed += 1;
        if let Some(out) = decoder.decode(packet) {
            rq_recovered = Some(out);
            eprintln!(
                "[v3 BW] RaptorQ converged after {} packets",
                packets_fed
            );
            break;
        }
    }
    let rq_recovered = match rq_recovered {
        Some(r) => r,
        None => {
            eprintln!(
                "[v3 BW] RaptorQ FAILED to converge after {} packets (insufficient or too damaged)",
                packets_fed
            );
            return Err(
                "v3 decode: RaptorQ did not converge — too few or too damaged cells".into(),
            );
        }
    };
    eprintln!("[v3 BW] RaptorQ output: {} bytes", rq_recovered.len());

    // Stage 4: decompress + size validation.
    let plaintext = match anchor.compression {
        Compression::None => {
            eprintln!("[v3 BW] decompression: none (raw)");
            rq_recovered
        }
        Compression::Zstd => match zstd::decode_all(rq_recovered.as_slice()) {
            Ok(out) => {
                eprintln!(
                    "[v3 BW] zstd: {} → {} bytes",
                    rq_recovered.len(),
                    out.len()
                );
                out
            }
            Err(e) => {
                eprintln!("[v3 BW] zstd FAILED: {e}");
                return Err(format!("v3 decode: zstd: {e}"));
            }
        },
    };
    if plaintext.len() as u64 != anchor.file_size {
        eprintln!(
            "[v3 BW] SIZE MISMATCH: got {} bytes, anchor said {}",
            plaintext.len(),
            anchor.file_size
        );
        return Err(format!(
            "v3 decode: decompressed size {} doesn't match anchor's claimed file_size {}",
            plaintext.len(),
            anchor.file_size
        ));
    }
    eprintln!(
        "[v3 BW] OK: recovered {} bytes (matches anchor)",
        plaintext.len()
    );
    Ok(plaintext)
}

/// Sample pixels across the page; report whether any have
/// asymmetric R/G/B beyond a small noise tolerance. PDFs produced
/// by the legacy v1/v2 codec or the v3 B&W codec render to
/// near-grayscale (R == G == B per pixel), while v3 CMY pages
/// have characteristic cyan/magenta/yellow ink colors. Sampling
/// every Nth pixel is enough — a v3 CMY page has thousands of
/// non-grayscale pixels, vs zero on a B&W rendered PDF.
fn has_color(page: &DecodePage) -> bool {
    if page.rgb.len() != (page.width as usize) * (page.height as usize) * 3 {
        return false;
    }
    let n = (page.width as usize) * (page.height as usize);
    if n == 0 {
        return false;
    }
    // Tolerance: ±16 across channels accounts for PDF rasteriser
    // dithering and JPEG-style compression that pdfium might do
    // on B&W images. Real CMY content has ink colors with
    // 200+-spread between max and min channels per pixel.
    let tol: i32 = 16;
    let stride = (n / 1000).max(1); // ~1000 sample points per page
    let mut color_count = 0u32;
    let mut checked = 0u32;
    let mut i = 0usize;
    while i < n {
        let r = page.rgb[i * 3] as i32;
        let g = page.rgb[i * 3 + 1] as i32;
        let b = page.rgb[i * 3 + 2] as i32;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        if max - min > tol {
            color_count += 1;
        }
        checked += 1;
        i += stride;
    }
    // ≥ 5% of sampled pixels show color → CMY input.
    color_count * 20 > checked
}

/// Decode v3 CMY pages with stage-by-stage diagnostic logging to
/// stderr. Same shape as [`run_v3_decode`] but with per-channel
/// (C / M / Y) breakdown so the user can see when one channel
/// fades catastrophically (the canonical yellow-on-aged-paper
/// failure mode) and the surviving channels still pool enough
/// packets for RaptorQ. Captured by swaplog.
fn run_v3_cmy_decode(req: &DecodeRequest) -> Result<Vec<u8>, String> {
    use ampaper::v3::cell::{
        AnchorPayload, CELL_BYTES, Compression, DecodedCell, SYMBOL_BYTES, decode_cell,
    };
    use ampaper::v3::cmyk::decompose_cmy;
    use ampaper::v3::finder::detect_geometry;
    use ampaper::v3::page::{PageBitmap, PageGeometry, parse_page};
    use ampaper::v3::RgbPageBitmap;
    use raptorq::{Decoder, EncodingPacket, ObjectTransmissionInformation};

    eprintln!("[v3 CMY] decode start: {} page(s)", req.pages.len());
    for (i, p) in req.pages.iter().enumerate() {
        eprintln!(
            "[v3 CMY]   page {}: bitmap {}x{}",
            i + 1,
            p.width,
            p.height
        );
    }
    if req.pages.is_empty() {
        return Err("v3 CMY decode: no input pages".into());
    }

    let pages: Vec<RgbPageBitmap> = req
        .pages
        .iter()
        .map(|p| RgbPageBitmap {
            pixels: p.rgb.clone(),
            width: p.width,
            height: p.height,
        })
        .collect();

    // Stage 1: luma view of page 0 + finder detection. The luma
    // composite carries the finder pattern as composite-black,
    // robust to per-channel ink fade.
    let luma = rgb_to_luma(&pages[0]);
    log_luma_stats("v3 CMY", &luma.pixels);
    save_diag_luma("cmy-luma.png", &luma.pixels, luma.width, luma.height);
    save_diag_rgb(
        "cmy-rgb.png",
        &pages[0].pixels,
        pages[0].width,
        pages[0].height,
    );
    let detected = match detect_geometry(&luma) {
        Ok(g) => {
            eprintln!(
                "[v3 CMY] luma detect_geometry: nx={} ny={} ppd={} (page_dots={}x{})",
                g.nx,
                g.ny,
                g.pixels_per_dot,
                g.nx * 32 + 16,
                g.ny * 32 + 16
            );
            for (label, hit) in [
                ("TL", &g.finders[0]),
                ("TR", &g.finders[1]),
                ("BL", &g.finders[2]),
            ] {
                eprintln!(
                    "[v3 CMY]   {} center=({:.1},{:.1}) unit={:.3}",
                    label, hit.center_x, hit.center_y, hit.unit
                );
            }
            g
        }
        Err(e) => {
            eprintln!("[v3 CMY] luma detect_geometry FAILED: {e}");
            return Err(format!("v3 CMY decode: {e}"));
        }
    };
    let geom = PageGeometry {
        nx: detected.nx,
        ny: detected.ny,
        pixels_per_dot: detected.pixels_per_dot,
    };
    let cells_expected = geom.cells_per_page();

    // Stage 2: per-page, per-channel parse + cell-validity counting.
    let mut all_cells: Vec<[u8; CELL_BYTES]> = Vec::new();
    let mut anchor: Option<AnchorPayload> = None;
    let mut anchor_disagreement = false;
    let mut total_data_pkts = 0u32;

    for (page_idx, rgb) in pages.iter().enumerate() {
        eprintln!(
            "[v3 CMY] page {}/{}: decompose RGB → C/M/Y layers",
            page_idx + 1,
            pages.len()
        );
        let (c_layer, m_layer, y_layer) = decompose_cmy(rgb);
        let layers: [(&str, &PageBitmap); 3] =
            [("C", &c_layer), ("M", &m_layer), ("Y", &y_layer)];
        let mut page_total_anchors = 0u32;
        let mut page_total_data = 0u32;
        let mut page_total_invalid = 0u32;
        let mut page_channels_ok = 0u32;
        for (label, layer) in layers {
            match parse_page(layer, &geom) {
                Ok(cells) => {
                    let extracted = cells.len();
                    let mut anchors = 0u32;
                    let mut data = 0u32;
                    let mut invalid = 0u32;
                    for cell in &cells {
                        match decode_cell(cell) {
                            Ok(DecodedCell::Anchor(p)) => {
                                anchors += 1;
                                match anchor {
                                    None => anchor = Some(p),
                                    Some(prev) => {
                                        if prev.oti != p.oti
                                            || prev.file_size != p.file_size
                                            || prev.total_pages != p.total_pages
                                            || prev.compression != p.compression
                                        {
                                            anchor_disagreement = true;
                                        }
                                    }
                                }
                            }
                            Ok(DecodedCell::Data { .. }) => data += 1,
                            Err(_) => invalid += 1,
                        }
                    }
                    eprintln!(
                        "[v3 CMY]   {} layer parse: {}/{} extracted — {} anchor + {} data + {} bad-CRC",
                        label, extracted, cells_expected, anchors, data, invalid
                    );
                    page_total_anchors += anchors;
                    page_total_data += data;
                    page_total_invalid += invalid;
                    page_channels_ok += 1;
                    all_cells.extend(cells);
                }
                Err(e) => {
                    // CMY decode tolerates per-channel parse failure
                    // (yellow ink fade is the canonical case). Surviving
                    // channels still contribute; RaptorQ recovers from
                    // K + small overhead.
                    eprintln!(
                        "[v3 CMY]   {} layer parse FAILED: {e} — channel skipped",
                        label
                    );
                }
            }
        }
        eprintln!(
            "[v3 CMY] page {} totals: {}/3 channels OK, {} anchors, {} data, {} bad-CRC",
            page_idx + 1,
            page_channels_ok,
            page_total_anchors,
            page_total_data,
            page_total_invalid
        );
        total_data_pkts += page_total_data;
    }
    if anchor_disagreement {
        eprintln!("[v3 CMY] anchor metadata disagreement across channels");
        return Err("v3 CMY decode: per-channel anchors disagree on file metadata".into());
    }
    let anchor = match anchor {
        Some(a) => {
            eprintln!(
                "[v3 CMY] anchor: file_size={} total_pages={} page_index={} compression={:?}",
                a.file_size, a.total_pages, a.page_index, a.compression
            );
            a
        }
        None => {
            eprintln!("[v3 CMY] no valid anchor cell on any color channel");
            return Err("v3 CMY decode: no valid anchor cell on any color channel".into());
        }
    };

    // Stage 3: pool data cells across all channels, feed RaptorQ.
    let oti = ObjectTransmissionInformation::deserialize(&anchor.oti);
    let k_min = (anchor.file_size as usize).div_ceil(SYMBOL_BYTES);
    eprintln!(
        "[v3 CMY] RaptorQ: pooling {} data packets across all channels, K_source≥{} (need K + small overhead to converge)",
        total_data_pkts, k_min
    );
    let mut decoder = Decoder::new(oti);
    let mut packet_buf = Vec::with_capacity(4 + SYMBOL_BYTES);
    let mut rq_recovered: Option<Vec<u8>> = None;
    let mut packets_fed = 0u32;
    for cell_bytes in &all_cells {
        let Ok(DecodedCell::Data { payload_id, symbol }) = decode_cell(cell_bytes) else {
            continue;
        };
        packet_buf.clear();
        packet_buf.extend_from_slice(&payload_id);
        packet_buf.extend_from_slice(symbol);
        let packet = EncodingPacket::deserialize(&packet_buf);
        packets_fed += 1;
        if let Some(out) = decoder.decode(packet) {
            rq_recovered = Some(out);
            eprintln!(
                "[v3 CMY] RaptorQ converged after {} packets",
                packets_fed
            );
            break;
        }
    }
    let rq_recovered = match rq_recovered {
        Some(r) => r,
        None => {
            eprintln!(
                "[v3 CMY] RaptorQ FAILED to converge after {} packets (insufficient or too damaged across all channels)",
                packets_fed
            );
            return Err(
                "v3 CMY decode: RaptorQ did not converge — too few or too damaged cells across all channels".into(),
            );
        }
    };
    eprintln!("[v3 CMY] RaptorQ output: {} bytes", rq_recovered.len());

    // Stage 4: decompress + size validation.
    let plaintext = match anchor.compression {
        Compression::None => {
            eprintln!("[v3 CMY] decompression: none (raw)");
            rq_recovered
        }
        Compression::Zstd => match zstd::decode_all(rq_recovered.as_slice()) {
            Ok(out) => {
                eprintln!(
                    "[v3 CMY] zstd: {} → {} bytes",
                    rq_recovered.len(),
                    out.len()
                );
                out
            }
            Err(e) => {
                eprintln!("[v3 CMY] zstd FAILED: {e}");
                return Err(format!("v3 CMY decode: zstd: {e}"));
            }
        },
    };
    if plaintext.len() as u64 != anchor.file_size {
        eprintln!(
            "[v3 CMY] SIZE MISMATCH: got {} bytes, anchor said {}",
            plaintext.len(),
            anchor.file_size
        );
        return Err(format!(
            "v3 CMY decode: decompressed size {} doesn't match anchor's claimed file_size {}",
            plaintext.len(),
            anchor.file_size
        ));
    }
    eprintln!(
        "[v3 CMY] OK: recovered {} bytes (matches anchor)",
        plaintext.len()
    );
    Ok(plaintext)
}

/// ITU-R BT.601 luma conversion. Same coefficients as the v3 lib's
/// internal `rgb_to_luma_bitmap`. Replicated here to avoid widening
/// the v3 lib's public API for one diagnostic helper. Used to
/// produce a luma view for `detect_geometry` in
/// [`run_v3_cmy_decode`] — finder patterns sit at composite-black
/// luma 0 regardless of per-channel ink fade.
fn rgb_to_luma(rgb: &ampaper::v3::RgbPageBitmap) -> ampaper::v3::PageBitmap {
    let n = (rgb.width as usize) * (rgb.height as usize);
    let mut pixels = vec![0u8; n];
    for (i, p) in pixels.iter_mut().enumerate() {
        let r = rgb.pixels[i * 3] as u32;
        let g = rgb.pixels[i * 3 + 1] as u32;
        let b = rgb.pixels[i * 3 + 2] as u32;
        *p = ((299 * r + 587 * g + 114 * b) / 1000).min(255) as u8;
    }
    ampaper::v3::PageBitmap {
        pixels,
        width: rgb.width,
        height: rgb.height,
    }
}

/// Where the v3 decode diagnostic dumps land. Co-located with the
/// gitignored `scratch/` directory so they don't accidentally get
/// committed. Path is relative to the GUI's working directory,
/// which under the VS Code build/rebuild task is the workspace
/// root. Emitting them as PNG keeps file sizes manageable while
/// preserving exact pixel values (no JPEG quantization).
const DIAG_DUMP_DIR: &str = "scratch/decode-debug";

/// Compute and log min / p10 / p50 / p90 / max of a luma bitmap,
/// plus the Otsu threshold the run-length detector will use. Tells
/// the user at a glance whether composite-black finder pixels
/// actually came through the scan as low luma — if min and p10 are
/// both near 0 the finders are intact and any "0 finders detected"
/// failure is downstream (rotation, cropping, etc.). If min sits
/// at luma 40+ the scan crushed contrast and even the darkest
/// pixels aren't recognizable as "black."
fn log_luma_stats(tag: &str, luma: &[u8]) {
    if luma.is_empty() {
        eprintln!("[{tag}] luma stats: empty bitmap");
        return;
    }
    let mut hist = [0u32; 256];
    for &p in luma {
        hist[p as usize] += 1;
    }
    let total = luma.len() as u64;
    let percentile = |q: f32| -> u8 {
        let target = ((total as f32) * q).round() as u64;
        let mut acc = 0u64;
        for (v, &c) in hist.iter().enumerate() {
            acc += c as u64;
            if acc >= target {
                return v as u8;
            }
        }
        255
    };
    let min = hist.iter().position(|&c| c > 0).unwrap_or(0) as u8;
    let max = (255 - hist.iter().rev().position(|&c| c > 0).unwrap_or(0)) as u8;
    let p10 = percentile(0.1);
    let p50 = percentile(0.5);
    let p90 = percentile(0.9);
    let otsu = ampaper::v3::threshold::otsu_threshold(luma);
    eprintln!(
        "[{tag}] luma stats: min={min} p10={p10} p50={p50} p90={p90} max={max}, Otsu threshold={otsu}"
    );
    if min > 30 {
        eprintln!(
            "[{tag}]   ⚠ darkest pixel is luma {min}; composite-black finders should hit luma 0-15. Scan likely had contrast crushed (document mode? brightness offset?)"
        );
    }
    if p90 < 200 {
        eprintln!(
            "[{tag}]   ⚠ p90 luma {p90} suggests background isn't reading as white; scan might be tinted or under-exposed"
        );
    }
}

/// Save a grayscale bitmap to `DIAG_DUMP_DIR/<filename>` for
/// inspection. Failures (missing dir, IO error) are logged but
/// don't fail the decode — diagnostic dumps are best-effort. The
/// directory is created if missing.
fn save_diag_luma(filename: &str, luma: &[u8], width: u32, height: u32) {
    let dir = std::path::PathBuf::from(DIAG_DUMP_DIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[diag] couldn't create {DIAG_DUMP_DIR}: {e}");
        return;
    }
    let path = dir.join(filename);
    match image::save_buffer(
        &path,
        luma,
        width,
        height,
        image::ExtendedColorType::L8,
    ) {
        Ok(()) => eprintln!("[diag] saved luma view → {}", path.display()),
        Err(e) => eprintln!("[diag] couldn't save {}: {e}", path.display()),
    }
}

/// Save an RGB bitmap (3 bytes/pixel) to `DIAG_DUMP_DIR/<filename>`.
fn save_diag_rgb(filename: &str, rgb: &[u8], width: u32, height: u32) {
    let dir = std::path::PathBuf::from(DIAG_DUMP_DIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[diag] couldn't create {DIAG_DUMP_DIR}: {e}");
        return;
    }
    let path = dir.join(filename);
    match image::save_buffer(
        &path,
        rgb,
        width,
        height,
        image::ExtendedColorType::Rgb8,
    ) {
        Ok(()) => eprintln!("[diag] saved RGB view → {}", path.display()),
        Err(e) => eprintln!("[diag] couldn't save {}: {e}", path.display()),
    }
}

/// Run scan_extract over a single page and bucket each resulting
/// cell into a [`CellStatus`]. Also walks the cells once more to
/// pull the original filename out of the first valid SuperBlock —
/// PB 1.10 stores it in `SuperBlock.name[0..32]` (FORMAT-V1.md §3.2),
/// v2 stores it in `V2SuperBlockCell1.name[0..64]` (FORMAT-V2.md
/// §2.1). Throwing this away would force the user to retype the
/// filename every time they save a recovered file.
///
/// Returns `None` when the geometry detector couldn't find the
/// PaperBack grid in the bitmap.
fn classify_page(page: &DecodePage) -> Option<PageReport> {
    let geometry: ScanGeometry = detect_geometry(&page.luma, page.width, page.height)?;
    let cells = scan_extract(&page.luma, page.width, page.height)?;
    debug_assert_eq!(cells.len(), (geometry.nposx * geometry.nposy) as usize);
    let statuses: Vec<CellStatus> = cells.iter().map(classify_cell).collect();
    let original_filename = extract_original_filename(&cells);
    Some(PageReport {
        source: page.source.clone(),
        nx: geometry.nposx,
        ny: geometry.nposy,
        cells: statuses,
        original_filename,
    })
}

/// Walk the page's cells looking for the first CRC-valid SuperBlock
/// (v1 or v2 cell 1) and return its embedded filename if any. Stops
/// at the first valid match. Filler-superblock copies sprinkled
/// across the trailing cells of a page all carry the same name, so
/// "first match" is correct.
fn extract_original_filename(cells: &[[u8; BLOCK_BYTES]]) -> Option<String> {
    for cell in cells {
        let block = Block::from_bytes(cell);
        if !block.verify_crc() {
            continue;
        }
        match block.addr {
            SUPERBLOCK_ADDR => {
                if let Ok(sb) = SuperBlock::from_bytes(cell)
                    && sb.verify_crc()
                {
                    // v1 reserves bytes 32..64 of `name` for the AES
                    // salt + IV (FORMAT-V1.md §3.2 / PAPERBAK-HACKS
                    // §2.1) — only bytes 0..32 are filename. Take a
                    // u8 slice up to the first NUL.
                    if let Some(name) = nul_terminated_utf8(&sb.name[..32]) {
                        return Some(name);
                    }
                }
            }
            V2_SUPERBLOCK_ADDR_CELL1 => {
                let parsed = V2SuperBlockCell1::from_data_bytes(&block.data);
                if let Some(name) = nul_terminated_utf8(&parsed.name) {
                    return Some(name);
                }
            }
            _ => {}
        }
    }
    None
}

/// Slice up to the first NUL, decoded as UTF-8. Returns None for an
/// empty / leading-NUL field. Lossy decode is fine — PB 1.10 wrote
/// Win32 ANSI bytes which are typically 7-bit ASCII for archival
/// filenames; v2 wrote real UTF-8.
fn nul_terminated_utf8(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    if end == 0 {
        return None;
    }
    let s = String::from_utf8_lossy(&bytes[..end]).into_owned();
    Some(s)
}

fn classify_cell(cell: &[u8; ampaper::block::BLOCK_BYTES]) -> CellStatus {
    let block = Block::from_bytes(cell);
    if !block.verify_crc() {
        // Before calling this Damaged, check if the underlying dot
        // pattern was actually empty paper — i.e., the cell sampled
        // to a uniform "no dots" / "all dots" pattern. After
        // scan_extract's per-row XOR descramble, a flat-white area
        // produces only the alternating-row scramble bytes
        // ({0x55, 0xAA}); a flat-black area produces the inverse.
        // Either way, ≤2 distinct byte values is a strong signal
        // the cell isn't real data — it's a margin, header strip,
        // or the blank space outside a small encode's data block
        // area. Mark Empty so the UI doesn't paint it red and make
        // the user think their scan is corrupted.
        if has_at_most_two_distinct_bytes(cell) {
            return CellStatus::Empty;
        }
        return CellStatus::Damaged;
    }
    // CRC verified — discriminate by addr.
    match block.addr {
        SUPERBLOCK_ADDR | V2_SUPERBLOCK_ADDR_CELL1 | V2_SUPERBLOCK_ADDR_CELL2 => {
            CellStatus::Super
        }
        addr => {
            // Data when ngroup == 0 and offset is in-range; recovery
            // when ngroup is 1..=15. Filler superblock copies past
            // the last group are `is_super`, already matched above.
            let ngroup = (addr >> 28) & 0x0F;
            let offset = addr & 0x0FFF_FFFF;
            if ngroup == 0 && offset < MAXSIZE {
                CellStatus::DataOk
            } else if ngroup != 0 {
                CellStatus::Recovery
            } else {
                // ngroup == 0 but offset >= MAXSIZE — treat as damaged
                // even though CRC verified, because the addr is bogus.
                CellStatus::Damaged
            }
        }
    }
}

/// True when the byte buffer contains at most two distinct values.
/// Used to discriminate "blank paper / margin" cells (which sample
/// to the alternating-row scramble pattern, i.e. {0x55, 0xAA}) from
/// "real data block whose CRC failed." A genuine 90-byte data block
/// with random or near-random content effectively never has only
/// two distinct values — the false-positive rate is on the order
/// of 2^-128 — so this is a safe heuristic.
fn has_at_most_two_distinct_bytes(buf: &[u8]) -> bool {
    let mut seen = [false; 256];
    let mut distinct = 0usize;
    for &b in buf {
        if !seen[b as usize] {
            seen[b as usize] = true;
            distinct += 1;
            if distinct > 2 {
                return false;
            }
        }
    }
    true
}

// ====================================================================
// PDF rasterization (Decode tab)
// ====================================================================
//
// Real-world scanner PDFs embed JPEG / JPEG2000 / CCITT-fax-encoded
// page images, sometimes with multiple image layers per page. To
// decode those we render via PDFium, the same engine Chrome uses —
// far more robust than any pure-Rust PDF renderer for arbitrary
// scanner output. PDFium itself is dynamically loaded at runtime via
// libloading; we ship the binary alongside ampaper-gui (or fall back
// to a system-installed copy).
//
// Render DPI: we default to 600. The user encoded at some printer
// DPI (typically 600); rendering the PDF at that DPI gives us a
// pixel grid tight enough that each ~3-pixel ampaper dot is well-
// resolved. Rendering at lower DPI (e.g., 200, the scanner's native)
// can collapse adjacent dots and break scan_decode's grid finder.

/// Default DPI for PDF page rasterization on the Decode path.
/// 300 DPI is the calibration sweet spot across the full range of
/// dot densities ampaper now produces (Safe ~30 dot/in, Normal
/// ~60-100, Compact ~100-200):
///   - Safe (30 dot/in)  → 300/30  = 10 px/dot ✓
///   - Normal (60-100)   → 300/60  =  5 px/dot, 300/100 = 3 px/dot ✓
///   - Compact (100-200) → 300/100 =  3 px/dot, 300/200 = 1.5
///   - PB-1.10 scans at 100 dot/in → 3 px/dot ✓
///
/// 200 dot/inch encodes at 300 DPI render (1.5 px/dot) sit on the
/// boundary; both pdf_round_trip and print_anyfile tests pass an
/// explicit 600 DPI override for that geometry. 600 DPI render
/// works for scanner / Normal / Compact but pushes Safe out of
/// scan_decode's range — bigger dots → fewer device pixels per
/// dot at fixed render DPI is the wrong relationship; what we
/// want is fewer pixels per dot at fixed RENDER, which means
/// LOWER render DPI for low-density encodes.
pub const DEFAULT_PDF_RENDER_DPI: u32 = 300;

/// PDF render DPI used after a v3 codec has been detected. 600
/// matches the Print tab's default `print_dpi`, so the v3 cell
/// sampler reads on integer pixel boundaries (ppd_eff =
/// encoder_ppd × render_dpi / print_dpi = 3 × 600/600 = 3 for the
/// GUI's default encode geometry). At the lower default of 300
/// DPI, ppd_eff falls to 1.5 and cell sampling becomes phase-
/// sensitive — a half-dot of image-to-bitmap offset shifts
/// sampled pixels enough to corrupt cells, and dense pages run
/// out of RaptorQ recovery margin.
pub const V3_PDF_RENDER_DPI: u32 = 600;

/// Try to bind to a Pdfium library, looking next to the running
/// executable first then falling back to system paths. Returns
/// `Result` instead of panicking the way `Pdfium::default()` does
/// — the worker surfaces this as an error message in the status bar
/// rather than crashing the GUI.
///
/// pdfium-render's bindings are process-global. After a successful
/// first call, subsequent `bind_to_library` calls return
/// `PdfiumLibraryBindingsAlreadyInitialized`; we treat that as
/// success and produce a fresh `Pdfium {}` zero-state handle that
/// reuses the existing global bindings. This keeps the function
/// stateless — no OnceLock or mutex needed — and lets a sibling
/// module (`crate::print`) keep its own copy without coordination.
pub(crate) fn bind_pdfium() -> Result<Pdfium, PdfiumError> {
    fn already_init(e: &PdfiumError) -> bool {
        matches!(e, PdfiumError::PdfiumLibraryBindingsAlreadyInitialized)
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    if let Some(dir) = exe_dir {
        let candidate = Pdfium::pdfium_platform_library_name_at_path(&dir);
        match Pdfium::bind_to_library(&candidate) {
            Ok(b) => return Ok(Pdfium::new(b)),
            Err(e) if already_init(&e) => return Ok(Pdfium {}),
            Err(_) => {}
        }
    }
    let cwd_candidate = Pdfium::pdfium_platform_library_name_at_path("./");
    match Pdfium::bind_to_library(&cwd_candidate) {
        Ok(b) => return Ok(Pdfium::new(b)),
        Err(e) if already_init(&e) => return Ok(Pdfium {}),
        Err(_) => {}
    }
    match Pdfium::bind_to_system_library() {
        Ok(b) => Ok(Pdfium::new(b)),
        Err(e) if already_init(&e) => Ok(Pdfium {}),
        Err(e) => Err(e),
    }
}

/// True when `bytes` start with the `%PDF-` magic that every PDF
/// 1.x / 2.x file begins with. Quick check before we spin up
/// pdfium, which is heavy and can fail in non-PDF-specific ways.
pub fn looks_like_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF-")
}

/// Re-render any PDF-sourced [`DecodePage`]s at a different DPI,
/// preserving the order. Image-sourced pages pass through
/// unchanged (they don't have a re-renderable source — they
/// already arrived at the user's chosen scan resolution).
///
/// Errors when at least one PDF source fails to re-render OR the
/// re-rendered page count doesn't match the original group from
/// that source. Both signal a state corruption (file changed on
/// disk, pdfium binding issue, etc.) that should surface to the
/// user rather than silently mix bitmaps from different DPIs.
fn rerender_pdf_pages_at(
    pages: &[DecodePage],
    dpi: u32,
) -> Result<Vec<DecodePage>, String> {
    if pages.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<DecodePage> = Vec::with_capacity(pages.len());
    let mut i = 0;
    while i < pages.len() {
        let source = pages[i].source.clone();
        let mut j = i + 1;
        while j < pages.len() && pages[j].source == source {
            j += 1;
        }
        let group_len = j - i;
        let mut sniff = [0u8; 8];
        let n = std::fs::File::open(&source)
            .and_then(|mut f| {
                use std::io::Read;
                f.read(&mut sniff)
            })
            .unwrap_or(0);
        let is_pdf = n > 0 && looks_like_pdf(&sniff[..n]);
        if is_pdf {
            let rendered = render_pdf_pages(&source, dpi)?;
            if rendered.len() != group_len {
                return Err(format!(
                    "re-render at {dpi} DPI of {} produced {} page(s); expected {}",
                    source.display(),
                    rendered.len(),
                    group_len
                ));
            }
            out.extend(rendered);
        } else {
            out.extend_from_slice(&pages[i..j]);
        }
        i = j;
    }
    Ok(out)
}

/// Render every page of `pdf_path` to an 8-bit grayscale bitmap at
/// `dpi` and return them as [`DecodePage`]s ready to feed into
/// [`DecodeJob::spawn`].
///
/// `Pdfium::default()` looks for the pdfium dynamic library next to
/// the running executable first, then in standard system paths.
/// When neither succeeds (e.g., the user hasn't placed pdfium.dll
/// alongside ampaper-gui.exe yet), we surface a clear error rather
/// than crashing.
pub fn render_pdf_pages(pdf_path: &std::path::Path, dpi: u32) -> Result<Vec<DecodePage>, String> {
    if dpi == 0 {
        return Err("DPI must be > 0".into());
    }
    let pdfium = bind_pdfium().map_err(|e| {
        // Distribution builds ship pdfium next to the .exe via
        // gui/build.rs. This branch only fires when the vendored
        // binary went missing or the build script didn't run (e.g.,
        // someone is running a stripped-down deployment that
        // dropped the DLL). Surface enough detail to recover.
        format!(
            "PDFium library not found ({e}). The expected pdfium.dll / \
             libpdfium.so / libpdfium.dylib should ship next to the \
             ampaper-gui binary; if it's missing, restore it from \
             gui/vendor/pdfium/<target>/ or rebuild from a clean checkout."
        )
    })?;
    let document = pdfium
        .load_pdf_from_file(pdf_path, None)
        .map_err(|e| format!("PDFium failed to open {}: {e}", pdf_path.display()))?;

    let mut out = Vec::new();
    for page in document.pages().iter() {
        // Render at the requested DPI: PDF page size is in points
        // (1/72 inch), so width_px = page_width_in_inches * dpi.
        let width_in = page.width().value / 72.0;
        let height_in = page.height().value / 72.0;
        let target_w = (width_in * dpi as f32).round().max(1.0) as i32;
        let target_h = (height_in * dpi as f32).round().max(1.0) as i32;

        // Disable PDFium's default anti-aliasing. We're rendering a
        // high-contrast dot pattern; smoothing blends adjacent dots
        // into 50%-grey pixels that scan_decode can't classify
        // cleanly (RS recovers single-byte errors but not whole-
        // block fuzz). Pixel-perfect black/white is what the codec
        // designed for, and the scanner-output use case prefers
        // sharp source over screen-readable smoothing too.
        let config = PdfRenderConfig::new()
            .set_target_width(target_w)
            .set_target_height(target_h)
            .set_image_smoothing(false)
            .set_path_smoothing(false)
            .set_text_smoothing(false);
        let bitmap = page
            .render_with_config(&config)
            .map_err(|e| format!("PDFium failed to render page: {e}"))?;
        let dyn_image = bitmap
            .as_image()
            .map_err(|e| format!("PDFium bitmap → image conversion failed: {e}"))?;
        let luma = dyn_image.to_luma8();
        let rgb = dyn_image.to_rgb8();
        let (w, h) = luma.dimensions();
        out.push(DecodePage {
            source: pdf_path.to_path_buf(),
            luma: luma.into_raw(),
            rgb: rgb.into_raw(),
            width: w,
            height: h,
        });
    }
    if out.is_empty() {
        return Err(format!("PDF has no pages: {}", pdf_path.display()));
    }
    Ok(out)
}
