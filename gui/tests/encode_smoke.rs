// Smoke test for the worker pipeline. Constructs an `EncodeRequest`
// programmatically and drives it through `EncodeJob::spawn`, then
// verifies BMPs land on disk and decode back to the input bytes.
//
// This is a thin integration test, not a UI test — egui clicks are
// hard to script. The real validation is "does the worker thread
// pipeline produce correct files end-to-end."

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ampaper::block::NGROUP_DEFAULT;
use ampaper::encoder::EncodeOptions;
use ampaper::page::{BLACK_PAPER, PageGeometry};

#[path = "../src/worker.rs"]
mod worker;
use worker::{EncodeJob, EncodeMessage, EncodeRequest};

fn small_geometry() -> PageGeometry {
    PageGeometry {
        ppix: 600,
        ppiy: 600,
        dpi: 200,
        dot_percent: 70,
        // 12x6 cell page: matches src/encoder.rs's small_geometry().
        width: 12 * 35 * 3 + 2,
        height: 6 * 35 * 3 + 2,
        print_border: false,
    }
}

fn run(req: EncodeRequest) -> Result<Vec<PathBuf>, String> {
    let job = EncodeJob::spawn(req, || {});
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if Instant::now() > deadline {
            return Err("worker did not finish within 10s".into());
        }
        match job.rx.recv_timeout(Duration::from_millis(50)) {
            Ok(EncodeMessage::Done { files }) => return Ok(files),
            Ok(EncodeMessage::Failed(e)) => return Err(e),
            Ok(EncodeMessage::Started) => eprintln!("[worker] started"),
            Ok(EncodeMessage::Status(s)) => eprintln!("[worker] {s}"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("worker channel disconnected before Done".into());
            }
        }
    }
}

#[test]
fn worker_v1_encode_writes_bmp_per_page_and_round_trips() {
    let tmp = std::env::temp_dir().join("ampaper-gui-encode-test-v1");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let input_path = tmp.join("input.bin");
    let payload: Vec<u8> = (0..500u32).map(|i| (i * 31) as u8).collect();
    std::fs::write(&input_path, &payload).unwrap();

    let req = EncodeRequest {
        input_path: input_path.clone(),
        output_dir: tmp.clone(),
        output_stem: "out".into(),
        options: EncodeOptions {
            geometry: small_geometry(),
            redundancy: NGROUP_DEFAULT,
            compress: false,
            black: BLACK_PAPER,
        },
        v2_password: None,
    };
    let files = run(req).expect("v1 encode should succeed");
    assert_eq!(files.len(), 1, "500 bytes fits in one page at this geometry");
    let path = &files[0];
    assert!(path.exists(), "BMP file should exist on disk");
    assert!(
        path.file_name().unwrap().to_string_lossy().ends_with(".bmp"),
        "output should be a .bmp file"
    );

    // Round-trip: decode the BMP we just wrote and assert it
    // recovers the input bytes. This is the M6 cross-check, applied
    // to the GUI worker's actual on-disk output.
    let bmp = image::open(path).unwrap().to_luma8();
    let bitmap = bmp.into_raw();
    let opts = ampaper::decoder::DecodeOptions {
        geometry: small_geometry(),
        threshold: ampaper::page::DEFAULT_THRESHOLD,
    };
    let recovered = ampaper::decoder::decode(&[bitmap], &opts, None).unwrap();
    assert_eq!(recovered, payload);
}

#[test]
fn worker_v2_encode_writes_bmp_per_page_and_round_trips_with_password() {
    let tmp = std::env::temp_dir().join("ampaper-gui-encode-test-v2");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let input_path = tmp.join("input.bin");
    let payload = b"v2 worker round trip".to_vec();
    std::fs::write(&input_path, &payload).unwrap();

    let req = EncodeRequest {
        input_path: input_path.clone(),
        output_dir: tmp.clone(),
        output_stem: "out".into(),
        options: EncodeOptions {
            geometry: small_geometry(),
            redundancy: NGROUP_DEFAULT,
            compress: false,
            black: BLACK_PAPER,
        },
        v2_password: Some("correct horse".into()),
    };
    let files = run(req).expect("v2 encode should succeed");
    assert!(!files.is_empty());

    let bmp = image::open(&files[0]).unwrap().to_luma8();
    let bitmap = bmp.into_raw();
    let opts = ampaper::decoder::DecodeOptions {
        geometry: small_geometry(),
        threshold: ampaper::page::DEFAULT_THRESHOLD,
    };
    let recovered =
        ampaper::decoder::decode(&[bitmap], &opts, Some(b"correct horse")).unwrap();
    assert_eq!(recovered, payload);
}
