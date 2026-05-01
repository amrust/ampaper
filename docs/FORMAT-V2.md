# ampaper v2 — bitstream format

Wire spec for ampaper's forward-compatible v2 format. Companion to
[FORMAT-V1.md](FORMAT-V1.md), which documents the legacy PaperBack
1.10 format ampaper reads but never writes. v2 is what `ampaper`
produces when the user asks for encrypted output, and the substrate
M12 (color, adaptive RS, density experiments) extends.

The same physical block-on-paper layout from FORMAT-V1.md §2 carries
v2 — same RS(255,223,33) shortened to (128,96), same CRC-16, same
dot grid + sync raster + page geometry. v2 changes only the
**logical layer above the block**: the SuperBlock and the
optional encryption envelope.

> **Status.** Spec frozen 2026-05-01; implementation lives in
> `src/v2.rs` + `src/format_v2.rs` (lands at M11). v1 decoders see
> v2 cells with `addr` outside `[0, MAXSIZE)` and `ngroup() == 15`,
> which falls outside the valid recovery range — they silently skip
> the cells, so v2 input neither corrupts nor confuses a v1 reader.

---

## 1. Design goals

Ranked. Conflicts resolve in favor of the higher entry.

1. **Authenticated encryption with strong KDF.** AES-256-GCM with
   PBKDF2-HMAC-SHA-256 at 600,000 iterations. Full 128-bit GCM tag,
   no truncation. Per `docs/ENCRYPTION-DECISION.md`.
2. **Layer cleanness.** Encryption sits ABOVE the block layer.
   M12's color encoding, adaptive (n, k) RS, and dot-shape
   experiments are layer-orthogonal — they cannot break or
   complicate the v2 crypto envelope.
3. **Forward extensibility.** Reserved bits in feature flags +
   reserved bytes in the v2 SuperBlock cell 2 give room to mark
   future encoding modes without bumping the format major version.
4. **Graceful rejection by v1 decoders.** v1 readers fed a v2 file
   silently skip every v2 SuperBlock cell rather than corrupting
   anything. v1 cannot decode v2 — it has no key, no GCM —
   but it does not produce wrong output either.
5. **Single-encode determinism.** Given a fixed (plaintext, password,
   options), the encoded bytes are deterministic except for the
   KDF salt and GCM IV (both fresh OS entropy per encode). Tests
   pin this via known-salt/known-iv vectors.

---

## 2. v2 SuperBlock layout

The v2 SuperBlock occupies **two cells** instead of v1's one.
Each cell is a normal 128-byte block on the wire (so RS + CRC
remain identical to v1) with a v2-specific `addr` value as its
type marker:

| Cell role | `addr` value | v1 decoder sees |
|---|---|---|
| v2 SuperBlock part 1 | `0xFFFFFFFE` | `is_super()=false`, `ngroup()=15` (out of range) → silently skipped |
| v2 SuperBlock part 2 | `0xFFFFFFFD` | same — silently skipped |

Both cells are placed on every page, duplicated like the v1
SuperBlock (`redundancy + 1` copies per group string + filler in
trailing cells). A v2 page is fully self-describing if either part 1
copy + either part 2 copy survive scanning.

### 2.1. Cell 1 — file metadata

`addr = 0xFFFFFFFE`. The 90-byte data field carries:

```
offset  size  field            notes
  0       1   format_version   = 2 for current spec; future bumps reserved
  1       1   feature_flags    bitfield, see §2.3
  2       2   page             1-based page index (LE u16)
  4       2   page_count       total pages in this encode (LE u16)
  6       4   datasize         ciphertext + 16-byte GCM tag (LE u32)
 10       4   origsize         plaintext byte count, pre-compress (LE u32)
 14       4   pagesize         bytes of (compressed) data per page (LE u32)
 18       8   modified         Win32 FILETIME, u64 LE
 26      64   name[64]         UTF-8, NUL-terminated; max 63 chars + NUL
                               (no AES salt+IV stuffed in here — that hack
                               from v1 §3.2 is dropped; v2 has cell 2 for
                               crypto material)
total: 90 bytes
```

