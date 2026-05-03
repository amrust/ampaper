// v3 page geometry + bitmap rendering and parsing.
//
// Phase 2 first slice (the original): rendered cells in an
// nx × ny grid; the parser had to be told the geometry exactly.
//
// Phase 2.5 (this file): adds three QR-style corner finder patterns
// (top-left, top-right, bottom-left) to every page. The parser
// uses raster-scan finder detection to locate the data grid in
// the bitmap — it works on bitmaps where the page sits anywhere
// inside a larger white canvas, and tolerates modest scale drift
// (the per-dot pixel size is computed from finder distances, not
// taken from the geometry).
//
// Layout in dots (where each dot is `pixels_per_dot` pixels in the
// rendered bitmap):
//
//   ┌──────────────────────────────┐
//   │ ▣      ←  page_width_dots → ▣│   ▣ = 7×7 finder pattern
//   │   ┌──────────────────────┐   │
//   │   │   nx × ny data grid  │   │
//   │   │   (32 dots per cell) │   │
//   │   │                      │   │
//   │   └──────────────────────┘   │
//   │ ▣                            │
//   └──────────────────────────────┘
//                                    ↑ no finder at bottom-right —
//                                      asymmetry tells future
//                                      rotation handler which way
//                                      is up
//
// FINDER_MARGIN_DOTS (= 8 = 7 finder + 1 quiet zone) on each side
// separates the data grid from the page edge. Total page bitmap
// is therefore (nx · 32 + 16) × (ny · 32 + 16) dots, scaled up by
// `pixels_per_dot`.
//
// Phase 2.5 first slice limitations (lifted in subsequent slices):
//   - Axis-aligned bitmaps only. Rotation correction needs the
//     three finders' relative positions to compute an affine
//     transform; deferred.
//   - Fixed midpoint thresholding (`< 128 = black`). Real scanner
//     output needs Otsu / per-region adaptive thresholding —
//     deferred to the threshold/noise slice.
//   - Single-pixel sampling at each dot's geometric center. Real
//     scans need integration over the dot area to suppress edge
//     bleed — deferred.

use super::cell::{CELL_BYTES, CELL_DOTS};
use super::finder::{
    FINDER_MARGIN_DOTS, FINDER_SIZE_DOTS, FinderError, draw_finder, locate_finders,
};

/// White and black pixel values used in the rendered bitmap.
/// Matches `crate::page::WHITE` / `BLACK_PAPER` for visual parity
/// when v3 pages share a viewer with v1 pages.
pub const WHITE: u8 = 255;
pub const BLACK: u8 = 0;

/// Page-level geometry — how cells are laid out on a page bitmap
/// and how big each data dot is in device pixels. Bitmap
/// dimensions include the finder margin on every side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageGeometry {
    /// Cells per row in the data grid.
    pub nx: u32,
    /// Cells per column in the data grid.
    pub ny: u32,
    /// Device pixels per data dot. 1 = unscaled (smallest bitmap),
    /// 6 = PB-1.10-style 600-DPI printer × 100-dot/inch encoding.
    pub pixels_per_dot: u32,
}

impl PageGeometry {
    /// Total cells per page (data grid only — finders aren't cells).
    #[must_use]
    pub fn cells_per_page(&self) -> u32 {
        self.nx * self.ny
    }

    /// Width of the page in dots, including the finder margins on
    /// both sides.
    #[must_use]
    pub fn page_width_dots(&self) -> u32 {
        self.nx * CELL_DOTS + 2 * FINDER_MARGIN_DOTS
    }

    /// Height of the page in dots, including the finder margins on
    /// top and bottom.
    #[must_use]
    pub fn page_height_dots(&self) -> u32 {
        self.ny * CELL_DOTS + 2 * FINDER_MARGIN_DOTS
    }

    /// Bitmap pixel width.
    #[must_use]
    pub fn pixel_width(&self) -> u32 {
        self.page_width_dots() * self.pixels_per_dot
    }

