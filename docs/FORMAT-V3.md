# ampaper v3 — wire format spec

**Status:** Phase 1 (bytes-level codec) + Phase 2 first slice (cell
+ page-bitmap layer, synthetic round-trip). Finder patterns and
real-scanner registration are still deferred to Phase 2.5; phases
3+ (compression, encryption, density, modulation) are deferred to
their respective milestones (see "Roadmap" below). Both the
bytes-level and the page-level wire formats defined here are stable;
later phases extend them without breaking them.

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

| Phase | Adds                                              | Target density vs v1 at 100 dot/in |
|-------|---------------------------------------------------|-----------------------------------|
| 1     | Bytes-level RaptorQ codec, magic + OTI header     | n/a (no bitmap layer yet)         |
| 2.0   | Cell layout, page bitmap render/parse, anchor block | n/a (synthetic round-trip)      |
| 2.5   | QR-style finder patterns + scan-side registration | 1.3-1.5×                          |
| 3     | zstd-long compression + AES-256-GCM outer layer   | 1.3-1.5× (compression-dependent)  |
| 4     | High dot density (250-300 dot/in)                 | ~3×                               |
| 5     | 4-level grayscale modulation (2 bits/dot)         | ~5×                               |
| 6     | CMYK 1-bit-per-channel (~3 effective bits/dot)    | ~8-9× best case                   |
| 7     | Multi-level CMYK (premium, opt-in)                | ~15-20× best case                 |

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

### Phase 2.5 will add

- QR-style finder patterns at the four page corners. Lets a
  scan-side decoder find the page boundary and correct skew /
  rotation before sampling cell pixels.
- Edge timing tracks (alternating black/white dots along the top
  and left margins) for cell-grid registration. Replaces the
  current "decoder must know the geometry exactly" assumption.
- Threshold + dot-center sampling that tolerates real scanner
  noise, ink bleed, and cheap-CCD blur. The current parser
  samples the geometric center of each dot's pixel block, which
  is fine for synthetic bitmaps but not yet for paper.

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
