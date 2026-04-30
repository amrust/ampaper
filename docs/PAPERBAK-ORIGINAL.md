# PaperBack 1.10 — original feature set + bitstream format

This document is the working analysis of Oleh Yuschuk's PaperBack 1.10. It is the **ground truth** ampaper must round-trip against; if a scan of a 1.10-printed page doesn't decode in ampaper, ampaper is wrong.

> **Source:** `paperbak-1.10.src.zip` from <https://www.ollydbg.de/Paperbak/> — drop the unzipped tree under `reference/paperbak-1.10/` (gitignored) before working on this. Do **not** copy-paste code from it; we re-implement.

## What the program does

Prints a file as a grid of dots on A4 / Letter paper, scans the print back, and reconstructs the original file with error correction. Aimed at long-term archival ("paper outlives every magnetic / SSD / optical medium") with redundancy enough that smudges, folds, and partial fading are survivable.

## Top-level pipeline

```
encode:   file  →  bzip2  →  AES-192-CBC  →  Reed-Solomon  →  page bitmap  →  GDI print
decode:   scan  →  raster sync  →  block extraction  →  RS decode  →  AES decrypt  →  bzip2 inflate  →  file
```

Each stage has a fixed serialization that we have to match exactly.

## Per-page parameters (from the official site + 1.10 source)

| Parameter | Value | Notes |
|---|---|---|
| Paper | A4 / Letter | Configurable |
| Printer DPI | 600 (typical) | Dot pitch independent of this; see below |
| Dot density | ≤ ½ × printer DPI | i.e. printable cell ≥ 2×2 device dots |
| Dot fill | ~70% of cell | Black ink coverage of the dot cell |
| Scanner DPI | ≥ 900 physical | "physical" — interpolated DPI doesn't count |
| Capacity (uncompressed) | ~500 KB / sheet | A4 / Letter at 600 DPI |
| Capacity (compressed) | up to ~3 MB / sheet | bzip2 of compressible source |
| Redundancy | 1:5 | RS recovers 1 unreadable block per 5 consecutive |

## Bitstream format (high level — fill in from source)

> **TODO:** read `paperbak-1.10.src.zip` and pin down the exact byte layout of:
>
> - Page header (magic, version, page index, total pages, file size, file name, timestamp)
> - Block header (per-RS-block sequence number, flags, payload length)
> - Crypto envelope (AES key derivation, IV per page, MAC if any)
> - Compression envelope (bzip2 stream framing, end-of-stream marker)
> - Sync raster (the corner / edge fiducials the decoder uses to register the grid)

These are the load-bearing details; the unit tests live or die by them.

## Source-file inventory (mirrored in mrpods filenames)

The 1.10 source ships under a single directory; major translation units expected:

| File | Role |
|---|---|
| `Paperbak.cpp` | Application entry / wndproc |
| `Controls.cpp` | Win32 dialogs + child-window glue |
| `Decoder.cpp` | Scan → bytes |
| `Printer.cpp` | Bytes → printer DC |
| `Scanner.cpp` | TWAIN driver (the 32-bit-only piece) |
| `Ecc.cpp` | Reed-Solomon (Phil Karn's BSD-licensed RS) |
| `Crc16.cpp` | Per-block CRC |
| `Fileproc.cpp` | BMP I/O |
| `Service.cpp` | Misc. helpers |
| `bzlib/*.c` | bzip2 vendored |
| `aes/*.c` | AES-192-CBC implementation |
| `Resource.rc` | Win32 resources |

Mapping these to ampaper modules is M1's job; for now the names are the index.

## Known quirks the rewrite must inherit (or document as fixed)

- **AES-192, not 256.** Version 1.00 had an effective key strength under 50 bits; 1.10 fixed it by switching to AES-192 in CBC mode with key stretching. We must reproduce 1.10's exact KDF + IV layout for the legacy read path.
- **TWAIN 32-bit only.** The scanner glue uses TWAIN DSM and the available datasource list is filtered by process bitness — a 32-bit DSM binds to 32-bit datasources and vice versa, and most scanner manufacturers only ship 32-bit DSMs. mrpods' x64 build is broken for scanning for this reason. Our answer: WIA primary, TWAIN fallback in 32-bit-DSM-bridge mode.
- **WinMain shutdown bug.** Original 1.10 has a path where `WinMain` doesn't exit cleanly; mrpods documents fixing it. We'll do it right structurally rather than as a one-line patch.
- **Borland-isms.** Hand-rolled memory layout assumptions, integer-size assumptions, near/far-pointer ghosts. The mrpods README mentions specific spots where ChatGPT-suggested hacks made things "work" without anyone fully understanding why — those are exactly the spots Rust's ownership model rewrites cleanly. Treat any 1:1 port of those hacks as a smell.

## What we are **not** porting

- The 16-bit-era assumption that `INT_MAX` is `0x7FFF`.
- TWAIN DS chooser UI (replaced by WIA's modern device picker; TWAIN fallback uses a separate 32-bit helper process).
- Borland-specific compiler directives, message-map macros, and resource quirks.
- The original Win32 menu structure (mrpods improved this; we follow mrpods' layout — see `MRPODS-DELTA.md`).

## Open questions

- Exact KDF for AES-192: PBKDF2? How many rounds? Salt placement? **Need to read source.**
- Sync raster geometry: corner squares only, or full edge fiducials? **Need to read source.**
- Block interleaving inside a page: row-major or RS-shard interleaved? **Need to read source.**
- BMP-on-disk format vs. printer-DC bitstream: same or different? **Need to read source.**

These are the M1 deliverables — no encoder work starts until they're answered.