    /// Bitmap pixel height.
    #[must_use]
    pub fn pixel_height(&self) -> u32 {
        self.page_height_dots() * self.pixels_per_dot
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
    /// Bitmap pixel buffer is shorter than `width × height` bytes.
    BitmapTruncated { expected: usize, got: usize },
    /// Bitmap is too small to fit even the finder patterns + a
    /// minimum-sized data grid for the supplied geometry.
    BitmapTooSmall { expected_min: (u32, u32), got: (u32, u32) },
    /// Finder pattern detection failed — no v3 page found in the
    /// supplied bitmap, or geometry mismatch put the expected
    /// top-right / bottom-left finder past the search radius.
    FinderDetection(FinderError),
    /// Cell sampling at the derived grid origin would read past
    /// the bitmap edge. Indicates the geometry says the data grid
    /// is larger than the bitmap actually contains, even though
    /// finders were found.
    CellSamplingOutOfBounds,
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BitmapTruncated { expected, got } => write!(
                f,
                "bitmap pixel buffer truncated: expected {expected} bytes, got {got}"
            ),
            Self::BitmapTooSmall { expected_min, got } => write!(
                f,
                "bitmap too small: need at least {}×{}, got {}×{}",
                expected_min.0, expected_min.1, got.0, got.1
            ),
            Self::FinderDetection(e) => write!(f, "{e}"),
            Self::CellSamplingOutOfBounds => f.write_str(
                "cell sampling out of bounds — geometry exceeds bitmap content",
            ),
        }
    }
}

impl std::error::Error for ParseError {}

impl From<FinderError> for ParseError {
    fn from(e: FinderError) -> Self {
        Self::FinderDetection(e)
    }
}

