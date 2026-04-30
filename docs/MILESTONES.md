# ampaper milestones

Phased plan. Each milestone is a unit of "this works end-to-end and ships," not a checklist of internal refactors. Order is dependency-driven: every M consumes only what M-prior produced.

The first six milestones (M1–M6) establish **binary compatibility** with PaperBack 1.10. Nothing past M7 happens until M6 is green; otherwise we end up with a self-consistent fork that can't read existing prints.

## M0 — Scaffolding (this commit)

- Repo skeleton, GPL-3.0 license, NOTICE attribution to ollydbg + mrpods
- Empty `Cargo.toml`, stub `src/main.rs`
- Analysis docs: `PAPERBAK-ORIGINAL.md`, `MRPODS-DELTA.md`, `ENCRYPTION-DECISION.md`, this file
- No code yet

## M1 — Format spec + golden vectors

The single highest-leverage milestone. Output is a written-down, byte-level spec of the 1.10 page format and a `tests/golden/` directory of real 1.10 prints to decode against.

- [ ] Pull `paperbak-1.10.src.zip`; place under `reference/paperbak-1.10/` (gitignored).
- [ ] Write `docs/FORMAT-V1.md` covering: page header, block header, sync raster geometry, RS interleaving, AES envelope, bzip2 framing, BMP-on-disk vs printer-DC layout. Cite source file + line for every constant.
- [ ] Capture 5–10 golden PNGs of real 1.10 prints (with known-plaintext input files) under `tests/golden/v1-paperbak/`. These are the unit-test ground truth forever.
- [ ] Document the "decode this PNG, expect this SHA-256" check.

**Done when:** the format doc is complete enough that a third party could implement a decoder from it, and we have enough golden vectors that "PaperBack 1.10 print round-trips" is a real test, not a vibe.

## M2 — Reed-Solomon

- [ ] Pick a Rust RS crate compatible with Phil Karn's parameters (n, k, primitive polynomial, generator). Preferred: `reed-solomon-erasure` or hand-rolled to match exactly.
- [ ] Property test: encode → introduce ≤ (n-k)/2 errors → decode = identity.
- [ ] Vector test: decode a known 1.10 RS-encoded byte block and match the source.

## M3 — bzip2

- [ ] Wrap the `bzip2` crate (Rust bindings to libbzip2) with the upstream block-size choice pinned to whatever 1.10 uses.
- [ ] Round-trip property test against `bzip2` CLI output.
- [ ] Decode a 1.10 captured bzip2 stream → match plaintext.

## M4 — Page geometry + dot grid

Pure software, no printer. Encode a known byte sequence to a `Vec<u8>` representing the page bitmap; decode the same bitmap back. No printing, no scanning yet.

- [ ] Synthesize the sync raster + dot grid layout from M1's spec.
- [ ] Encode a fixed 500-byte payload to a 600-DPI bitmap; decode it; assert byte-equal.
- [ ] Property test: random payloads round-trip through the full geometry pipeline.

## M5 — Encoder pipeline (file → bitmap)

End-to-end: bytes in, valid 1.10 page bitmap out (saved as PNG/BMP for now).

- [ ] `encode(file_bytes, options) -> Vec<PageBitmap>` where `options` covers compression on/off, encryption on/off, redundancy.
- [ ] Output a single test PNG that, when printed and scanned, decodes correctly in PaperBack 1.10. (Manual cross-check; first sign of real success.)

## M6 — Decoder pipeline (bitmap → file)

End-to-end the other way.

- [ ] `decode(scanned_png) -> Result<Vec<u8>>`.
- [ ] Decode every golden vector from M1; assert SHA-256 match.
- [ ] Decode our own M5 encoder output; assert byte-identity.

**M6 is the binary-compatibility gate.** Don't start M7 until this is fully green.

## M7 — Legacy AES-192 read path

- [ ] Implement the 1.10 KDF + CBC envelope **decode-only**.
- [ ] Mark every legacy-AES-192 code path with `#[deprecated]` + a doc comment pointing to the security caveats.
- [ ] Decode a 1.10 AES-192-encrypted golden vector; assert plaintext match.

## M8 — Win32 GUI

Match mrpods' menu structure (it's better than 1.10's). Win32 via `windows-rs`, comctl32 v6.

- [ ] Main window + menu bar (File / Edit / View / Help structure cribbed from mrpods)
- [ ] Encode dialog: file picker → options (compression / encryption mode / redundancy) → progress
- [ ] Decode dialog: bitmap picker → preview → "save decoded file"
- [ ] About box with proper attribution to ollydbg + mrpods + ampaper

## M9 — Printing

- [ ] GDI / GDI+ print path for the page bitmap. Honor printer DPI; don't let drivers re-sample the dot grid.
- [ ] Print preview.
- [ ] Multi-page print job (one page = one bitmap output from M5).

## M10 — Scanning

- [ ] WIA 2.0 primary: device picker, scan to bitmap, hand off to M6 decoder.
- [ ] TWAIN fallback: spawn a 32-bit helper process to drive 32-bit DSM-only scanners (the bridge mrpods never built).
- [ ] Auto-detect ≥ 900-DPI capability; warn if user picks a profile under spec.

## M11 — AES-256-GCM forward mode

- [ ] New "ampaper format v2" flag in the page header (does NOT collide with v1).
- [ ] AES-256-GCM with PBKDF2-HMAC-SHA-256 KDF (parameters in `ENCRYPTION-DECISION.md`).
- [ ] Encrypt-write + decrypt-read; property tests; KAT vectors.
- [ ] Refuse to write v1 AES-192 (read-only legacy posture).

## M12 — Density experiments

Past parity, the actually-fun part. Goals: more bytes per sheet without sacrificing the redundancy posture that makes paper backups archival-grade.

- [ ] Color encoding: print CMYK / RGB sub-channels independently. 4× density floor; needs a color-corrected scan path.
- [ ] Adaptive RS: vary (n, k) per-block based on local optical conditions detected at decode time.
- [ ] Dot-shape exploration: round vs square vs hatched, fitness against real-world fading patterns.
- [ ] Algorithmic geometry: hexagonal packing for higher dot-density at fixed printer DPI.

Each experiment ships behind a v2-flag bit; v1 compatibility never regresses.

## Out of scope (parity / soundness reasons)

- Cross-platform GUI (Linux / macOS): the encoder/decoder library is portable, the Windows GUI is not. A separate `ampaper-cli` crate covers headless usage.
- Mobile capture (phone-camera as scanner): interesting but requires real perspective-correction work; deferred.
- Cloud / network sync: this is an offline tool by design.
