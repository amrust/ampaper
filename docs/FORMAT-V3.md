# ampaper v3 — wire format spec

**Status:** Phase 1 (bytes-level codec) + Phase 2 (cell layer) +
Phase 2.5a (corner finder patterns, offset + scale-drift
tolerance) + Phase 2.5b (rotation correction via affine transform
from the three finder centers, ±~5-10° tolerance). Adaptive
thresholding and real-scanner noise tolerance are deferred to
Phase 2.5c. Phases 3+ (compression, encryption, density,
modulation) are deferred to their respective milestones (see
"Roadmap" below). The bytes-level, page-level, and Phase 2.5
page-bitmap wire formats defined here are stable; later phases
extend them without breaking them.

**Scope:** v3 is a clean-sheet codec that lives **alongside**, not
on top of, the legacy decoder. The legacy v1 (PB 1.10 binary
compatibility) and v2 (AES-256-GCM forward format) decoders in
`src/decoder.rs`, `src/scan.rs`, and `src/format_v2.rs` are frozen
and will continue to read every PB 1.10 BMP and ampaper-v1/v2 PDF
ever produced.

## Why v3 exists

PB 1.10's wire format was designed for late-1990s consumer hardware
constraints (1-bit dot, 600-DPI laser, 200-DPI flatbed scan,
fixed-block RS+XOR) that no longer apply. Three structural
inefficiencies dominate:

1. **Group-layout waste.** PB 1.10's redundancy-5 layout uses 6
   parallel "strings" (5 data + 1 recovery) of `nstring + 1` cells
   each. When `cells_per_page mod 6 ≠ 1`, the remainder cells (up
   to 6 per page) render as blank paper or filler superblock copies.
   With 690 cells per Letter page at 100 dot/in, 6 cells per page
   are wasted.
2. **Per-cell separator overhead.** Each 32×32-dot data cell sits
   in a 35×35-dot footprint with a 3-dot gutter on the bottom and
   right edges, ~17% pixel waste per cell. The gutter exists so
   the per-cell scan-decode finder can lock onto each cell's grid.
3. **SuperBlock copy-per-string.** Six SuperBlock copies per page
   (one anchoring each string) consume 6 cells of capacity that
   carry no payload, only metadata redundancy. Modern ECC handles
   metadata loss differently.

v3 addresses all three:

1. **Rateless ECC** (RaptorQ, RFC 6330) eliminates fixed group
   structure. Any K + small-overhead encoded packets recover the
   source object regardless of which packets survive. No remainder.
2. **Page-level finder patterns** (planned, Phase 2) replace
   per-cell separators with QR-style corner anchors and edge
   timing tracks, recovering the 17% gutter waste.
3. **Single anchor block per page** (planned, Phase 2) carries the
   page's slice of the global OTI + a file-level integrity tag.
   The RaptorQ packet stream is the data; the anchor is small.

## Roadmap

| Phase  | Adds                                              | Target density vs v1 at 100 dot/in |
|--------|---------------------------------------------------|-----------------------------------|
| 1      | Bytes-level RaptorQ codec, magic + OTI header     | n/a (no bitmap layer yet)         |
| 2.0    | Cell layout, page bitmap render/parse, anchor block | n/a (synthetic round-trip)      |
| 2.5a   | QR-style corner finders + offset/scale-drift detection | n/a (still synthetic — but page can sit anywhere in a larger bitmap) |
| 2.5b   | Rotation correction via affine transform from 3 finders + template-match filter | n/a (synthetic + rotated) |
| 2.5c   | Adaptive thresholding + sub-pixel sampling        | 1.3-1.5×                          |
| 3      | zstd-long compression + AES-256-GCM outer layer   | 1.3-1.5× (compression-dependent)  |
| 4      | High dot density (250-300 dot/in)                 | ~3×                               |
| 5      | 4-level grayscale modulation (2 bits/dot)         | ~5×                               |
| 6      | CMYK 1-bit-per-channel (~3 effective bits/dot)    | ~8-9× best case                   |
| 7      | Multi-level CMYK (premium, opt-in)                | ~15-20× best case                 |

Each phase ships an encoder + decoder + acceptance test. Phase 1
acceptance: round-trip 1 MB byte buffers + survive 33% packet drop
in the rateless ECC.

## Phase 1: bytes-level wire format

This is the slice currently implemented in `src/v3/`. It is a
self-contained byte-level codec with no dependency on any physical
layer.

### Magic & version

