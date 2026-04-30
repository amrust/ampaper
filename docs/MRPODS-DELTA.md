# mrpods — what to take, what to leave

[mrpods](https://github.com/sheafdynamics/mrpods) is a modern C++ rewrite of PaperBack 1.10 by sheafdynamics. It's a useful reference for ampaper because it's already done one round of "translate Borland C++ to MSVC C++ and find the bugs," but it's not a target — ampaper aims past mrpods on several axes.

## Take

- **GUI / menu structure.** mrpods' menu system is a clear improvement over PaperBack 1.10's. Layout, ordering, and dialog flow are worth mirroring for the ampaper Win32 GUI. (See `mrpods/Resource.rc` + `Controls.cpp` for the canonical layout.)
- **WinMain shutdown fix.** mrpods documents the original 1.10 `WinMain` exit bug; reuse the fix conceptually (Rust will structure the message loop differently anyway).
- **30%-faster decoder claim.** Whatever optimizations mrpods made to the decoder are worth diffing against 1.10 — likely cache-friendlier block traversal or SIMD-friendly RS arithmetic.
- **VS2022 toolchain expectations.** Resource compiler quirks, manifest format, comctl32 v6 dependence — applicable to our `cargo build` + `embed-resource` story.
- **File-structure inventory.** Same-named .cpp files as 1.10, but cleaned up — useful as a sanity check on the module split when we draft ampaper's `src/` tree.

## Leave

- **Removed AES.** mrpods strips encryption entirely and prints a deprecation message on legacy AES-192 documents. ampaper does **not** drop this. We need:
  - **Read-only AES-192** for legacy decode (so existing PaperBack 1.10 prints stay readable forever).
  - **Forward AES-256-GCM** as the new default (see `ENCRYPTION-DECISION.md`).
  - The bar for "remove encryption" is higher than mrpods set; we stay full-featured.
- **Disabled compiler optimizations.** mrpods turns off optimizer flags "for stability" — that's a code-smell sign of UB the rewrite didn't catch. In Rust, we don't get that escape hatch (and don't need it).
- **One-off ChatGPT-suggested memory hacks.** The mrpods commit history / comments mention spots where adjustments were made empirically without understanding the root cause. Do **not** port any of those 1:1; if a Rust port hits a similar symptom, root-cause it in the format spec instead. This is the central reason we're rewriting in Rust.
- **Incomplete x64 scanner story.** mrpods' README has a TODO: *"Add a scanning bridge or switch to WIA to resolve TWAIN x86 dependency in x64."* That's the exact gap ampaper closes by making WIA the primary path on Windows.

## Cross-check uses

For every encoder/decoder change we make, **three-way cross-check**:

1. ampaper round-trips its own output — fastest signal.
2. ampaper decodes a 1.10-printed PNG (golden vector). Hard ground truth.
3. mrpods decodes ampaper's "legacy mode" output. Confirms we're not silently drifting from the format on the encode side.

If all three agree, we're correct. If only (1) agrees, we have a self-consistent fork that breaks compatibility — bad.

## Architectural choices that diverge

| Concern | mrpods | ampaper |
|---|---|---|
| GUI toolkit | Win32 + comctl32 | Win32 + comctl32 (same — keeps the look + matches mrpods' superior menu) |
| Scanner | TWAIN (x64 broken) | WIA primary, TWAIN-via-32-bit-bridge fallback |
| Encryption | none | AES-192 read, AES-256-GCM write |
| Build | VS2022 .sln | `cargo build` + `embed-resource` |
| Testing | none documented | property tests (proptest) + golden-vector decode |
| Cross-platform | Win-only | core encoder/decoder cross-platform; GUI / printing / scanning Windows-only |

## Open mrpods questions

- What were the specific Borland → MSVC pain points? Reading the early commit history of mrpods would surface the spots most likely to have similar Borland-isms still hidden in 1.10. **TODO** when we have the repo cloned to `reference/mrpods/`.
- Did mrpods change any binary-format constants? The README claims compatibility with "legacy unencrypted documents" — we need to verify they didn't subtly drift the page header / RS block size / interleaving when modernizing.
