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

use ampaper::block::{Block, MAXSIZE, SUPERBLOCK_ADDR};
use ampaper::encoder::{
    EncodeError, EncodeOptions, EncodedPage, FileMeta, encode, encode_v2,
};
use ampaper::format_v2::{V2_SUPERBLOCK_ADDR_CELL1, V2_SUPERBLOCK_ADDR_CELL2};
use ampaper::scan::{ScanGeometry, detect_geometry, scan_decode, scan_extract};

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
/// cell into a [`CellStatus`]. Returns `None` when the geometry
/// detector couldn't find the PaperBack grid in the bitmap.
fn classify_page(page: &DecodePage) -> Option<PageReport> {
    let geometry: ScanGeometry = detect_geometry(&page.luma, page.width, page.height)?;
    let cells = scan_extract(&page.luma, page.width, page.height)?;
    debug_assert_eq!(cells.len(), (geometry.nposx * geometry.nposy) as usize);
    let statuses = cells.iter().map(classify_cell).collect();
    Some(PageReport {
        source: page.source.clone(),
        nx: geometry.nposx,
        ny: geometry.nposy,
        cells: statuses,
    })
}

fn classify_cell(cell: &[u8; ampaper::block::BLOCK_BYTES]) -> CellStatus {
    let block = Block::from_bytes(cell);
    if !block.verify_crc() {
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