| Offset | Bytes | Field            | Value (Phase 1) |
|--------|-------|------------------|-----------------|
| 0      | 8     | Magic            | `b"AMPAPER3"`   |
| 8      | 1     | Version          | `0x01`          |
| 9      | 3     | Reserved         | `0x00 0x00 0x00`|
| 12     | 12    | RaptorQ OTI      | (RFC 6330 §3.3.2) |
| 24     | ...   | Encoded packets  | (see below)      |

The 8-byte ASCII magic `AMPAPER3` is intentionally legible in a hex
dump and unambiguously distinct from any v1/v2 header. The
dispatcher routes inputs to v3 vs the legacy decoder by checking
this magic before any other parsing.

The 3 reserved bytes between the version byte and the OTI must be
zero in version 1. A non-zero reserved byte is a hard decode error
(refuse to guess what a future revision means).

### RaptorQ OTI

The 12-byte ObjectTransmissionInformation per RFC 6330 §3.3.2
carries:

- Transfer length F (5 bytes, big-endian) — the source object size
  in bytes.
- Reserved (1 byte, must be zero per RFC).
- Symbol size T (2 bytes, big-endian).
- Number of source blocks Z (1 byte).
- Number of sub-blocks N (2 bytes, big-endian).
- Symbol alignment Al (1 byte).

Read literally from the `raptorq` crate's
`ObjectTransmissionInformation::serialize()` output. The decoder
hands these 12 bytes to `ObjectTransmissionInformation::deserialize`
and constructs a RaptorQ `Decoder` from the result.

### Encoded packets

After the 24-byte header, the body is a stream of RaptorQ
EncodingPackets in their RFC 6330 wire form:

| Offset within packet | Bytes | Field              |
|----------------------|-------|--------------------|
| 0                    | 4     | Payload ID (SBN+ESI) |
| 4                    | T     | Symbol payload     |

Where T is the symbol size from the OTI. Packets are contiguous
and the same fixed size; the decoder walks the body in `4 + T` byte
strides.

Packet ordering is not significant — RaptorQ's recovery property
holds regardless of which K + overhead packets survive. The encoder
emits source packets first, then repair packets (per RaptorQ
convention), but a decoder MUST NOT rely on that ordering.

### Recovery probability

After receiving K + h encoded packets, the RaptorQ decoder recovers
the source object with probability `1 - 1/256^(h+1)` (Qualcomm's
"RaptorQ Technical Overview"). For paper-archive use (decades on a
shelf, patchy fade + edge tear), even modest repair budgets give
effectively perfect recovery. Phase 1 default: 10 repair packets,
revisited in Phase 2 against measured scanner loss profiles.

### Differences from v1/v2

| Aspect                | v1 (PB 1.10)         | v2 (ampaper)             | v3 (this spec)        |
|-----------------------|----------------------|--------------------------|-----------------------|
| ECC                   | RS(255,223) + XOR    | RS(255,223) + XOR        | RaptorQ rateless      |
| Group structure       | r+1 fixed strings    | r+1 fixed strings        | None (any K recovers) |
| SuperBlock copies     | r+1 per page         | r+1 per page (×2 cells)  | 1 anchor (Phase 2)    |
| Per-cell separator    | 3-dot gutter         | 3-dot gutter             | None (Phase 2)        |
| Compression           | bzip2 (optional)     | bzip2 (optional)         | zstd (Phase 3)        |
| Encryption            | AES-192-CBC (read)   | AES-256-GCM              | AES-256-GCM (Phase 3) |
| Per-page wasted cells | up to 6              | up to 6                  | 0                     |

## Phase 2 first slice: cell + page-bitmap layer

This is the slice currently implemented in `src/v3/cell.rs`,
`src/v3/page.rs`, and `src/v3/codec.rs`. It packages the
Phase 1 bytes-level codec into 32×32-dot cells, lays them out as
a `nx × ny` grid on a page, and renders the grid to an 8-bit
grayscale bitmap. The decoder reverses the process from a
pixel-perfect bitmap (real-scanner tolerance is Phase 2.5).

### Cell layout

Each cell is a fixed 128 bytes that round-trips through a 32×32
dot pattern. Bit packing is MSB-first row-major: byte 0 holds
dots (0,0)..(0,7) — top row, leftmost 8 dots, with bit 7 of
byte 0 = dot (0,0).

| Offset | Bytes | Field                                  |
|--------|-------|----------------------------------------|
| 0      | 2     | CRC-16 over bytes 2..128, XORed with `0x7633` |
| 2      | 1     | Cell type (`0x00` data, `0x01` anchor) |
| 3      | 1     | Reserved (must be zero)                |
| 4      | 4     | Discriminator (payload-ID for data, `b"ANCR"` for anchor) |
| 8      | 120   | Payload                                |

