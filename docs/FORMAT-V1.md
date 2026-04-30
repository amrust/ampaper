# PaperBack v1.10 — bitstream format

This is the byte-level specification of the PaperBack 1.10 page format, derived from Oleh Yuschuk's source. Every constant is cited with `file:line` from `reference/paperbak-1.10.src/`. Citations are the contract; if the source disagrees with this doc, the source wins and this doc is wrong.

The goal is that a third party could implement an interoperable v1 encoder and decoder from this document alone.

---

## 1. Constants

| Symbol | Value | Source |
|---|---|---|
| `VERSIONHI` / `VERSIONLO` | 1 / 10 | `paperbak.h:26-27` |
| `NDOT` | 32 dots | block side length, `paperbak.h:50` |
| `NDATA` | 90 bytes | data payload per block, `paperbak.h:51` |
| `MAXSIZE` | 0x0FFFFF80 | max input file size (~256 MB), `paperbak.h:52` |
| `SUPERBLOCK` | 0xFFFFFFFF | sentinel `addr` for the superblock, `paperbak.h:53` |
| `NGROUP` | 5 (default) | RS group size; redundancy 1-of-(N+1), `paperbak.h:55` |
| `NGROUPMIN` / `NGROUPMAX` | 2 / 10 | range of valid ngroup, `paperbak.h:56-57` |
| `AESKEYLEN` | 24 bytes | AES-192, `paperbak.h:35` |
| `PASSLEN` | 33 (incl. NUL) | max password length, `paperbak.h:33` |
| `PACKLEN` | 65536 | bzip2 read-buffer chunk size, `paperbak.h:129` |
| `PBM_COMPRESSED` | 0x01 | mode-bit, `paperbak.h:70` |
| `PBM_ENCRYPTED` | 0x02 | mode-bit, `paperbak.h:71` |

Mode field is `uchar` on the wire (`t_superdata.mode`, `paperbak.h:78`).

## 2. Block-on-paper layout (`t_data`, 128 bytes)

A data block is 128 bytes serialized to a 32×32 dot grid. Layout is little-endian on x86 because the source was Borland C++ on Windows; everything is read/written through pointer aliasing into native structs, no explicit byte-swapping anywhere. This locks the wire format to LE.

```
offset  size  field
  0      4    addr           (ulong, little-endian)
  4     90    data[NDATA]    (uchar[90])
 94      2    crc            (ushort, little-endian)
 96     32    ecc[32]        (Reed-Solomon parity)
total: 128 bytes
```

Source: `t_data` struct at `paperbak.h:59-64`. Compile-time assertion at `paperbak.h:66-68` enforces `sizeof(t_data)==128`.

### 2.1. `addr` field

For ordinary data blocks, `addr` is the byte offset into the (compressed, padded, optionally-encrypted) data stream. Always a multiple of `NDATA` (90).

For recovery blocks (XOR-checksum blocks, see §6), the high nibble carries `ngroup`:

```
real_offset = addr & 0x0FFFFFFF
ngroup      = (addr >> 28) & 0x0F      // 0 = data block, >0 = recovery block
```

Encoder: `Printer.cpp:881` (`cksum.addr = offset ^ (redundancy<<28)`).
Decoder: `Decoder.cpp:859-862`.

For superblocks, `addr == SUPERBLOCK == 0xFFFFFFFF`. The struct is reinterpreted as `t_superdata` (§3).

### 2.2. CRC-16

Polynomial: CCITT (poly `0x1021`, init `0x0000`, no reflection, no final XOR). Lookup table at `Crc16.cpp:50-83`; algorithm at `Crc16.cpp:85-90`:

```c
crc = ((crc<<8) ^ crctab[(crc>>8) ^ data_byte]) & 0xFFFF;
```

The CRC covers `addr` + `data` (the first 94 bytes of the block: `NDATA + sizeof(ulong)`), then is XOR'd with `0x55AA` before storage to avoid trivial all-zero false matches:

```c
block->crc = (ushort)(Crc16((uchar*)block, NDATA+sizeof(ulong)) ^ 0x55AA);
```

Encoder: `Printer.cpp:174`. Decoder verification: `Decoder.cpp:235`.

### 2.3. Reed-Solomon ECC

CCSDS-style Reed-Solomon, derived from Phil Karn's `rs.c` implementation. Parameters:

| Parameter | Value | Source |
|---|---|---|
| Symbol size | 8 bits (GF(256)) | `Ecc.cpp` (alpha[256] table) |
| Field generator polynomial | 0x187 (`x^8 + x^7 + x^2 + x + 1`) | inferred from `alpha[]` at `Ecc.cpp:15-48` (0x80 << 1 wraps to 0x87 = 0x100 ^ 0x187) |
| Codeword length n | 255 | header comment, `Ecc.cpp:3` |
| Message length k | 223 | header comment; n - parity = 255 - 32 |
| Parity bytes | 32 | `block->ecc[32]` in `t_data` |
| First consecutive root (FCR) | 112 | `s[i] = data[j] ^ alpha[(index[s[i]] + (112+i)*11) % 255]` at `Ecc.cpp:126` |
| Primitive root step | 11 | same line, multiplier `11`; also visible in encode at `Ecc.cpp:101` (`poly[32-j]`) |
| Pad value | 127 | `Encode8(data, ecc, 127)` at `Printer.cpp:176`; `Decode8(data, NULL, 0, 127)` at `Decoder.cpp:231` |

The pad means: virtual codeword length is 255, first 127 bytes are virtual zeros (not transmitted), real input is `255 - 127 - 32 = 96` data bytes (= `addr` 4 + `data` 90 + `crc` 2), 32 parity bytes follow. Effective shortened code is `(128, 96)` with 32 parity bytes able to correct up to 16 errors.

Karn's poly table at `Ecc.cpp:85-91` is precomputed for this specific (FCR=112, prim=11) case.

### 2.4. Dot mapping (32×32 → 128 bytes)

Each row of the dot grid encodes one 32-bit little-endian `ulong` of the block. Block is read as `((ulong*)block)[0..31]` — index 0 is the topmost row, index 31 the bottom row.

Before placing dots, each row is XORed with a striping mask to break up large solid regions (which would cause optical decoding pain on near-zero or near-FF blocks):

```c
t = ((ulong*)block)[j];
if ((j & 1) == 0) t ^= 0x55555555;       // even rows
else              t ^= 0xAAAAAAAA;       // odd rows
```

Encoder: `Printer.cpp:179-184`. Decoder inverse: `Decoder.cpp:224-225`.

Within a row, bit 0 (LSB) is drawn at the leftmost dot column and bit 31 at the rightmost. A set bit means a black (printed) dot. Encoder: `Printer.cpp:186-196` (`if (t & 1) { ... } t >>= 1; x += dx;`).

## 3. Superblock (`t_superdata`, 128 bytes)

The superblock identifies the page and the file. Same wire size as a data block (compile-time asserted at `paperbak.h:88-90`); replaces `t_data` whenever `addr == SUPERBLOCK`. Layout from `paperbak.h:73-86`:

```
offset  size  field
  0      4    addr            == 0xFFFFFFFF (SUPERBLOCK)
  4      4    datasize        size of compressed-and-padded data, bytes
  8      4    pagesize        bytes of (compressed) data carried by this page
 12      4    origsize        size of original uncompressed file, bytes
 16      1    mode            bitmask: PBM_COMPRESSED|PBM_ENCRYPTED
 17      1    attributes      Win32 file-attribute bits (see below)
 18      2    page            1-based page index
 20      8    modified        Win32 FILETIME (100-ns ticks since 1601-01-01 UTC)
 28      2    filecrc         CRC-16 of the compressed-but-unencrypted data
 30     64    name[64]        file name; bytes 32..63 repurposed as salt+IV when encrypted
 94      2    crc             CRC-16 over the previous 94 bytes (XOR'd with 0x55AA)
 96     32    ecc[32]         Reed-Solomon parity, same scheme as t_data
total: 128 bytes
```

