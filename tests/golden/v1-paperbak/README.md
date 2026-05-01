# PaperBack 1.10 golden vectors

This directory holds bitmaps produced by **PaperBack 1.10's own encoder**,
paired with the original bytes that were fed into it. Every M6 (binary-
compatibility gate) test decodes one of these bitmaps and asserts the
recovered bytes match the input byte-for-byte.

The directory is the **ground truth** for v1 binary compatibility; ampaper
is correct only insofar as it can decode every vector here.

## File convention

Each vector is **two or three files** sharing a base name:

```
<name>.input          Raw bytes fed into PaperBack 1.10's encoder.
                      May be any size up to MAXSIZE (256 MB minus 128).
<name>.bmp            PaperBack 1.10's encoder output. Captured via the
                      "save to BMP" debug path: in PB 1.10's GUI choose
                      "File → Save bitmap…" instead of "File → Print",
                      or pass a non-empty `outbmp` to `Printfile()` if
                      driving programmatically. See FORMAT-V1.md §8.
                      Stored as BMP because that's the only format PB
                      1.10 itself can read back; converting to PNG
                      and feeding that to PB 1.10 won't work (PB 1.10
                      doesn't know PNG). ampaper's decoder reads
                      either format via the `image` crate, but the
                      ground-truth artifact stays as the BMP that PB
                      1.10 emitted.
<name>.txt            (Recommended) Capture metadata in plain text:
                        - PaperBack version + checksum of the binary used
                        - Encoder options (dpi, dotpercent, redundancy,
                          compression, encryption)
                        - Date captured + capturer's notes
                      Read once; never asserted against.
```

### Paper-print captures

PaperBack 1.10 has **no save-scan feature** — scans are consumed
live by the decoder. To capture a paper-print round-trip:

1. Print the page from PB 1.10 (File → Print).
2. Scan it back with your own scanner software at ≥ 900 DPI
   physical resolution.
3. Save the scanner output as PNG (or any format the `image` crate
   reads).
4. Drop in `tests/golden/v1-paperbak/scanned/<name>.scan.png`.

ampaper's decoder doesn't go through PB 1.10 to read these — it
ingests the scanned image directly via `scan::scan_decode`. PB 1.10
itself wouldn't be able to read a PNG without a separate BMP
conversion, but that's PB 1.10's limitation, not ours.

For multi-page captures (input larger than one page), PaperBack 1.10
emits one BMP per page named `<base>_NNNN.bmp`. Keep them all under the
same `<name>` prefix and decode in order:

```
big_input.input
big_input_0001.bmp
big_input_0002.bmp
big_input.txt
```

## Test contract

For each vector under `tests/golden/v1-paperbak/`, the M6 decoder test
asserts:

```rust
let input = std::fs::read("tests/golden/v1-paperbak/<name>.input")?;
let bmps  = collect_bmps_for("<name>");        // 1+ files
let recovered = ampaper::decode(&bmps)?;
assert_eq!(recovered, input, "vector <name> failed to round-trip");
```

The "decode this BMP, expect this SHA-256" framing from
`docs/MILESTONES.md` resolves to byte-equality against `.input`; SHA-256
is just a stable way to surface a mismatch without diffing megabytes of
binary in test output. Tests should print `sha256(recovered)` and
`sha256(input)` on failure for that reason.

## Recommended encoder settings

Use PaperBack 1.10's (and mrpods's) **source defaults** unless a
vector deliberately tests off-default options. These values are the
empirical sweet spot for max-data-per-page-without-too-many-errors —
they were iterated on during mrpods development and are baked into
both source trees identically:

```
dpi (Raster)         = 200
dotpercent (Dot size) = 70
compression          = 2 (Maximum)
redundancy           = 5
print_border         = 0 (off)
print_header         = 1 (on)
margin units         = 1/1000 inch
margin left          = 1000  (1.0 inch)
margin right         = 400   (0.4 inch)
margin top           = 400   (0.4 inch)
margin bottom        = 500   (0.5 inch)
```

Record the actual values used per vector in the `<name>.txt` file
so future readers can reproduce.

## Starter vectors in this directory

| File | Content | Size |
|---|---|---|
| `lorem.input` | Standard 5-sentence Lorem Ipsum (English) | 446 bytes |

Capture workflow for the lorem vector:
1. Open `lorem.input` in PaperBack 1.10 (File → Open input file)
2. Use the source defaults from above
3. File → Save bitmap → `lorem.bmp`
4. (Optional) Drop a `lorem.txt` with the encoder options and PB
   1.10 version + checksum used
5. Commit `lorem.bmp` and (if present) `lorem.txt`

PB 1.10 truncates the bitmap height to fit just the rows the data
needs (Printer.cpp:817-826) — for a 446-byte input at default
settings the BMP comes out around 1.5 MB rather than the ~26 MB
of a full A4 page. Comfortable for a git commit.

## Future vector matrix

Beyond the lorem starter, expand coverage along these axes:

| Axis | Variants worth one vector each |
|---|---|
| Compression mode | off, fast (level 1), max (level 9) |
| Encryption | off, AES-192-CBC with a known password |
| Redundancy | NGROUP_MIN (2), default (5), NGROUP_MAX (10) |
| Page count | single-page, ~3-page (forces multi-page handling) |
| Input shape | high-entropy (random bytes), low-entropy (text/source code), all-zeros (edge case) |

5–10 well-chosen vectors hit every code path in the v1 codec.

## What NOT to commit

- The capturing PaperBack 1.10 binary itself (license unclear; binary
  redistribution conditions vary by where it's hosted).
- Any input file containing real personal data — these vectors are
  public artifacts in a public repo. Use synthetic / public-domain
  inputs only (lorem ipsum, a Project Gutenberg excerpt, `/dev/urandom`
  output committed alongside as the .input).
(no exclusions beyond the personal-data and binary-redistribution
rules above — BMPs ARE committed directly because PB 1.10's I/O is
BMP-only and we don't want to introduce a format conversion in the
ground-truth path.)

## Why this directory ships in the public repo

Unlike `/reference/` (which holds copyrighted upstream source under
the original PaperBack license), the vectors here are **derived works**
of synthetic inputs we created. The .input files are ours; the .bmp
files are mechanical outputs of PaperBack 1.10's encoder applied to
those inputs. Both are GPL-3.0-or-later under ampaper's license,
consistent with PB 1.10's GPL-2-or-later upstream.