The CRC XOR mask `0x7633` (= ASCII `"v3"`) serves two roles:

1. **Domain separation from blank paper.** ampaper's `crc16` is
   CRC-16/XMODEM (init 0). Without an XOR mask, an all-zero cell
   stores stored_crc = 0, computed_crc = 0, and validates as a
   data cell with payload-ID `[0,0,0,0]`. RaptorQ would receive
   that as a fake packet. The XOR pushes blank cells out of the
   valid-CRC space.
2. **Domain separation from v1.** PB 1.10's blocks XOR their CRC
   with `0x55AA`. Choosing a different constant for v3 means a
   v1 cell can never accidentally satisfy a v3 reader's CRC and
   vice versa, even on otherwise-identical bytes.

### Data cells

For data cells, the discriminator at bytes 4..8 is the RaptorQ
payload ID (1-byte SBN + 3-byte ESI per RFC 6330 §3.2). The 120
payload bytes at 8..128 are the RaptorQ symbol — symbol size T =
120 falls out of `RAPTORQ_MTU = 124` (4 bytes payload-ID + 120
bytes symbol on the wire) once the encoder applies the default
8-byte alignment.

### Anchor cells

For anchor cells, the discriminator is `b"ANCR"`. The 120-byte
payload area carries file-level metadata:

| Offset (within payload) | Bytes | Field             |
|-------------------------|-------|-------------------|
| 0                       | 12    | RaptorQ OTI       |
| 12                      | 8     | File size (u64 LE) |
| 20                      | 4     | Total pages (u32 LE) |
| 24                      | 4     | Page index (u32 LE)  |
| 28                      | 92    | Reserved (zero in v1; future filename + mtime + attrs) |

Cell 0 of every page is an anchor. The decoder picks the first
anchor it finds — anchors on subsequent pages are redundant
copies, useful when a corner of one page is destroyed. Anchors
across the same encoded file's pages MUST agree on OTI + file
size + total page count; disagreement is a hard decode error.

### Page layout

