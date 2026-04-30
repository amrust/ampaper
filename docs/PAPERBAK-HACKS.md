# PaperBack 1.10 — Borland-isms and weird hacks catalog

This is the inventory of things in `reference/paperbak-1.10.src/` that look like (a) Borland-specific compiler / runtime dependencies, (b) hand-rolled memory or layout assumptions, or (c) intentional-but-cryptic bit hacks.

The point of this catalog is the rule from `feedback_no_borland_hacks.md`: when porting to Rust, **don't transliterate** these. Every entry is tagged with how to handle it:

- **FORMAT** — load-bearing for v1 binary compatibility. Rust must reproduce the same observable bytes (this is "the wire is the spec"). The C technique used is incidental; the result must match.
- **TOOLCHAIN** — Borland C++Builder 5.03-specific syntax/lib. Drop entirely; Rust + cargo replaces it.
- **PORT-CLEAN** — works fine in C, but the Rust idiom is different (and better). Don't copy the structure; re-derive from purpose.
- **SMELL** — the source itself flags it as suspect (a comment, an "empirical" tag, or a "hack" in a comment). Investigate root cause; don't paper over with an equivalent Rust hack.

References cite `file:line` in `reference/paperbak-1.10.src/`.

---

## 1. Toolchain — drop entirely

### 1.1. C++Builder 5.03 project files
**TOOLCHAIN.** `PaperBak.bpr` (XML project, BCB.05.03), `paperbak.mak` (Borland make), `PaperBak.res` / `Resource.res` (compiled resources). `Vcl50.lib` runtime, `tlink32` linker, compile flags `-O2 -a8 -b -d -k- -tW -tWM -Vx -Ve -ff -X-`. **Replacement:** `Cargo.toml` + `embed-resource` for `.rc` files.

### 1.2. `#pragma hdrstop`
**TOOLCHAIN.** Borland precompiled-header sentinel, present in every .cpp. **Replacement:** none — Rust has no header concept.

### 1.3. Borland path split: `fnsplit` / `fnmerge` and `MAXPATH` / `MAXFILE` / `MAXEXT` / `MAXDIR` / `MAXDRIVE`
**TOOLCHAIN.** `Service.cpp:78,81-85`, `Printer.cpp:523-524, 952`. Borland-defined macros (260 / 9 / 5 / 66 / 3 on Win32). **Replacement:** `std::path::Path` / `PathBuf` from the Rust stdlib.

### 1.4. `stricmp` / `strnicmp`
**PORT-CLEAN.** `Fileproc.cpp:69`, `Main.cpp:389`. Case-insensitive compare; ASCII-only and not Unicode-aware. **Replacement:** `str::eq_ignore_ascii_case` for ASCII, `str::to_lowercase().eq(...)` for Unicode-correct compares (we should never need the latter for filename matching here).

### 1.5. `GlobalAlloc(GMEM_FIXED, ...)` / `GlobalFree`
**PORT-CLEAN.** Used everywhere for buffers (`Printer.cpp:303,310,720`, `Fileproc.cpp:95-96`, etc.). Win16-era heap API; on modern Windows it's a thin wrapper over `HeapAlloc`. **Replacement:** `Box<[u8]>` / `Vec<u8>` — Rust's allocator handles all of this.

### 1.6. The `#define unique` extern trick
**PORT-CLEAN.** `paperbak.h:20-24` — `MAINPROG` defined in one .cpp, all globals (`hinst`, `hwmain`, `pagesetup`, `printdata`, `procdata`, `fproc[NFILE]`, `password`, ...) declared via `unique` so the same header serves as both `extern` and definition. C trick to avoid splitting decl/def. **Replacement:** Rust modules + `pub static` / `OnceLock`. The whole "globals everywhere" pattern goes away when the encoder/decoder become structs.

