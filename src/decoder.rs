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
use crate::format_v2::{
    PBM_V2_COMPRESSED, PBM_V2_ENCRYPTED, V2_FEATURE_FLAGS_KNOWN, V2_FORMAT_VERSION, V2_GCM_TAG_LEN,
    V2_SUPERBLOCK_ADDR_CELL1, V2_SUPERBLOCK_ADDR_CELL2, V2SuperBlockCell1, V2SuperBlockCell2,
};
use crate::legacy_aes;
use crate::page::{self, PageGeometry};
use crate::v2_crypto::{V2CryptoError, build_aad, decrypt_v2, derive_key_v2};

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
    /// The SuperBlock asserts encrypted data and the caller did not
    /// pass a password. Re-call `decode` with `Some(password)` once
    /// the user supplies it.
    PasswordRequired,
    /// A password was supplied but the post-decrypt CRC of the
    /// recovered plaintext does not match `SuperBlock.filecrc`.
    /// Mirrors `Fileproc.cpp:319-321`'s "Invalid password, please
    /// try again" check. Caller may retry with a different password.
    InvalidPassword,
    /// One or more groups have ≥ 2 missing blocks; XOR-checksum
    /// recovery can only fix exactly one missing block per group.
    /// Carries the byte-offset of the first unrecoverable region.
    UnrecoverableGap { offset: u32 },
    /// SuperBlock fields disagree across pages (different files
    /// printed and scanned together?). The decoder picks the first
    /// SuperBlock and surfaces this error if any later page's
    /// SuperBlock contradicts it on the load-bearing fields.
    InconsistentSuperBlocks,
    /// AES decrypt rejected the input — usually a wrong-length
    /// buffer (e.g. SuperBlock reports a non-16-aligned datasize).
    /// Distinct from InvalidPassword which fires after a
    /// successful-but-garbage decrypt.
    DecryptFailed(legacy_aes::LegacyAesError),
    /// bzip2 decompression failed on the recovered buffer. Usually
    /// means a block went missing in a way recovery couldn't fix
    /// and the resulting buffer is corrupt.
    BzipFailed(String),
    /// v2 decode error from the AES-256-GCM layer (wrong password,
    /// tampered ciphertext, tampered AAD, or truncated buffer).
    V2DecryptFailed(V2CryptoError),
    /// v2 cell 1 (file metadata) was found on at least one page but
    /// no valid v2 cell 2 (crypto envelope) survived CRC across any
    /// page. Without cell 2's KDF salt + GCM IV we cannot derive
    /// the AES key.
    IncompleteV2Header,
    /// v2 cell 1's `format_version` field is not 2. Reserved for
    /// future v3+ files this implementation cannot read.
    UnsupportedFormatVersion { format_version: u8 },
    /// v2 cell 1's `feature_flags` has bits set that this build
    /// does not understand. Reserved bits are M12 features (color
    /// encoding, adaptive RS, dot-shape, hex packing); a v2 file
    /// with those bits set requires a newer ampaper to decode.
    UnsupportedFeature { feature_flags: u8, unknown_bits: u8 },
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSuperBlock => f.write_str("no SuperBlock decoded successfully on any page"),
            Self::PasswordRequired => f.write_str(
                "SuperBlock asserts PBM_ENCRYPTED; pass Some(password) to decode the data",
            ),
            Self::InvalidPassword => f.write_str(
                "post-decrypt CRC mismatch — the supplied password does not match the encoded data",
            ),
            Self::UnrecoverableGap { offset } => write!(
                f,
                "≥2 blocks missing in the group containing byte offset {offset}; \
                 v1 redundancy is 1-of-N and cannot recover this gap"
            ),
            Self::InconsistentSuperBlocks => f.write_str(
                "SuperBlocks across pages disagree on file metadata; \
                 inputs likely come from different prints",
            ),
            Self::DecryptFailed(e) => write!(f, "AES-CBC decrypt failed: {e}"),
            Self::BzipFailed(msg) => write!(f, "bzip2 decompression failed: {msg}"),
            Self::V2DecryptFailed(e) => write!(f, "v2 AES-256-GCM decrypt failed: {e}"),
            Self::IncompleteV2Header => f.write_str(
                "v2 cell 1 found but no valid v2 cell 2; cannot derive key without salt+IV",
            ),
            Self::UnsupportedFormatVersion { format_version } => write!(
                f,
                "v2 cell 1 reports format_version={format_version}; this build only reads {V2_FORMAT_VERSION}"
            ),
            Self::UnsupportedFeature {
                feature_flags,
                unknown_bits,
            } => write!(
                f,
                "v2 cell 1 sets unknown feature_flags bits {unknown_bits:#04x} (full flags {feature_flags:#04x}); requires a newer ampaper build"
            ),
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
pub fn decode(
    pages: &[Vec<u8>],
    options: &DecodeOptions,
    password: Option<&[u8]>,
) -> Result<Vec<u8>, DecodeError> {
    let geometry = &options.geometry;
    let threshold = options.threshold;

    // --- Steps 1-2: extract + CRC-filter cells ---------------------
    // We classify each CRC-valid cell into one of:
    //   - v1 SuperBlock (addr 0xFFFFFFFF)
    //   - v2 cell 1 (addr 0xFFFFFFFE)
    //   - v2 cell 2 (addr 0xFFFFFFFD)
    //   - data block (addr < MAXSIZE, ngroup == 0)
    //   - recovery block (ngroup in 2..=10)
    // and dispatch to v1 or v2 reassembly based on which SuperBlock
    // type wins. v2 cells take precedence — v2 is a forward format.
    let mut superblock: Option<SuperBlock> = None;
    let mut v2_cell1: Option<V2SuperBlockCell1> = None;
    let mut v2_cell2: Option<V2SuperBlockCell2> = None;
    let mut data_blocks: std::collections::BTreeMap<u32, [u8; NDATA]> = Default::default();
    let mut recovery_blocks: Vec<(u32, u8, [u8; NDATA])> = Vec::new(); // (offset, ngroup, data)
    let mut metadata_inconsistency = false;

    for bitmap in pages {
        let cells = page::extract(geometry, bitmap, threshold);
        for cell in cells {
            let block = Block::from_bytes(&cell);
            if !block.verify_crc() {
                continue;
            }
            // v2 SuperBlock cells: discriminate by the addr sentinel
            // before falling through to is_super / is_data / is_recovery.
            if block.addr == V2_SUPERBLOCK_ADDR_CELL1 {
                let parsed = V2SuperBlockCell1::from_data_bytes(&block.data);
                if v2_cell1.is_none() {
                    v2_cell1 = Some(parsed);
                }
                continue;
            }
            if block.addr == V2_SUPERBLOCK_ADDR_CELL2 {
                let parsed = V2SuperBlockCell2::from_data_bytes(&block.data);
                if v2_cell2.is_none() {
                    v2_cell2 = Some(parsed);
                }
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

    // v2 takes precedence: if we found a v2 cell 1, dispatch to v2.
    // Pass cell2 through as Option so decode_v2 can validate cell1
    // BEFORE complaining about missing cell2 — UnsupportedFeature on
    // a forward-format file should win over IncompleteV2Header.
    if let Some(cell1) = v2_cell1 {
        return decode_v2(cell1, v2_cell2, &data_blocks, &recovery_blocks, password);
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

    // --- Step 5.5: optional AES-192-CBC decrypt --------------------
    // Mirrors Fileproc.cpp:292-327. salt + iv live in the upper 32
    // bytes of SuperBlock.name (FORMAT-V1.md §3.2 / PAPERBAK-HACKS.md
    // §2.1). After decrypt, recompute Crc16(plaintext) and compare
    // to SuperBlock.filecrc — that's the password verification.
    if superblock.mode & PBM_ENCRYPTED != 0 {
        let password = password.ok_or(DecodeError::PasswordRequired)?;
        let salt: &[u8; 16] = superblock.name[32..48]
            .try_into()
            .expect("16 bytes from a 64-byte slice");
        let iv: &[u8; 16] = superblock.name[48..64]
            .try_into()
            .expect("16 bytes from a 64-byte slice");
        let key = legacy_aes::derive_key_v1(password, salt);
        legacy_aes::decrypt_v1_in_place(&mut buf, &key, iv).map_err(DecodeError::DecryptFailed)?;
        let computed = crate::crc::crc16(&buf);
        if computed != superblock.filecrc {
            return Err(DecodeError::InvalidPassword);
        }
    }

    // --- Step 6-7: optional bzip2 + truncate -----------------------
    if superblock.mode & PBM_COMPRESSED != 0 {
        buf = bz::decompress(&buf).map_err(|e| DecodeError::BzipFailed(e.to_string()))?;
    }
    buf.truncate(superblock.origsize as usize);
    Ok(buf)
}

/// v2 reassembly + decryption. Called from `decode` when v2 cells are
/// detected on the input pages. Spec: docs/FORMAT-V2.md §4.2.
///
/// Pipeline:
///   1. Validate cell 1 (`format_version`, `feature_flags`).
///   2. Reassemble the ciphertext-with-tag buffer of size `datasize`
///      from the data blocks; XOR-recover any group missing exactly
///      one block.
///   3. Build AAD from cell 1's structural fields.
///   4. Decrypt via AES-256-GCM with key = PBKDF2(password, kdf_salt).
///   5. If `feature_flags & PBM_V2_COMPRESSED`, bzip2-decompress.
///   6. Truncate to `origsize`.
fn decode_v2(
    cell1: V2SuperBlockCell1,
    cell2: Option<V2SuperBlockCell2>,
    data_blocks: &std::collections::BTreeMap<u32, [u8; NDATA]>,
    recovery_blocks: &[(u32, u8, [u8; NDATA])],
    password: Option<&[u8]>,
) -> Result<Vec<u8>, DecodeError> {
    // --- Step 1: validate cell 1 -----------------------------------
    // Cell 1 validation MUST run before we require cell 2 — a v3 file
    // (or a v2 file with reserved feature_flags bits) should surface
    // a forward-format error rather than IncompleteV2Header even if
    // its cell 2 happens to also be missing on a damaged scan.
    if cell1.format_version != V2_FORMAT_VERSION {
        return Err(DecodeError::UnsupportedFormatVersion {
            format_version: cell1.format_version,
        });
    }
    let unknown_bits = cell1.feature_flags & !V2_FEATURE_FLAGS_KNOWN;
    if unknown_bits != 0 {
        return Err(DecodeError::UnsupportedFeature {
            feature_flags: cell1.feature_flags,
            unknown_bits,
        });
    }
    if cell1.feature_flags & PBM_V2_ENCRYPTED == 0 {
        // Current spec only writes encrypted v2 (the whole point of
        // v2 is the AEAD envelope; unencrypted output stays on the v1
        // wire). A future ampaper might introduce unencrypted v2 for
        // pure forward-format use; until then, reject loudly.
        return Err(DecodeError::UnsupportedFeature {
            feature_flags: cell1.feature_flags,
            unknown_bits: PBM_V2_ENCRYPTED,
        });
    }
    let cell2 = cell2.ok_or(DecodeError::IncompleteV2Header)?;
    let password = password.ok_or(DecodeError::PasswordRequired)?;

    // --- Step 2: reassemble ciphertext + tag buffer ----------------
    let datasize = cell1.datasize;
    let mut buf = vec![0u8; datasize as usize];
    let mut filled = vec![false; (datasize.div_ceil(NDATA as u32)) as usize];
    for (&offset, data) in data_blocks {
        if offset >= datasize {
            continue;
        }
        let off = offset as usize;
        let copy_len = NDATA.min(buf.len() - off);
        buf[off..off + copy_len].copy_from_slice(&data[..copy_len]);
        let block_index = off / NDATA;
        if block_index < filled.len() {
            filled[block_index] = true;
        }
    }

    // XOR-checksum recovery: same algebra as the v1 path. Pulling
    // this out as a shared helper would be nice cleanup but is M11+1
    // work; for now we duplicate the loop to keep the v2 changeset
    // self-contained.
    for (recovery_offset, ngroup, recovery_data) in recovery_blocks {
        let group_size = *ngroup as usize;
        let group_start = *recovery_offset as usize;
        let group_first_block = group_start / NDATA;
        if group_first_block + group_size > filled.len() {
            continue;
        }
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
            continue;
        }
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
            let end = (off + NDATA).min(buf.len());
            if end <= off {
                continue;
            }
            for (r, &b) in recovered.iter_mut().zip(buf[off..end].iter()) {
                *r ^= b;
            }
        }
        let off = missing_idx * NDATA;
        let copy_len = NDATA.min(buf.len().saturating_sub(off));
        buf[off..off + copy_len].copy_from_slice(&recovered[..copy_len]);
        filled[missing_idx] = true;
    }
    for (i, &ok) in filled.iter().enumerate() {
        if !ok {
            return Err(DecodeError::UnrecoverableGap {
                offset: (i * NDATA) as u32,
            });
        }
    }

    // --- Step 3-4: build AAD, derive key, decrypt ------------------
    let aad = build_aad(
        cell1.feature_flags,
        cell1.page_count,
        cell1.origsize,
        cell1.datasize,
    );
    let key = derive_key_v2(password, &cell2.kdf_salt);
    let mut plaintext =
        decrypt_v2(&key, &cell2.gcm_iv, &aad, &buf).map_err(|e| match e {
            V2CryptoError::InvalidPassword => DecodeError::InvalidPassword,
            other => DecodeError::V2DecryptFailed(other),
        })?;
    // After AES-GCM, plaintext.len() == buf.len() - V2_GCM_TAG_LEN.
    debug_assert_eq!(
        plaintext.len() + V2_GCM_TAG_LEN,
        buf.len(),
        "GCM plaintext length should equal ciphertext length minus tag"
    );

    // --- Step 5-6: optional decompress + truncate to origsize ------
    if cell1.feature_flags & PBM_V2_COMPRESSED != 0 {
        plaintext = bz::decompress(&plaintext).map_err(|e| DecodeError::BzipFailed(e.to_string()))?;
    }
    plaintext.truncate(cell1.origsize as usize);
    Ok(plaintext)
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
        let recovered = decode(&bitmaps, &dec_opts, None).unwrap();
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
        let recovered = decode(&bitmaps, &dec_opts, None).unwrap();
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
        let recovered = decode(&bitmaps, &dec_opts, None).unwrap();
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
            let recovered = decode(&bitmaps, &dec, None).unwrap();
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

        let recovered = decode(&[bitmap], &dec_opts, None).unwrap();
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

        let err = decode(&[bitmap], &dec_opts, None).unwrap_err();
        assert!(
            matches!(err, DecodeError::UnrecoverableGap { .. }),
            "expected UnrecoverableGap, got {err:?}"
        );
    }

    /// Build a fully-encrypted single-page bitmap from `plaintext`
    /// with the given password, salt, and iv. Mirrors the relevant
    /// parts of PaperBack 1.10's `Printer.cpp:444-498` + page render
    /// without exposing v1 forward-AES from the encoder API. Returns
    /// (page bitmap, geometry) for feeding to decode/scan_decode.
    fn build_encrypted_page(
        plaintext: &[u8],
        password: &[u8],
        salt: &[u8; 16],
        iv: &[u8; 16],
    ) -> (Vec<u8>, page::PageGeometry) {
        use crate::block::{Block, ECC_BYTES, NDATA, PBM_ENCRYPTED, SUPERBLOCK_ADDR, SuperBlock};
        use crate::legacy_aes;

        let geometry = small_geometry();

        // Pad plaintext to 16 bytes (Printer.cpp:417-420). Encrypt
        // in place via the test-only helper.
        let aligned_len = (plaintext.len() + 15) & !15;
        let mut buf = vec![0u8; aligned_len];
        buf[..plaintext.len()].copy_from_slice(plaintext);
        let filecrc = crate::crc::crc16(&buf);
        let key = legacy_aes::derive_key_v1(password, salt);
        legacy_aes::encrypt_v1_in_place_for_testing(&mut buf, &key, iv);

        // Build SuperBlock with name[32..48] = salt, name[48..64] = iv
        // per FORMAT-V1.md §3.2.
        let mut name = [0u8; 64];
        name[..7].copy_from_slice(b"enc.bin");
        name[32..48].copy_from_slice(salt);
        name[48..64].copy_from_slice(iv);
        let pagesize = aligned_len as u32;
        let datasize = aligned_len as u32;
        let origsize = plaintext.len() as u32;
        let mut sb = SuperBlock {
            datasize,
            pagesize,
            origsize,
            mode: PBM_ENCRYPTED,
            attributes: 0x80,
            page: 1,
            modified: 0,
            filecrc,
            name,
            crc: 0,
            ecc: [0; ECC_BYTES],
        };
        sb.crc = sb.compute_crc();
        sb.ecc = sb.compute_ecc();
        let sb_bytes = sb.to_bytes();

        // Build data blocks: split buf into NDATA chunks, each with
        // its byte offset as the addr field.
        let mut placed: Vec<page::PlacedBlock> = Vec::new();
        let n_data = aligned_len.div_ceil(NDATA);
        let nstring = 1u32; // single group; small payload
        let redundancy = 5u32;
        // Simple layout for the test: cell 0 = SuperBlock, cells
        // 1..=n_data = data blocks, then the rest is filler SuperBlocks.
        // This doesn't replicate Printer.cpp's exact rotation formula
        // but it produces a valid bitmap whose SuperBlock points to
        // the encrypted data — enough to exercise decode's encrypt
        // path. (The full layout test happens in encoder.rs.)
        let _ = (nstring, redundancy); // silence unused-var lints
        placed.push(page::PlacedBlock {
            cell_index: 0,
            bytes: sb_bytes,
        });
        for i in 0..n_data {
            let off = i * NDATA;
            let take = (aligned_len - off).min(NDATA);
            let mut data = [0u8; NDATA];
            data[..take].copy_from_slice(&buf[off..off + take]);
            let mut block = Block {
                addr: off as u32,
                data,
                crc: 0,
                ecc: [0; ECC_BYTES],
            };
            block.crc = block.compute_crc();
            block.ecc = block.compute_ecc();
            placed.push(page::PlacedBlock {
                cell_index: (i + 1) as u32,
                bytes: block.to_bytes(),
            });
        }
        // Fill remaining cells with extra SuperBlock copies so the
        // decoder's "any SuperBlock seen" check has plenty of CRC-
        // verified candidates regardless of which cells the scanner
        // happens to read cleanly.
        let total_cells = geometry.nx() * geometry.ny();
        for cell in (n_data + 1) as u32..total_cells {
            // Skip cells that conflict with placed addrs.
            if placed.iter().any(|p| p.cell_index == cell) {
                continue;
            }
            placed.push(page::PlacedBlock {
                cell_index: cell,
                bytes: sb_bytes,
            });
        }

        let bitmap = page::render(&geometry, &placed, page::BLACK_PAPER);
        // crate::block::SUPERBLOCK_ADDR is referenced via name[..]
        // path; silence dead-imports warning if any.
        let _ = SUPERBLOCK_ADDR;
        (bitmap, geometry)
    }

    /// Encrypted page round-trip: decode with the right password
    /// recovers the plaintext byte-for-byte.
    #[test]
    fn encrypted_page_round_trips_with_correct_password() {
        let plaintext = b"the quick brown fox jumps over the lazy dog";
        let password = b"correct horse battery staple";
        let salt = [0x42u8; 16];
        let iv = [0x99u8; 16];
        let (bitmap, geometry) = build_encrypted_page(plaintext, password, &salt, &iv);
        let opts = DecodeOptions {
            geometry,
            threshold: page::DEFAULT_THRESHOLD,
        };
        let recovered = decode(&[bitmap], &opts, Some(password))
            .expect("decode with correct password must succeed");
        assert_eq!(recovered, plaintext);
    }

    /// No password supplied for an encrypted page → PasswordRequired.
    #[test]
    fn encrypted_page_without_password_is_password_required() {
        let plaintext = b"secrets need passwords";
        let password = b"swordfish";
        let salt = [0x11u8; 16];
        let iv = [0x22u8; 16];
        let (bitmap, geometry) = build_encrypted_page(plaintext, password, &salt, &iv);
        let opts = DecodeOptions {
            geometry,
            threshold: page::DEFAULT_THRESHOLD,
        };
        let err = decode(&[bitmap], &opts, None).unwrap_err();
        assert_eq!(err, DecodeError::PasswordRequired);
    }

    /// Wrong password → InvalidPassword (post-decrypt CRC mismatch).
    /// This is the user-friendly distinction from PasswordRequired.
    #[test]
    fn encrypted_page_with_wrong_password_is_invalid_password() {
        let plaintext = b"secrets need passwords";
        let password = b"swordfish";
        let salt = [0x33u8; 16];
        let iv = [0x44u8; 16];
        let (bitmap, geometry) = build_encrypted_page(plaintext, password, &salt, &iv);
        let opts = DecodeOptions {
            geometry,
            threshold: page::DEFAULT_THRESHOLD,
        };
        let err = decode(&[bitmap], &opts, Some(b"wrong password")).unwrap_err();
        assert_eq!(err, DecodeError::InvalidPassword);
    }

    // === v2 decode error-path tests (M11) ====================
    //
    // Round-trip success cases live in encoder.rs alongside the
    // encode_v2 entry points; these tests exercise the decoder's
    // error surface (PasswordRequired, InvalidPassword,
    // UnsupportedFormatVersion, UnsupportedFeature,
    // IncompleteV2Header).

    fn v2_pages(
        payload: &[u8],
        password: &[u8],
        salt: &[u8; 32],
        iv: &[u8; 12],
    ) -> (Vec<Vec<u8>>, page::PageGeometry) {
        let geometry = small_geometry();
        let opts = crate::encoder::EncodeOptions {
            geometry,
            redundancy: 5,
            compress: false,
            black: page::BLACK_PAPER,
        };
        let pages = crate::encoder::encode_v2_with_kat(payload, &opts, &meta(), password, salt, iv)
            .expect("encode_v2 must succeed in tests");
        let bitmaps: Vec<Vec<u8>> = pages.iter().map(|p| p.bitmap.clone()).collect();
        (bitmaps, geometry)
    }

    /// v2 page decoded without password → PasswordRequired.
    #[test]
    fn v2_no_password_is_password_required() {
        let salt = [0x42u8; 32];
        let iv = [0x99u8; 12];
        let (pages, geometry) = v2_pages(b"hello", b"correct horse", &salt, &iv);
        let opts = DecodeOptions {
            geometry,
            threshold: page::DEFAULT_THRESHOLD,
        };
        let err = decode(&pages, &opts, None).unwrap_err();
        assert_eq!(err, DecodeError::PasswordRequired);
    }

    /// v2 page decoded with wrong password → InvalidPassword.
    #[test]
    fn v2_wrong_password_is_invalid_password() {
        let salt = [0x42u8; 32];
        let iv = [0x99u8; 12];
        let (pages, geometry) = v2_pages(b"hello", b"correct horse", &salt, &iv);
        let opts = DecodeOptions {
            geometry,
            threshold: page::DEFAULT_THRESHOLD,
        };
        let err = decode(&pages, &opts, Some(b"wrong password")).unwrap_err();
        assert_eq!(err, DecodeError::InvalidPassword);
    }

    /// Decoder rejects v2 cell 1 with reserved (M12) feature_flags
    /// bits set. Pins forward-compat: a future ampaper that adds bit
    /// 2 (color) emits files this version of ampaper refuses cleanly.
    #[test]
    fn v2_unknown_feature_flags_is_unsupported_feature() {
        use crate::block::{Block, ECC_BYTES};
        use crate::format_v2::V2SuperBlockCell1;

        // Build a v2 page with a synthetic cell 1 carrying a reserved
        // bit (bit 2 = future color encoding).
        let geometry = small_geometry();
        let mut name = [0u8; 64];
        name[..4].copy_from_slice(b"test");
        let cell1 = V2SuperBlockCell1 {
            format_version: 2,
            feature_flags: 0b0000_0111, // bit 0 + bit 1 + reserved bit 2
            page: 1,
            page_count: 1,
            datasize: 16,
            origsize: 0,
            pagesize: 16,
            modified: 0,
            name,
        };
        let cell1_block = cell1.to_block();

        // Synthesize a page bitmap with cell 1 placed at cell index 0.
        // The decoder's classification by addr fires regardless of
        // whether we have a valid cell 2 — UnsupportedFeature must
        // win before IncompleteV2Header.
        let mut placed = vec![page::PlacedBlock {
            cell_index: 0,
            bytes: cell1_block.to_bytes(),
        }];
        // Fill remaining cells with dummy blocks so the bitmap renders.
        let dummy = Block {
            addr: 0,
            data: [0u8; NDATA],
            crc: 0,
            ecc: [0u8; ECC_BYTES],
        };
        let total_cells = geometry.nx() * geometry.ny();
        for cell in 1..total_cells {
            placed.push(page::PlacedBlock {
                cell_index: cell,
                bytes: dummy.to_bytes(),
            });
        }
        let bitmap = page::render(&geometry, &placed, page::BLACK_PAPER);
        let opts = DecodeOptions {
            geometry,
            threshold: page::DEFAULT_THRESHOLD,
        };
        let err = decode(&[bitmap], &opts, Some(b"pw")).unwrap_err();
        assert!(
            matches!(
                err,
                DecodeError::UnsupportedFeature {
                    unknown_bits: 0b100,
                    ..
                }
            ),
            "expected UnsupportedFeature with bit 2 set, got {err:?}"
        );
    }

    /// Decoder rejects v2 cell 1 with a wrong format_version.
    #[test]
    fn v2_unsupported_format_version_is_rejected() {
        use crate::block::{Block, ECC_BYTES};
        use crate::format_v2::V2SuperBlockCell1;

        let geometry = small_geometry();
        let cell1 = V2SuperBlockCell1 {
            format_version: 99, // not 2
            feature_flags: PBM_V2_ENCRYPTED,
            page: 1,
            page_count: 1,
            datasize: 16,
            origsize: 0,
            pagesize: 16,
            modified: 0,
            name: [0; 64],
        };
        let cell1_block = cell1.to_block();
        let mut placed = vec![page::PlacedBlock {
            cell_index: 0,
            bytes: cell1_block.to_bytes(),
        }];
        let dummy = Block {
            addr: 0,
            data: [0u8; NDATA],
            crc: 0,
            ecc: [0u8; ECC_BYTES],
        };
        let total_cells = geometry.nx() * geometry.ny();
        for cell in 1..total_cells {
            placed.push(page::PlacedBlock {
                cell_index: cell,
                bytes: dummy.to_bytes(),
            });
        }
        let bitmap = page::render(&geometry, &placed, page::BLACK_PAPER);
        let opts = DecodeOptions {
            geometry,
            threshold: page::DEFAULT_THRESHOLD,
        };
        let err = decode(&[bitmap], &opts, Some(b"pw")).unwrap_err();
        assert_eq!(
            err,
            DecodeError::UnsupportedFormatVersion { format_version: 99 }
        );
    }

    /// v2 cell 1 present but no v2 cell 2 → IncompleteV2Header. The
    /// decoder needs cell 2 for the KDF salt + GCM IV; without it,
    /// no key derivation is possible.
    #[test]
    fn v2_missing_cell2_is_incomplete_header() {
        use crate::block::{Block, ECC_BYTES};
        use crate::format_v2::V2SuperBlockCell1;

        let geometry = small_geometry();
        let cell1 = V2SuperBlockCell1 {
            format_version: 2,
            feature_flags: PBM_V2_ENCRYPTED,
            page: 1,
            page_count: 1,
            datasize: 16,
            origsize: 0,
            pagesize: 16,
            modified: 0,
            name: [0; 64],
        };
        let cell1_block = cell1.to_block();
        let mut placed = vec![page::PlacedBlock {
            cell_index: 0,
            bytes: cell1_block.to_bytes(),
        }];
        let dummy = Block {
            addr: 0,
            data: [0u8; NDATA],
            crc: 0,
            ecc: [0u8; ECC_BYTES],
        };
        let total_cells = geometry.nx() * geometry.ny();
        for cell in 1..total_cells {
            placed.push(page::PlacedBlock {
                cell_index: cell,
                bytes: dummy.to_bytes(),
            });
        }
        let bitmap = page::render(&geometry, &placed, page::BLACK_PAPER);
        let opts = DecodeOptions {
            geometry,
            threshold: page::DEFAULT_THRESHOLD,
        };
        let err = decode(&[bitmap], &opts, Some(b"pw")).unwrap_err();
        assert_eq!(err, DecodeError::IncompleteV2Header);
    }
}
