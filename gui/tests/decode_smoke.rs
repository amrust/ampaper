// Worker.rs is shared into this test binary via `#[path = ...]`,
// which makes its public-API surface look "unused" because the GUI
// consumer code (views/decode.rs etc.) isn't compiled into this
// crate. The fields ARE read in the production binary; silence the
// noise to keep the workspace clippy-clean.
#![allow(dead_code)]

// Smoke test for the decode-side worker pipeline. Mirrors
// encode_smoke.rs but inverted: encode a payload synthetically, save
// the resulting BMP to disk, then drive `DecodeJob::spawn` over that
// BMP and assert (a) the worker recovers the original bytes and
// (b) the per-cell classification grid is sensible (mostly non-
// damaged).
//
// This is the cross-check for what the user sees when they drag a
// .bmp into the GUI: the same code path runs here, just without the
// egui rendering step.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ampaper::block::NGROUP_DEFAULT;
use ampaper::encoder::{EncodeOptions, FileMeta, encode, encode_v2_with_kat};
use ampaper::page::{BLACK_PAPER, PageGeometry};

#[path = "../src/worker.rs"]
mod worker;
use worker::{
    CellStatus, DecodeJob, DecodeMessage, DecodePage, DecodeRequest, PageReport,
};

fn scan_geometry() -> PageGeometry {
    // Same scan_geometry the lib's scan tests use — wide enough that
    // detect_geometry locks on robustly.
    PageGeometry {
        ppix: 600,
        ppiy: 600,
        dpi: 200,
        dot_percent: 70,
        // 16x21 cells with the full sync raster.
        width: 16 * 35 * 3 + 2 * 35 * 3,
        height: 21 * 35 * 3 + 2 * 35 * 3,
        print_border: true,
    }
}

fn meta() -> FileMeta<'static> {
    FileMeta {
        name: "smoke.bin",
        modified: 0,
        attributes: 0x80,
    }
}

fn write_bmp(path: &std::path::Path, bitmap: &[u8], w: u32, h: u32) {
    use image::ImageEncoder;
    let mut buf = std::fs::File::create(path).unwrap();
    image::codecs::bmp::BmpEncoder::new(&mut buf)
        .write_image(bitmap, w, h, image::ExtendedColorType::L8)
        .unwrap();
}

fn run(req: DecodeRequest) -> Result<(Vec<u8>, Vec<PageReport>), String> {
    let job = DecodeJob::spawn(req, || {});
    let mut reports = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if Instant::now() > deadline {
            return Err("worker did not finish within 30s".into());
        }
        match job.rx.recv_timeout(Duration::from_millis(50)) {
            Ok(DecodeMessage::PageClassified(r)) => reports.push(r),
            Ok(DecodeMessage::Done { plaintext }) => return Ok((plaintext, reports)),
            Ok(DecodeMessage::Failed(e)) => return Err(e),
            Ok(DecodeMessage::Started) => eprintln!("[worker] started"),
            Ok(DecodeMessage::Status(s)) => eprintln!("[worker] {s}"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("worker channel disconnected before Done".into());
            }
        }
    }
}

fn percent_damaged(report: &PageReport) -> f32 {
    if report.cells.is_empty() {
        return 100.0;
    }
    let damaged = report
        .cells
        .iter()
        .filter(|s| **s == CellStatus::Damaged)
        .count();
    100.0 * damaged as f32 / report.cells.len() as f32
}

#[test]
fn worker_v1_decode_round_trips_clean_bitmap_and_recovers_filename() {
    let tmp = std::env::temp_dir().join("ampaper-gui-decode-test-v1");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let payload: Vec<u8> = (0..500u32).map(|i| (i * 31) as u8).collect();
    let opts = EncodeOptions {
        geometry: scan_geometry(),
        redundancy: NGROUP_DEFAULT,
        compress: false,
        black: BLACK_PAPER,
    };
    let pages = encode(&payload, &opts, &meta()).unwrap();
    let bmp_path: PathBuf = tmp.join("page-001.bmp");
    write_bmp(&bmp_path, &pages[0].bitmap, pages[0].width, pages[0].height);

    let img = image::open(&bmp_path).unwrap().to_luma8();
    let (w, h) = img.dimensions();
    let req = DecodeRequest {
        pages: vec![DecodePage {
            source: bmp_path.clone(),
            luma: img.into_raw(),
            width: w,
            height: h,
        }],
        password: None,
    };
    let (recovered, reports) = run(req).expect("v1 decode should succeed");
    assert_eq!(recovered, payload);

    // Classification grid must be present + mostly clean.
    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    assert!(report.nx > 0 && report.ny > 0, "geometry should be detected");
    let damaged = percent_damaged(report);
    assert!(
        damaged <= 1.0,
        "synthetic clean bitmap should have ≤1% damaged cells, got {damaged:.1}%"
    );

    // Filename recovery: meta() in this test uses "smoke.bin", which
    // PaperBack 1.10's 31-char cap fits comfortably. The worker
    // should pull this out of the SuperBlock so the GUI's "Save
    // as..." defaults to "smoke.bin" instead of the bitmap stem.
    assert_eq!(
        report.original_filename.as_deref(),
        Some("smoke.bin"),
        "v1 SuperBlock filename should round-trip through worker classification"
    );
}

