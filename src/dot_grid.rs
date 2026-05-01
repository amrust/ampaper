// Block bytes <-> 32x32 dot grid conversion. Per FORMAT-V1.md §2.4
// (and `Printer.cpp:179-198` / `Decoder.cpp:224-225`):
//
// Each row j of the 32-by-32 dot grid is one 32-bit little-endian
// u32 of the block's 128 wire bytes (so row j reads bytes[4*j..4*j+4]),
// XOR'd with a per-row striping mask:
//   - even rows (j % 2 == 0): mask = 0x55555555
//   - odd  rows (j % 2 == 1): mask = 0xAAAAAAAA
//
// Within a row, bit 0 (the LSB of the masked u32) is drawn at the
// leftmost dot column; bit 31 at the rightmost. A set bit means a
// black dot on paper.
//
// The XOR scramble exists to defend against optical decoding pain on
// nearly-all-zero or nearly-all-FF blocks: it converts those into
// regular checkerboards rather than solid black/white squares. It is
// part of the format, not a presentation choice — the decoder must
// undo the same masks before interpreting the bits as block bytes.
//
// This module is the wire layer between `crate::block` (128 bytes
// shaped as Block / SuperBlock) and the page-level rendering layer
// (M4 — converting these 32-row grids into actual pixel bitmaps).

use crate::block::BLOCK_BYTES;

/// One row of a 32-by-32 dot grid, packed as a u32. Bit 0 = leftmost
/// dot (column 0); bit 31 = rightmost (column 31). A set bit indicates
/// a black dot on paper. Stored AFTER the per-row XOR scramble — i.e.
/// these are the bits as drawn, not the underlying block bytes.
pub type GridRow = u32;

/// Number of rows / columns in a block's dot grid. Equals
/// [`crate::block::NDOT`]; named locally to keep the module
/// self-contained.
pub const ROWS: usize = 32;

/// Per-row XOR mask applied to the underlying u32 before drawing.
/// Even rows get `0x55555555`; odd rows get `0xAAAAAAAA`. Mirrors
/// `Printer.cpp:181-184` (encode) and `Decoder.cpp:224-225` (decode).
const fn row_mask(row: usize) -> u32 {
    if row & 1 == 0 {
        0x5555_5555
    } else {
        0xAAAA_AAAA
    }
}

/// Convert a 128-byte block (in wire form) into the 32 rows of dot
/// bits as they will appear on paper. The XOR striping is applied;
/// callers can render each `GridRow` directly as a bitmap row by
/// walking bits from LSB (column 0) to MSB (column 31).
#[must_use]
pub fn block_to_dot_grid(bytes: &[u8; BLOCK_BYTES]) -> [GridRow; ROWS] {
    let mut grid = [0u32; ROWS];
    for (j, slot) in grid.iter_mut().enumerate() {
        let chunk: [u8; 4] = bytes[4 * j..4 * j + 4].try_into().unwrap();
        *slot = u32::from_le_bytes(chunk) ^ row_mask(j);
    }
    grid
}

