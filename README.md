# ampaper

A Rust port of [PaperBack](https://www.ollydbg.de/Paperbak/) by Oleh Yuschuk — back up files as printable, scannable bitmaps on paper for long-term archival storage.

> **Status:** Scaffolding. See [docs/MILESTONES.md](docs/MILESTONES.md) for the phased plan.

## Goal

Reproduce the functionality of upstream PaperBack 1.10 in idiomatic Rust, then carry it into the 21st century:

1. **Binary compatibility first.** Pages produced by PaperBack 1.10 (and bitmap scans of those pages) must decode in ampaper. ampaper's "legacy" output must decode in PaperBack 1.10. The bitstream is the unit-test ground truth.
2. **Real x64 + ARM64 support.** PaperBack was Borland-32-bit-only; mrpods got 32-bit modern C++ working but never closed the x64 scanner story. Rust + WIA (Windows Image Acquisition) over TWAIN gives us native 64-bit and ARM64 builds.
3. **Memory safety.** The original C had quirky memory bugs that the mrpods rewrite papered over with one-off hacks; Rust's ownership model is the right tool for the page-buffer / block-decoder pipeline.
4. **Forward improvements** — once parity ships, density and durability gains:
   - AES-256-GCM as the new default cipher (AES-192 stays for read-only legacy decode).
   - Algorithmic use of color (CMYK / RGB sub-channels) for higher per-sheet density.
   - Better redundancy / interleaving choices than the original 1:5 RS code.

## What is PaperBack?

A Windows utility that prints a file as a grid of black-and-white dots on paper — typically ~500 KB uncompressed (or ~3 MB of compressible source code) per A4 / Letter page at 600 DPI — with bzip2 compression, AES-192 encryption, and Reed-Solomon error correction baked in. You feed a printed page through a scanner (≥900 DPI physical) and the decoder reconstructs the original file.

## Why Rust

- **No more Borland weirdness.** The 1.10 source has hand-rolled memory dances that survived only because the original compiler tolerated them; rewriting in Rust forces the invariants out into the type system.
- **Native 64-bit + ARM.** No 16-bit-pointer assumptions, no TWAIN-DSM-bitness gymnastics — drive WIA from x64 directly.
- **Cross-platform decoder.** The encode / decode pipeline is platform-independent. The Windows-specific surface (printing, scanning, the GUI) sits behind `cfg(windows)`; the core round-trips on Linux / macOS so anyone can decode an old scan.
- **Memory safety on untrusted input.** A scanner-fed bitmap is attacker-controllable; bounds-checking everything by default is the right posture.

## License

GPL-3.0-or-later, same as upstream PaperBack. As a derivative work, this license is required (see [NOTICE](NOTICE) and [LICENSE](LICENSE)).

Original PaperBack © 2007-2013 Oleh Yuschuk.

## Building

```
cargo build --release
```

Requires Rust 1.85+. Windows GUI / printing / scanning paths additionally need the Windows SDK.

## Roadmap

Tracked in GitHub issues once the repo is published. The high-level milestones are in [docs/MILESTONES.md](docs/MILESTONES.md):

- **M0** — scaffolding (this commit)
- **M1** — page format spec extraction + golden-vector test fixtures from PaperBack 1.10 output
- **M2** — Reed-Solomon encoder/decoder (Phil Karn-compatible)
- **M3** — bzip2 wrapper (link `bzip2` crate, pin block size to upstream's choice)
- **M4** — page geometry + dot grid + sync raster
- **M5** — encoder pipeline (file → compressed → encrypted → ECC → page bitmap)
- **M6** — decoder pipeline (scanned bitmap → blocks → ECC → decrypted → decompressed → file)
- **M7** — legacy AES-192 read path
- **M8** — Win32 GUI (mrpods-style menu structure, modernized)
- **M9** — printing (GDI / GDI+)
- **M10** — scanning (WIA, TWAIN as fallback)
- **M11** — AES-256-GCM forward mode
- **M12** — density experiments (color encoding, denser layouts)

## Reference projects

- **Original PaperBack 1.10** by Oleh Yuschuk — ground truth for the bitstream format. Source zip: `paperbak-1.10.src.zip` from <https://www.ollydbg.de/Paperbak/>. The `paperbak-1.10` source tree is the canonical reference; ampaper must round-trip the same bytes.
- **MRPODS** — modern C++ rewrite at <https://github.com/sheafdynamics/mrpods>. Useful for: GUI / menu structure (better than the 1.10 layout), modern build conventions, the WinMain shutdown fix. Not useful for: AES (removed in mrpods); some load-bearing memory hacks that we should *not* port (Rust solves them properly).

## Not affiliated

ampaper is an independent re-implementation. It is not affiliated with, endorsed by, or sponsored by Oleh Yuschuk, the original PaperBack project, or the MRPODS project. For the original Borland C++ version, go to [ollydbg.de/Paperbak](https://www.ollydbg.de/Paperbak/).