### 2.2. Cell 2 — crypto envelope

`addr = 0xFFFFFFFD`. The 90-byte data field carries:

```
offset  size  field            notes
  0      32   kdf_salt         32 bytes of OS entropy. PBKDF2 salt.
                               Empty (all zeros) when feature_flags bit 0
                               (encrypted) is unset.
 32      12   gcm_iv           96-bit GCM nonce. Empty when not encrypted.
 44      46   reserved         Zero-filled. Reserved for future features
                               (M12 adaptive-RS parameters, color profile
                               tag, dot-shape descriptor, etc.).
total: 90 bytes
```

### 2.3. Feature flags (cell 1 byte 1)

```
bit 0    PBM_V2_ENCRYPTED        AES-256-GCM envelope active
bit 1    PBM_V2_COMPRESSED       bzip2 wrapped around plaintext (pre-encrypt)
bit 2    reserved (color encoding — M12)
bit 3    reserved (adaptive RS — M12)
bit 4    reserved (dot-shape variant — M12)
bit 5    reserved (hexagonal packing — M12)
bit 6-7  reserved
```

Decoders MUST reject (with a clear error) any v2 file whose
`feature_flags` has reserved bits set that they don't understand.
This is the forward-compat lever: M12 sets bit 2 to mark a color-
encoded file, and pre-M12 v2 readers fail loudly with
`UnsupportedFeature(bit_2)` rather than producing garbage.

### 2.4. Why two cells

90 bytes per cell is too tight for v2's metadata + crypto material:

- KDF salt (32) + GCM IV (12) = 44 bytes of pure crypto in cell 2.
- v1 SuperBlock ate 4 (datasize) + 4 (pagesize) + 4 (origsize) +
  1 (mode) + 1 (attributes) + 2 (page) + 8 (modified) + 2 (filecrc)
  + 64 (name) = 90 bytes for non-crypto metadata. Adding 44 bytes
  of crypto pushes us to 134.
- Removing fields to fit (truncating filename, dropping modified)
  loses real value. The v1 "stuff salt+IV in name[64]" hack
  (PAPERBAK-HACKS.md §2.1) is exactly the kind of fragility we want
  to NOT inherit.

Two cells gives 180 bytes of usable space. 90 for metadata + 90 for
crypto is clean, leaves 46 bytes of cell-2 reserved space for M12,
and matches v1's "SuperBlock as a special block" pattern naturally.

---

## 3. Encryption envelope

Plain language: AES-256-GCM with file-level tag, computed once over
the entire encrypted buffer. Not per-block. See
`docs/ENCRYPTION-DECISION.md` for why.

### 3.1. Inputs

- `password` — caller-supplied byte string. May be a passphrase,
  a hardware-derived secret, etc.
- `kdf_salt` — 32 bytes, fresh `getrandom` per encode.
- `gcm_iv` — 12 bytes, fresh `getrandom` per encode.
- `plaintext` — the bytes the user wants encoded. May be
  pre-compressed via bzip2 if `feature_flags` bit 1 is set.

### 3.2. Key derivation

```
key = PBKDF2-HMAC-SHA-256(password, kdf_salt, iter=600_000, dkLen=32)
```

600,000 iterations is the OWASP 2023 minimum for SHA-256 PBKDF2.
The iteration count is **part of the format** — it is NOT stored
on the wire; both encoder and decoder use the constant. If we ever
need to bump it, that is a v3 format break.

### 3.3. AAD construction

The Associated Authenticated Data passed to AES-GCM binds the tag
to structural metadata that isn't a cipher input on its own:

```
AAD = b"ampaper-v2-aad"          (14 bytes, ASCII literal)
   || feature_flags               (1 byte)
   || page_count.to_le_bytes()    (2 bytes — u16 LE)
   || origsize.to_le_bytes()      (4 bytes — u32 LE)
   || datasize.to_le_bytes()      (4 bytes — u32 LE)
                                  total: 25 bytes
```

Rationale per field:

- **Magic string** prevents tag collisions if a future format vN
  uses GCM with a different envelope.