/// Inverse of [`block_to_dot_grid`]: reconstruct the 128 wire bytes
/// from the rendered 32-row dot grid by undoing the per-row XOR
/// scramble. Bit-exact inverse — composing the two functions in
/// either order is the identity for any input.
#[must_use]
pub fn dot_grid_to_block(grid: &[GridRow; ROWS]) -> [u8; BLOCK_BYTES] {
    let mut out = [0u8; BLOCK_BYTES];
    for (j, &row) in grid.iter().enumerate() {
        let unmasked = row ^ row_mask(j);
        out[4 * j..4 * j + 4].copy_from_slice(&unmasked.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip identity for an arbitrary non-trivial block.
    #[test]
    fn round_trip_arbitrary_block() {
        let bytes: [u8; BLOCK_BYTES] = core::array::from_fn(|i| (i as u8).wrapping_mul(31));
        let grid = block_to_dot_grid(&bytes);
        let recovered = dot_grid_to_block(&grid);
        assert_eq!(recovered, bytes);
    }

    /// An all-zero block must render as the alternating 0x55555555 /
    /// 0xAAAAAAAA pattern — i.e. a regular checkerboard. This is the
    /// whole point of the XOR scramble: a degenerate block becomes a
    /// dense pattern that the optical decoder can still find.
    #[test]
    fn all_zero_block_renders_as_checkerboard() {
        let bytes = [0u8; BLOCK_BYTES];
        let grid = block_to_dot_grid(&bytes);
        for (j, &row) in grid.iter().enumerate() {
            let expected = if j % 2 == 0 { 0x5555_5555 } else { 0xAAAA_AAAA };
            assert_eq!(row, expected, "row {j}");
        }
    }

    /// All-FF block must render as the same alternating pattern as
    /// all-zero, just with the masks swapped — even rows become
    /// 0xAAAAAAAA, odd rows become 0x55555555. Same checkerboard,
    /// shifted by one column.
    #[test]
    fn all_ff_block_renders_as_inverted_checkerboard() {
        let bytes = [0xFFu8; BLOCK_BYTES];
        let grid = block_to_dot_grid(&bytes);
        for (j, &row) in grid.iter().enumerate() {
            let expected = if j % 2 == 0 { 0xAAAA_AAAA } else { 0x5555_5555 };
            assert_eq!(row, expected, "row {j}");
        }
    }

    /// Bit-position convention: byte[0] = 0x01 sets the underlying u32
    /// of row 0 to 0x00000001, which after XOR with 0x55555555 becomes
    /// 0x55555554. Pin this value so a future refactor that
    /// accidentally reverses bit order, swaps endianness, or applies
    /// the wrong mask trips this test loudly.
    #[test]
    fn lsb_convention_is_byte_0_bit_0_to_column_0() {
        let mut bytes = [0u8; BLOCK_BYTES];
        bytes[0] = 0x01;
        let grid = block_to_dot_grid(&bytes);
        // Underlying u32 = 0x00000001 (LE). XOR'd with 0x55555555 (even row 0).
        assert_eq!(grid[0], 0x5555_5554);
        // Bit 0 of grid[0] corresponds to column 0; here it's clear
        // (XOR turned the underlying 1 into 0 at that position).
        assert_eq!(
            grid[0] & 1,
            0,
            "column 0 of row 0 is white when byte[0] bit 0 is set"
        );
        // All other rows are unaffected (still pure mask).
        for (j, &row) in grid.iter().enumerate().skip(1) {
            let expected = if j % 2 == 0 { 0x5555_5555 } else { 0xAAAA_AAAA };
            assert_eq!(row, expected, "row {j}");
        }
    }

    /// Last byte of the block lives at offset 127; it's the high byte
    /// of row 31's u32. Pin its placement to defend against an
    /// off-by-one in the row-to-byte slicing.
    #[test]
    fn last_byte_lives_in_high_position_of_last_row() {
        let mut bytes = [0u8; BLOCK_BYTES];
        bytes[127] = 0x80; // high bit of byte 127
        let grid = block_to_dot_grid(&bytes);
        // bytes[124..128] = [0, 0, 0, 0x80]; LE u32 = 0x80000000.
        // Row 31 is odd: XOR with 0xAAAAAAAA -> 0x2AAAAAAA.
        assert_eq!(grid[31], 0x2AAA_AAAA);
        // Underlying high bit (bit 31, column 31) was set; XOR'd
        // with 0xAAAAAAAA's bit 31 (1) clears it -> column 31 of row 31
        // is white when byte[127] high bit is set.
        assert_eq!(grid[31] >> 31, 0, "column 31 of row 31 is white");
    }

    /// Composing block_to_dot_grid then dot_grid_to_block is the
    /// identity for ANY input. This is the strongest correctness
    /// statement we can pin without external reference vectors.
    #[test]
    fn round_trip_holds_for_diverse_patterns() {
        // Several patterns with distinct entropy and bit-density
        // characteristics. Each must round-trip byte-exact.
        let patterns: [[u8; BLOCK_BYTES]; 5] = [
            [0u8; BLOCK_BYTES],
            [0xFFu8; BLOCK_BYTES],
            core::array::from_fn(|i| i as u8),
            core::array::from_fn(|i| (i as u8).wrapping_mul(91).wrapping_add(7)),
            core::array::from_fn(|i| if i % 2 == 0 { 0x55 } else { 0xAA }),
        ];
        for (idx, bytes) in patterns.iter().enumerate() {
            let grid = block_to_dot_grid(bytes);
            let recovered = dot_grid_to_block(&grid);
            assert_eq!(&recovered, bytes, "pattern {idx} did not round-trip");
        }
    }
}