### 1.7. `#if sizeof(t_data)!=128`
**SMELL.** `paperbak.h:66-68, 88-90`. Per ISO C/C++, `sizeof` is not legal in a preprocessor `#if` (it's evaluated at compile time, not preprocess time). Borland's preprocessor accepted it; MSVC and GCC reject it. The intent is right (assert struct layout); the syntax is non-standard. **Replacement:** Rust `const _: () = assert!(std::mem::size_of::<Block>() == 128);` at module level.

### 1.8. `OPENFILENAME_SIZE_VERSION_400` workaround
**TOOLCHAIN.** `Service.cpp:64,88` — `lStructSize = min(OPENFILENAME_SIZE_VERSION_400, sizeof(ofn))`. Pins the struct size to the older Win9x layout to dodge a Windows version-check quirk. Comment: *"Correct Windows bu... feature."* **Replacement:** if/when we wire the file picker via `windows-rs`, use the modern struct directly; the Win9x compatibility era is over.

### 1.9. Date-format space-instead-of-NUL trick
**SMELL.** `Service.cpp:53`: `s[l-1]=' ';  // Yuck, that's Windows`. Rewrites the NUL terminator from `GetDateFormat`'s output as a space so `GetTimeFormat` appends after it without overwriting. The Yuck comment is the developer flagging their own workaround. **Replacement:** format date+time directly with `chrono::format`/`SystemTime`, no concat-via-NUL-stomp.

### 1.10. `typedef unsigned long ulong`
**SMELL.** `paperbak.h:40`. `long` is 32-bit on Win32 (LLP64) but **64-bit on Linux/macOS x64 (LP64)**. The whole format spec implicitly assumes `ulong == u32` — true on Win32, false on Linux64. The compile-time `sizeof(t_data)==128` assertion would fire on a Linux 64-bit build, which is the reason it was put there. **Replacement:** Rust uses `u32` explicitly. Same outcome, no ambiguity.

### 1.11. Fixed-size `res[64][2]` for printer resolutions
**SMELL.** `Printer.cpp:107`: comment *"I'm too lazy to allocate the memory dynamically and assume that no sound driver will support more than 64 different resolutions."* Fine in 2007, still fine in 2026, but it's a documented shortcut. **Replacement:** dynamic `Vec<(u32,u32)>` in Rust — there's no laziness penalty.

---

## 2. Memory layout hacks

### 2.1. Salt + IV stuffed into `name[64]`
**FORMAT.** `Printer.cpp:472`, `Fileproc.cpp:303` — explicit comment: `// hack: put the salt & iv at the end of the name field`. The 64-byte name field is split: bytes 0..32 = filename (capped at 31 chars + NUL), bytes 32..48 = AES PBKDF2 salt, bytes 48..64 = AES-CBC IV.

This is the **most explicit hack in the codebase** and is **load-bearing for v1 read**. Encoder caps the filename at 31 chars by `strncpy(name, fil, 32)` followed by `name[31] = '\0'` (`Printer.cpp:526-527`), reserving the upper 32 bytes for the crypto material regardless of whether encryption is on.

**For ampaper v1 (read):** reproduce exactly. Document this dual layout in the wire spec.
**For ampaper v2 (write):** stop doing this. v2 page header gets a proper `salt: [u8; 32]` and `iv: [u8; 12]` field, and `name` is allowed up to 64 useful chars. See `ENCRYPTION-DECISION.md`.

### 2.2. Recovery-block tag in high nibble of `addr`
**FORMAT.** `Printer.cpp:881`: `cksum.addr = offset ^ (redundancy<<28)`. Decoder: `Decoder.cpp:859-862`: `addr & 0x0FFFFFFF` is real offset, `(addr>>28) & 0x0F` is `ngroup` (0=data, >0=recovery). Maxes file size at 2^28 = 256 MB (matches `MAXSIZE = 0x0FFFFF80` at `paperbak.h:52`).

**FORMAT.** Reproduce as-is for v1. (For v2 we'd use a `flags: u8` byte and a full 32-bit `offset`, but that's a v2-flag-bit decision.)

### 2.3. Recovery-block init pattern: `cksum.data = 0xFF; cksum ^= block_i`
**FORMAT.** `Printer.cpp:882-897`. The recovery block starts as 0xFF in every byte, then XORs each data block in. Decoder inverts (`Fileproc.cpp:217`: `*pr++^=0xFF`) and XORs all *valid* blocks of the group, leaving the missing one. Cute trick — the inversion on decode means a complete-group XOR yields all-ones.

Reproduce as-is — it's what's on the wire.

### 2.4. Per-row XOR mask `0x55555555` / `0xAAAAAAAA`
**FORMAT.** `Printer.cpp:181-184` (encode), `Decoder.cpp:224-225` (decode). Even rows XOR'd with `0x55555555`, odd with `0xAAAAAAAA`. Goal: a block of all-zeroes or all-FFs becomes a checkerboard, not a solid square — better for the optical decoder (no large constant regions to fail edge detection on).

Reproduce as-is.

### 2.5. CRC final XOR `0x55AA`
**FORMAT.** `Printer.cpp:174`, verified at `Decoder.cpp:235`. Stored CRC = `Crc16(addr+data) ^ 0x55AA`. Defends against a "block of all zeros that happens to have CRC 0" false-positive read (a missing/blank block could otherwise be mistaken for a valid all-zero block).

Reproduce as-is.

### 2.6. AES alignment — bzip2 buffer padded to 16 bytes with zeros
**FORMAT.** `Printer.cpp:417-420`: `alignedsize = (datasize+15) & ~15`, then zero-fill. bzip2 ignores trailing zeros, so on decode no special handling is needed. The reported `superdata.datasize` is the **aligned** size, not the raw bzip2 size.

This is correct format-level behavior (AES-CBC needs 16-byte blocks); reproduce as-is.

### 2.7. Pointer-arithmetic into struct fields
**SMELL.** `salt = (uchar*)(superdata.name) + 32`. Relies on `name[64]` being a flat byte array with no padding — true here (it's `char[64]`), but the pattern hides format constants in pointer math. **Rust replacement:** define `name: [u8; 32]`, `salt: [u8; 16]`, `iv: [u8; 16]` as separate fields in a `#[repr(C)]` struct, asserting total size 64. Same wire bytes, no pointer arithmetic.

---

## 3. Empirical / unexplained constants

These are decoder-side heuristics. The format doesn't depend on them — they're in **how to read a noisy scan**, not **what's on the page**. The Rust port can replace them with cleaner algorithms (or keep them as-is if they ship working). Calling them out so we don't reverently transliterate magic numbers.

### 3.1. The "two bottles of Weissbier" rotation formula
**SMELL (self-flagged).** `Printer.cpp:906-907`:
```c
// Optimal shift between the first columns of the strings is
// nx/(redundancy+1). Next line calculates how I must rotate the j-th
// string. Best understandable after two bottles of Weissbier.
rot = (nx/(redundancy+1)*j - k%nx + nx) % nx;
```

This is **FORMAT** — it determines which cell each block goes in, so the layout is on-the-wire. But the formula's derivation isn't obvious. Test it by encoding a known page with the same `nx`/`redundancy` settings and verifying every block lands in the cell PaperBack 1.10 puts it in.

### 3.2. Sharpness factor `1.3/dotsize - 0.1`
**SMELL.** `Decoder.cpp:533`: `sharpfactor += 1.3/dotsize - 0.1` with self-comment "this correction is empirical." Clamped `[0, 2]`. **Replacement:** keep as-is for round-trip parity in M6; revisit with a better optical model post-parity.

### 3.3. Border-correction empirical formula
**SMELL.** `Decoder.cpp:526-530`:
```c
// Empirical formula: the larger the angle, the more imprecise is the
// expected position of the block.
if (border<=0.0) {
  border = max(fabs(angle_x), fabs(angle_y))*5.0 + 0.4;
}
```
**Replacement:** keep, document.

### 3.4. Peak threshold `amax*3/4`
**SMELL.** `Decoder.cpp:78`: peak threshold = 75% of max amplitude, with the comment "This solution at least works in 90% of all cases." **Replacement:** keep, but understand: the decoder gives up on 10% of pages where this fails. A more principled threshold (e.g. Otsu's method on the histogram) would help, but only after parity.

### 3.5. The 9-combo factor/threshold matrix in `Recognizebits`
**SMELL.** `Decoder.cpp:177-187`: tries `(factor, lcorr)` ∈ {(1000,0), (32,0), (16,0), (1000,Δ), (32,Δ), (16,Δ), (1000,-Δ), (32,-Δ), (16,-Δ)} where Δ = (cmin-cmax)/16. Brute-force search across thresholds. With 8 orientations × 9 dot-shifts × 4 dot-sizes × 9 (factor,threshold) combos that's ~2592 candidate decodes per block. **Replacement:** keep; it's the recognition heuristic, not the format. It's slow but correct.

### 3.6. "Square Roots and Other Incomprehensible Things"
**SMELL (self-flagged, comedic).** `Decoder.cpp:773-776`: pseudo-dispersion calculation, dev acknowledges it's not real statistical dispersion:
```c
// Dispersion in the mathematical sense is a bit different beast
// (includes Division, Square Roots and Other Incomprehensible
// Things), but we are interested only in the shift corresponding
// to the maximum.
disp = syy*SUBDX*SUBDY - sy*sy;
```
The 20%-as-10% threshold (`Decoder.cpp:787`) is also empirical. **Replacement:** keep, document; revisit post-parity.

### 3.7. `static int lastgood` in `Recognizebits`
**PORT-CLEAN.** `Decoder.cpp:163`: function-local `static int lastgood` caches the last working `(factor, lcorr)` combo to start the next call's search there — usually the same combo works for the whole page. Stateful function-local statics are not idiomatic Rust. **Replacement:** carry `last_good_combo: u8` on the decoder struct.

---

## 4. Goto / control flow

### 4.1. `goto finish` in `Decode8`
**PORT-CLEAN.** `Ecc.cpp:135, 202, 232`. Phil Karn's RS implementation uses `goto finish` for early exits (no syndrome error → success; Berlekamp-Massey degree mismatch → error). Standard 1990s C-style for cleanup. **Replacement:** Rust `Result<(), DecodeErr>` with early `return`. The RS algorithm is unchanged — just the control flow.

---

## 5. Crypto risks (not Borland, but worth flagging)

### 5.1. Precompiled `crypto.lib`
**SMELL — format-critical unknown.** `CRYPTO/crypto.lib` ships precompiled with no `.c` source in the tree. Headers (`pwd2key.h`, `aes.h`, `hmac.h`, `sha2.h`) tell us the API but not the implementation. Specifically: `derive_key` is "an implementation of RFC2898 PBKDF2" but **we don't know which HMAC variant**. Most likely PBKDF2-HMAC-SHA-256 (since `sha2.h` is the only hash header), but it could be HMAC-SHA-1.

**M7 gate:** before declaring v1 AES-192 read complete, run a known-vector test: take `password="foo"`, `salt=[0u8;16]`, `iter=524288`, derive a 24-byte key with our reimplementation, and compare bytewise against a key derived from `crypto.lib` against a captured 1.10 ciphertext. If wrong, switch hash and retry. This is documented as TODO in `FORMAT-V1.md` §6.3.2.

### 5.2. CRC-16 as integrity check, not MAC
**FORMAT (legacy weakness).** `Saverestoredfile` (`Fileproc.cpp:318-322`) uses `Crc16(plaintext)` to confirm the password was right. CRC is not a MAC; a forged ciphertext that happens to decrypt to a valid CRC passes the check. Documented in `ENCRYPTION-DECISION.md` as a reason v2 uses AEAD.

### 5.3. Password in a global `extern` buffer
**PORT-CLEAN.** `paperbak.h:367`: `unique char password[PASSLEN]` — global, accessible from any TU. After key derivation, source carefully wipes it (`Printer.cpp:478`, `Fileproc.cpp:305`). The wipe is correct; the global location is the problem (a wider attack surface than necessary, and there are paths that don't wipe). **Replacement:** in Rust, pass password as a stack-local `Zeroizing<String>` (from the `zeroize` crate) into the KDF and let it drop. No globals, no missed wipes.

### 5.4. AES-192 vs the 2007 standard
**FORMAT.** Choice of AES-192 over AES-256 was a 2007 reaction to the ~50-bit-effective-strength flaw in PaperBack 1.00. Not a hack — a deliberate format choice. Documented in `PAPERBAK-ORIGINAL.md` and `ENCRYPTION-DECISION.md`.

---

## 6. Silent-failure patterns

### 6.1. Compression silently disabled on init failure
**SMELL.** `Printer.cpp:332-339`:
```c
success=BZ2_bzCompressInit(&print->bzstream, ...);
if (success!=BZ_OK) {
  print->compression=0;              // Disable compression
  print->step++;
  return;
};
```
User asked for compression; if bzip2 init fails, encode proceeds without it and the user is never told. **Replacement:** propagate the error. A failed compression init should surface a real error, not silently change behavior.

### 6.2. Compression silently disabled on grow-in-buffer
**FORMAT-ADJACENT.** `Printer.cpp:371-377, 397-403`: if compression *grows* the data (already-compressed input) the encoder restarts from scratch with compression off and clears `PBM_COMPRESSED`. This **is** observable on the wire (the `mode` byte changes), so a v1 round-trip needs to reproduce the behavior. But the user-facing pattern (silently change settings) is wrong. **Replacement:** detect the grow case **before** writing, surface a "compression made it bigger; switching to stored" notice to the UI, then proceed.

---

## 7. Threading / state

### 7.1. Singleton globals everywhere
**PORT-CLEAN.** `paperbak.h:42,184,241,285,297,328,362-385` — every piece of program state is a global (`hinst`, `hwmain`, `pagesetup`, `printdata`, `procdata`, `fproc[NFILE]`, `password`, `infile`, `outfile`, `inbmp`, `outbmp`, `dpi`, `dotpercent`, `compression`, `redundancy`, `printheader`, `printborder`, `autosave`, `bestquality`, `encryption`, `opentext`, `marginunits`, `marginleft`, `marginright`, `margintop`, `marginbottom`). The whole app is one big mutable global state.

This is a UI concern, not a format one — but it means there's no way to encode two files concurrently, no way to embed the decoder in another tool, and no way to test in parallel. **Replacement:** turn `printdata` and `procdata` into structs owned by the caller. The encoder/decoder become library functions taking `&mut EncodeState` / `&mut DecodeState`. Keep `t_fproc` as a plain `FileBuilder` struct that the multi-page decoder accumulates into.

### 7.2. Greedy orientation lock
**PORT-CLEAN (kept-but-documented).** `Decoder.cpp:170-171, 239`: on the first block that decodes successfully with a given orientation, the orientation is locked for the rest of the page. Saves ~8× decode work, but means a single false-positive on a low-quality page can lock the wrong orientation. **Replacement:** keep the optimization but track confidence; on multiple subsequent failures, unlock and retry orientation detection.

---

## 8. Known-format constraints worth re-flagging

These are not hacks, just non-obvious format rules that fall out of the implementation. Listed here because if you skim the source you'd miss them.

- **Filename is 31 chars + NUL, not 64.** Even when encryption is off, the high 32 bytes of `name[64]` are not used (`Printer.cpp:526-527` caps at 31; `name[31]=0`). This is to keep the salt+IV slot reserved unconditionally.
- **`superdata.datasize` is post-AES-padding size, not bzip2 output size.** (`Printer.cpp:417` aligns then assigns at `:511`.)
- **Page count is computed from `datasize / pagesize`.** First page is page 1 (1-based), `superdata.page` is 1-based, decoder converts via `(page-1)*pagesize/NDATA` to find first block on page (`Fileproc.cpp:234`).
- **Maximum file size is 256 MB minus 128 bytes** (`MAXSIZE = 0x0FFFFF80`). The high nibble of `addr` is taken by the recovery-block tag (§2.2), so byte offsets are limited to 28 bits.
- **AES key length is fixed at 24 bytes (AES-192).** Comment at `paperbak.h:35` says *"AES key length in bytes (16, 24, or 32)"* — implying you could change it — but the rest of the codebase hardcodes the format identifier, so changing this would break compatibility.

---

## 9. What to carry forward into ampaper

**Reproduce verbatim (FORMAT entries):** §2.1, §2.2, §2.3, §2.4, §2.5, §2.6, §3.1 (the rotation formula — must produce same cell layout), §5.4 (AES-192 read-only).

**Drop entirely (TOOLCHAIN):** §1.1 through §1.11. Cargo + Rust stdlib replaces all of it.

**Re-derive cleanly (PORT-CLEAN):** §1.4, §1.5, §1.6, §3.7, §4.1, §5.3, §7.1, §7.2. Same observable behavior, idiomatic Rust shape.

**Investigate root cause (SMELL):** §1.7 (use Rust's compile-time assert), §1.10 (use `u32`), §3.2-§3.6 (keep for parity, replace post-M6), §5.1 (M7 must verify HMAC variant experimentally), §6.1 (don't silently disable), §6.2 (don't silently disable settings; surface to UI).

**Format-critical unknowns to verify before claiming v1 compat:**
- §5.1 — PBKDF2 hash variant.
- §3.1 — the Weissbier rotation formula's actual cell mapping for all `(nx, redundancy)` combos seen in the wild.

These are the M1-finishing items. Once both are verified against captured 1.10 prints, M1 is complete and M2 (Reed-Solomon) can start.
