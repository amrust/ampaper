// v3 codec round-trip tests — bytes-level slice (M12 phase 1).
//
// These exercise the public `ampaper::v3::{encode, decode}` API end
// to end. Where the existing v1/v2 tests (golden_decode.rs et al.)
// pin binary compatibility with PB 1.10 / mrpods output, these tests
// pin v3's design promises:
//   - Round-trip is byte-exact for arbitrary inputs.
//   - The blob has the documented header (magic + version + OTI).
//   - The blob is self-describing: callers don't need to share
//     state with the encoder beyond the bytes.
//   - The rateless ECC actually loses some packets and still
//     recovers — that's the whole reason RaptorQ was chosen over
//     v1's RS+XOR.

use ampaper::v3::{DecodeError, EncodeOptions, decode, encode};

#[test]
fn round_trips_short_payload() {
    let plaintext = b"the quick brown fox jumps over the lazy dog";
    let blob = encode(plaintext, &EncodeOptions::default()).unwrap();
    let recovered = decode(&blob).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn round_trips_one_kilobyte() {
    // Deterministic but non-trivially-patterned bytes.
    let plaintext: Vec<u8> = (0u32..1024).map(|i| (i.wrapping_mul(7).wrapping_add(13) & 0xFF) as u8).collect();
    let blob = encode(&plaintext, &EncodeOptions::default()).unwrap();
    let recovered = decode(&blob).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn round_trips_one_megabyte() {
    // 1 MB pseudo-random — exercises RaptorQ's multi-source-block
    // codepath (RFC 6330 §4.3 splits large objects into sub-blocks
    // when transfer length exceeds a per-symbol-size threshold).
    let mut plaintext = Vec::with_capacity(1_048_576);
    let mut x: u32 = 0xDEAD_BEEF;
    for _ in 0..1_048_576 {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
        plaintext.push((x >> 16) as u8);
    }
    let blob = encode(&plaintext, &EncodeOptions::default()).unwrap();
    let recovered = decode(&blob).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn empty_input_rejected_at_encode() {
    let err = encode(b"", &EncodeOptions::default()).unwrap_err();
    assert!(matches!(err, ampaper::v3::EncodeError::EmptyInput));
}

#[test]
fn header_layout_matches_spec() {
    let blob = encode(b"hi", &EncodeOptions::default()).unwrap();

    // Magic
    assert_eq!(&blob[0..8], b"AMPAPER3");
    // Version
    assert_eq!(blob[8], 1);
    // Reserved (must be zero in version 1)
    assert_eq!(&blob[9..12], &[0u8, 0, 0]);
    // OTI: 12 bytes; we don't pin its contents (raptorq picks them
    // based on input size + MTU), only that the slice exists.
    assert_eq!(blob[12..24].len(), 12);
    // Body must follow the 24-byte header.
    assert!(blob.len() > 24);
}

#[test]
fn decode_rejects_legacy_v1_v2_blobs() {
    // Anything that doesn't start with `b"AMPAPER3"` should fail
    // with BadMagic, not partial decode or panic. This is the
    // gate that keeps the dispatcher safe — v1/v2 bytes never
    // accidentally interpret as v3 input.
    for payload in [
        b"PaperBack 1.10 BMP".as_slice(),     // legacy text
        b"\x00\x00\x00\x00\x00\x00\x00\x00",  // all zeros
        b"AMPAPER2!".as_slice(),              // close but not exact
        &[0xFFu8; 128],                       // anything else
    ] {
        let err = decode(payload).unwrap_err();
        match err {
            DecodeError::BadMagic | DecodeError::TooShort { .. } => {}
            other => panic!("expected BadMagic/TooShort, got {other:?} for {payload:?}"),
        }
    }
}

#[test]
fn rateless_ecc_recovers_when_some_packets_are_dropped() {
    // The whole reason RaptorQ replaced PB-1.10's RS+XOR group
    // structure: any K + small overhead packets recover the file,
    // regardless of WHICH packets survive. This test simulates
    // patchy paper damage by dropping every Nth packet from the
    // encoded stream and confirms decode still succeeds.

    let plaintext: Vec<u8> = (0u32..4096).map(|i| (i.wrapping_mul(31).wrapping_add(7) & 0xFF) as u8).collect();
    let opts = EncodeOptions {
        mtu: 256,
        // Generous repair budget — well above what's needed for
        // the loss profile below. The cell-layer phase will tune
        // this against measured scanner loss.
        repair_packets: 30,
    };
    let blob = encode(&plaintext, &opts).unwrap();

    // Header is fixed-size 24 bytes. After it, packets are
    // contiguous fixed-stride. Symbol size at MTU=256 is 256 (no
    // alignment stretching for this size); per-packet wire size
    // = 4 (payload ID) + 256 = 260.
    const HEADER: usize = 24;
    const PACKET: usize = 260;
    let body = &blob[HEADER..];
    assert_eq!(body.len() % PACKET, 0, "test setup: packet stride mismatch");
    let n_packets = body.len() / PACKET;
    assert!(n_packets >= 16, "expected enough packets to drop several");

    // Drop every third packet — ~33% loss, well within RaptorQ's
    // tolerance with 30 repair packets in a stream of ~50.
    let mut damaged = blob[..HEADER].to_vec();
    for i in 0..n_packets {
        if i % 3 != 2 {
            damaged.extend_from_slice(&body[i * PACKET..(i + 1) * PACKET]);
        }
    }

    let recovered = decode(&damaged).unwrap();
    assert_eq!(recovered, plaintext, "RaptorQ should recover from 33% packet loss");
}

#[test]
fn corrupted_blob_fails_cleanly() {
    let plaintext = b"some bytes to encode";
    let blob = encode(plaintext, &EncodeOptions::default()).unwrap();

    // Truncate the blob mid-packet. Should fail with
    // PacketStreamMisaligned, not panic and not silently return
    // partial data.
    let truncated = &blob[..blob.len() - 7];
    let err = decode(truncated).unwrap_err();
    assert!(
        matches!(err, DecodeError::PacketStreamMisaligned { .. }),
        "expected PacketStreamMisaligned, got {err:?}"
    );
}
