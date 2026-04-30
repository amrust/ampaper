// bzip2 wrapper for the v1 PaperBack pipeline.
//
// PaperBack 1.10 invokes libbzip2 with block size 1 ("fast") or 9
// ("max") depending on the user's compression option (see
// `Printer.cpp:334-335` and FORMAT-V1.md §6.1). The output stream is
// standard bzip2; nothing about how PaperBack uses the format is
// non-standard, so any conformant bzip2 implementation can decode it.
//
// We wrap the `bzip2` crate (which can sit on either pure-Rust
// `libbz2-rs-sys` or FFI `bzip2-sys`). Decoder bytes are bit-identical
// across backends because the bzip2 stream format is fully specified.
// Encoder bytes may differ between backends due to compressor heuristics
// (block selection, run-length-encoding of repeats), so the cross-check
// against the `bzip2` CLI is via decompress-roundtrip rather than
// byte-for-byte parity on compressed output.

use std::io::{Read, Write};

/// bzip2 block size, picked at compression time. Decompression
/// reads it from the stream header and does not need this value.
///
/// PaperBack 1.10's `Printer.cpp:334-335` selects 1 when the user
/// asks for "fast" and 9 (the default) when they ask for "max".
/// There is no level-2..8 path on the encoder side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockSize {
    /// 100 KB internal block size — fastest, lowest compression ratio.
    /// Maps to bzip2 level 1. PaperBack UI label: "fast".
    Fast = 1,
    /// 900 KB internal block size — slowest, highest compression ratio.
    /// Maps to bzip2 level 9. PaperBack UI label: "max" (the default).
    Max = 9,
}

impl BlockSize {
    fn level(self) -> u32 {
        self as u32
    }
}

/// Compress `data` into a self-contained bzip2 stream.
///
/// In-memory compression of a finite slice is infallible at the
/// `std::io` layer (the underlying `Vec<u8>` writer cannot fail), so
/// this function returns the compressed bytes directly.
#[must_use]
pub fn compress(data: &[u8], block_size: BlockSize) -> Vec<u8> {
    let mut encoder = bzip2::write::BzEncoder::new(
        Vec::with_capacity(data.len()),
        bzip2::Compression::new(block_size.level()),
    );
    encoder
        .write_all(data)
        .expect("BzEncoder write to Vec<u8> cannot fail");
    encoder
        .finish()
        .expect("BzEncoder finalize on Vec<u8> cannot fail")
}

