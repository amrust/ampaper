// ampaper — Rust port of PaperBack 1.10.
// Copyright (C) 2026  ampaper contributors. Licensed GPL-3.0-or-later.
//
// The wire-format spec is docs/FORMAT-V1.md. The catalog of Borland-era
// hacks the Rust port deliberately does NOT transliterate is in
// docs/PAPERBAK-HACKS.md. Read both before extending this crate.
//
// Module layout — legacy decoder vs v3 codec:
//   - `block`, `bz`, `crc`, `decoder`, `dot_grid`, `ecc`, `encoder`,
//     `format_v2`, `legacy_aes`, `page`, `scan`, `v2_crypto` are
//     the LEGACY codec (PB-1.10 v1 + ampaper v2 forward-format with
//     AES-256-GCM). FROZEN: every byte ever printed by PB 1.10 or
//     by ampaper v1/v2 still decodes through this code, and that
//     guarantee is load-bearing. Bug fixes only. Do not refactor.
//   - `v3` is the new codec (M12+, see docs/FORMAT-V3.md). Designed
//     from scratch for higher density via RaptorQ rateless ECC,
//     page-level finder patterns, and (eventually) multi-channel
//     CMYK modulation. Lives alongside the legacy decoder, not on
//     top of it — `v3` and the legacy modules share no code, only
//     low-level primitives that don't constrain either format.

pub mod block;
pub mod bz;
pub mod crc;
pub mod decoder;
pub mod dot_grid;
pub mod ecc;
pub mod encoder;
pub mod format_v2;
pub mod legacy_aes;
pub mod page;
pub mod scan;
pub mod v2_crypto;
pub mod v3;
