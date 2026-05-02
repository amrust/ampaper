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
    pub luma: Vec<u8>,
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

    // First pass: classify each page's cells. We do this even if
    // the eventual decode fails — the user still wants to see WHY
    // (e.g., almost everything red = the scan is too noisy / the
    // grid wasn't recovered).
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
/// 300 DPI is the sweet spot for typical scanner-produced PDFs:
///   - PB 1.10 default dot density is 100 dot/inch; a 300-DPI
///     render gives 3 device pixels per dot, exactly what
///     scan_decode's grid finder is calibrated for.
///   - Real scanner output is usually 200–300 DPI native; rendering
///     at the same DPI matches the captured pixels without
///     up-sampling artefacts.
///   - For ampaper-produced PDFs encoded at 100 dot/inch (the new
///     default matching PB 1.10), this also gives 3 pixels per dot
///     after the inches → mm → inches PDF page-size roundtrip.
///
/// PDFs whose encode used a denser dot grid (200 dot/inch from
/// older ampaper builds, for example) would want a higher render
/// DPI; tests that exercise that path pass an explicit DPI rather
/// than relying on the default.
pub const DEFAULT_PDF_RENDER_DPI: u32 = 300;

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
        let (w, h) = luma.dimensions();
        out.push(DecodePage {
            source: pdf_path.to_path_buf(),
            luma: luma.into_raw(),
            width: w,
            height: h,
        });
    }
    if out.is_empty() {
        return Err(format!("PDF has no pages: {}", pdf_path.display()));
    }
    Ok(out)
}
