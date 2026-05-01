# Third-party dependency licenses

ampaper is licensed **GPL-3.0-or-later** (see [LICENSE](LICENSE)).
Dependencies in `Cargo.lock` are listed below with their declared
SPDX license identifiers. Every entry is GPL-3.0-compatible per the
[FSF GPL-compatible license list](https://www.gnu.org/licenses/license-list.html);
the combined work ships under GPL-3.0-or-later.

The list reflects `Cargo.lock` at commit time. Run
`cargo metadata --format-version 1` (or `cargo tree`) to regenerate.

## Runtime dependencies

| Crate | Version | License |
|---|---|---|
| `bzip2` | 0.6 | MIT OR Apache-2.0 |
| `libbz2-rs-sys` | 0.2 | bzip2-1.0.6 |

`bzip2-1.0.6` is Julian R. Seward's original libbzip2 license — a
BSD-style permissive grant covering the algorithm and reference
implementation. `libbz2-rs-sys` is a pure-Rust reimplementation
distributed under that same terms; see
<https://github.com/trifectatechfoundation/libbzip2-rs>. FSF lists
the libbzip2 license as GPL-compatible.

## Test-only dependencies

These ship with `cargo test` builds but are not in the release
binary. They support golden-vector decoding tests against PaperBack
1.10's BMP encoder output.

| Crate | Version | License |
|---|---|---|
| `image` | 0.25 | MIT OR Apache-2.0 |
| `bytemuck` | 1 | Zlib OR Apache-2.0 OR MIT |
| `byteorder-lite` | 0.1 | Unlicense OR MIT |
| `moxcms` | 0.7 | BSD-3-Clause OR Apache-2.0 |
| `pxfm` | 0.1 | BSD-3-Clause OR Apache-2.0 |
| `png` | 0.18 | MIT OR Apache-2.0 |
| `num-traits` | 0.2 | MIT OR Apache-2.0 |
| `bitflags` | 2 | MIT OR Apache-2.0 |
| `crc32fast` | 1 | MIT OR Apache-2.0 |
| `cfg-if` | 1 | MIT OR Apache-2.0 |
| `fdeflate` | 0.3 | MIT OR Apache-2.0 |
| `flate2` | 1 | MIT OR Apache-2.0 |
| `simd-adler32` | 0.3 | MIT |
| `miniz_oxide` | 0.8 | MIT OR Zlib OR Apache-2.0 |
| `adler2` | 2 | 0BSD OR MIT OR Apache-2.0 |

## Build-only dependencies

| Crate | Version | License |
|---|---|---|
| `autocfg` | 1 | Apache-2.0 OR MIT |

## Compatibility summary

- The standard Rust ecosystem pattern is **MIT-OR-Apache-2.0 dual
  license**, which is GPL-3.0-compatible: a GPL project picks the
  compatible side (Apache-2.0 for GPL-3, or MIT — both work) and the
  combined work is GPL-3.
- BSD-3-Clause, BSD-2-Clause, 0BSD, Unlicense, and Zlib are all
  permissive and GPL-3.0-compatible.
- The `bzip2-1.0.6` license is GPL-3.0-compatible per the FSF list.
- No AGPL, no GPL-2-only, no proprietary EULAs in the dependency
  graph. No copyleft conflicts.

## What's excluded from this list

- The original PaperBack v1.10 source under `reference/paperbak-1.10/`
  (gitignored, not bundled). It's GPL-3.0 itself; ampaper is a clean-
  room re-implementation that reads the source for understanding,
  not for direct copying.
- The original mrpods source under `reference/mrpods/` (also
  gitignored). Same posture — reference material, not a dependency.
- PaperBack 1.10's bundled bzip2 1.0.6 — we use the pure-Rust
  `libbz2-rs-sys` instead, so this isn't a runtime dep.