#[test]
fn worker_v2_decode_round_trips_with_password() {
    let tmp = std::env::temp_dir().join("ampaper-gui-decode-test-v2");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let payload = b"v2 decode worker round trip".to_vec();
    let opts = EncodeOptions {
        geometry: scan_geometry(),
        redundancy: NGROUP_DEFAULT,
        compress: false,
        black: BLACK_PAPER,
    };
    let salt = [0x77u8; 32];
    let iv = [0x88u8; 12];
    let pages =
        encode_v2_with_kat(&payload, &opts, &meta(), b"correct horse", &salt, &iv).unwrap();
    let bmp_path: PathBuf = tmp.join("page-001.bmp");
    write_bmp(&bmp_path, &pages[0].bitmap, pages[0].width, pages[0].height);

    let img = image::open(&bmp_path).unwrap().to_luma8();
    let (w, h) = img.dimensions();
    let req = DecodeRequest {
        pages: vec![DecodePage {
            source: bmp_path,
            luma: img.into_raw(),
            width: w,
            height: h,
        }],
        password: Some("correct horse".into()),
    };
    let (recovered, reports) = run(req).expect("v2 decode should succeed");
    assert_eq!(recovered, payload);
    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    let damaged = percent_damaged(report);
    assert!(
        damaged <= 1.0,
        "v2 clean bitmap should have ≤1% damaged cells, got {damaged:.1}%"
    );
    assert_eq!(
        report.original_filename.as_deref(),
        Some("smoke.bin"),
        "v2 cell 1 filename should round-trip through worker classification"
    );
}

/// Cross-check against the real PaperBack 1.10 golden vector. The
/// lorem.bmp file was produced by PB 1.10 from lorem.input; the
/// worker should recover the filename `lorem.input` from the
/// SuperBlock (NOT default to `lorem.recovered.bin`). Mirrors the
/// concrete bug the user reported: dragging a real PB 1.10 BMP in
/// must default the save dialog to the original filename.
#[test]
fn worker_recovers_filename_from_real_paperbak_1_10_bmp() {
    let bmp_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("tests")
        .join("golden")
        .join("v1-paperbak")
        .join("lorem.bmp");
    if !bmp_path.exists() {
        eprintln!("skipping golden test — {} not present", bmp_path.display());
        return;
    }

    let img = image::open(&bmp_path).unwrap().to_luma8();
    let (w, h) = img.dimensions();
    let req = DecodeRequest {
        pages: vec![DecodePage {
            source: bmp_path,
            luma: img.into_raw(),
            width: w,
            height: h,
        }],
        password: None,
    };
    let (recovered, reports) = run(req).expect("PB 1.10 golden BMP must decode");

    // The recovered bytes match the committed lorem.input file.
    let expected = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("tests")
            .join("golden")
            .join("v1-paperbak")
            .join("lorem.input"),
    )
    .unwrap();
    assert_eq!(recovered, expected);

    // The original filename must be present and equal to the file
    // that was fed into PB 1.10 — `lorem.input`. This is the
    // user-facing fix: "Save as..." defaults to the right name.
    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0].original_filename.as_deref(),
        Some("lorem.input"),
        "real PaperBack 1.10 BMP must round-trip the original filename"
    );
}

#[test]
fn worker_v2_decode_with_wrong_password_fails_cleanly() {
    let tmp = std::env::temp_dir().join("ampaper-gui-decode-test-v2-wrong");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let opts = EncodeOptions {
        geometry: scan_geometry(),
        redundancy: NGROUP_DEFAULT,
        compress: false,
        black: BLACK_PAPER,
    };
    let salt = [0x55u8; 32];
    let iv = [0x66u8; 12];
    let pages =
        encode_v2_with_kat(b"hi", &opts, &meta(), b"good password", &salt, &iv).unwrap();
    let bmp_path = tmp.join("page-001.bmp");
    write_bmp(&bmp_path, &pages[0].bitmap, pages[0].width, pages[0].height);

    let img = image::open(&bmp_path).unwrap().to_luma8();
    let (w, h) = img.dimensions();
    let req = DecodeRequest {
        pages: vec![DecodePage {
            source: bmp_path,
            luma: img.into_raw(),
            width: w,
            height: h,
        }],
        password: Some("wrong password".into()),
    };
    let err = run(req).expect_err("decode with wrong password must fail");
    assert!(
        err.to_lowercase().contains("password") || err.to_lowercase().contains("tag"),
        "error should mention password or tag verification, got: {err}"
    );
}
