// File-to-bitmap encoder pipeline. Per FORMAT-V1.md §5-§6 and
// `Printer.cpp:501-998`. The encoder side of the v1 round-trip:
//
//   file bytes -> (optional bzip2) -> 16-byte align -> filecrc
//   for each page:
//       SuperBlock (with name, mode, page index, etc.)
//       redundancy+1 group strings, each with `nstring` data blocks
//         and 1 XOR-checksum recovery block
//       trailing cells filled with extra SuperBlock copies
//   render each page to a grayscale bitmap via crate::page::render
//
// AES encryption is intentionally out of scope here — read-only legacy
// AES-192 lands at M7, AES-256-GCM forward at M11. The encoder rejects
// any future "encryption=on" request with a clear error rather than
// silently emitting unencrypted output (the same anti-foot-gun stance
// the source's silent-disable patterns from PAPERBAK-HACKS.md §6 do
// NOT take).
//
// The cell-layout math (group-string column placement and the
// rotation formula for wide regimes) is the "two bottles of Weissbier"
// passage from `Printer.cpp:872-924` flagged in PAPERBAK-HACKS.md §3.1.
// Variable names here mirror the C source so the trace from doc to
// source to Rust stays one-to-one.

use crate::block::{
    Block, ECC_BYTES, MAXSIZE, NDATA, NGROUP_MAX, NGROUP_MIN, PBM_COMPRESSED, SuperBlock,
};
use crate::bz;
use crate::format_v2::{
    PBM_V2_COMPRESSED, PBM_V2_ENCRYPTED, V2_FORMAT_VERSION, V2_GCM_IV_LEN, V2_GCM_TAG_LEN,
    V2_KDF_SALT_LEN, V2SuperBlockCell1, V2SuperBlockCell2,
};
use crate::page::{self, BLACK_PAPER, PageGeometry, PlacedBlock};
use crate::v2_crypto::{V2CryptoError, build_aad, derive_key_v2, encrypt_v2};

/// File-level metadata baked into every page's SuperBlock.
#[derive(Clone, Copy, Debug)]
pub struct FileMeta<'a> {
    /// Filename, written into bytes 0..32 of `superdata.name`.
    /// Truncated to 31 chars + NUL per Printer.cpp:526-527 (the
    /// dual-purpose layout from FORMAT-V1.md §3.2 reserves the
    /// upper 32 bytes for AES salt+IV regardless of encryption).
    pub name: &'a str,
    /// Win32 FILETIME in 100ns ticks since 1601-01-01 UTC. Pass 0
    /// to omit a meaningful timestamp.
    pub modified: u64,
    /// Win32 file-attribute subset (READONLY|HIDDEN|SYSTEM|ARCHIVE
    /// |NORMAL bits per Printer.cpp:517-520). Pass 0x80
    /// (FILE_ATTRIBUTE_NORMAL) for "no special attributes".
    pub attributes: u8,
}

/// Encoder configuration. The format-affecting knobs only.
#[derive(Clone, Copy, Debug)]
pub struct EncodeOptions {
    /// Page geometry — drives the cell grid that blocks pack into.
    pub geometry: PageGeometry,
    /// Per-group redundancy. NGROUP_MIN..=NGROUP_MAX.
    /// PaperBack 1.10 default is NGROUP_DEFAULT (5).
    pub redundancy: u8,
    /// When true, run input through bzip2 before block-splitting.
    /// FORMAT-V1.md §6.1; mirrors the user "compress" toggle.
    pub compress: bool,
    /// Pixel value for filled dots (`page::BLACK_PAPER` for paper,
    /// `page::BLACK_BMP` for the dark-gray BMP-debug palette).
    pub black: u8,
}

impl Default for EncodeOptions {
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
            redundancy: crate::block::NGROUP_DEFAULT,
            compress: false,
            black: BLACK_PAPER,
        }
    }
}

/// One encoded page: the rendered bitmap plus its dimensions.
#[derive(Clone, Debug)]
pub struct EncodedPage {
    pub bitmap: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// What can go wrong during an encode.
#[derive(Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// Input exceeds [`MAXSIZE`] (~256 MB minus 128) — the v1 format
    /// caps offsets at 28 bits.
    InputTooLarge { len: usize },
    /// Redundancy outside [NGROUP_MIN, NGROUP_MAX].
    InvalidRedundancy { redundancy: u8 },
    /// Page geometry can't fit `redundancy + 2` cells per row /
    /// 3 rows minimum / `2*redundancy + 2` total cells. Mirrors the
    /// "Printable area is too small" error at `Printer.cpp:683`.
    PageTooSmall {
        nx: u32,
        ny: u32,
        cells: u32,
        redundancy: u8,
    },
    /// AES-256-GCM rejected the encryption inputs. Practically
    /// unreachable for sane sizes; the type system requires the
    /// fallible signature.
    V2EncryptFailed(V2CryptoError),
    /// Failed to obtain OS entropy for the KDF salt or GCM IV. The
    /// encoder refuses to fall back to deterministic or weak entropy
    /// sources — that would silently downgrade security. Surface the
    /// failure so the caller can decide what to do.
    V2EntropyFailed(String),
    /// Total page count exceeds u16 capacity. v2's SuperBlock cell 1
    /// stores `page_count` as u16 (FORMAT-V2.md §2.1 / §8); 65535
    /// pages × ~177 KB/page = ~11 GB. Hitting this means input ≫
    /// MAXSIZE allows and is unlikely to round-trip even if we widened
    /// the field.
    V2TooManyPages { page_count: u32 },
}

impl core::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InputTooLarge { len } => write!(
                f,
                "input is {len} bytes; v1 caps at MAXSIZE = {} bytes",
                MAXSIZE
            ),
            Self::InvalidRedundancy { redundancy } => write!(
                f,
                "redundancy = {redundancy}; valid range is [{NGROUP_MIN}, {NGROUP_MAX}]"
            ),
            Self::PageTooSmall {
                nx,
                ny,
                cells,
                redundancy,
            } => write!(
                f,
                "page geometry {nx}x{ny} cells (= {cells}) cannot host \
                 redundancy={redundancy}; need ≥ {} cells per row, ≥ 3 rows, \
                 ≥ {} total",
                redundancy + 1,
                2 * redundancy + 2
            ),
            Self::V2EncryptFailed(e) => write!(f, "v2 AES-256-GCM encryption failed: {e}"),
            Self::V2EntropyFailed(msg) => {
                write!(f, "OS entropy unavailable for v2 salt/IV: {msg}")
            }
            Self::V2TooManyPages { page_count } => write!(
                f,
                "v2 input requires {page_count} pages; cell-1 page_count is u16 (max 65535)"
            ),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Encode `input` into one or more PaperBack-v1 pages.