/// Render `cells` to a [`PageBitmap`] sized for `geometry`. Every
/// page gets three corner finder patterns drawn first, then the
/// data grid at the per-edge `FINDER_MARGIN_DOTS` offset. If the
/// caller supplies fewer than `cells_per_page` cells, the trailing
/// cells are rendered as blank — they fail CRC at parse time and
/// the rateless ECC fills in.
#[must_use]
pub fn render_page(cells: &[[u8; CELL_BYTES]], geometry: &PageGeometry) -> PageBitmap {
    let width = geometry.pixel_width();
    let height = geometry.pixel_height();
    let mut pixels = vec![WHITE; (width as usize) * (height as usize)];

    let scale = geometry.pixels_per_dot;
    let page_w_dots = geometry.page_width_dots();
    let page_h_dots = geometry.page_height_dots();

    // Three corner finders. Top-left at dot (0, 0); top-right at
    // dot (page_w - 7, 0); bottom-left at dot (0, page_h - 7).
    // No bottom-right finder — the asymmetry is the orientation
    // signal for future rotation handling.
    draw_finder(&mut pixels, width, 0, 0, scale);
    draw_finder(&mut pixels, width, page_w_dots - FINDER_SIZE_DOTS, 0, scale);
    draw_finder(&mut pixels, width, 0, page_h_dots - FINDER_SIZE_DOTS, scale);

    // Data grid sits at offset (FINDER_MARGIN_DOTS, FINDER_MARGIN_DOTS)
    // in dot space.
    let cells_per_page = geometry.cells_per_page() as usize;
    let scale_us = scale as usize;
    let cell_pitch = CELL_DOTS as usize * scale_us;
    let grid_origin_x = FINDER_MARGIN_DOTS as usize * scale_us;
    let grid_origin_y = FINDER_MARGIN_DOTS as usize * scale_us;

    for (idx, cell) in cells.iter().take(cells_per_page).enumerate() {
        let cell_col = idx % geometry.nx as usize;
        let cell_row = idx / geometry.nx as usize;
        let cell_x_origin = grid_origin_x + cell_col * cell_pitch;
        let cell_y_origin = grid_origin_y + cell_row * cell_pitch;

        for inner_row in 0..CELL_DOTS as usize {
            for inner_col in 0..CELL_DOTS as usize {
                let bit_idx = inner_row * CELL_DOTS as usize + inner_col;
                let byte = cell[bit_idx / 8];
                let bit_pos = 7 - (bit_idx % 8);
                if (byte >> bit_pos) & 1 == 1 {
                    let dot_x = cell_x_origin + inner_col * scale_us;
                    let dot_y = cell_y_origin + inner_row * scale_us;
                    for dy in 0..scale_us {
                        let row_start = (dot_y + dy) * width as usize;
                        for dx in 0..scale_us {
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
/// it. The parser:
///
///   1. Locates the three corner finders via raster scan.
///   2. Derives the data-grid origin in pixel coordinates from the
///      top-left finder's center and an offset of
///      `FINDER_MARGIN_DOTS - 3` dots (3 = finder center offset
///      from finder top-left corner).
///   3. Computes the per-dot pixel size from the horizontal
///      distance between top-left and top-right finders, divided
///      by the known dot-distance between their centers
///      (`page_width_dots - 7`). Same calculation vertically.
///      Geometry's stored `pixels_per_dot` is treated as a hint;
///      the actual sampling uses the measured value, so modest
///      print-scan scale drift is tolerated.
///   4. Samples each cell's 32×32 dots at the geometric center of
///      each dot's pixel block, using the measured per-dot pixel
///      size and a fixed midpoint (< 128) threshold.
pub fn parse_page(
    bitmap: &PageBitmap,
    geometry: &PageGeometry,
) -> Result<Vec<[u8; CELL_BYTES]>, ParseError> {
    let needed = (bitmap.width as usize) * (bitmap.height as usize);
    if bitmap.pixels.len() < needed {
        return Err(ParseError::BitmapTruncated {
            expected: needed,
            got: bitmap.pixels.len(),
        });
    }
    // Quick floor: bitmap must be at least as big as the page would
    // be at pixels_per_dot=1.
    let min_w = geometry.page_width_dots();
    let min_h = geometry.page_height_dots();
    if bitmap.width < min_w || bitmap.height < min_h {
        return Err(ParseError::BitmapTooSmall {
            expected_min: (min_w, min_h),
            got: (bitmap.width, bitmap.height),
        });
    }

    // 1+2. Locate the three corner finders.
    let [tl, tr, bl] = locate_finders(
        bitmap,
        geometry.page_width_dots(),
        geometry.page_height_dots(),
    )?;

    // 3. Measured per-dot pixel size, in each axis. Use both axes
    // for robustness; the dx/dy split also makes future non-square
    // pixel ratios cheaper to add.
    let center_dots_horiz = (geometry.page_width_dots() - FINDER_SIZE_DOTS) as f32;
    let center_dots_vert = (geometry.page_height_dots() - FINDER_SIZE_DOTS) as f32;
    let dx = (tr.center_x - tl.center_x) / center_dots_horiz;
    let dy = (bl.center_y - tl.center_y) / center_dots_vert;
    if dx <= 0.0 || dy <= 0.0 {
        return Err(ParseError::FinderDetection(FinderError::NoTopRight));
    }

    // The finder's central dot sits at page-dot (3, 3) — so its
    // GEOMETRIC center is at page-dot (3.5, 3.5) in continuous
    // coordinates. The data-grid origin (top-left corner of dot
    // FINDER_MARGIN_DOTS) is therefore offset by
    // (FINDER_MARGIN_DOTS - 3.5) dots from the detected finder
    // center. Off-by-half here costs a half-dot per cell at every
    // scale > 1 — caught by the page round-trip tests when we
    // first wrote this slice.
    let center_offset_dots = FINDER_MARGIN_DOTS as f32 - 3.5;
    let grid_origin_x = tl.center_x + center_offset_dots * dx;
    let grid_origin_y = tl.center_y + center_offset_dots * dy;

    // 4. Sample cells.
    let cells_per_page = geometry.cells_per_page() as usize;
    let mut cells = Vec::with_capacity(cells_per_page);
    let bitmap_w = bitmap.width as usize;
    for idx in 0..cells_per_page {
        let cell_col = (idx % geometry.nx as usize) as f32;
        let cell_row = (idx / geometry.nx as usize) as f32;
        let cell_x = grid_origin_x + cell_col * CELL_DOTS as f32 * dx;
        let cell_y = grid_origin_y + cell_row * CELL_DOTS as f32 * dy;

        let mut cell = [0u8; CELL_BYTES];
        for inner_row in 0..CELL_DOTS as usize {
            for inner_col in 0..CELL_DOTS as usize {
                // Sample at the geometric center of the dot. The
                // continuous coordinate `cell_x + (col + 0.5) * dx`
                // sits inside the pixel block of dot `col`; the
                // pixel containing that point is at `floor` of the
                // coordinate. `f32 as i64` truncates toward zero,
                // which equals floor for positive values — and our
                // grid origin is always inside the bitmap, so the
                // coordinate is always positive. Using `round`
                // here would tip sampling onto the WRONG dot at
                // scale=1 (8.5 → 9 picks dot 9 instead of dot 8).
                let sx = cell_x + (inner_col as f32 + 0.5) * dx;
                let sy = cell_y + (inner_row as f32 + 0.5) * dy;
                let pxi = sx as i64;
                let pyi = sy as i64;
                if pxi < 0
                    || pyi < 0
                    || pxi >= bitmap.width as i64
                    || pyi >= bitmap.height as i64
                {
                    return Err(ParseError::CellSamplingOutOfBounds);
                }
                let pixel = bitmap.pixels[(pyi as usize) * bitmap_w + pxi as usize];
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

/// Embed a rendered page bitmap into a larger all-white bitmap with
/// `(left_pad, top_pad)` pixels of margin on the left and top, plus
/// `(right_pad, bottom_pad)` on the right and bottom. The result
/// simulates a flatbed-scan style frame where the actual page sits
/// somewhere inside a larger scanned area. Used by integration tests
/// to validate that finder-based detection works on offset pages.
#[must_use]
pub fn pad_with_white(
    inner: &PageBitmap,
    left_pad: u32,
    top_pad: u32,
    right_pad: u32,
    bottom_pad: u32,
) -> PageBitmap {
    let width = inner.width + left_pad + right_pad;
    let height = inner.height + top_pad + bottom_pad;
    let mut pixels = vec![WHITE; (width as usize) * (height as usize)];
    for y in 0..inner.height {
        let dst_row = ((y + top_pad) * width + left_pad) as usize;
        let src_row = (y * inner.width) as usize;
        pixels[dst_row..dst_row + inner.width as usize]
            .copy_from_slice(&inner.pixels[src_row..src_row + inner.width as usize]);
    }
    PageBitmap { pixels, width, height }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_geometry() -> PageGeometry {
        PageGeometry { nx: 4, ny: 3, pixels_per_dot: 1 }
    }

    #[test]
    fn dimensions_include_finder_margin() {
        let g = PageGeometry { nx: 5, ny: 7, pixels_per_dot: 6 };
        // 5×32 + 2×8 = 176 dots wide, 7×32 + 2×8 = 240 dots tall.
        assert_eq!(g.page_width_dots(), 176);
        assert_eq!(g.page_height_dots(), 240);
        assert_eq!(g.pixel_width(), 176 * 6);
        assert_eq!(g.pixel_height(), 240 * 6);
        // cells_per_page is the data-grid count, NOT including any
        // notional "finder cells".
        assert_eq!(g.cells_per_page(), 35);
    }

    #[test]
    fn round_trips_random_cells_at_scale_1() {
        let geom = small_geometry();
        let mut cells = Vec::new();
        for i in 0..geom.cells_per_page() {
            let mut cell = [0u8; CELL_BYTES];
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
    fn page_in_larger_canvas_round_trips() {
        // The Phase 2.5 unlock: a page sitting somewhere inside a
        // larger all-white bitmap (simulating a flatbed scanner
        // capturing an entire sheet, with the data area in the
        // middle) still decodes via finder-based detection.
        let geom = PageGeometry { nx: 3, ny: 2, pixels_per_dot: 4 };
        let mut cells = Vec::new();
        for i in 0..geom.cells_per_page() {
            let mut cell = [0u8; CELL_BYTES];
            for (j, b) in cell.iter_mut().enumerate() {
                *b = ((i as usize).wrapping_mul(11).wrapping_add(j * 3) & 0xFF) as u8;
            }
            cells.push(cell);
        }
        let bitmap = render_page(&cells, &geom);
        // Pad with 50px left, 30px top, 70px right, 40px bottom.
        let padded = pad_with_white(&bitmap, 50, 30, 70, 40);
        let parsed = parse_page(&padded, &geom).unwrap();
        assert_eq!(parsed, cells);
    }

    #[test]
    fn empty_cells_render_finders_only() {
        // After Phase 2.5, "all empty cells" is no longer all-white
        // — the three corner finders are always drawn. Verify that
        // the pixels OUTSIDE the finder regions are still white.
        let geom = small_geometry();
        let cells: Vec<[u8; CELL_BYTES]> = vec![[0u8; CELL_BYTES]; geom.cells_per_page() as usize];
        let bitmap = render_page(&cells, &geom);
        // Quick sanity check: the center of the bitmap (deep inside
        // the data grid, far from any finder) must be white when
        // all data cells are zero.
        let cx = bitmap.width / 2;
        let cy = bitmap.height / 2;
        let center_pixel = bitmap.pixels[(cy * bitmap.width + cx) as usize];
        assert_eq!(center_pixel, WHITE);
        // And the bitmap is NOT all-white (finders are drawn).
        assert!(bitmap.pixels.contains(&BLACK));
    }

    #[test]
    fn parse_rejects_too_small_bitmap() {
        let geom = small_geometry();
        let bad = PageBitmap {
            pixels: vec![WHITE; 100],
            width: 10,
            height: 10,
        };
        let err = parse_page(&bad, &geom).unwrap_err();
        assert!(matches!(err, ParseError::BitmapTooSmall { .. }));
    }

    #[test]
    fn parse_rejects_blank_canvas_with_no_page() {
        // No finders → the parser refuses cleanly rather than
        // returning bogus cells.
        let geom = small_geometry();
        let blank = PageBitmap {
            pixels: vec![WHITE; (geom.pixel_width() * geom.pixel_height()) as usize],
            width: geom.pixel_width(),
            height: geom.pixel_height(),
        };
        let err = parse_page(&blank, &geom).unwrap_err();
        assert!(matches!(err, ParseError::FinderDetection(_)));
    }
}