### 3.1. `attributes`

Encoder masks the Win32 attribute bits to a useful subset (`Printer.cpp:517-520`):

```
FILE_ATTRIBUTE_READONLY  = 0x0001
FILE_ATTRIBUTE_HIDDEN    = 0x0002
FILE_ATTRIBUTE_SYSTEM    = 0x0004
FILE_ATTRIBUTE_ARCHIVE   = 0x0020
FILE_ATTRIBUTE_NORMAL    = 0x0080
```

Stored as a single byte (other bits ignored on decode).

### 3.2. `name[64]` — dual purpose

The name field is 64 bytes but is split when encryption is on:

- **Bytes 0..31** — file name, NUL-terminated, capped at 31 chars by encoder (`Printer.cpp:526-527`).
- **Bytes 32..47** — 16-byte AES PBKDF2 salt (only when `mode & PBM_ENCRYPTED`). `Printer.cpp:472-477`.
- **Bytes 48..63** — 16-byte AES-CBC IV (only when `mode & PBM_ENCRYPTED`). `Printer.cpp:488`.

The encoder calls this layout a "hack" in a comment (`Printer.cpp:472`). It is load-bearing for legacy compatibility and ampaper's v1 read path must reproduce it exactly.

### 3.3. Per-page redundancy

Every page carries `redundancy + 1` superblock copies (one per group string), plus more superblocks padded into any cells unused at end-of-page (`Printer.cpp:921-924`). Decoder accepts the first valid superblock seen on the page (`Decoder.cpp:843-855`).

## 4. Per-page geometry

All measurements are in printer-resolution pixels (`ppix`/`ppiy` from `GetDeviceCaps`).

### 4.1. Dot pitch and dot size

```
dx = max(ppix / dpi,          2)   // X dot pitch
dy = max(ppiy / dpi,          2)   // Y dot pitch
px = max((dx * dotpercent)/100, 1) // X dot size
py = max((dy * dotpercent)/100, 1) // Y dot size
```

Source: `Printer.cpp:665-668`. `dpi` and `dotpercent` are user options (default `dpi=200`, `dotpercent=70`).

Floor of 2 on `dx`/`dy` means each cell is at least 2×2 device pixels — the "dot density ≤ ½ × printer DPI" rule from the README is emergent from this floor.

### 4.2. Cell size

Each block's cell on the page is `(NDOT+3)*dx` × `(NDOT+3)*dy`. The +3 splits as: 2 dots of "border" before the data dots and 1 dot of "border" after (see §4.4). `Printer.cpp:170-171`.

### 4.3. Page-level dimensions

Given printable width/height (after subtracting margins, header, footer, and bitmap border):

```
nx = (width  - px - 2*border) / ((NDOT+3)*dx)   // Printer.cpp:680
ny = (height - py - 2*border) / ((NDOT+3)*dy)   // Printer.cpp:681
```

Bitmap final size (DWORD-aligned for BMP convention):

```
bitmap_width  = (nx*(NDOT+3)*dx + px + 2*border + 3) & 0xFFFFFFFC   // Printer.cpp:687
bitmap_height =  ny*(NDOT+3)*dy + py + 2*border                     // Printer.cpp:688
```

`border` is `dx*16` if `printborder=1`, else `25` for BMP output, else `0` (paper). `Printer.cpp:670-675`.

### 4.4. Block placement within bitmap

For block index `k` (0-based, row-major in `nx`-wide grid), top-left corner of its dot grid is:

```
x_block = (k % nx) * (NDOT+3)*dx + 2*dx + border
y_block = (k / nx) * (NDOT+3)*dy + 2*dy + border
```

