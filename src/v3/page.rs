// v3 page geometry + bitmap rendering and parsing (Phase 2 first
// slice). Turns a stream of 128-byte cells into an 8-bit grayscale
// bitmap and back.
//
// Layout: cells are arranged in a row-major nx × ny grid. Cell index
// `i` lives at column `i % nx`, row `i / nx`. Each cell occupies a
// 32×32 dot block; each dot is rendered as a `pixels_per_dot` ×
// `pixels_per_dot` block of 8-bit grayscale pixels (black for set
// bits, white for unset). The page bitmap is therefore
// (nx · 32 · pixels_per_dot) × (ny · 32 · pixels_per_dot) pixels.
//
// `pixels_per_dot` is the printer-pixel scale: 1 means each data
// dot is 1 device pixel (smallest possible bitmap, useful for
// synthetic round-trip tests), 6 matches PB-1.10's calibration of
// 600-DPI printer × 100-dot/inch data density. Phase 2 first slice
// punts on per-page finder patterns and assumes the parser knows
// the geometry exactly — calibration / page-corner registration
// lands in Phase 2.5 once we tackle real scanner output.
//
// Bit packing inside a cell: byte 0 of the cell holds dots
// (0,0)..(0,7) — top row, leftmost 8 dots, MSB-first (so bit 7 of
// byte 0 is dot (0,0), bit 6 is dot (0,1), etc.). This matches the
// natural way to read a cell as a 32×32 bit raster.

use super::cell::{CELL_BYTES, CELL_DOTS};

/// White and black pixel values used in the rendered bitmap.
/// Matches `crate::page::WHITE` / `BLACK_PAPER` for visual parity
/// when v3 pages share a viewer with v1 pages.
pub const WHITE: u8 = 255;
pub const BLACK: u8 = 0;

/// Page-level geometry — how cells are laid out on a page bitmap
/// and how big each data dot is in device pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageGeometry {
    /// Cells per row.
    pub nx: u32,
    /// Cells per column.
    pub ny: u32,
    /// Device pixels per data dot. 1 = unscaled (smallest bitmap),
    /// 6 = PB-1.10-style 600-DPI printer × 100-dot/inch encoding.
    pub pixels_per_dot: u32,
}

impl PageGeometry {
    /// Total cells per page.
    #[must_use]
    pub fn cells_per_page(&self) -> u32 {
        self.nx * self.ny
    }

    /// Bitmap pixel width.
    #[must_use]
    pub fn pixel_width(&self) -> u32 {
        self.nx * CELL_DOTS * self.pixels_per_dot
    }

    /// Bitmap pixel height.
    #[must_use]
    pub fn pixel_height(&self) -> u32 {
        self.ny * CELL_DOTS * self.pixels_per_dot
    }
}