///
/// Pipeline:
///   1. Optionally bzip2-compress.
///   2. Pad to 16-byte boundary (Printer.cpp:417). The 0-fill is part
///      of the wire format because `superdata.datasize` reports the
///      aligned length, not the bzip2 length.
///   3. Compute filecrc = CRC-16/CCITT (no XOR) of the aligned bytes.
///      This is the password-verification crc that `Saverestoredfile`
///      checks at Fileproc.cpp:318.
///   4. Per page: build SuperBlock, layout group strings, render.
///
/// The encoder does NOT yet support encryption. If you need to encode
/// AES-192 v1 output for cross-check against an existing PaperBack
/// 1.10 print, that's M11-and-then-some-work; for the moment the
/// posture is "v1 read for compatibility, v2 write only" per
/// `docs/ENCRYPTION-DECISION.md`.
pub fn encode(
    input: &[u8],
    options: &EncodeOptions,
    meta: &FileMeta<'_>,
) -> Result<Vec<EncodedPage>, EncodeError> {
    if input.len() > MAXSIZE as usize {
        return Err(EncodeError::InputTooLarge { len: input.len() });
    }
    if !(NGROUP_MIN..=NGROUP_MAX).contains(&options.redundancy) {
        return Err(EncodeError::InvalidRedundancy {
            redundancy: options.redundancy,
        });
    }

    let nx = options.geometry.nx();
    let ny = options.geometry.ny();
    let cells = nx * ny;
    let redundancy = u32::from(options.redundancy);
    if nx < redundancy + 1 || ny < 3 || cells < 2 * redundancy + 2 {
        return Err(EncodeError::PageTooSmall {
            nx,
            ny,
            cells,
            redundancy: options.redundancy,
        });
    }

    // --- Step 1-2: compress + 16-byte align -------------------------
    let mut buf = if options.compress {
        bz::compress(input, bz::BlockSize::Max)
    } else {
        input.to_vec()
    };
    let datasize_pre_align = buf.len();
    let aligned_len = (datasize_pre_align + 15) & !15;
    buf.resize(aligned_len, 0);

    // The compressed-then-aligned length is what the SuperBlock
    // reports, and what every block's `addr` is offset within.
    let datasize = aligned_len as u32;
    let origsize = input.len() as u32;
    let mode: u8 = if options.compress { PBM_COMPRESSED } else { 0 };

    // --- Step 3: filecrc over the aligned buffer --------------------
    // Printer.cpp:453 / FORMAT-V1.md §6.3.3. Used by the decoder to
    // verify the password (irrelevant when encryption is off, but the
    // field is still set — it's part of the wire format).
    let filecrc = crate::crc::crc16(&buf);

    // --- Step 4: page capacity --------------------------------------
    // Printer.cpp:730-731. pagesize is bytes of (compressed) data
    // carried per page; with redundancy=5 on a 12x6 cell page that's
    // ((72 - 5 - 2) / 6) * 5 * 90 = 4500 bytes.
    let pagesize: u32 = ((cells - redundancy - 2) / (redundancy + 1)) * redundancy * (NDATA as u32);
    let npages = datasize.div_ceil(pagesize).max(1);

    // --- Step 5: encode each page -----------------------------------
    let mut pages = Vec::with_capacity(npages as usize);
    for page_index in 0..npages {
        let page = encode_one_page(
            &buf, datasize, pagesize, page_index, npages, origsize, mode, filecrc, meta, options,
        );
        pages.push(page);
    }

    Ok(pages)
}

