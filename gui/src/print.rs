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

/// One page's worth of bitmap to print.
#[derive(Clone)]
pub struct PrintPage {
    /// 8-bit grayscale, row-major, length = `width * height`.
    pub bitmap: Vec<u8>,
    pub width: u32,
    pub height: u32,
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

/// PaperBack 1.10's drop-any-file UX: each input is either a
/// pre-rendered bitmap (BMP/PNG/JPG — pass straight through), a PDF
/// (already-rendered output OR scanner output — keep as-is for
/// printing), or anything else (raw file → encode through the
/// codec using the supplied settings).
///
/// Returns the bitmaps in the order the user dropped them; raw
/// inputs that span multiple pages produce multiple bitmaps in
/// place. The caller hands the result to [`print_pages`] or
/// [`save_pages_as_pdf`] without caring which path each came from.
pub fn prepare_print_pages(
    paths: &[impl AsRef<Path>],
    encode_options: &ampaper::encoder::EncodeOptions,
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
            InputKind::Pdf => {
                // PDF on the Print tab is treated as "this is
                // already a printable doc" — pass it through to
                // print_pages or copy verbatim for save-as-pdf.
                // For now we render it back to bitmaps so the same
                // PrintPage flow handles all cases. A future
                // optimisation could short-circuit "PDF in → PDF
                // out" without re-rendering, but that's pure
                // efficiency.
                let pages = render_pdf_via_pdfium(p)?;
                out.extend(pages);
            }
            InputKind::Raw => {
                // Encode this file through the codec.
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
                let encoded = match v2_password {
                    Some(pw) if !pw.is_empty() => {
                        ampaper::encoder::encode_v2(
                            &bytes,
                            encode_options,
                            &meta,
                            pw.as_bytes(),
                        )
                        .map_err(|e| PrintError::Io {
                            path: p.display().to_string(),
                            message: format!("encode_v2: {e}"),
                        })?
                    }
                    _ => ampaper::encoder::encode(&bytes, encode_options, &meta)
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

/// What the first few bytes of a file say about its format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputKind {
    Image,
    Pdf,
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
    if head.starts_with(b"%PDF-") {
        return Ok(InputKind::Pdf);
    }
    // BMP "BM", PNG \x89PNG, JPEG \xFF\xD8\xFF.
    if head.starts_with(b"BM")
        || head.starts_with(b"\x89PNG")
        || head.starts_with(&[0xFFu8, 0xD8, 0xFF])
    {
        return Ok(InputKind::Image);
    }
    Ok(InputKind::Raw)
}

/// Render every page of a PDF into [`PrintPage`]s via pdfium. Lives
/// here (not just in worker.rs) so the Print tab can pass
/// already-PDF inputs straight through without touching Decode-tab
/// internals. Mirrors the worker.rs implementation; we tolerate the
/// duplication because keeping print.rs's API surface
/// self-contained is worth more than DRY.
fn render_pdf_via_pdfium(path: &Path) -> Result<Vec<PrintPage>, PrintError> {
    use pdfium_render::prelude::PdfRenderConfig;
    const RENDER_DPI: u32 = 1200;

    let pdfium = bind_pdfium_local().map_err(|e| PrintError::Io {
        path: path.display().to_string(),
        message: format!(
            "PDFium library not found ({e}). The expected pdfium.dll / \
             libpdfium.so / libpdfium.dylib should ship next to the \
             ampaper-gui binary; if it's missing, restore it from \
             gui/vendor/pdfium/<target>/ or rebuild from a clean checkout."
        ),
    })?;
    let document = pdfium.load_pdf_from_file(path, None).map_err(|e| PrintError::Io {
        path: path.display().to_string(),
        message: format!("PDFium failed to open: {e}"),
    })?;
    let mut out = Vec::new();
    for page in document.pages().iter() {
        let width_in = page.width().value / 72.0;
        let height_in = page.height().value / 72.0;
        let target_w = (width_in * RENDER_DPI as f32).round().max(1.0) as i32;
        let target_h = (height_in * RENDER_DPI as f32).round().max(1.0) as i32;
        let config = PdfRenderConfig::new()
            .set_target_width(target_w)
            .set_target_height(target_h)
            .set_image_smoothing(false)
            .set_path_smoothing(false)
            .set_text_smoothing(false);
        let bitmap = page.render_with_config(&config).map_err(|e| PrintError::Io {
            path: path.display().to_string(),
            message: format!("PDFium render: {e}"),
        })?;
        let dyn_image = bitmap.as_image().map_err(|e| PrintError::Io {
            path: path.display().to_string(),
            message: format!("PDFium bitmap → image: {e}"),
        })?;
        let luma = dyn_image.to_luma8();
        let (w, h) = luma.dimensions();
        out.push(PrintPage {
            bitmap: luma.into_raw(),
            width: w,
            height: h,
        });
    }
    Ok(out)
}

/// Stateless pdfium binder — handles `PdfiumLibraryBindingsAlreadyInitialized`
/// as a success case (the global bindings already exist; we hand
/// back a fresh zero-state `Pdfium {}` that reuses them). Same
/// shape as `crate::worker::bind_pdfium`. We keep a local copy here
/// so this file compiles cleanly when `#[path]`-included into a
/// test or example crate that doesn't have a sibling worker module.
fn bind_pdfium_local() -> Result<pdfium_render::prelude::Pdfium, pdfium_render::prelude::PdfiumError>
{
    use pdfium_render::prelude::{Pdfium, PdfiumError};
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
    let cwd = Pdfium::pdfium_platform_library_name_at_path("./");
    match Pdfium::bind_to_library(&cwd) {
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
    doc_name: &str,
    path: &Path,
) -> Result<(), PrintError> {
    use printpdf::{
        ImageCompression, ImageOptimizationOptions, Mm, Op, PdfDocument, PdfPage,
        PdfSaveOptions, RawImage, RawImageData, RawImageFormat, XObjectTransform,
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
    let mut pdf_pages = Vec::with_capacity(pages.len());

    for page in pages {
        // Embed the raw 8-bit grayscale bytes. printpdf has an R8
        // (single-channel u8) format, exactly what our codec emits.
        let raw = RawImage {
            width: page.width as usize,
            height: page.height as usize,
            data_format: RawImageFormat::R8,
            pixels: RawImageData::U8(page.bitmap.clone()),
            tag: Vec::new(),
        };
        let image_id = doc.add_image(&raw);

        // Place the image at the origin with the right DPI transform.
        // printpdf's XObjectTransform.dpi tells the layer "this image
        // is meant to print at this many pixels per inch" — combined
        // with the page size below, that fixes pixel pitch on paper.
        let ops = vec![Op::UseXobject {
            id: image_id,
            transform: XObjectTransform {
                dpi: Some(dpi as f32),
                ..Default::default()
            },
        }];

        // PDF page size = bitmap inches, converted to Mm.
        let width_mm = (page.width as f32 / dpi as f32) * 25.4;
        let height_mm = (page.height as f32 / dpi as f32) * 25.4;
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
