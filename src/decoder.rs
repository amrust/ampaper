// Bitmap-to-file decoder pipeline. Per FORMAT-V1.md §6 (decode side)
// and `Fileproc.cpp:182-376` / `Decoder.cpp:817-905`. This module
// covers the **synthetic** decoder path: callers hand in a list of
// rendered page bitmaps with a known [`PageGeometry`], and the
// decoder extracts blocks via [`crate::page::extract`], filters by
// CRC, drives the data buffer reassembly using the SuperBlock for
// metadata, applies XOR-checksum recovery for any one-block-missing
// group, then optionally runs bzip2 decompression.
//
// The full **scan** decoder (histogram peak grid registration,
// rotation tolerance, sharpening, multi-orientation search) is the
// follow-on M6 piece — that one consumes scanner-fed bitmaps where
// the geometry isn't known a priori. It will reuse the SuperBlock /
// recovery / decompression logic in this module.
//
// Sub-bullet status closing relative to MILESTONES.md:
//   * "Decode our own M5 encoder output; assert byte-identity" — done
//     here via tests that compose encoder + decoder.
//   * "Decode every M1 golden vector; assert SHA-256 match" — blocked
//     on M1 step 3 (capturing real PaperBack 1.10 prints).
//   * "decode(scanned_png) -> Result<Vec<u8>>" — the synthetic API
//     here is the lower half; the scan-style entry point lands when
//     the histogram-peak code is written.

use crate::block::{
    Block, NDATA, NGROUP_MAX, NGROUP_MIN, PBM_COMPRESSED, PBM_ENCRYPTED, SuperBlock,
};
use crate::bz;
use crate::page::{self, PageGeometry};

/// Decoder configuration. Geometry must match what the encoder used
/// to render each input page; reading a bitmap with the wrong dx /
/// dy / cell-size assumptions samples in the wrong places and gets
/// garbage. The scan-style decoder (M6 follow-on) infers geometry
/// from histogram peaks instead.
#[derive(Clone, Copy, Debug)]
pub struct DecodeOptions {
    /// Page geometry used at encode time.
    pub geometry: PageGeometry,
    /// Black/white threshold passed to [`page::extract`]. Defaults
    /// to [`page::DEFAULT_THRESHOLD`].
    pub threshold: u8,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            geometry: PageGeometry {
                ppix: 600,
                ppiy: 600,
                dpi: 200,
                dot_percent: 70,
                width: 4800,
                height: 6600,
                print_border: false,
            },
            threshold: page::DEFAULT_THRESHOLD,
        }
    }
}