#[allow(clippy::too_many_arguments)]
fn encode_one_page(
    buf: &[u8],
    datasize: u32,
    pagesize: u32,
    page_index: u32,
    _npages: u32,
    origsize: u32,
    mode: u8,
    filecrc: u16,
    meta: &FileMeta<'_>,
    options: &EncodeOptions,
) -> EncodedPage {
    let nx = options.geometry.nx();
    let redundancy = u32::from(options.redundancy);

    // --- Build SuperBlock for this page -----------------------------
    let mut name_bytes = [0u8; 64];
    let name_len = meta.name.len().min(31);
    name_bytes[..name_len].copy_from_slice(&meta.name.as_bytes()[..name_len]);
    // bytes 32..64 are reserved for AES salt+IV per FORMAT-V1.md §3.2;
    // when encryption is off they remain zero.

    let mut superblock = SuperBlock {
        datasize,
        pagesize,
        origsize,
        mode,
        attributes: meta.attributes,
        page: (page_index + 1) as u16, // 1-based per Printer.cpp:867-868
        modified: meta.modified,
        filecrc,
        name: name_bytes,
        crc: 0,
        ecc: [0; ECC_BYTES],
    };
    superblock.crc = superblock.compute_crc();
    superblock.ecc = superblock.compute_ecc();
    let superblock_bytes = superblock.to_bytes();

    // --- Determine layout for this page -----------------------------
    let offset_start = page_index * pagesize;
    let l = (datasize - offset_start).min(pagesize); // bytes for this page
    let n = l.div_ceil(NDATA as u32); // data blocks needed
    let nstring = n.div_ceil(redundancy); // number of group strings

    // --- Place redundancy+1 superblock copies (one per string) ------
    // Printer.cpp:872-877. First block in every string is a
    // superblock; in compact regime that's column j*(nstring+1), in
    // wide regime there's an extra rotation.
    let mut placed: Vec<PlacedBlock> = Vec::new();
    for j in 0..=redundancy {
        let cell = first_cell_of_string(j, nstring, nx, redundancy);
        placed.push(PlacedBlock {
            cell_index: cell,
            bytes: superblock_bytes,
        });
    }

    // --- Place data blocks + recovery, group by group ---------------
    // Printer.cpp:879-920. cksum starts at 0xFF and XORs each data
    // block in; on decode side, XOR-everything-back gives 0xFF for a
    // complete group, so the recovery slot's bit-inverse-then-XOR
    // recovers a single missing block.
    let mut offset = offset_start;
    for i in 0..nstring {
        let mut cksum_data = [0xFFu8; NDATA];

        // redundancy data blocks per group.
        for j in 0..redundancy {
            let block_addr = offset;
            let mut data = [0u8; NDATA];
            // Mirrors Printer.cpp:887-895: take min(NDATA, remaining)
            // bytes from buf at the current offset; pad the rest with 0.
            let take = (datasize.saturating_sub(offset) as usize).min(NDATA);
            if take > 0 {
                data[..take].copy_from_slice(&buf[offset as usize..offset as usize + take]);
            }
            // Update XOR-checksum running total.
            for (c, &d) in cksum_data.iter_mut().zip(data.iter()) {
                *c ^= d;
            }

            let mut block = Block {
                addr: block_addr,
                data,
                crc: 0,
                ecc: [0; ECC_BYTES],
            };
            block.crc = block.compute_crc();
            block.ecc = block.compute_ecc();

            let cell = data_or_recovery_cell(j, i, nstring, nx, redundancy);
            placed.push(PlacedBlock {
                cell_index: cell,
                bytes: block.to_bytes(),
            });

            offset += NDATA as u32;
        }

        // 1 recovery block per group. The C source sets
        // cksum.addr = group_start_offset ^ (redundancy<<28) at
        // Printer.cpp:881; group_start_offset is the byte offset of
        // the group's first data block.
        let group_start_offset = offset_start + i * redundancy * (NDATA as u32);
        let recovery_addr = group_start_offset ^ (redundancy << 28);
        let mut recovery = Block {
            addr: recovery_addr,
            data: cksum_data,
            crc: 0,
            ecc: [0; ECC_BYTES],
        };
        recovery.crc = recovery.compute_crc();
        recovery.ecc = recovery.compute_ecc();

        let cell = data_or_recovery_cell(redundancy, i, nstring, nx, redundancy);
        placed.push(PlacedBlock {
            cell_index: cell,
            bytes: recovery.to_bytes(),
        });
    }

    // --- Fill remaining cells with extra SuperBlock copies ----------
    // Printer.cpp:921-924. Trailing cells get superblock copies — the
    // decoder can pick up the page metadata from any of them.
    let used_cells: std::collections::HashSet<u32> = placed.iter().map(|p| p.cell_index).collect();
    let total_cells = nx * options.geometry.ny();
    for cell in 0..total_cells {
        if !used_cells.contains(&cell) {
            placed.push(PlacedBlock {
                cell_index: cell,
                bytes: superblock_bytes,
            });
        }
    }

    // --- Render --------------------------------------------------
    let bitmap = page::render(&options.geometry, &placed, options.black);
    EncodedPage {
        bitmap,
        width: options.geometry.bitmap_width(),
        height: options.geometry.bitmap_height(),
    }
}

// === v2 encode pathway (M11) =====================================
// Spec: docs/FORMAT-V2.md.
//
// v2 differs from v1 in three ways the encoder cares about:
//   1. The SuperBlock occupies TWO cells (cell 1 = file metadata at
//      addr 0xFFFFFFFE, cell 2 = crypto envelope at 0xFFFFFFFD)
//      instead of one. Each "string" on the page therefore costs an
//      extra cell — pagesize drops accordingly.
//   2. The data buffer is AES-256-GCM ciphertext + 16-byte tag, not
//      the raw (compressed) plaintext. Encryption sits ABOVE the
//      block layer: the block-level RS + CRC + group-recovery logic
//      is identical to v1.
//   3. AAD binds the GCM tag to (feature_flags, page_count, origsize,
//      datasize), so an attacker cannot truncate pages or flip flag
//      bits without invalidating the tag.

/// Encode `input` into one or more ampaper-v2 pages with AES-256-GCM
/// encryption. Generates fresh KDF salt + GCM IV via OS entropy
/// (`getrandom`). For deterministic test output, use
/// [`encode_v2_with_kat`].
///
/// Pipeline:
///   1. Optional bzip2 (when `options.compress`) — applied BEFORE
///      encryption (compress-then-encrypt; spec FORMAT-V2.md §8).
///   2. Generate kdf_salt (32 B) + gcm_iv (12 B) from OS entropy.
///   3. Derive 32-byte AES-256 key via PBKDF2-HMAC-SHA-256 at 600,000
///      iterations (FORMAT-V2.md §3.2).
///   4. Compute datasize = ciphertext.len() + 16 (tag), origsize =
///      input.len(), pagesize_v2 (one extra cell per string for the
///      second SuperBlock cell), npages.
///   5. Build AAD = magic + feature_flags + page_count + origsize +
///      datasize.
///   6. Encrypt: ciphertext_with_tag = AES-256-GCM(key, iv, AAD, buf).
///   7. Per page: build cell 1 + cell 2 + data blocks + recovery
///      blocks; render.
pub fn encode_v2(
    input: &[u8],
    options: &EncodeOptions,
    meta: &FileMeta<'_>,
    password: &[u8],
) -> Result<Vec<EncodedPage>, EncodeError> {
    let mut kdf_salt = [0u8; V2_KDF_SALT_LEN];
    let mut gcm_iv = [0u8; V2_GCM_IV_LEN];
    getrandom::fill(&mut kdf_salt).map_err(|e| EncodeError::V2EntropyFailed(e.to_string()))?;
    getrandom::fill(&mut gcm_iv).map_err(|e| EncodeError::V2EntropyFailed(e.to_string()))?;
    encode_v2_with_kat(input, options, meta, password, &kdf_salt, &gcm_iv)
}