- **feature_flags** binds the tag to "is this file claimed to be
  compressed" etc., so an attacker can't flip flag bits to make
  the decoder mis-handle the plaintext.
- **page_count** prevents page-truncation attacks: removing pages
  changes the AAD, tag fails.
- **origsize / datasize** prevent length-tampering attacks.

The AAD is computed locally by both encoder and decoder from the
v2 SuperBlock. It is NOT stored on the wire as a separate field;
re-deriving it from the SuperBlock fields is what makes tampering
detectable.

### 3.4. Encryption

```
ciphertext_with_tag = AES-256-GCM-encrypt(
    key       = key,
    iv        = gcm_iv,
    plaintext = plaintext,
    aad       = AAD,
)
// ciphertext_with_tag.len() == plaintext.len() + 16
```

The encoder writes `ciphertext_with_tag` to the data blocks across
the page(s). `datasize` = `ciphertext_with_tag.len()`.

Tag placement: appended to the ciphertext, so the last 16 bytes of
the recovered data buffer are the tag. The decoder splits as
`(ciphertext, tag) = (recovered[..datasize-16], recovered[datasize-16..])`.

### 3.5. Decryption

```
plaintext = AES-256-GCM-decrypt(
    key       = derive_key_v2(password, kdf_salt),
    iv        = gcm_iv,
    ciphertext_with_tag = recovered_buffer,
    aad       = derive_aad(superblock),
)
```

Failure modes:
- Wrong password → key mismatch → tag verification fails →
  `DecodeError::InvalidPassword`.
- Tampered ciphertext or AAD → tag verification fails → same error.
- Truncated buffer (`datasize` claims more bytes than blocks
  recovered) → `DecodeError::UnrecoverableGap` from the block-level
  recovery pass before decrypt is even attempted.

Once the GCM tag verifies, the bytes are authenticated. If the
file was bzip2-compressed pre-encrypt (`feature_flags` bit 1),
decompress the plaintext to recover the original input.

---

## 4. Data flow

### 4.1. Encode

```
plaintext  →  [bzip2 if compress]  →  buf
buf  →  AES-256-GCM(key, gcm_iv, AAD)  →  ciphertext_with_tag
ciphertext_with_tag  →  split into NDATA chunks  →  data blocks
data blocks + RS recovery + v2 SuperBlock copies (cells 1 & 2)
                                              →  cell layout
cell layout  →  page::render  →  bitmap
```

Page layout (cell placement) is the same as v1's group-string
layout (FORMAT-V1.md §5): `redundancy + 1` strings per page, each
starting with a v2 SuperBlock pair (cells 1 and 2 in adjacent
positions), followed by `nstring` data blocks and one recovery
block. Trailing cells fill with extra v2 SuperBlock pair copies.

### 4.2. Decode

```
bitmap(s)  →  page::extract or scan_extract  →  cells
cells  →  CRC-filter, RS-correct  →  valid blocks
valid blocks classified by addr:
    0xFFFFFFFE  →  v2 SuperBlock cell 1 (first valid copy)
    0xFFFFFFFD  →  v2 SuperBlock cell 2 (first valid copy)
    others      →  data blocks (offset = addr & 0x0FFFFFFF)
SuperBlock cell 1 + cell 2  →  metadata + crypto envelope
data blocks reassembled into buf of size datasize
buf split as (ciphertext, tag)
AES-256-GCM-decrypt(ciphertext, key, gcm_iv, AAD, tag)  →  plaintext
[bzip2 if compress flag]  →  original input
```

Decoder rejects with explicit errors at every checkpoint:

- No v2 cell-1 found → `NoSuperBlock`
- v2 cell-1 found but no v2 cell-2 → `IncompleteV2Header`
- `format_version != 2` → `UnsupportedFormatVersion`
- Unknown bits set in `feature_flags` → `UnsupportedFeature`
- Encrypted but no password → `PasswordRequired`
- Tag verification fails → `InvalidPassword`

---

## 5. Compatibility with v1

A v1 decoder reading a v2 page sees:

- Cells with `addr == 0xFFFFFFFE` or `0xFFFFFFFD`. v1 logic:
  ```
  is_super()    = (addr == 0xFFFFFFFF)         → false
  is_recovery() = !is_super && ngroup() != 0   → true (ngroup=15)
  ```
  v1 then checks `(NGROUP_MIN..=NGROUP_MAX).contains(&ngroup)`,
  i.e., `2..=10`. 15 is out of range, so v1 silently drops the
  cell from `recovery_blocks`. The cell contributes nothing to v1's
  reassembly.
- Data blocks with `addr` in `0..datasize`. v1 will treat these as
  ordinary data and try to reassemble them. Without a v1 SuperBlock
  it will fail with `NoSuperBlock`. If a v2 file happens to be on
  the same page as a v1 file (impossible in practice — encoders
  produce one or the other), v1 might decode the v1 file and treat
  v2 data as filler past `datasize`.

In short: a v1 decoder fed v2 input fails with `NoSuperBlock` and
produces no output. There is no risk of v1 silently mis-decoding
v2 data.

A v2 decoder reading a v1 page sees no cells with `addr ∈
{0xFFFFFFFE, 0xFFFFFFFD}` and falls back to v1 path, which is
accepted with a logged warning per the legacy-AES-192 read posture.

---

## 6. Test vectors

Test vectors are committed under `tests/golden/v2/` once M11
implementation lands. Each vector is a (input, password, kdf_salt,
gcm_iv, expected_bitmap) tuple. The deterministic-with-known-salt-
and-iv property lets tests assert byte-equal encoding without
randomness.

KAT (known-answer test) seeds:

| Vector | Plaintext | Password | KDF salt | GCM IV |
|---|---|---|---|---|
| `simple` | `b"hello, ampaper v2"` | `b"correct horse"` | 32 × 0x42 | 12 × 0x99 |
| `compressed` | 100× `b"PaperBack archives"` | `b"swordfish"` | 32 × 0x11 | 12 × 0x22 |
| `multipage` | 200 KB random (LCG seed 0xCAFE) | `b"long passphrase"` | 32 × 0xAA | 12 × 0xBB |

For each, the test asserts:
1. Encoder produces the expected pixel bytes for the bitmap.
2. Decoder recovers the original plaintext byte-for-byte.
3. Decoder with wrong password returns `InvalidPassword`.

---

## 7. Reserved space and future format versions

**Cell 2 reserved 46 bytes** (offset 44 onward) accommodate:

- M12 color profile descriptor (bit 2 of feature_flags)
- M12 adaptive-RS per-page parameters (bit 3)
- M12 dot-shape descriptor (bit 4)
- M12 hexagonal packing parameters (bit 5)

Each M12 feature defines its sub-layout when implemented. Until
then, the bytes MUST be zero.

**format_version** field in cell 1 is the lever for incompatible
breaks. Bumping to 3 is reserved for future changes that can't
fit into reserved bits or reserved bytes — for example, switching
the KDF, or extending the SuperBlock to three cells.

---

## 8. Open questions for implementer review

- **Endianness inside v2 SuperBlock.** Spec says LE for all
  multi-byte fields, matching v1. Confirm before locking.
- **GCM IV length.** 12 bytes is the standard 96-bit GCM nonce.
  Some implementations use 16 bytes (extended-IV mode) — we use
  12 for spec compliance and AES-NI fast paths.
- **Compression order with encryption.** Spec says compress THEN
  encrypt (compress applies to plaintext, encrypt to compressed
  bytes). Standard practice; CRIME-style length-leak doesn't apply
  to a paper-archive threat model.
- **Empty-input handling.** A 0-byte `origsize` plaintext encrypts
  to a 16-byte ciphertext-with-tag. Confirm this round-trips
  cleanly through the page layer.
- **Multi-page page_count cap.** u16 holds 65535 pages. At the
  capacity table from v1 (~177 KB / page), that's 11 GB of
  plaintext per encode — well past the v1 `MAXSIZE` cap of 256 MB.
  v2 inherits the 28-bit-`addr` MAXSIZE cap from v1's data block
  layout; revisit if M12 lifts that.