/// Decompress a bzip2 stream back to the original bytes. Returns
/// `std::io::Error` for malformed streams (truncated, bad CRC,
/// unrecognized header) — the underlying bzip2 errors are surfaced
/// as `io::ErrorKind::InvalidData` by the bzip2 crate.
pub fn decompress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut decoder = bzip2::read::BzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty input round-trips cleanly. bzip2's "empty" stream is
    /// 14 bytes (header + EOF marker), which still must decompress
    /// to zero bytes.
    #[test]
    fn round_trip_empty() {
        for level in [BlockSize::Fast, BlockSize::Max] {
            let compressed = compress(b"", level);
            assert!(!compressed.is_empty(), "bzip2 always emits a header");
            assert_eq!(
                decompress(&compressed).unwrap(),
                Vec::<u8>::new(),
                "level {level:?} did not round-trip empty input"
            );
        }
    }

    /// Short input round-trips at both block sizes. The compressed
    /// output is larger than the input here (overhead) — that's fine,
    /// the test is correctness, not ratio.
    #[test]
    fn round_trip_short_payload() {
        let input = b"PaperBack archives bytes onto paper as redundant dot grids.\n";
        for level in [BlockSize::Fast, BlockSize::Max] {
            let compressed = compress(input, level);
            let recovered = decompress(&compressed).unwrap();
            assert_eq!(&recovered[..], input);
        }
    }

    /// Random-ish input across a few sizes round-trips. Pseudo-random
    /// because compressible input is the easy case; an LCG output
    /// approximates worst-case (uncompressible) bytes.
    #[test]
    fn round_trip_lcg_payloads_various_sizes() {
        for size in [1usize, 16, 1024, 64 * 1024] {
            let input: Vec<u8> = (0..size)
                .map(|i| (i as u32).wrapping_mul(1103515245).wrapping_add(12345) as u8)
                .collect();
            for level in [BlockSize::Fast, BlockSize::Max] {
                let compressed = compress(&input, level);
                let recovered = decompress(&compressed).unwrap();
                assert_eq!(recovered, input, "size {size} level {level:?}");
            }
        }
    }

    /// Decompress a stream captured from `bzip2 -9` v1.0.8 (mingw64
    /// build) on the input below. Cross-checks our decoder against
    /// the canonical bzip2 CLI's output for a known plaintext.
    ///
    /// To regenerate:
    ///   printf 'PaperBack archives bytes onto paper as redundant dot grids.\n' \
    ///     | bzip2 -9c | xxd -i
    #[test]
    fn decompresses_bzip2_cli_level_9_output() {
        let cli_output: [u8; 92] = [
            0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x51, 0xcb, 0x45, 0x75,
            0x00, 0x00, 0x05, 0xd7, 0x80, 0x00, 0x10, 0x40, 0x01, 0x10, 0x00, 0x40, 0x00, 0x3e,
            0xe9, 0xdf, 0x20, 0x20, 0x00, 0x54, 0x50, 0xd0, 0x03, 0x4d, 0x34, 0x62, 0x0d, 0x53,
            0xc5, 0x33, 0x50, 0xf4, 0x23, 0xc5, 0x3d, 0x41, 0xe5, 0xe2, 0x41, 0x41, 0x04, 0xbf,
            0x19, 0x96, 0x6d, 0xaf, 0x89, 0x49, 0x7b, 0x28, 0xad, 0x5e, 0x2d, 0xe9, 0xd3, 0x3a,
            0xae, 0x8b, 0x70, 0xc0, 0x8e, 0x90, 0x04, 0xb7, 0x56, 0x88, 0x13, 0x7c, 0x5d, 0xc9,
            0x14, 0xe1, 0x42, 0x41, 0x47, 0x2d, 0x15, 0xd4,
        ];
        let recovered = decompress(&cli_output).unwrap();
        assert_eq!(
            recovered,
            b"PaperBack archives bytes onto paper as redundant dot grids.\n"
        );
    }

    /// Same input, but the `-1` (fast) level. Differs from the level-9
    /// output only in byte 3 of the header (b'1' vs b'9'); both must
    /// decompress to the same plaintext.
    #[test]
    fn decompresses_bzip2_cli_level_1_output() {
        let cli_output: [u8; 92] = [
            0x42, 0x5a, 0x68, 0x31, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x51, 0xcb, 0x45, 0x75,
            0x00, 0x00, 0x05, 0xd7, 0x80, 0x00, 0x10, 0x40, 0x01, 0x10, 0x00, 0x40, 0x00, 0x3e,
            0xe9, 0xdf, 0x20, 0x20, 0x00, 0x54, 0x50, 0xd0, 0x03, 0x4d, 0x34, 0x62, 0x0d, 0x53,
            0xc5, 0x33, 0x50, 0xf4, 0x23, 0xc5, 0x3d, 0x41, 0xe5, 0xe2, 0x41, 0x41, 0x04, 0xbf,
            0x19, 0x96, 0x6d, 0xaf, 0x89, 0x49, 0x7b, 0x28, 0xad, 0x5e, 0x2d, 0xe9, 0xd3, 0x3a,
            0xae, 0x8b, 0x70, 0xc0, 0x8e, 0x90, 0x04, 0xb7, 0x56, 0x88, 0x13, 0x7c, 0x5d, 0xc9,
            0x14, 0xe1, 0x42, 0x41, 0x47, 0x2d, 0x15, 0xd4,
        ];
        let recovered = decompress(&cli_output).unwrap();
        assert_eq!(
            recovered,
            b"PaperBack archives bytes onto paper as redundant dot grids.\n"
        );
    }

    /// Header byte 3 of any bzip2 stream encodes the block size as
    /// ASCII digit '1'..'9'. Pin that our compress sets the right one
    /// for each [`BlockSize`] variant — catches accidentally swapping
    /// the enum-to-level mapping.
    #[test]
    fn compressed_stream_header_records_block_size() {
        let max = compress(b"x", BlockSize::Max);
        assert_eq!(&max[..4], b"BZh9", "Max must produce 'BZh9' header");

        let fast = compress(b"x", BlockSize::Fast);
        assert_eq!(&fast[..4], b"BZh1", "Fast must produce 'BZh1' header");
    }

    /// Garbage input must surface as Err, not panic or hang. The
    /// decoder is the place where untrusted bytes (a corrupted scan)
    /// enter the pipeline; this is the bare minimum the boundary
    /// must guarantee.
    #[test]
    fn decompress_rejects_garbage() {
        // Random bytes that aren't a valid bzip2 stream.
        let garbage = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let result = decompress(&garbage);
        assert!(result.is_err(), "decompress accepted non-bzip2 bytes");
    }

    /// Truncated stream (valid header, missing tail) must error.
    #[test]
    fn decompress_rejects_truncated_stream() {
        let full = compress(
            b"some payload long enough to span past the header",
            BlockSize::Max,
        );
        // Cut off the last 4 bytes — the trailing CRC. bzip2 must notice.
        let truncated = &full[..full.len() - 4];
        let result = decompress(truncated);
        assert!(result.is_err(), "decompress accepted truncated stream");
    }
}
