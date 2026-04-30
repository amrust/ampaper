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
<name>.txt            (Recommended) Capture metadata in plain text:
                        - PaperBack version + checksum of the binary used
                        - Encoder options (dpi, dotpercent, redundancy,
                          compression, encryption)
                        - Date captured + capturer's notes
                      Read once; never asserted against.
```

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

## Recommended initial vectors

When the first batch is captured, aim for diversity across these axes:

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
- `.scan.bmp` / `.scan.png` files (a printed-then-scanned capture
  rather than the encoder's BMP-debug output). Those are harder to
  reproduce deterministically and belong in a separate `scanned/`
  subdirectory if/when we add scan-decode tests.

## Why this directory ships in the public repo

Unlike `/reference/` (which holds copyrighted upstream source under
the original PaperBack license), the vectors here are **derived works**
of synthetic inputs we created. The .input files are ours; the .bmp
files are mechanical outputs of PaperBack 1.10's encoder applied to
those inputs. Both are GPL-3.0-or-later under ampaper's license,
consistent with PB 1.10's GPL-2-or-later upstream.