/// One rendered page bitmap. 8-bit grayscale, row-major, length =
/// width × height. Pixel values are either [`WHITE`] or [`BLACK`]
/// — the render path doesn't produce in-betweens, the parse path
/// thresholds at the midpoint.
#[derive(Clone, Debug)]
pub struct PageBitmap {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Bitmap dimensions don't match what the geometry says they
    /// should be. Phase 2 first slice has no auto-detect; the
    /// caller must hand the parser a bitmap whose dimensions
    /// already match the geometry's `pixel_width` × `pixel_height`.
    BitmapSizeMismatch {
        expected: (u32, u32),
        got: (u32, u32),
    },
    /// Bitmap pixel buffer is shorter than `width × height` bytes.
    BitmapTruncated { expected: usize, got: usize },
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BitmapSizeMismatch { expected, got } => write!(
                f,
                "bitmap size mismatch: expected {}×{}, got {}×{}",
                expected.0, expected.1, got.0, got.1
            ),
            Self::BitmapTruncated { expected, got } => write!(
                f,
                "bitmap pixel buffer truncated: expected {expected} bytes, got {got}"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// Render `cells` to a [`PageBitmap`] sized for `geometry`. If the
/// caller supplies fewer than `cells_per_page` cells, the trailing
/// cells are rendered as blank (all-zero, all-white) — they'll
/// fail CRC at parse time and the rateless ECC fills in. Extra
/// cells beyond `cells_per_page` are ignored.
#[must_use]
pub fn render_page(cells: &[[u8; CELL_BYTES]], geometry: &PageGeometry) -> PageBitmap {
    let width = geometry.pixel_width();
    let height = geometry.pixel_height();
    let mut pixels = vec![WHITE; (width as usize) * (height as usize)];

    let cells_per_page = geometry.cells_per_page() as usize;
    let scale = geometry.pixels_per_dot as usize;
    let cell_pitch = CELL_DOTS as usize * scale;

    for (idx, cell) in cells.iter().take(cells_per_page).enumerate() {
        let cell_col = idx % geometry.nx as usize;
        let cell_row = idx / geometry.nx as usize;
        let cell_x_origin = cell_col * cell_pitch;
        let cell_y_origin = cell_row * cell_pitch;

        for inner_row in 0..CELL_DOTS as usize {
            for inner_col in 0..CELL_DOTS as usize {
                let bit_idx = inner_row * CELL_DOTS as usize + inner_col;
                let byte = cell[bit_idx / 8];
                let bit_pos = 7 - (bit_idx % 8);
                if (byte >> bit_pos) & 1 == 1 {
                    let dot_x = cell_x_origin + inner_col * scale;
                    let dot_y = cell_y_origin + inner_row * scale;
                    for dy in 0..scale {
                        let row_start = (dot_y + dy) * width as usize;
                        for dx in 0..scale {
                            pixels[row_start + dot_x + dx] = BLACK;
                        }
                    }
                }
            }
        }
    }

    PageBitmap { pixels, width, height }
}

/// Parse a [`PageBitmap`] back into the cell stream that produced
/// it. Phase 2 first slice — no calibration, no rotation, no
/// noise tolerance: assumes pixel-perfect alignment with
/// `geometry`. The Phase 2.5 scan-side parser will lift these
/// restrictions.
pub fn parse_page(
    bitmap: &PageBitmap,
    geometry: &PageGeometry,
) -> Result<Vec<[u8; CELL_BYTES]>, ParseError> {
    let expected_w = geometry.pixel_width();
    let expected_h = geometry.pixel_height();
    if bitmap.width != expected_w || bitmap.height != expected_h {
        return Err(ParseError::BitmapSizeMismatch {
            expected: (expected_w, expected_h),
            got: (bitmap.width, bitmap.height),
        });
    }
    let needed = (bitmap.width as usize) * (bitmap.height as usize);
    if bitmap.pixels.len() < needed {
        return Err(ParseError::BitmapTruncated {
            expected: needed,
            got: bitmap.pixels.len(),
        });
    }

    let scale = geometry.pixels_per_dot as usize;
    let cell_pitch = CELL_DOTS as usize * scale;
    let cells_per_page = geometry.cells_per_page() as usize;
    let mut cells = Vec::with_capacity(cells_per_page);

    for idx in 0..cells_per_page {
        let cell_col = idx % geometry.nx as usize;
        let cell_row = idx / geometry.nx as usize;
        let cell_x_origin = cell_col * cell_pitch;
        let cell_y_origin = cell_row * cell_pitch;

        let mut cell = [0u8; CELL_BYTES];
        for inner_row in 0..CELL_DOTS as usize {
            for inner_col in 0..CELL_DOTS as usize {
                // Sample the center pixel of the dot's pixel block.
                // For scale=1 this is the only pixel; for scale>1
                // taking the center is robust to any anti-aliasing
                // a future renderer might introduce on the edges.
                let dot_x = cell_x_origin + inner_col * scale + scale / 2;
                let dot_y = cell_y_origin + inner_row * scale + scale / 2;
                let pixel = bitmap.pixels[dot_y * bitmap.width as usize + dot_x];
                if pixel < 128 {
                    let bit_idx = inner_row * CELL_DOTS as usize + inner_col;
                    cell[bit_idx / 8] |= 1 << (7 - (bit_idx % 8));
                }
            }
        }
        cells.push(cell);
    }

    Ok(cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_geometry() -> PageGeometry {
        PageGeometry { nx: 4, ny: 3, pixels_per_dot: 1 }
    }

    #[test]
    fn dimensions_match_geometry() {
        let g = PageGeometry { nx: 5, ny: 7, pixels_per_dot: 6 };
        assert_eq!(g.cells_per_page(), 35);
        assert_eq!(g.pixel_width(), 5 * 32 * 6);
        assert_eq!(g.pixel_height(), 7 * 32 * 6);
    }

    #[test]
    fn round_trips_random_cells_at_scale_1() {
        let geom = small_geometry();
        let mut cells = Vec::new();
        for i in 0..geom.cells_per_page() {
            let mut cell = [0u8; CELL_BYTES];
            // Deterministic but cell-dependent contents.
            for (j, b) in cell.iter_mut().enumerate() {
                *b = ((i as usize).wrapping_mul(31).wrapping_add(j) & 0xFF) as u8;
            }
            cells.push(cell);
        }
        let bitmap = render_page(&cells, &geom);
        let parsed = parse_page(&bitmap, &geom).unwrap();
        assert_eq!(parsed, cells);
    }

    #[test]
    fn round_trips_random_cells_at_scale_6() {
        // PB-1.10-style scaling — 6 device pixels per data dot.
        let geom = PageGeometry { nx: 3, ny: 2, pixels_per_dot: 6 };
        let mut cells = Vec::new();
        for i in 0..geom.cells_per_page() {
            let mut cell = [0u8; CELL_BYTES];
            for (j, b) in cell.iter_mut().enumerate() {
                *b = ((i as usize).wrapping_mul(17).wrapping_add(j * 5) & 0xFF) as u8;
            }
            cells.push(cell);
        }
        let bitmap = render_page(&cells, &geom);
        let parsed = parse_page(&bitmap, &geom).unwrap();
        assert_eq!(parsed, cells);
    }

    #[test]
    fn parse_rejects_mismatched_dimensions() {
        let geom = small_geometry();
        let bad = PageBitmap {
            pixels: vec![WHITE; 100],
            width: 10,
            height: 10,
        };
        let err = parse_page(&bad, &geom).unwrap_err();
        assert!(matches!(err, ParseError::BitmapSizeMismatch { .. }));
    }

    #[test]
    fn empty_cells_render_to_all_white() {
        let geom = small_geometry();
        let cells: Vec<[u8; CELL_BYTES]> = vec![[0u8; CELL_BYTES]; geom.cells_per_page() as usize];
        let bitmap = render_page(&cells, &geom);
        assert!(bitmap.pixels.iter().all(|&p| p == WHITE));
    }
}