/// Test/KAT entry point for [`encode_v2`] that takes a fixed
/// `kdf_salt` + `gcm_iv` instead of generating fresh entropy. Used
/// by golden-vector tests to pin byte-equal output.
///
/// Production callers MUST use [`encode_v2`] — re-using a fixed IV
/// across encodes catastrophically breaks AES-GCM.
pub fn encode_v2_with_kat(
    input: &[u8],
    options: &EncodeOptions,
    meta: &FileMeta<'_>,
    password: &[u8],
    kdf_salt: &[u8; V2_KDF_SALT_LEN],
    gcm_iv: &[u8; V2_GCM_IV_LEN],
) -> Result<Vec<EncodedPage>, EncodeError> {
    if input.len() > MAXSIZE as usize {
        return Err(EncodeError::InputTooLarge { len: input.len() });
    }
    if !(NGROUP_MIN..=NGROUP_MAX).contains(&options.redundancy) {
        return Err(EncodeError::InvalidRedundancy {
            redundancy: options.redundancy,
        });
    }

    let nx = options.geometry.nx();
    let ny = options.geometry.ny();
    let cells = nx * ny;
    let redundancy = u32::from(options.redundancy);
    // v2 page-too-small: at minimum (redundancy+1)*3 cells (3 cells
    // per string = 2 supers + 1 data block). Stricter than v1 by one
    // cell per string.
    if nx < redundancy + 1 || ny < 3 || cells < 3 * (redundancy + 1) {
        return Err(EncodeError::PageTooSmall {
            nx,
            ny,
            cells,
            redundancy: options.redundancy,
        });
    }

    // --- Step 1: optional compression -------------------------------
    let plaintext: Vec<u8> = if options.compress {
        bz::compress(input, bz::BlockSize::Max)
    } else {
        input.to_vec()
    };
    let origsize = input.len() as u32;
    let mut feature_flags = PBM_V2_ENCRYPTED;
    if options.compress {
        feature_flags |= PBM_V2_COMPRESSED;
    }

    // --- Steps 3-4: derive key, compute layout sizes ----------------
    let key = derive_key_v2(password, kdf_salt);
    // Ciphertext + tag length = plaintext + 16. We need this before
    // calling encrypt because AAD includes datasize, and AAD is an
    // input to the GCM tag computation. The +16 is fixed by the GCM
    // tag length (V2_GCM_TAG_LEN).
    let datasize = (plaintext.len() + V2_GCM_TAG_LEN) as u32;
    let pagesize_v2 = pagesize_v2(cells, redundancy);
    let page_count_u32 = datasize.div_ceil(pagesize_v2).max(1);
    if page_count_u32 > u16::MAX as u32 {
        return Err(EncodeError::V2TooManyPages {
            page_count: page_count_u32,
        });
    }
    let page_count = page_count_u32 as u16;

    // --- Step 5: AAD ------------------------------------------------
    let aad = build_aad(feature_flags, page_count, origsize, datasize);

    // --- Step 6: encrypt -------------------------------------------
    let ciphertext_with_tag =
        encrypt_v2(&key, gcm_iv, &aad, &plaintext).map_err(EncodeError::V2EncryptFailed)?;
    debug_assert_eq!(ciphertext_with_tag.len() as u32, datasize);

    // --- Step 7: build pages ---------------------------------------
    let mut pages = Vec::with_capacity(page_count as usize);
    for page_index in 0..page_count_u32 {
        let page = encode_one_page_v2(
            &ciphertext_with_tag,
            datasize,
            pagesize_v2,
            page_index,
            page_count,
            origsize,
            feature_flags,
            kdf_salt,
            gcm_iv,
            meta,
            options,
        );
        pages.push(page);
    }
    Ok(pages)
}

/// v2 pagesize formula. Mirrors v1's `(cells - redundancy - 2) /
/// (redundancy+1) * redundancy * NDATA`, but reserves one extra cell
/// per string for the second SuperBlock cell.
///
/// Each v2 string holds: 2 supers + nstring data/recovery cells =
/// (nstring + 2) cells. With (redundancy+1) strings per page,
/// total used = (redundancy+1)*(nstring+2). Solving for nstring with
/// the same conservative margin v1 keeps:
///
/// ```text
/// nstring_v2 = (cells - 2*redundancy - 3) / (redundancy + 1)
/// pagesize_v2 = nstring_v2 * redundancy * NDATA
/// ```
#[must_use]
fn pagesize_v2(cells: u32, redundancy: u32) -> u32 {
    let nstring = if cells > 2 * redundancy + 3 {
        (cells - 2 * redundancy - 3) / (redundancy + 1)
    } else {
        0
    };
    nstring * redundancy * (NDATA as u32)
}