/// What can go wrong during a decode.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// No SuperBlock survived CRC verification across all input
    /// pages — the decoder has no metadata to drive reassembly.
    NoSuperBlock,
    /// The SuperBlock asserts encrypted data, which this decoder
    /// path doesn't handle. Encrypted v1 reads land at M7.
    EncryptionNotSupported,
    /// One or more groups have ≥ 2 missing blocks; XOR-checksum
    /// recovery can only fix exactly one missing block per group.
    /// Carries the byte-offset of the first unrecoverable region.
    UnrecoverableGap { offset: u32 },
    /// SuperBlock fields disagree across pages (different files
    /// printed and scanned together?). The decoder picks the first
    /// SuperBlock and surfaces this error if any later page's
    /// SuperBlock contradicts it on the load-bearing fields.
    InconsistentSuperBlocks,
    /// bzip2 decompression failed on the recovered buffer. Usually
    /// means a block went missing in a way recovery couldn't fix
    /// and the resulting buffer is corrupt.
    BzipFailed(String),
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSuperBlock => f.write_str("no SuperBlock decoded successfully on any page"),
            Self::EncryptionNotSupported => {
                f.write_str("SuperBlock asserts PBM_ENCRYPTED; legacy AES-192 read lands at M7")
            }
            Self::UnrecoverableGap { offset } => write!(
                f,
                "≥2 blocks missing in the group containing byte offset {offset}; \
                 v1 redundancy is 1-of-N and cannot recover this gap"
            ),
            Self::InconsistentSuperBlocks => f.write_str(
                "SuperBlocks across pages disagree on file metadata; \
                 inputs likely come from different prints",
            ),
            Self::BzipFailed(msg) => write!(f, "bzip2 decompression failed: {msg}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decode a list of page bitmaps (in any order, no duplicates needed
/// — pages are identified by their SuperBlock's `page` field) back
/// into the original input bytes.
///
/// Pipeline:
///   1. Extract every cell from every bitmap.
///   2. CRC-filter the cells; reject anything whose `Block::verify_crc`
///      fails. Distinguish data, recovery, and SuperBlock cells.
///   3. Pick a SuperBlock (the first that verifies); it tells us
///      `datasize`, `origsize`, mode, and (when v2 lands) crypto
///      material.
///   4. Allocate a buffer of `datasize` bytes; fill in data block
///      bytes by their `addr` offsets.
///   5. For each group with exactly 1 missing data block, XOR the
///      recovery block with the surviving data blocks of the group
///      and invert the running 0xFF — recovers the missing data.
///   6. If `mode & PBM_COMPRESSED`, bzip2-decompress.
///   7. Truncate to `origsize` and return.
pub fn decode(pages: &[Vec<u8>], options: &DecodeOptions) -> Result<Vec<u8>, DecodeError> {
    let geometry = &options.geometry;
    let threshold = options.threshold;

    // --- Steps 1-2: extract + CRC-filter cells ---------------------
    let mut superblock: Option<SuperBlock> = None;
    let mut data_blocks: std::collections::BTreeMap<u32, [u8; NDATA]> = Default::default();
    let mut recovery_blocks: Vec<(u32, u8, [u8; NDATA])> = Vec::new(); // (offset, ngroup, data)
    let mut any_encrypted = false;
    let mut metadata_inconsistency = false;

    for bitmap in pages {
        let cells = page::extract(geometry, bitmap, threshold);
        for cell in cells {
            let block = Block::from_bytes(&cell);
            if !block.verify_crc() {
                continue;
            }
            if block.is_super() {
                let parsed = match SuperBlock::from_bytes(&cell) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if !parsed.verify_crc() {
                    continue;
                }
                if parsed.mode & PBM_ENCRYPTED != 0 {
                    any_encrypted = true;
                }
                if let Some(existing) = superblock {
                    // Pages must agree on file metadata. Compare the
                    // load-bearing fields; ignore `page` since that
                    // varies between pages.
                    if existing.datasize != parsed.datasize
                        || existing.origsize != parsed.origsize
                        || existing.mode != parsed.mode
                        || existing.filecrc != parsed.filecrc
                        || existing.name != parsed.name
                    {
                        metadata_inconsistency = true;
                    }
                } else {
                    superblock = Some(parsed);
                }
            } else if block.is_data() {
                data_blocks.entry(block.offset()).or_insert(block.data);
            } else if block.is_recovery() {
                let ngroup = block.ngroup();
                if (NGROUP_MIN..=NGROUP_MAX).contains(&ngroup) {
                    recovery_blocks.push((block.offset(), ngroup, block.data));
                }
            }
        }
    }

    // Encryption is checked first because it's a security-relevant
    // signal — even a single encrypted SuperBlock copy means the
    // user's data is in an encryption envelope this decoder can't
    // handle, and falling through to "looks unencrypted" would risk
    // emitting ciphertext as plaintext.
    if any_encrypted {
        return Err(DecodeError::EncryptionNotSupported);
    }
    if metadata_inconsistency {
        return Err(DecodeError::InconsistentSuperBlocks);
    }
    let superblock = superblock.ok_or(DecodeError::NoSuperBlock)?;

    // --- Step 4: assemble buffer from data blocks ------------------
    let datasize = superblock.datasize;
    let mut buf = vec![0u8; datasize as usize];
    let mut filled = vec![false; (datasize.div_ceil(NDATA as u32)) as usize];
    for (&offset, data) in &data_blocks {
        if offset >= datasize {
            // Filler block past end of useful data — encoder pads
            // partial groups with these (Printer.cpp:889-895). Ignore.
            continue;
        }
        let off = offset as usize;
        let copy_len = (NDATA).min(buf.len() - off);
        buf[off..off + copy_len].copy_from_slice(&data[..copy_len]);
        let block_index = off / NDATA;
        if block_index < filled.len() {
            filled[block_index] = true;
        }
    }

    // --- Step 5: XOR-checksum recovery for missing blocks ----------
    // Mirrors Fileproc.cpp:213-230. For each recovery block, walk
    // its group: if exactly one data block is missing, the recovery
    // block's data XOR-inverted gives back the missing block.
    for (recovery_offset, ngroup, recovery_data) in &recovery_blocks {
        let ngroup = *ngroup as u32;
        let group_size = ngroup as usize;
        let group_start = *recovery_offset as usize;
        let group_first_block = group_start / NDATA;
        if group_first_block + group_size > filled.len() {
            continue; // group extends past the data buffer
        }

        // Count missing blocks in the group, remember the first.
        let mut missing_count = 0usize;
        let mut missing_idx = 0usize;
        for k in 0..group_size {
            let bi = group_first_block + k;
            if !filled[bi] {
                missing_count += 1;
                missing_idx = bi;
            }
        }
        if missing_count != 1 {
            // 0 missing: no work needed. ≥2 missing: cannot recover
            // from a single XOR-checksum block; defer the error to
            // the gap-detection pass below.
            continue;
        }

        // Reconstruct the missing block by XOR-inverting the running
        // 0xFF with the surviving data blocks. Equivalent (since XOR
        // is associative) to recovery ^ XOR(all_present_blocks):
        //   recovery_data = 0xFF ^ d0 ^ ... ^ d_{n-1}
        //   d_missing = recovery_data ^ 0xFF ^ d_0 ^ ... ^ d_{n-1, k != missing}
        let mut recovered = *recovery_data;
        for r in &mut recovered {
            *r ^= 0xFF;
        }
        for k in 0..group_size {
            let bi = group_first_block + k;
            if bi == missing_idx {
                continue;
            }
            let off = bi * NDATA;
            // Clamp to buf.len() — the last block of a non-aligned
            // group can extend past datasize, where the encoder
            // zero-padded. Bytes past buf.len() are implicit zeros
            // for XOR purposes, so leaving `recovered`'s tail bytes
            // unmodified there is correct.
            let end = (off + NDATA).min(buf.len());
            if end <= off {
                continue;
            }
            for (r, &b) in recovered.iter_mut().zip(buf[off..end].iter()) {
                *r ^= b;
            }
        }

        let off = missing_idx * NDATA;
        let copy_len = (NDATA).min(buf.len().saturating_sub(off));
        buf[off..off + copy_len].copy_from_slice(&recovered[..copy_len]);
        filled[missing_idx] = true;
    }

    // --- Gap detection: any unfilled block past recovery is fatal --
    // Filler blocks past the buffer end were never expected (their
    // offsets land outside `filled`'s range). For blocks inside
    // `filled`, every entry must be true after recovery.
    for (i, &ok) in filled.iter().enumerate() {
        if !ok {
            return Err(DecodeError::UnrecoverableGap {
                offset: (i * NDATA) as u32,
            });
        }
    }

    // --- Step 6-7: optional bzip2 + truncate -----------------------
    if superblock.mode & PBM_COMPRESSED != 0 {
        buf = bz::decompress(&buf).map_err(|e| DecodeError::BzipFailed(e.to_string()))?;
    }
    buf.truncate(superblock.origsize as usize);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{EncodeOptions, FileMeta, encode};

    fn small_geometry() -> PageGeometry {
        PageGeometry {
            ppix: 600,
            ppiy: 600,
            dpi: 200,
            dot_percent: 70,
            width: 12 * 35 * 3 + 2,
            height: 6 * 35 * 3 + 2,
            print_border: false,
        }
    }

    fn meta() -> FileMeta<'static> {
        FileMeta {
            name: "test.bin",
            modified: 0,
            attributes: 0x80,
        }
    }

    fn encode_decode_options(geometry: PageGeometry) -> (EncodeOptions, DecodeOptions) {
        let enc = EncodeOptions {
            geometry,
            redundancy: 5,
            compress: false,
            black: page::BLACK_PAPER,
        };
        let dec = DecodeOptions {
            geometry,
            threshold: page::DEFAULT_THRESHOLD,
        };
        (enc, dec)
    }

    fn pages_to_bitmaps(pages: &[crate::encoder::EncodedPage]) -> Vec<Vec<u8>> {
        pages.iter().map(|p| p.bitmap.clone()).collect()
    }

    #[test]
    fn round_trip_single_page() {
        let geometry = small_geometry();
        let (enc_opts, dec_opts) = encode_decode_options(geometry);
        let payload: Vec<u8> = (0..500u32).map(|i| (i * 31) as u8).collect();
        let pages = encode(&payload, &enc_opts, &meta()).unwrap();
        let bitmaps = pages_to_bitmaps(&pages);
        let recovered = decode(&bitmaps, &dec_opts).unwrap();
        assert_eq!(recovered, payload);
    }

    #[test]
    fn round_trip_multi_page() {
        let geometry = small_geometry();
        let (enc_opts, dec_opts) = encode_decode_options(geometry);
        let payload: Vec<u8> = (0..10_000u32).map(|i| (i * 7) as u8).collect();
        let pages = encode(&payload, &enc_opts, &meta()).unwrap();
        assert!(pages.len() >= 3, "10000 bytes should require multi-page");
        let bitmaps = pages_to_bitmaps(&pages);
        let recovered = decode(&bitmaps, &dec_opts).unwrap();
        assert_eq!(recovered, payload);
    }

    #[test]
    fn round_trip_compressed() {
        let geometry = small_geometry();
        let mut enc_opts = encode_decode_options(geometry).0;
        enc_opts.compress = true;
        let dec_opts = encode_decode_options(geometry).1;
        let mut payload = Vec::new();
        for _ in 0..100 {
            payload.extend_from_slice(b"PaperBack archives bytes onto paper. ");
        }
        let pages = encode(&payload, &enc_opts, &meta()).unwrap();
        let bitmaps = pages_to_bitmaps(&pages);
        let recovered = decode(&bitmaps, &dec_opts).unwrap();
        assert_eq!(recovered, payload);
    }

    /// Decoding bitmaps that were encoded with one redundancy and
    /// decoded with the matching geometry recovers the input bytes
    /// regardless of the redundancy choice. Tests both ends of the
    /// valid range.
    #[test]
    fn round_trip_redundancy_min_and_max() {
        let geometry = small_geometry();
        let payload: Vec<u8> = (0..400u32).map(|i| i as u8).collect();
        for r in [NGROUP_MIN, NGROUP_MAX] {
            let enc = EncodeOptions {
                geometry,
                redundancy: r,
                compress: false,
                black: page::BLACK_PAPER,
            };
            let dec = DecodeOptions {
                geometry,
                threshold: page::DEFAULT_THRESHOLD,
            };
            let pages = encode(&payload, &enc, &meta()).unwrap();
            let bitmaps = pages_to_bitmaps(&pages);
            let recovered = decode(&bitmaps, &dec).unwrap();
            assert_eq!(recovered, payload, "redundancy = {r}");
        }
    }

    /// Recovery test: paint over the dot region of one data block in
    /// the rendered bitmap with white pixels (simulating a print
    /// defect or smudge that destroys exactly one block). The
    /// decoder must still recover the original payload via the
    /// group's XOR-checksum recovery block.
    #[test]
    fn recovers_one_missing_block_per_group_via_xor_checksum() {
        let geometry = small_geometry();
        let (enc_opts, dec_opts) = encode_decode_options(geometry);
        let payload: Vec<u8> = (0..448u32).map(|i| i as u8).collect();
        let pages = encode(&payload, &enc_opts, &meta()).unwrap();
        let mut bitmap = pages[0].bitmap.clone();

        // Paint cell 1 (the first data block of string 0) entirely
        // white. cell_index 1 is at column 1, row 0 of a 12-cell-wide
        // page; its origin is column 1 * 35 * 3 + 2 * 3 = 111 pixels.
        let dx = geometry.dx() as usize;
        let dy = geometry.dy() as usize;
        let cell_w = page::CELL_SIZE_DOTS * dx;
        let cell_h = page::CELL_SIZE_DOTS * dy;
        let bitmap_width = geometry.bitmap_width() as usize;
        let (x0, y0) = geometry.block_origin_pixels(1);
        for y in y0 as usize..(y0 as usize + cell_h) {
            for x in x0 as usize..(x0 as usize + cell_w) {
                if y < geometry.bitmap_height() as usize && x < bitmap_width {
                    bitmap[y * bitmap_width + x] = page::WHITE;
                }
            }
        }

        let recovered = decode(&[bitmap], &dec_opts).unwrap();
        assert_eq!(
            recovered, payload,
            "decoder failed to recover from one missing data block"
        );
    }

    /// Painting over TWO data blocks in the same group must produce
    /// UnrecoverableGap (the v1 format's 1-of-N posture).
    ///
    /// Compact-regime layout for redundancy=5, nstring=1 places data
    /// blocks at odd-indexed cells: 1, 3, 5, 7, 9 (with SuperBlock
    /// copies at even indices and the recovery block at cell 11).
    /// Killing cells 1 AND 3 takes out two data blocks of group 0.
    #[test]
    fn rejects_two_missing_blocks_in_same_group() {
        let geometry = small_geometry();
        let (enc_opts, dec_opts) = encode_decode_options(geometry);
        let payload: Vec<u8> = (0..448u32).map(|i| i as u8).collect();
        let pages = encode(&payload, &enc_opts, &meta()).unwrap();
        let mut bitmap = pages[0].bitmap.clone();

        let dx = geometry.dx() as usize;
        let dy = geometry.dy() as usize;
        let cell_w = page::CELL_SIZE_DOTS * dx;
        let cell_h = page::CELL_SIZE_DOTS * dy;
        let bitmap_width = geometry.bitmap_width() as usize;
        for cell in [1u32, 3u32] {
            let (x0, y0) = geometry.block_origin_pixels(cell);
            for y in y0 as usize..(y0 as usize + cell_h) {
                for x in x0 as usize..(x0 as usize + cell_w) {
                    if y < geometry.bitmap_height() as usize && x < bitmap_width {
                        bitmap[y * bitmap_width + x] = page::WHITE;
                    }
                }
            }
        }

        let err = decode(&[bitmap], &dec_opts).unwrap_err();
        assert!(
            matches!(err, DecodeError::UnrecoverableGap { .. }),
            "expected UnrecoverableGap, got {err:?}"
        );
    }

    /// Encrypted SuperBlocks must surface as EncryptionNotSupported
    /// rather than silently producing garbage. The encoder doesn't
    /// emit encrypted output yet, but we can fake a bitmap whose
    /// SuperBlock has PBM_ENCRYPTED set to exercise the rejection path.
    #[test]
    fn rejects_encrypted_data() {
        let geometry = small_geometry();
        let (enc_opts, dec_opts) = encode_decode_options(geometry);
        let pages = encode(b"hi", &enc_opts, &meta()).unwrap();
        let mut bitmap = pages[0].bitmap.clone();

        // Find a SuperBlock cell, flip its mode byte to add PBM_ENCRYPTED,
        // recompute CRC and ECC, and write the cell back.
        let cells = page::extract(&geometry, &bitmap, dec_opts.threshold);
        let mut faked = false;
        for (cell_index, cell_bytes) in cells.iter().enumerate() {
            if let Ok(mut s) = SuperBlock::from_bytes(cell_bytes)
                && s.verify_crc()
            {
                s.mode |= PBM_ENCRYPTED;
                s.crc = s.compute_crc();
                s.ecc = s.compute_ecc();
                let new_bytes = s.to_bytes();
                let placed = page::PlacedBlock {
                    cell_index: cell_index as u32,
                    bytes: new_bytes,
                };
                // Repaint just this one cell. We can't easily call
                // page::render for a single cell, so paint the cell
                // white first then re-render the page with the new
                // SuperBlock copies in addition to existing blocks.
                // Simpler: paint the cell white and re-render the
                // single-cell update by hand via a helper.
                let _ = placed; // unused — fall back to full re-render below
                let mut rerender_blocks = Vec::new();
                for (idx, original_cell) in cells.iter().enumerate() {
                    let block = Block::from_bytes(original_cell);
                    if block.is_super() && idx == cell_index {
                        rerender_blocks.push(page::PlacedBlock {
                            cell_index: idx as u32,
                            bytes: new_bytes,
                        });
                    } else {
                        rerender_blocks.push(page::PlacedBlock {
                            cell_index: idx as u32,
                            bytes: *original_cell,
                        });
                    }
                }
                bitmap = page::render(&geometry, &rerender_blocks, page::BLACK_PAPER);
                faked = true;
                break;
            }
        }
        assert!(faked, "could not find a SuperBlock to fake encryption on");

        let err = decode(&[bitmap], &dec_opts).unwrap_err();
        assert_eq!(err, DecodeError::EncryptionNotSupported);
    }
}
