// ampaper v3 — modern, efficient codec.
//
// v3 is a clean-sheet design alongside (NOT replacing) the legacy
// v1/v2 decoder. The legacy decoder lives in `crate::decoder`,
// `crate::scan`, and `crate::format_v2` and stays frozen — it
// continues to read every PB-1.10 BMP and every ampaper-v1/v2 PDF
// ever produced. v3 is a parallel codec for new archives.
//
// Phase 1 (this module's first slice) — bytes-level codec:
//   - RaptorQ (RFC 6330) rateless ECC: any K + small overhead
//     encoded packets recover the source. Replaces v1's fixed
//     redundancy=5 RS+XOR group structure, eliminating the
//     per-page wasted-trailing-cells remainder that group layout
//     produces.
//   - 24-byte v3 header: 8-byte magic `AMPAPER3` + version + 12-
//     byte RaptorQ OTI. Distinct from any v1/v2 header so the
//     legacy decoder will never mis-route a v3 input.
//   - No bitmap layer yet, no compression, no encryption. Each
//     comes in a later phase.
//
// Subsequent phases (planned):
//   2. Cell + bitmap layer — pack RaptorQ packets into 32×32 dot
//      cells, page-level finder patterns (vs PB-1.10's per-cell
//      separators), tighter packing.
//   3. Compression + encryption — zstd-long pre-compression,
//      AES-256-GCM as a v2-style outer layer.
//   4. Higher dot densities — 250-300 dot/inch with calibration
//      strip + page corner fiducials.
//   5. Modulation upgrade — 4-level grayscale (2 bits/dot).
//   6. CMYK — 4 channels × 1 bit per channel ≈ 3 effective bits/
//      dot in practice (see docs/FORMAT-V3.md "Channel asymmetry").
//
// See docs/FORMAT-V3.md for the wire-format spec. Each phase
// extends the spec with backward-incompatible bumps to the version
// byte; v3 decoders MUST reject unknown versions.

pub mod cell;
pub mod codec;
pub mod decoder;
pub mod encoder;
pub mod format;
pub mod page;

pub use codec::{
    PageDecodeError, PageEncodeError, decode_pages, encode_pages,
};
pub use decoder::{DecodeError, decode};
pub use encoder::{EncodeError, EncodeOptions, encode};
pub use page::{PageBitmap, PageGeometry};