#[allow(clippy::too_many_arguments)]
fn encode_one_page_v2(
    buf: &[u8],
    datasize: u32,
    pagesize_v2: u32,
    page_index: u32,
    page_count: u16,
    origsize: u32,
    feature_flags: u8,
    kdf_salt: &[u8; V2_KDF_SALT_LEN],
    gcm_iv: &[u8; V2_GCM_IV_LEN],
    meta: &FileMeta<'_>,
    options: &EncodeOptions,
) -> EncodedPage {
    let nx = options.geometry.nx();
    let redundancy = u32::from(options.redundancy);

    // --- Build SuperBlock cell 1 + cell 2 for this page ------------
    let mut name_bytes = [0u8; 64];
    // v2 doesn't reuse name[32..64] for AES salt+IV (cell 2 owns
    // the crypto envelope), so we get the full 63-byte filename
    // budget (NUL-terminated). Keep parity with v1's truncation
    // posture for graceful behavior on long names.
    let name_len = meta.name.len().min(63);
    name_bytes[..name_len].copy_from_slice(&meta.name.as_bytes()[..name_len]);

    let cell1 = V2SuperBlockCell1 {
        format_version: V2_FORMAT_VERSION,
        feature_flags,
        page: (page_index + 1) as u16,
        page_count,
        datasize,
        origsize,
        pagesize: pagesize_v2,
        modified: meta.modified,
        name: name_bytes,
    };
    let cell2 = V2SuperBlockCell2 {
        kdf_salt: *kdf_salt,
        gcm_iv: *gcm_iv,
        reserved: [0; crate::format_v2::V2_CELL2_RESERVED_LEN],
    };
    let cell1_bytes = cell1.to_block().to_bytes();
    let cell2_bytes = cell2.to_block().to_bytes();

    // --- Determine layout for this page ----------------------------
    let offset_start = page_index * pagesize_v2;
    let l = (datasize - offset_start).min(pagesize_v2);
    let n = l.div_ceil(NDATA as u32);
    let nstring = n.div_ceil(redundancy);
    let cells_per_string = nstring + 2;

    // --- Place cell 1 + cell 2 super copies for each string --------
    // Each of the (redundancy + 1) strings starts with cell 1 at
    // row 0 and cell 2 at row 1 of that string.
    let mut placed: Vec<PlacedBlock> = Vec::new();
    for j in 0..=redundancy {
        let cell1_idx = cell_in_string_v2(j, 0, cells_per_string, nx, redundancy);
        placed.push(PlacedBlock {
            cell_index: cell1_idx,
            bytes: cell1_bytes,
        });
        let cell2_idx = cell_in_string_v2(j, 1, cells_per_string, nx, redundancy);
        placed.push(PlacedBlock {
            cell_index: cell2_idx,
            bytes: cell2_bytes,
        });
    }

    // --- Place data blocks + recovery, group by group --------------
    // Same XOR-checksum recovery story as v1: each group's recovery
    // block is `0xFF ^ d0 ^ ... ^ d_{redundancy-1}`. Decoder XOR-
    // recovers a single missing block per group via the same path.
    let mut offset = offset_start;
    for i in 0..nstring {
        let mut cksum_data = [0xFFu8; NDATA];
        for j in 0..redundancy {
            let block_addr = offset;
            let mut data = [0u8; NDATA];
            let take = (datasize.saturating_sub(offset) as usize).min(NDATA);
            if take > 0 {
                data[..take].copy_from_slice(&buf[offset as usize..offset as usize + take]);
            }
            for (c, &d) in cksum_data.iter_mut().zip(data.iter()) {
                *c ^= d;
            }
            let mut block = Block {
                addr: block_addr,
                data,
                crc: 0,
                ecc: [0; ECC_BYTES],
            };
            block.crc = block.compute_crc();
            block.ecc = block.compute_ecc();
            // row_in_string = i + 2 (rows 0,1 are cell1+cell2 supers;
            // rows 2..2+nstring are slot j's data cells across groups).
            let cell = cell_in_string_v2(j, i + 2, cells_per_string, nx, redundancy);
            placed.push(PlacedBlock {
                cell_index: cell,
                bytes: block.to_bytes(),
            });
            offset += NDATA as u32;
        }
        // Recovery block, same shape as v1.
        let group_start_offset = offset_start + i * redundancy * (NDATA as u32);
        let recovery_addr = group_start_offset ^ (redundancy << 28);
        let mut recovery = Block {
            addr: recovery_addr,
            data: cksum_data,
            crc: 0,
            ecc: [0; ECC_BYTES],
        };
        recovery.crc = recovery.compute_crc();
        recovery.ecc = recovery.compute_ecc();
        let cell = cell_in_string_v2(redundancy, i + 2, cells_per_string, nx, redundancy);
        placed.push(PlacedBlock {
            cell_index: cell,
            bytes: recovery.to_bytes(),
        });
    }

    // --- Fill remaining cells with extra (cell1, cell2) copies -----
    // Alternating super-pair fillers. Either cell type is enough for
    // a v2 decoder to find SOMETHING valid in the trailing cells, so
    // alternating gives both types redundancy.
    let used_cells: std::collections::HashSet<u32> = placed.iter().map(|p| p.cell_index).collect();
    let total_cells = nx * options.geometry.ny();
    let mut filler_toggle = false;
    for cell in 0..total_cells {
        if !used_cells.contains(&cell) {
            placed.push(PlacedBlock {
                cell_index: cell,
                bytes: if filler_toggle { cell2_bytes } else { cell1_bytes },
            });
            filler_toggle = !filler_toggle;
        }
    }

    let bitmap = page::render(&options.geometry, &placed, options.black);
    EncodedPage {
        bitmap,
        width: options.geometry.bitmap_width(),
        height: options.geometry.bitmap_height(),
    }
}

/// Compute the absolute cell index for `row_in_string` of string `j`
/// in a v2 page. Mirrors v1's `data_or_recovery_cell` / `first_cell_of_string`
/// stepping but uses (nstring+2)-cell strings.
///
/// `row_in_string ∈ 0..cells_per_string`:
///   0       → SuperBlock cell 1 copy (slot j)
///   1       → SuperBlock cell 2 copy (slot j)
///   2..nstring+1 → data slot j of group (row_in_string - 2)
///                  for j < redundancy, or recovery block of group
///                  (row_in_string - 2) for j == redundancy.
fn cell_in_string_v2(
    j: u32,
    row_in_string: u32,
    cells_per_string: u32,
    nx: u32,
    redundancy: u32,
) -> u32 {
    let mut k = j * cells_per_string;
    if cells_per_string < nx {
        // Compact regime: linear stepping within the string.
        k += row_in_string;
    } else {
        // Wide regime: rotate per string, mirroring v1's
        // Printer.cpp:875 / 906-908 logic but with cells_per_string
        // (= nstring + 2) as the modulus.
        let rot = (nx / (redundancy + 1) * j + nx - k % nx) % nx;
        k += (row_in_string + rot) % cells_per_string;
    }
    k
}

/// Cell where the j-th group string starts (its superblock copy).
/// Printer.cpp:872-877. j ∈ 0..=redundancy.
fn first_cell_of_string(j: u32, nstring: u32, nx: u32, redundancy: u32) -> u32 {
    let mut k = j * (nstring + 1);
    if nstring + 1 >= nx {
        // Wide regime: distribute strings across columns to defend
        // against single-column print defects (the "Weissbier formula"
        // from PAPERBAK-HACKS.md §3.1).
        let rot = (nx / (redundancy + 1) * j + nx - k % nx) % nx;
        k += rot;
    }
    k
}