A page's cells are laid out row-major in a `nx × ny` grid. Cell
index `i` lives at column `i % nx`, row `i / nx`. The bitmap is
`(nx · 32 · pixels_per_dot) × (ny · 32 · pixels_per_dot)` 8-bit
grayscale pixels. `pixels_per_dot` is the printer-pixel scale
(1 = unscaled, 6 = PB-1.10's 600-DPI × 100-dot/in calibration).

Cell 0 is the anchor. Cells 1..nx·ny carry RaptorQ packets. The
encoder fills pages sequentially: page 0 carries the first
`nx·ny - 1` packets, page 1 the next slice, etc. The last page
may have trailing all-zero cells (which fail the CRC and are
skipped by the decoder).

## Phase 2.5 first slice: corner finder patterns

This is the slice currently implemented in `src/v3/finder.rs` and
the updated `src/v3/page.rs`. Adds three QR-style 7×7 finder
patterns at the corners of every v3 page bitmap and rewrites
`parse_page` to find the data grid using them — the decoder no
longer needs the page bitmap to be cropped to exact size or
rendered at exactly the geometry's `pixels_per_dot`.

### Page layout (Phase 2.5)

Page bitmap dimensions in dots:

```
page_width_dots  = nx · 32 + 2 · FINDER_MARGIN_DOTS
page_height_dots = ny · 32 + 2 · FINDER_MARGIN_DOTS
```

where `FINDER_MARGIN_DOTS = 8` (= `FINDER_SIZE_DOTS` 7 +
`FINDER_QUIET_DOTS` 1). The page bitmap is then scaled by
`pixels_per_dot` to its final pixel dimensions.

Three finders sit at:

- Top-left: dots `(0..7, 0..7)`
- Top-right: dots `(page_width - 7..page_width, 0..7)`
- Bottom-left: dots `(0..7, page_height - 7..page_height)`

The bottom-right corner is deliberately empty. The asymmetry
provides an unambiguous orientation signal, used by the planned
Phase 2.5b rotation handler.

The data grid sits at dot offset `(FINDER_MARGIN_DOTS,
FINDER_MARGIN_DOTS)` from the page's outer edge. Cells are laid
out exactly as in Phase 2.0 inside the data grid.

### Finder pattern (7×7 dots)

```
B B B B B B B
B W W W W W B
B W B B B W B
B W B B B W B    ← center dot at (3, 3)
B W B B B W B
B W W W W W B
B B B B B B B
```

A horizontal or vertical scan through the center yields a
`1:1:3:1:1` dark:light:dark:light:dark run-length signature —
which is what the detector keys on. Same shape as a QR finder.

### Detection algorithm (Phase 2.5 first slice)

1. Raster-scan rows from the top of the bitmap. For each row,
   collect run-lengths and look for a 5-run sequence matching
   `1:1:3:1:1` dark:light:dark:light:dark within ±50%
   per-segment tolerance.
2. On a row hit, verify with a column scan at the candidate
   center column. Both row and column unit estimates must agree
   within ±25% (otherwise the candidate is a chance run-length
   collision).
3. The first verified hit is the top-left finder.
4. From the top-left finder's `(center_x, center_y, unit)` and
   the supplied `(page_width_dots, page_height_dots)`, compute
   the expected positions of the top-right and bottom-left
   finders. Search for finders within ±6 dots of those expected
   positions (generous tolerance to absorb modest scale drift).
5. Compute per-axis pixels-per-dot (`dx`, `dy`) from the
   horizontal and vertical finder distances. Use these — not the
   geometry's stored `pixels_per_dot` — for cell sampling.
6. Sample each cell's 32×32 dots at the geometric center of
   each dot's pixel block (`floor(grid_origin + (col + 0.5) ·
   dx)`). Fixed midpoint threshold (`< 128 = black`).

### Pinned design notes (gotchas caught en route)

- **Finder center sits at page-dot 3.5, not 3.** The middle dot
  of the 7×7 finder is at index (3, 3); its geometric center in
  continuous coordinates is at (3.5, 3.5) — a half-dot offset.
  The grid-origin computation must subtract 3.5, not 3, from
  `FINDER_MARGIN_DOTS`. Off-by-half here costs a half-dot
  mis-sample at every scale and corrupts every cell.
- **Cell sampling uses floor, not round.** At `pixels_per_dot=1`,
  the continuous sample coordinate is always X.5; rounding
  half-away-from-zero would tip every sample onto the next dot
  and produce all-wrong cell content. `f32 as i64` truncation
  (= floor for positive coordinates) gives the correct dot.
- **No bottom-right finder.** Three finders are enough for the
  current axis-aligned offset/scale handling, and reserving the
  fourth corner for "no finder" gives Phase 2.5b a free
  orientation signal without a wire-format bump.

## Phase 2.5b: rotation correction via affine transform

This is the slice currently extending `src/v3/finder.rs` and
`src/v3/page.rs`. Drops Phase 2.5a's "axis-aligned only"
assumption — the parser now solves a 2D affine transform from the
three known finder positions, so rotation, modest skew, and
already-handled scale drift fall out of the same math. Tolerates
roughly ±5–10° rotation, depending on the printer-pixel scale
(higher scale → more tolerance).

### Detection upgrade

The Phase 2.5a detector raster-scanned for the FIRST 1:1:3:1:1 hit
and assumed it was the top-left finder, then expected-position-
searched for top-right and bottom-left. That works at 0° rotation
but breaks as soon as the page is tilted: the "first hit" is no
longer reliably the top-left finder, and the expected positions
are no longer where the other finders actually are.

Phase 2.5b replaces it with an all-finders-then-identify approach:

1. **`find_all_finders`** scans every row + every matching column
   for 1:1:3:1:1 hits, then greedily clusters hits within
   `3 · unit` pixels into one representative per finder.
   Cluster hit count is the first-pass confidence signal.
2. **Template match filter.** Each cluster gets sampled against
   the actual 7×7 dot pattern at its center (using the cluster's
   estimated `unit` for the per-dot scale). Real finders score
   47–49 / 49; chance 1:1:3:1:1 collisions in random data cells
   typically score below 40. The filter cutoff is 40 (≈82%).
3. **Pick top 3 by template score**, sort the survivors by
   cluster-hit count for tie-breaking.
4. **`identify_corners`** runs the L-shape + cross-product
   analysis: the vertex opposite the longest pairwise side is
   TL; cross product of (TL→p1) × (TL→p2) is positive when
   (p1, p2) is in the order (TR, BL).
5. **Proportion sanity check.** The dot-distance ratio
   `dist(TL, TR) / dist(TL, BL)` should approximate the page's
   `(W − 7) / (H − 7)`, regardless of rotation. ±30% tolerance.

The template match filter is the load-bearing change for
robustness at low `pixels_per_dot`. At scale=1 each real finder
contributes only ~3 cluster hits, which is too small a margin
above chance hits for cluster count alone to rank reliably.
Sampling the 7×7 area against the known `FINDER_DOTS` template
is a much sharper filter — caught the regression where Phase
2.5b's all-finders detector failed `round_trips_one_megabyte`
because false positives in random multi-page data fooled the
top-3-by-count selection.

### Affine transform

Given three known finder centers in page-dot space:

```
TL_dot = (3.5, 3.5)
TR_dot = (page_width_dots − 3.5, 3.5)
BL_dot = (3.5, page_height_dots − 3.5)
```

and three measured pixel positions `(TL.x, TL.y)`, `(TR.x, TR.y)`,
`(BL.x, BL.y)`, the parser solves for `a, b, c, d, tx, ty` in:

```
pixel_x = a · page_x + b · page_y + tx
pixel_y = c · page_x + d · page_y + ty
```

Six unknowns, six equations. Closed-form solution:

```
a  = (TR.x − TL.x) / (W − 7)
b  = (BL.x − TL.x) / (H − 7)
c  = (TR.y − TL.y) / (W − 7)
d  = (BL.y − TL.y) / (H − 7)
tx = TL.x − a · 3.5 − b · 3.5
ty = TL.y − c · 3.5 − d · 3.5
```

For axis-aligned input, `b = c = 0` and `a = d = pixels_per_dot`,
so the transform reduces to the simpler grid-origin math from
Phase 2.5a. For rotated input, `b` and `c` carry the rotation;
modest skew (where `a ≠ d` or the off-diagonals don't match) is
also handled implicitly.

Cell sampling projects each dot's geometric center
(`(cell_origin + col + 0.5, cell_origin + row + 0.5)` in page-dot
space) through the transform, then floors to integer pixel
coordinates. Floor (not round) for the same reason as Phase 2.5a:
at scale=1, every continuous coordinate is X.5 and round
half-away-from-zero would tip every sample onto the next dot.

### Phase 2.5c will add

- **Adaptive thresholding.** Otsu or per-region thresholding
  instead of fixed `< 128`. Real scanner output rarely has the
  bimodal histogram synthetic bitmaps do.
- **Sub-pixel sampling.** Integrate over each dot's pixel area
  rather than sampling the geometric-center pixel. Suppresses
  edge bleed where ink dots aren't perfect circles.

## Channel asymmetry (Phase 6+ planning note)

When CMYK modulation lands (Phase 6), the four channels MUST NOT
share an ECC stream. Two reasons:

1. **Dye fade is not symmetric.** Yellow dye on consumer paper
   fades 3-5× faster than black under typical indoor light. A
   shared ECC stream would treat all four channels as equally
   reliable; in practice yellow degrades first.
2. **Scanner color separation is imperfect.** Cheap CCD scanners
   have narrow gamuts (especially in cyan), and color-channel
   crosstalk is real. The yellow channel reads noisier than the
   black channel under almost all conditions.

The plan: each channel runs its own RaptorQ stream (independent
OTI, independent K, independent repair budget). Yellow and cyan
get more repair packets than black and magenta. The decoder
recovers each channel separately; failure of any single channel
doesn't prevent recovery of the others.

Tighter integration (cross-channel error correction) is possible
but waits until Phase 6 has measured real CMYK round-trip fidelity
on consumer hardware.

## Open questions

- **Symbol size T at the cell layer.** Phase 1 uses MTU=256 as a
  placeholder. The cell-layer phase will compute T from the chosen
  physical density: `T = cell_payload_capacity - per_cell_overhead`.
  At 32×32-dot 1-bit cells, payload capacity is 128 bytes, so T
  will likely settle around 120 (after CRC + payload-ID + cell-
  type bits). Final number depends on the cell layout decisions in
  Phase 2.
- **Anchor block contents.** Will carry: file metadata (name, size,
  mtime), AES-256-GCM tag (Phase 3+), per-page index for visual
  ordering, file-level CRC. Detailed layout in the Phase 2 spec.
- **Multi-page packet distribution.** For inputs that span multiple
  printed pages, the encoder distributes packets across pages such
  that any K + overhead packets (collected from any subset of
  pages) recover the file. Per-page packet count balances scan
  efficiency vs partial-recovery resilience.
- **Color calibration palette.** Phase 6+ needs a known-RGB strip
  on each page so the decoder can compute a per-print ICC-like
  transform. Likely a corner palette block plus a center-of-page
  spot check.
