// ampaper — Rust port of PaperBack 1.10.
// Copyright (C) 2026  ampaper contributors. Licensed GPL-3.0-or-later.
//
// The wire-format spec is docs/FORMAT-V1.md. The catalog of Borland-era
// hacks the Rust port deliberately does NOT transliterate is in
// docs/PAPERBAK-HACKS.md. Read both before extending this crate.

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