Source: `Printer.cpp:170-171`. Y coordinate is then flipped because BMP rows run bottom-up: `bits += (height - y_block - 1)*width + x_block` (`Printer.cpp:172`). The `2*dx`/`2*dy` offset is the gap consumed by the sync raster (§4.5) before the block's first data dot.

### 4.5. Sync raster (border fill)

When `printborder=1`, the page draws a regular sync raster in the cells around the data grid. Every `(blockx, blocky)` outside `[0..nx)×[0..ny)` is filled with a "fillblock" — a 32×32 dot pattern that is **not** an encoded data block but an alignment fiducial.

Pattern (`Printer.cpp:212-220`):

```
even rows (j%2==0):  t = 0x55555555
odd  rows (j%2==1):  t = 0xAAAAAAAA          (default)
                       0                     (top border:    blocky<0  and j<=24)
                       0                     (bottom border: blocky>=ny and j>8)
                       0xAA000000            (left  border:  blockx<0)
                       0x000000AA            (right border:  blockx>=nx)
```

The reduced-coverage edge cases (`0`, `0xAA000000`, `0x000000AA`) make the outermost borders distinguishable from the inner sync raster, giving the decoder a way to find the page edges. Cells inside the data grid use the standard `0x55555555`/`0xAAAAAAAA` alternation.

Together with the 1-dot-thick black grid lines drawn between cells (`Printer.cpp:830-855`), this is the v1 "sync raster geometry" — there is no separate corner-square fiducial like a QR code. The decoder finds the grid by histogram / FFT-like peak detection over the entire raster (§7).

### 4.6. Page capacity

```
pagesize = ((nx*ny - redundancy - 2) / (redundancy+1)) * redundancy * NDATA
```

Source: `Printer.cpp:730-731`. The `-redundancy-2` reserves cells for the per-string superblock copies; the divisor `(redundancy+1)` partitions the remaining cells into groups of `redundancy` data + 1 recovery block. Capacity is in bytes of *compressed* data — the original file may be larger.

## 5. Page-to-page block layout (interleaving)

Blocks on a page are arranged into "strings" — one string per redundancy slot plus one recovery string.

Definitions (per page, `Printer.cpp:819-823`):

```
n        = ceil(min(remaining_data, pagesize) / NDATA)   // data blocks needed this page
nstring  = ceil(n / redundancy)                          // number of groups
total    = (nstring+1) * (redundancy+1) + 1              // total cells used
ny_used  = max(ceil(total / nx), 3)                      // last page may shrink to ≥3 rows
```

### 5.1. Cell index assignment

The encoder uses two layout regimes depending on whether `nstring+1 < nx`:

**Compact regime** (`nstring+1 < nx`, `Printer.cpp:874, 901`):
- Group string `j` (j ∈ 0..redundancy) occupies column `j*(nstring+1) + i` for data block `i`.
- Superblock for string `j` is at column `j*(nstring+1)` (i.e. `i=0`).

**Wide regime** (`nstring+1 >= nx`, `Printer.cpp:875, 906-908`):
- Adds a `nx/(redundancy+1)` shift between strings to spread blocks across columns:

```c
rot = (nx/(redundancy+1)*j - k%nx + nx) % nx;
k  += (i+1+rot) % (nstring+1);
```

The intent (`Printer.cpp:870-877` comments) is to ensure the data and recovery blocks of the same group never share a column — this defends against a stuck-on-one-column print defect (e.g. damaged laser diode line).

### 5.2. Recovery block

Per group, the encoder maintains `cksum.data[NDATA]` initialized to `0xFF` and XORs each data block into it (`Printer.cpp:881-897`):

```
cksum.addr = offset ^ (redundancy << 28)        // see §2.1
cksum.data[i] = 0xFF ^ d0[i] ^ d1[i] ^ ... ^ d_{redundancy-1}[i]
```