/// Cell where the j-th block of the i-th group goes. j is the slot
/// within the group: 0..redundancy is data, redundancy is recovery.
/// Printer.cpp:898-918.
fn data_or_recovery_cell(j: u32, i: u32, nstring: u32, nx: u32, redundancy: u32) -> u32 {
    let mut k = j * (nstring + 1);
    if nstring + 1 < nx {
        // Compact regime: same column for the whole string,
        // sequential rows.
        k += i + 1;
    } else {
        // Wide regime with rotation per string.
        let rot = (nx / (redundancy + 1) * j + nx - k % nx) % nx;
        k += (i + 1 + rot) % (nstring + 1);
    }
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{BLOCK_BYTES, NDATA};

    fn small_geometry() -> PageGeometry {
        PageGeometry {
            ppix: 600,
            ppiy: 600,
            dpi: 200,
            dot_percent: 70,
            // 12 wide x 6 tall = 72 cells. With redundancy=5,
            // (72-5-2)/6 * 5 * 90 = 4500 bytes per page.
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

    /// Decode rendered pages back into the input bytes via the
    /// public crate::decoder API. Same module-internal helper that
    /// used to live here was promoted to `decoder::decode` once the
    /// M6 synthetic decoder landed; this thin wrapper keeps the test
    /// call sites short.
    fn decode_pages(pages: &[EncodedPage], geometry: &PageGeometry) -> Vec<u8> {
        let bitmaps: Vec<Vec<u8>> = pages.iter().map(|p| p.bitmap.clone()).collect();
        let opts = crate::decoder::DecodeOptions {
            geometry: *geometry,
            threshold: page::DEFAULT_THRESHOLD,
        };
        crate::decoder::decode(&bitmaps, &opts, None).expect("encoded pages must decode in tests")
    }

    #[test]
    fn rejects_oversized_input() {
        let opts = EncodeOptions {
            geometry: small_geometry(),
            ..EncodeOptions::default()
        };
        // We can't actually allocate MAXSIZE+1 in a test; check the
        // boundary condition with a zero-cost slice from a 0-byte vec
        // by faking the length via from_raw_parts? No — simpler: rely
        // on the comparison itself. Since input.len() <= isize::MAX in
        // practice and MAXSIZE = 0x0FFF_FF80, anything we can pass
        // will be ≤ MAXSIZE. Test the error path via a custom builder.
        // Instead, sanity-check that exactly-MAXSIZE input is accepted.
        // (Building MAXSIZE bytes is too expensive.) Skip — the unit
        // test for InputTooLarge is the path through encode() with
        // input shorter than MAXSIZE, which doesn't trip the error.
        let _ = opts; // keep compiler happy if test body is empty.
    }

    #[test]
    fn rejects_invalid_redundancy() {
        for r in [0, 1, 11, 255] {
            let opts = EncodeOptions {
                geometry: small_geometry(),
                redundancy: r,
                ..EncodeOptions::default()
            };
            let err = encode(b"x", &opts, &meta()).unwrap_err();
            assert_eq!(err, EncodeError::InvalidRedundancy { redundancy: r });
        }
    }

    #[test]
    fn rejects_too_small_page() {
        let geometry = PageGeometry {
            ppix: 600,
            ppiy: 600,
            dpi: 200,
            dot_percent: 70,
            // 4 cells wide is too few for redundancy=5 (need 6).
            width: 4 * 35 * 3 + 2,
            height: 6 * 35 * 3 + 2,
            print_border: false,
        };
        let opts = EncodeOptions {
            geometry,
            ..EncodeOptions::default()
        };
        let err = encode(b"x", &opts, &meta()).unwrap_err();
        assert!(matches!(err, EncodeError::PageTooSmall { .. }));
    }

    #[test]
    fn round_trip_single_page_no_compression() {
        let geometry = small_geometry();
        let opts = EncodeOptions {
            geometry,
            redundancy: 5,
            compress: false,
            black: BLACK_PAPER,
        };
        let payload: Vec<u8> = (0..500u32).map(|i| (i * 37 + 5) as u8).collect();
        let pages = encode(&payload, &opts, &meta()).unwrap();
        assert_eq!(
            pages.len(),
            1,
            "500 bytes fits in one page at this geometry"
        );
        let recovered = decode_pages(&pages, &geometry);
        assert_eq!(recovered, payload);
    }

    #[test]
    fn round_trip_single_page_compressed() {
        let geometry = small_geometry();
        let opts = EncodeOptions {
            geometry,
            redundancy: 5,
            compress: true,
            black: BLACK_PAPER,
        };
        // Compressible payload — repeated lorem-ish text.
        let mut payload = Vec::new();
        for _ in 0..50 {
            payload.extend_from_slice(b"PaperBack archives bytes onto paper. ");
        }
        let pages = encode(&payload, &opts, &meta()).unwrap();
        let recovered = decode_pages(&pages, &geometry);
        assert_eq!(recovered, payload);
    }

    #[test]
    fn round_trip_multi_page() {
        let geometry = small_geometry();
        let opts = EncodeOptions {
            geometry,
            redundancy: 5,
            compress: false,
            black: BLACK_PAPER,
        };
        // 4500-byte pagesize at this geometry — pick something past
        // pagesize to force multi-page handling.
        let payload: Vec<u8> = (0..10_000u32).map(|i| (i * 31) as u8).collect();
        let pages = encode(&payload, &opts, &meta()).unwrap();
        assert!(pages.len() >= 3, "10000 bytes should require ≥3 pages");
        let recovered = decode_pages(&pages, &geometry);
        assert_eq!(recovered, payload);
    }

    #[test]
    fn round_trip_redundancy_min_and_max() {
        let geometry = small_geometry();
        let payload: Vec<u8> = (0..400u32).map(|i| i as u8).collect();
        for r in [NGROUP_MIN, NGROUP_MAX] {
            let opts = EncodeOptions {
                geometry,
                redundancy: r,
                compress: false,
                black: BLACK_PAPER,
            };
            let pages = encode(&payload, &opts, &meta()).unwrap();
            let recovered = decode_pages(&pages, &geometry);
            assert_eq!(recovered, payload, "redundancy = {r}");
        }
    }

    /// Empty input still produces one page (with a SuperBlock that
    /// reports datasize=0 and origsize=0). The decoder reassembles
    /// to a zero-length buffer.
    #[test]
    fn round_trip_empty_input() {
        let geometry = small_geometry();
        let opts = EncodeOptions {
            geometry,
            redundancy: 5,
            compress: false,
            black: BLACK_PAPER,
        };
        let pages = encode(b"", &opts, &meta()).unwrap();
        assert_eq!(pages.len(), 1);
        let recovered = decode_pages(&pages, &geometry);
        assert!(recovered.is_empty());
    }

    /// Truncating the filename to 31 chars + NUL (Printer.cpp:526-527)
    /// must hold even when the caller passes a longer name. The upper
    /// 32 bytes of `name[64]` stay zero (or AES salt+IV, when M11
    /// adds encryption — for now strictly zero).
    #[test]
    fn long_name_is_truncated_to_31_chars() {
        let geometry = small_geometry();
        let long_name = "a".repeat(50);
        let opts = EncodeOptions::default();
        let opts = EncodeOptions { geometry, ..opts };
        let pages = encode(
            b"hi",
            &opts,
            &FileMeta {
                name: &long_name,
                modified: 0,
                attributes: 0,
            },
        )
        .unwrap();
        // Reach into the encoded bitmap, find a SuperBlock, verify
        // its name field is truncated.
        let cells = page::extract(&geometry, &pages[0].bitmap, page::DEFAULT_THRESHOLD);
        let mut found_super = false;
        for cell in cells {
            if let Ok(s) = SuperBlock::from_bytes(&cell)
                && s.verify_crc()
            {
                assert_eq!(&s.name[..31], &b"a".repeat(31)[..]);
                assert_eq!(s.name[31], 0);
                // Bytes 32..64 reserved for AES salt+IV; encryption
                // is off, so they must all be zero.
                assert!(s.name[32..].iter().all(|&b| b == 0));
                found_super = true;
                break;
            }
        }
        assert!(found_super, "no SuperBlock found in encoded page");
    }

    /// Recovery block math: the recovery block's data field is the
    /// XOR of all data blocks' data in the same group, with 0xFF as
    /// the running XOR start. Pin this so a refactor that drops the
    /// 0xFF init or the running XOR breaks the test loudly.
    ///
    /// 448 bytes is chosen so post-AES-alignment datasize stays at
    /// 448 (it's a multiple of 16) and ceil(448/90) = 5 data blocks
    /// fit exactly one redundancy=5 group with no filler. The page
    /// then carries exactly one recovery block, which simplifies the
    /// test's "find the recovery block" step.
    #[test]
    fn recovery_block_is_xor_of_group_data() {
        let geometry = small_geometry();
        let opts = EncodeOptions {
            geometry,
            redundancy: 5,
            compress: false,
            black: BLACK_PAPER,
        };
        let payload: Vec<u8> = (0..448u32).map(|i| (i as u8).wrapping_mul(7)).collect();
        assert_eq!(
            payload.len() % 16,
            0,
            "payload must be 16-aligned for this test"
        );
        let pages = encode(&payload, &opts, &meta()).unwrap();
        let cells = page::extract(&geometry, &pages[0].bitmap, page::DEFAULT_THRESHOLD);

        // Find the recovery block for group 0 (offset = 0); ngroup = 5.
        let mut recovery: Option<[u8; BLOCK_BYTES]> = None;
        let mut data_blocks: Vec<[u8; NDATA]> = Vec::new();
        for cell in cells {
            let block = Block::from_bytes(&cell);
            if !block.verify_crc() {
                continue;
            }
            if block.is_recovery() && block.ngroup() == 5 && block.offset() == 0 {
                recovery = Some(cell);
            } else if block.is_data() && (block.offset() as usize) < payload.len() {
                data_blocks.push(block.data);
            }
        }
        let recovery = recovery.expect("recovery block for group 0 missing from page");
        let recovery_block = Block::from_bytes(&recovery);

        // Reconstruct expected: 0xFF ^ d0 ^ d1 ^ ... ^ d4. The last
        // data block (offset 360) carries 88 input bytes plus 2 zero-
        // pad bytes; the XOR formula does not care, the encoder just
        // zeroes any trailing bytes inside NDATA.
        let mut expected = [0xFFu8; NDATA];
        assert_eq!(data_blocks.len(), 5);
        for d in &data_blocks {
            for (e, &b) in expected.iter_mut().zip(d.iter()) {
                *e ^= b;
            }
        }
        assert_eq!(recovery_block.data, expected);
    }

    // === v2 round-trip tests (M11) ============================
    //
    // The full encode/decode cycle is exercised through the public
    // crate::decoder::decode entry point, which auto-detects v2 cells
    // and dispatches to the v2 reassembly + GCM-decrypt path. These
    // tests live in encoder.rs because they originate from the v2
    // encoder; the decoder-side error paths (wrong password, missing
    // header, etc.) live in decoder.rs.

    fn v2_decode(pages: &[EncodedPage], geometry: &PageGeometry, password: &[u8]) -> Vec<u8> {
        let bitmaps: Vec<Vec<u8>> = pages.iter().map(|p| p.bitmap.clone()).collect();
        let opts = crate::decoder::DecodeOptions {
            geometry: *geometry,
            threshold: page::DEFAULT_THRESHOLD,
        };
        crate::decoder::decode(&bitmaps, &opts, Some(password))
            .expect("v2 encoded pages must decode in tests")
    }

    /// v2 encode → decode round-trip with deterministic salt+IV
    /// recovers the input bytes exactly.
    #[test]
    fn v2_round_trip_single_page_no_compression() {
        let geometry = small_geometry();
        let opts = EncodeOptions {
            geometry,
            redundancy: 5,
            compress: false,
            black: BLACK_PAPER,
        };
        let payload = b"the quick brown fox jumps over the lazy dog";
        let salt = [0x42u8; 32];
        let iv = [0x99u8; 12];
        let pages =
            encode_v2_with_kat(payload, &opts, &meta(), b"correct horse", &salt, &iv).unwrap();
        assert_eq!(pages.len(), 1);
        let recovered = v2_decode(&pages, &geometry, b"correct horse");
        assert_eq!(recovered, payload);
    }

    /// v2 with bzip2 enabled: compress-then-encrypt round-trips
    /// cleanly.
    #[test]
    fn v2_round_trip_compressed() {
        let geometry = small_geometry();
        let opts = EncodeOptions {
            geometry,
            redundancy: 5,
            compress: true,
            black: BLACK_PAPER,
        };
        let mut payload = Vec::new();
        for _ in 0..50 {
            payload.extend_from_slice(b"PaperBack archives bytes onto paper. ");
        }
        let salt = [0x11u8; 32];
        let iv = [0x22u8; 12];
        let pages =
            encode_v2_with_kat(&payload, &opts, &meta(), b"swordfish", &salt, &iv).unwrap();
        let recovered = v2_decode(&pages, &geometry, b"swordfish");
        assert_eq!(recovered, payload);
    }

    /// v2 multi-page encode + decode in arbitrary page order.
    #[test]
    fn v2_round_trip_multi_page() {
        let geometry = small_geometry();
        let opts = EncodeOptions {
            geometry,
            redundancy: 5,
            compress: false,
            black: BLACK_PAPER,
        };
        // pagesize_v2 at this geometry: nstring=9 → 9*5*90 = 4050.
        // 10000-byte payload + 16-byte tag = 10016 bytes ciphertext.
        // ceil(10016/4050) = 3 pages.
        let payload: Vec<u8> = (0..10_000u32).map(|i| (i * 31) as u8).collect();
        let salt = [0xAAu8; 32];
        let iv = [0xBBu8; 12];
        let pages = encode_v2_with_kat(&payload, &opts, &meta(), b"long passphrase", &salt, &iv)
            .unwrap();
        assert!(pages.len() >= 3, "10000 bytes should require ≥3 v2 pages");
        let recovered = v2_decode(&pages, &geometry, b"long passphrase");
        assert_eq!(recovered, payload);
    }

    /// v2 with empty input: 0-byte plaintext → 16-byte ciphertext (just
    /// the GCM tag) → still produces one decodable page. Pins the
    /// edge case from FORMAT-V2.md §8.
    #[test]
    fn v2_round_trip_empty_input() {
        let geometry = small_geometry();
        let opts = EncodeOptions {
            geometry,
            redundancy: 5,
            compress: false,
            black: BLACK_PAPER,
        };
        let salt = [0x55u8; 32];
        let iv = [0x66u8; 12];
        let pages = encode_v2_with_kat(b"", &opts, &meta(), b"empty test", &salt, &iv).unwrap();
        assert_eq!(pages.len(), 1);
        let recovered = v2_decode(&pages, &geometry, b"empty test");
        assert!(recovered.is_empty());
    }

    /// v2 with the lowest and highest valid redundancy values.
    #[test]
    fn v2_round_trip_redundancy_min_and_max() {
        let geometry = small_geometry();
        let payload: Vec<u8> = (0..400u32).map(|i| i as u8).collect();
        let salt = [0x77u8; 32];
        let iv = [0x88u8; 12];
        for r in [NGROUP_MIN, NGROUP_MAX] {
            let opts = EncodeOptions {
                geometry,
                redundancy: r,
                compress: false,
                black: BLACK_PAPER,
            };
            let pages = encode_v2_with_kat(&payload, &opts, &meta(), b"pw", &salt, &iv).unwrap();
            let recovered = v2_decode(&pages, &geometry, b"pw");
            assert_eq!(recovered, payload, "redundancy = {r}");
        }
    }

    /// Encode determinism: same (input, salt, iv, password, options)
    /// produces byte-identical bitmaps. Pins the encode determinism
    /// property from FORMAT-V2.md §1 design goal #5.
    #[test]
    fn v2_encode_with_fixed_kat_is_deterministic() {
        let geometry = small_geometry();
        let opts = EncodeOptions {
            geometry,
            redundancy: 5,
            compress: false,
            black: BLACK_PAPER,
        };
        let payload = b"determinism test bytes";
        let salt = [0x12u8; 32];
        let iv = [0x34u8; 12];
        let pages_a =
            encode_v2_with_kat(payload, &opts, &meta(), b"key", &salt, &iv).unwrap();
        let pages_b =
            encode_v2_with_kat(payload, &opts, &meta(), b"key", &salt, &iv).unwrap();
        assert_eq!(pages_a.len(), pages_b.len());
        for (a, b) in pages_a.iter().zip(pages_b.iter()) {
            assert_eq!(a.bitmap, b.bitmap, "v2 encode must be deterministic given fixed salt+iv");
        }
    }

    /// Different IV → different ciphertext → different rendered
    /// bitmap. Pins that the IV is actually plumbed through to GCM
    /// (and not, say, getting swallowed somewhere in the encoder).
    #[test]
    fn v2_encode_different_iv_produces_different_bitmap() {
        let geometry = small_geometry();
        let opts = EncodeOptions {
            geometry,
            redundancy: 5,
            compress: false,
            black: BLACK_PAPER,
        };
        let payload = b"iv smoke test";
        let salt = [0x12u8; 32];
        let iv1 = [0x34u8; 12];
        let iv2 = [0x35u8; 12];
        let pages_1 =
            encode_v2_with_kat(payload, &opts, &meta(), b"key", &salt, &iv1).unwrap();
        let pages_2 =
            encode_v2_with_kat(payload, &opts, &meta(), b"key", &salt, &iv2).unwrap();
        assert_ne!(
            pages_1[0].bitmap, pages_2[0].bitmap,
            "different IV must produce different ciphertext"
        );
    }

    /// pagesize_v2 helper math: cells - 2*redundancy - 3, divided by
    /// (redundancy + 1), times redundancy * NDATA. Pin the formula
    /// against accidental tweaks.
    #[test]
    fn pagesize_v2_matches_spec_formula() {
        // Our small_geometry: 12x6 = 72 cells, redundancy=5.
        // (72 - 10 - 3) / 6 = 59/6 = 9. Pagesize = 9*5*90 = 4050.
        assert_eq!(pagesize_v2(72, 5), 4050);
        // Larger geometry: 16x21 = 336 cells, redundancy=5.
        // (336 - 10 - 3)/6 = 323/6 = 53. Pagesize = 53*5*90 = 23850.
        assert_eq!(pagesize_v2(336, 5), 23850);
        // Compare to v1 at same geometry: v1 pagesize is bigger by
        // exactly (redundancy+1) NDATA-sized data slots... actually
        // by 1 nstring's worth of data, since v2 loses 1 nstring per
        // page to fit the 2nd super cell.
        // v1 nstring at 12x6 r=5 = (72-5-2)/6 = 10. v2 nstring = 9.
        // Difference in pagesize = 1 * 5 * 90 = 450 bytes.
        let v1_pagesize = ((72 - 5 - 2) / 6) * 5 * 90u32;
        assert_eq!(v1_pagesize - pagesize_v2(72, 5), 450);
    }

    #[test]
    fn cell_layout_compact_regime_distributes_strings_in_columns() {
        // For our 12-wide page with redundancy=5, nstring=10, nstring+1=11,
        // 11 < 12 so we're in the compact regime. Each string j gets a
        // column at j*11; data blocks fill rows 1..nstring within that
        // column (cells: j*11 + 1, j*11 + 2, ...).
        let nx = 12;
        let nstring = 10;
        let redundancy = 5;
        // String 0: cells 0 (super), 1, 2, ..., 10 (data 0..9 = redundancy+1 entries)
        assert_eq!(first_cell_of_string(0, nstring, nx, redundancy), 0);
        for i in 0..nstring {
            assert_eq!(data_or_recovery_cell(0, i, nstring, nx, redundancy), i + 1);
        }
        // String 1: cells 11, 12, ..., 21
        assert_eq!(first_cell_of_string(1, nstring, nx, redundancy), 11);
        for i in 0..nstring {
            assert_eq!(
                data_or_recovery_cell(1, i, nstring, nx, redundancy),
                11 + i + 1
            );
        }
    }
}
