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

use ampaper::encoder::{
    EncodeError, EncodeOptions, EncodedPage, FileMeta, encode, encode_v2,
};

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