The 0xFF init means: for a complete group, `cksum.data ^ d0 ^ ... = 0xFF…` (all-ones), so decoder XORs everything back together and inverts (`Fileproc.cpp:217`). If exactly 1 block is missing, decoder XORs the remaining members with the cksum and inverts to recover (`Fileproc.cpp:213-230`). If ≥2 blocks are missing in a group, recovery fails — that's the "1-of-N" redundancy posture.

### 5.3. End-of-page padding

Any cells unused after the last group string get filled with copies of the superblock (`Printer.cpp:921-924`) — extra superblocks never hurt the decoder.

## 6. Data pipeline (file ↔ blocks)

```
encode:  file → bzip2 → align16 → AES-192-CBC → split into NDATA chunks → group/redundancy → per-block ECC → page bitmap
decode:  scan → bit grid → per-block ECC → group/redundancy fill-in → unsplit → AES-192-CBC decrypt → bzip2 inflate → file
```

### 6.1. bzip2

Library: vendored `bzlib/` (BZ2 1.0 series; the in-tree version is the upstream zip's bundled copy). Block size for compression: `1` if `compression==1` (fast), `9` if `compression==2` (max), default `9`. Init at `Printer.cpp:334-335`. Decompression is one-shot via `BZ2_bzBuffToBuffDecompress` (`Fileproc.cpp:344-345`).

The encoder note: if compression actually grows the data (low-entropy input), the encoder silently restarts with compression off and clears `PBM_COMPRESSED` (`Printer.cpp:370-377`, `397-403`).

### 6.2. AES alignment

After bzip2, the encoder pads the buffer to a 16-byte boundary with zeros (`Printer.cpp:415-420`):

```
alignedsize = (datasize + 15) & 0xFFFFFFF0
```

This is what gets written to the page as `superdata.datasize` (i.e., aligned size, *not* raw bzip2 size). bzip2 tolerates trailing zeros, so the decoder need not strip them.

### 6.3. AES-192-CBC

Cipher: AES-192 (`AESKEYLEN=24` at `paperbak.h:35`). Mode: CBC. Library: Brian Gladman's `aes.h` from `CRYPTO/`. Encryption at `Printer.cpp:489`, decryption at `Fileproc.cpp:313`.

#### 6.3.1. Salt + IV

Both salt (16 bytes) and IV (16 bytes) are generated together as 32 bytes from `CryptGenRandom` and stored in `superdata.name[32..64]`:

```c
salt = (uchar*)(superdata.name) + 32;          // Printer.cpp:472
GenerateRandomData(32, salt);                  // Printer.cpp:473
// ...
iv = salt + 16;                                // Printer.cpp:488
```

So `salt = name[32..48]` and `iv = name[48..64]`. Decoder reads the same way at `Fileproc.cpp:303, 312`.

#### 6.3.2. KDF

```c
derive_key(password, n, salt, 16, 524288, key, AESKEYLEN);
```

`Printer.cpp:477` and `Fileproc.cpp:304`. Function is Brian Gladman's PBKDF2 (`CRYPTO/pwd2key.h:32` — "implementation of RFC2898"). The hash variant is determined by which Gladman library this links against; the source ships only `crypto.lib` (precompiled). Since `CRYPTO/sha2.h` is the only hash header in the tree and `hmac.h` is present, the most likely (and what we MUST verify experimentally before declaring v1 round-trip) is **PBKDF2-HMAC-SHA-256**.

> **TODO (M7):** confirm HMAC variant by feeding a known password+salt to ampaper's KDF and matching it against `crypto.lib`'s output for a captured 1.10 print. If the variant is HMAC-SHA-1 not -SHA-256, this doc must be updated.

Iterations: **524288** (= 2^19). Salt length: 16 bytes. Output length: 24 bytes (just the AES key — IV is independent random data, not derived).

#### 6.3.3. Plaintext integrity

`superdata.filecrc` holds CRC-16 of the compressed-but-unencrypted buffer (`Printer.cpp:453, 522`). Decoder uses it to verify the password: decrypt, CRC the result, compare against `filecrc` (`Fileproc.cpp:318-322`). On mismatch, the decoded plaintext is discarded and the user is asked to re-enter. There is no separate AEAD tag — `filecrc` is the only integrity signal, which is a known v1 weakness (a forged ciphertext that happens to decrypt to a valid CRC would pass).

### 6.4. Block splitting

After `alignedsize` is fixed, the encoded buffer is sliced into `NDATA`-sized chunks. Each chunk's `addr` is its byte offset in the buffer. The last chunk may be a partial slice; bytes beyond the end of data are zeroed (`Printer.cpp:892-895`).

Total block count:

```
nblock = ceil(datasize / NDATA)
npages = ceil(datasize / pagesize)
```

(`Fileproc.cpp:94`, `Printer.cpp:788`.)

## 7. Decoder pipeline

The decoder runs as a state machine in `Decoder.cpp:909-948`. Steps:

1. **Find raster bounds** (`Getgridposition`, `Decoder.cpp:259-319`). Sample up to 256×256 grid points; at each, compute fast intensity range (`max - min` of 5 neighborhood pixels, 2-pixel reach). The range is high inside the data raster (alternating dots) and low in solid borders. 50% threshold gives bounding box.

2. **Estimate intensity + sharpness** (`Getgridintensity`, `Decoder.cpp:322-386`). Within a centered NHYST=1024 box, read all pixel values; `cmin` = 3rd percentile, `cmax` = 97th percentile. Sharpness factor: `(cmax-cmin)/(2*contrast) - 1` where `contrast` is the 5th-percentile of |adjacent-pixel-deltas|.

3. **Find vertical grid lines** (`Getxangle`, `Decoder.cpp:389-450`). Try angles in `[-NHYST/20*2, +NHYST/20*2]` step 2 (≈ ±10°). For each, build a column-summed histogram, run `Findpeaks` to extract average peak position + spacing. Best-weighted result wins.

4. **Find horizontal grid lines** (`Getyangle`, `Decoder.cpp:453-513`). Same idea, transposed. Sanity-check that `ystep ∈ [0.40, 2.50] * xstep` (i.e. dot pitch is roughly square).

5. **Prepare** (`Preparefordecoding`, `Decoder.cpp:516-602`). Allocate per-block buffers; estimate `nposx`, `nposy` (the loop bounds for block position scan). Pick max dot-sample size based on grid step: 1×1 if `xstep<2*(NDOT+3)`, 2×2, 3×3, or 4×4 otherwise.

6. **Decode each block position** (`Decodeblock`, `Decoder.cpp:607-815`). For each `(posx, posy)`:
   - Compute block position via affine: `x0 = xpeak + xstep*(posx - blockborder)`, `y0 = ypeak + ystep*(nposy - posy - 1 - blockborder)`. Note the Y-flip — bitmap is BMP-style upside-down.
   - Bilinear-interpolate a rotated sub-image into `buf2`, sharpen into `buf1`.
   - Locate per-block grid (column/row peaks) and verify spacing within ±1/16 of page-level step.
   - Per block, try 4 dot sizes × 9 sub-pixel shifts × 8 orientations × 9 (factor, threshold) combos.
   - For each candidate: extract 32×32 grid of bytes, threshold against neighbor-aware limit, pack into a `t_data`, XOR rows with the `0x55555555`/`0xAAAAAAAA` mask, run `Decode8` (RS), then verify `crc ^ 0x55AA == Crc16(addr+data)`.
   - On verify, lock in the orientation for the rest of the page; on M_BEST mode, keep searching for the lowest error count.

7. **Finish page** (`Finishpage`, `Fileproc.cpp:182-269`). Per group, if exactly 1 block is missing and a recovery block is present, XOR-recover. Update `t_fproc.datavalid[i]`: 0 = missing, 1 = valid, 2 = recovery (still pending).

8. **On last page complete**, AES-decrypt + bzip2-decompress + write file (`Saverestoredfile`, `Fileproc.cpp:274-376`).

The decoder explicitly tries 8 orientations (4 rotations × 2 mirrors, `Decoder.cpp:208-217`) which makes paper orientation auto-detect. This must be reproduced in ampaper or scans will fail when the user feeds the page rotated.

## 8. BMP-on-disk format (debug output, not paper)

When `outbmp` is non-empty, the encoder writes a `.bmp` file instead of printing. Layout:

```
sizeof(BITMAPFILEHEADER) = 14 bytes
  bfType        = 'BM' (= 0x4D42)        Printer.cpp:968
  bfSize        = total file size
  bfReserved1   = 0
  bfReserved2   = 0
  bfOffBits     = 14 + sizeof(BITMAPINFOHEADER) + 256*4 = 14 + 40 + 1024 = 1078

sizeof(BITMAPINFOHEADER) = 40 bytes      Printer.cpp:692-703
  biSize          = 40
  biWidth         = bitmap_width
  biHeight        = bitmap_height        (positive → bottom-up rows)
  biPlanes        = 1
  biBitCount      = 8                    (palette)
  biCompression   = BI_RGB (0)
  biSizeImage     = 0
  biXPelsPerMeter = ppix * 10000 / 254   Printer.cpp:979
  biYPelsPerMeter = ppiy * 10000 / 254   Printer.cpp:980
  biClrUsed       = 256
  biClrImportant  = 256

256 × 4 bytes palette                    Printer.cpp:704-708
  RGBA = (i, i, i, 0) for i in 0..255    (grayscale)

bitmap_width × bitmap_height bytes of pixel data (1 byte per pixel, palette index)
```

Width is `& 0xFFFFFFFC` aligned (`Printer.cpp:687`) so each row is naturally DWORD-aligned in the BMP; no per-row padding needed.

### 8.1. "Black" pixel value

When printing to paper, dots are pixel value 0 (`black = 0` at `Printer.cpp:615`). When saving to BMP for debugging, dots are pixel value 64 (dark gray, `black = 64` at `Printer.cpp:640`). This is to give the decoder a high-contrast palette ramp to find the grid peaks reliably on synthetic bitmaps.

## 9. Page-on-paper extras (decorative, non-format)

When `printheader=1`, the encoder draws above the data grid:

```
"<filename> [<modified-time>, <origsize> bytes] - page <p> of <P>"
```

(`Printer.cpp:931`) and below the grid:

```
"Recommended scanner resolution <N> dots per inch"
```

(`Printer.cpp:936-937`) where `N = max(ppix*3/dx, ppiy*3/dy)` — i.e. 3× the dot pitch. These are **not** part of the format; the decoder ignores everything outside the data raster. Reproducing them is a UI nicety, not a compatibility requirement.

---

## 10. Open questions

- **PBKDF2 hash variant** — see §6.3.2. Need experimental confirmation against captured AES-192 v1 prints before declaring M7 done.
- **bzip2 block-size choice on small files** — `compression=2` always uses block-size-9; verify no fallback to smaller for short inputs (the source unconditionally uses `9`, but worth a round-trip test).

## 11. Cross-check checklist (per `feedback_three_way_crosscheck.md`)

For any encoder/decoder change in ampaper:

1. Encode a test file with ampaper v1 mode, decode it with ampaper. SHA-256 round-trip match.
2. Decode a captured PaperBack 1.10 BMP/scan with ampaper. SHA-256 of decoded plaintext matches the known input.
3. Encode a test file with ampaper v1 mode (no encryption), decode it with mrpods (or PaperBack 1.10 itself). SHA-256 match.

All three must pass before claiming v1 binary compatibility.
