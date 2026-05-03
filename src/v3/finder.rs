// QR-style finder patterns for v3 page registration (Phase 2.5).
//
// Three 7×7-dot finder patterns sit in the top-left, top-right, and
// bottom-left corners of every v3 page. The bottom-right corner is
// deliberately empty — the asymmetry tells the decoder which way is
// up (added value lands when rotation handling arrives in a later
// slice; currently the parser assumes axis-aligned, but the
// asymmetric layout is fixed in the spec now so future rotation
// support doesn't require a wire-format bump).
//
// Finder pattern layout (each cell = one data dot):
//
//   B B B B B B B
//   B W W W W W B
//   B W B B B W B
//   B W B B B W B    ← QR-style 1:1:3:1:1 ratio when scanned
//   B W B B B W B      through the center, in any direction
//   B W W W W W B
//   B B B B B B B
//
// Detection: scan rows for runs that match 1:1:3:1:1 dark:light:
// dark:light:dark within ±50% per-segment tolerance. Confirm by
// scanning the column at the candidate center; both directions
// must match. The wide black center (3 units) gives a stable
// signature even with mild ink bleed or scanner blur — the same
// reason QR codes use the same shape.
//
// Phase 2.5 first slice (this file): axis-aligned detection only.
// The decoder finds the top-left finder via raster scan, then
// extrapolates to expected positions for top-right and bottom-left.
// Rotation, perspective, and noise tolerance are deferred to later
// slices in M12.

use super::page::{BLACK, PageBitmap};

/// One finder pattern is 7 dots on a side.
pub const FINDER_SIZE_DOTS: u32 = 7;

/// White quiet zone around each finder, in dots. 1 dot is enough
/// because we own both the encoder (which guarantees the data grid
/// edges aren't all-black) and the page-level layout (which puts
/// the finder at the actual corner of the bitmap).
pub const FINDER_QUIET_DOTS: u32 = 1;

/// Combined per-edge margin between the bitmap's outer edge and
/// the data grid: the finder itself plus its inner quiet zone.
pub const FINDER_MARGIN_DOTS: u32 = FINDER_SIZE_DOTS + FINDER_QUIET_DOTS;

/// The 7×7 finder pattern. `1` = black dot, `0` = white dot.
const FINDER_DOTS: [[u8; FINDER_SIZE_DOTS as usize]; FINDER_SIZE_DOTS as usize] = [
    [1, 1, 1, 1, 1, 1, 1],
    [1, 0, 0, 0, 0, 0, 1],
    [1, 0, 1, 1, 1, 0, 1],
    [1, 0, 1, 1, 1, 0, 1],
    [1, 0, 1, 1, 1, 0, 1],
    [1, 0, 0, 0, 0, 0, 1],
    [1, 1, 1, 1, 1, 1, 1],
];

const THRESHOLD: u8 = 128;

/// Run-length tolerance for the 1:1:3:1:1 detection, as a fraction
/// of the estimated unit size. ±50% is a deliberately wide tolerance
/// — it lets the detector survive mild ink bleed without false-
/// positive on noise. Tighter tolerances become viable once the
/// scan-side path adds adaptive thresholding.
const RUN_TOLERANCE: f32 = 0.5;

/// One detected finder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FinderHit {
    /// Pixel x of the finder's center dot.
    pub center_x: f32,
    /// Pixel y of the finder's center dot.
    pub center_y: f32,
    /// Estimated pixels per dot, derived from the run-length
    /// signature ((sum of 5 runs) / 7).
    pub unit: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinderError {
    /// No top-left finder found — either the bitmap is blank, the
    /// page is rotated past what this slice handles, or it isn't
    /// a v3 page at all.
    NoTopLeft,
    /// Top-right finder missing at the expected horizontal offset
    /// from the top-left finder.
    NoTopRight,
    /// Bottom-left finder missing at the expected vertical offset.
    NoBottomLeft,
    /// Bitmap pixel buffer can't fit the requested geometry.
    BitmapTooSmall,
}

impl core::fmt::Display for FinderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoTopLeft => f.write_str("no top-left finder pattern detected"),
            Self::NoTopRight => f.write_str(
                "top-right finder not found at expected position — \
                 page may be rotated or geometry mismatch",
            ),
            Self::NoBottomLeft => f.write_str(
                "bottom-left finder not found at expected position — \
                 page may be rotated or geometry mismatch",
            ),
            Self::BitmapTooSmall => f.write_str("bitmap too small for the requested geometry"),
        }
    }
}

impl std::error::Error for FinderError {}

/// Draw one finder pattern into `pixels` with its top-left dot at
/// page-dot coordinates `(x_dots, y_dots)`. Each dot is rendered as
/// a `scale × scale` pixel block, matching the page's
/// `pixels_per_dot`.
pub fn draw_finder(
    pixels: &mut [u8],
    bitmap_width: u32,
    x_dots: u32,
    y_dots: u32,
    scale: u32,
) {
    for (r, row) in FINDER_DOTS.iter().enumerate() {
        for (c, &dot) in row.iter().enumerate() {
            if dot == 0 {
                continue;
            }
            for dy in 0..scale {
                let py = (y_dots + r as u32) * scale + dy;
                let row_start = (py * bitmap_width) as usize;
                for dx in 0..scale {
                    let px = (x_dots + c as u32) * scale + dx;
                    pixels[row_start + px as usize] = BLACK;
                }
            }
        }
    }
}

/// One run of same-colored pixels along a scan line.
#[derive(Clone, Copy, Debug)]
struct Run {
    is_black: bool,
    start: u32,
    length: u32,
}

/// Collect the run-lengths along row `y` of the bitmap.
fn row_runs(bitmap: &PageBitmap, y: u32) -> Vec<Run> {
    let row_start = (y as usize) * (bitmap.width as usize);
    let mut runs = Vec::new();
    let mut prev_black = bitmap.pixels[row_start] < THRESHOLD;
    let mut run_start = 0u32;
    for x in 1..bitmap.width {
        let is_black = bitmap.pixels[row_start + x as usize] < THRESHOLD;
        if is_black != prev_black {
            runs.push(Run { is_black: prev_black, start: run_start, length: x - run_start });
            run_start = x;
            prev_black = is_black;
        }
    }
    runs.push(Run {
        is_black: prev_black,
        start: run_start,
        length: bitmap.width - run_start,
    });
    runs
}

/// Collect run-lengths along column `x`.
fn column_runs(bitmap: &PageBitmap, x: u32) -> Vec<Run> {
    let stride = bitmap.width as usize;
    let mut runs = Vec::new();
    let mut prev_black = bitmap.pixels[x as usize] < THRESHOLD;
    let mut run_start = 0u32;
    for y in 1..bitmap.height {
        let is_black = bitmap.pixels[(y as usize) * stride + x as usize] < THRESHOLD;
        if is_black != prev_black {
            runs.push(Run { is_black: prev_black, start: run_start, length: y - run_start });
            run_start = y;
            prev_black = is_black;
        }
    }
    runs.push(Run {
        is_black: prev_black,
        start: run_start,
        length: bitmap.height - run_start,
    });
    runs
}

/// Test whether five consecutive runs match the 1:1:3:1:1 dark:
/// light:dark:light:dark finder signature. Returns `Some((center,
/// unit))` on a match where `center` is the f32 pixel coordinate
/// of the geometric center of the wide-black run (i.e. the center
/// of the finder's central dot) and `unit` is the estimated
/// pixels-per-dot.
///
/// Returning the center as f32 (not u32) preserves the half-pixel
/// midpoint at scale > 1: a 3-dot run at scale=2 has length 6,
/// so the run-midpoint sits at +3.0 pixels from the start —
/// truncating to u32 would lose the 0.5-pixel precision the
/// downstream grid-origin computation relies on.
fn match_finder_runs(runs: &[Run]) -> Option<(f32, f32)> {
    if runs.len() != 5 {
        return None;
    }
    let [r0, r1, r2, r3, r4] = [runs[0], runs[1], runs[2], runs[3], runs[4]];
    if !(r0.is_black && !r1.is_black && r2.is_black && !r3.is_black && r4.is_black) {
        return None;
    }
    let total = r0.length + r1.length + r2.length + r3.length + r4.length;
    let unit = total as f32 / 7.0;
    if unit < 1.0 {
        return None;
    }
    let tol = unit * RUN_TOLERANCE;
    let ok = (r0.length as f32 - unit).abs() <= tol
        && (r1.length as f32 - unit).abs() <= tol
        && (r2.length as f32 - 3.0 * unit).abs() <= tol
        && (r3.length as f32 - unit).abs() <= tol
        && (r4.length as f32 - unit).abs() <= tol;
    if !ok {
        return None;
    }
    let center_pixel = r2.start as f32 + r2.length as f32 / 2.0;
    Some((center_pixel, unit))
}

/// Sweep `runs` for any 5-run window that matches the finder
/// signature. Returns the first match (lowest start coordinate).
fn first_finder_match(runs: &[Run]) -> Option<(f32, f32)> {
    runs.windows(5).find_map(match_finder_runs)
}

/// Locate the top-left finder by raster-scanning rows from the top
/// of the bitmap until one gives a 1:1:3:1:1 hit. Verify by also
/// scanning the column at the candidate center.
fn find_top_left(bitmap: &PageBitmap) -> Option<FinderHit> {
    for y in 0..bitmap.height {
        let runs = row_runs(bitmap, y);
        let Some((row_center_x, row_unit)) = first_finder_match(&runs) else {
            continue;
        };
        // Round to integer column for the column-scan verification.
        let col_x = row_center_x.round() as u32;
        if col_x >= bitmap.width {
            continue;
        }
        let col_runs = column_runs(bitmap, col_x);
        for window in col_runs.windows(5) {
            let Some((col_center_y, col_unit)) = match_finder_runs(window) else {
                continue;
            };
            // The two unit estimates should agree within ±25%.
            let unit_avg = (row_unit + col_unit) / 2.0;
            if (row_unit - col_unit).abs() > unit_avg * 0.25 {
                continue;
            }
            return Some(FinderHit {
                center_x: row_center_x,
                center_y: col_center_y,
                unit: unit_avg,
            });
        }
    }
    None
}

/// Look for a finder pattern whose center is near `(target_x,
/// target_y)`. Tolerates `±search_radius` in both axes. Used to
/// confirm the top-right and bottom-left finders at the positions
/// the geometry says they should be at, derived from the top-left
/// finder + expected page dimensions.
fn find_finder_near(
    bitmap: &PageBitmap,
    target_x: f32,
    target_y: f32,
    search_radius: f32,
) -> Option<FinderHit> {
    let y_min = (target_y - search_radius).max(0.0) as u32;
    let y_max = (target_y + search_radius).min(bitmap.height as f32 - 1.0) as u32;
    for y in y_min..=y_max {
        let runs = row_runs(bitmap, y);
        for window in runs.windows(5) {
            let Some((row_center_x, row_unit)) = match_finder_runs(window) else {
                continue;
            };
            if (row_center_x - target_x).abs() > search_radius {
                continue;
            }
            // Confirm with a column scan.
            let col_x = row_center_x.round() as u32;
            if col_x >= bitmap.width {
                continue;
            }
            let col_runs = column_runs(bitmap, col_x);
            for col_window in col_runs.windows(5) {
                let Some((col_center_y, col_unit)) = match_finder_runs(col_window) else {
                    continue;
                };
                if (col_center_y - target_y).abs() > search_radius {
                    continue;
                }
                let unit_avg = (row_unit + col_unit) / 2.0;
                return Some(FinderHit {
                    center_x: row_center_x,
                    center_y: col_center_y,
                    unit: unit_avg,
                });
            }
        }
    }
    None
}

/// Locate the three corner finders on a v3 page bitmap. Returns
/// `[top_left, top_right, bottom_left]`.
///
/// Strategy:
///   1. Raster-scan from the top of the bitmap to find the top-left
///      finder. The first 1:1:3:1:1 match in row+column-scan order
///      is taken to be the top-left finder.
///   2. From the top-left finder's position and the supplied
///      `page_width_dots` / `page_height_dots`, compute where the
///      top-right and bottom-left finders SHOULD be (in pixels,
///      using the top-left finder's `unit`).
///   3. Search around those expected positions with a generous
///      tolerance — accepts modest scale drift.
///
/// Phase 2.5 first slice — assumes axis-aligned bitmap. Rotation
/// support arrives in a later slice and will replace step 2's
/// "expected position" math with affine-transform inference from
/// all three finder centers.
pub fn locate_finders(
    bitmap: &PageBitmap,
    page_width_dots: u32,
    page_height_dots: u32,
) -> Result<[FinderHit; 3], FinderError> {
    if bitmap.pixels.len() < (bitmap.width as usize) * (bitmap.height as usize) {
        return Err(FinderError::BitmapTooSmall);
    }

    let tl = find_top_left(bitmap).ok_or(FinderError::NoTopLeft)?;

    // Top-right finder center is at dot (page_width_dots - 4, 3),
    // measured from the page's outer edge. Top-left finder center
    // is at dot (3, 3). So the horizontal dot-distance between the
    // two finder centers is page_width_dots - 7.
    let horiz_dots = page_width_dots as f32 - FINDER_SIZE_DOTS as f32;
    let target_tr_x = tl.center_x + horiz_dots * tl.unit;
    let target_tr_y = tl.center_y;
    let search_radius = tl.unit * 6.0; // ±6 dots — generous, covers scale drift up to ~10%
    let tr = find_finder_near(bitmap, target_tr_x, target_tr_y, search_radius)
        .ok_or(FinderError::NoTopRight)?;

    let vert_dots = page_height_dots as f32 - FINDER_SIZE_DOTS as f32;
    let target_bl_x = tl.center_x;
    let target_bl_y = tl.center_y + vert_dots * tl.unit;
    let bl = find_finder_near(bitmap, target_bl_x, target_bl_y, search_radius)
        .ok_or(FinderError::NoBottomLeft)?;

    Ok([tl, tr, bl])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v3::page::WHITE;

    /// Build a simple bitmap with one finder drawn at the given
    /// dot position, surrounded by white. Used for unit-testing the
    /// detector in isolation.
    fn bitmap_with_one_finder(
        width_dots: u32,
        height_dots: u32,
        finder_x_dots: u32,
        finder_y_dots: u32,
        scale: u32,
    ) -> PageBitmap {
        let width = width_dots * scale;
        let height = height_dots * scale;
        let mut pixels = vec![WHITE; (width * height) as usize];
        draw_finder(&mut pixels, width, finder_x_dots, finder_y_dots, scale);
        PageBitmap { pixels, width, height }
    }

    #[test]
    fn detects_lone_finder_at_scale_1() {
        let bm = bitmap_with_one_finder(20, 20, 5, 5, 1);
        let runs = row_runs(&bm, 5 + 3); // center row of the finder
        let (cx, unit) = first_finder_match(&runs).expect("should match");
        // Center dot is at column 5+3 = 8 → pixel center 8.5 at
        // scale=1 (dot 8 occupies pixels 8..9, run is 3 dots wide
        // = pixels 7..10, midpoint = 8.5).
        assert!((cx - 8.5).abs() < 0.6, "got cx={cx}");
        assert!((unit - 1.0).abs() < 0.5);
    }

    #[test]
    fn detects_lone_finder_at_scale_4() {
        let bm = bitmap_with_one_finder(20, 20, 3, 3, 4);
        // Finder occupies dots 3..10, scale 4 → pixels 12..40.
        // Center row: dot 3+3 = 6 → pixels 24..28. Use row 26.
        let runs = row_runs(&bm, 26);
        let (cx, unit) = first_finder_match(&runs).expect("should match at scale 4");
        // Center dot 6 → pixel range 24..28, geometric center 26.
        assert!((cx - 26.0).abs() < 1.0, "center {cx} not near 26");
        assert!((unit - 4.0).abs() < 1.0);
    }

    #[test]
    fn locates_three_finders_in_a_synthetic_page() {
        // Page is 50×40 dots at scale=2. Finders at TL, TR, BL.
        let pw = 50u32;
        let ph = 40u32;
        let scale = 2u32;
        let width = pw * scale;
        let height = ph * scale;
        let mut pixels = vec![WHITE; (width * height) as usize];
        draw_finder(&mut pixels, width, 0, 0, scale);
        draw_finder(&mut pixels, width, pw - FINDER_SIZE_DOTS, 0, scale);
        draw_finder(&mut pixels, width, 0, ph - FINDER_SIZE_DOTS, scale);
        let bm = PageBitmap { pixels, width, height };

        let [tl, tr, bl] = locate_finders(&bm, pw, ph).unwrap();
        // TL center is at the geometric center of the finder's
        // central dot (page-dot 3.5 → pixel 7.0 at scale=2).
        assert!((tl.center_x - 7.0).abs() < 1.0);
        assert!((tl.center_y - 7.0).abs() < 1.0);
        // TR center: page-dot (pw - 3.5, 3.5) → pixel (93, 7).
        let tr_target = (pw as f32 - 3.5) * scale as f32;
        assert!((tr.center_x - tr_target).abs() < 1.0);
        // BL center: page-dot (3.5, ph - 3.5) → pixel (7, 73).
        let bl_target = (ph as f32 - 3.5) * scale as f32;
        assert!((bl.center_y - bl_target).abs() < 1.0);
    }

    #[test]
    fn fails_with_no_top_left_on_blank_bitmap() {
        let bm = PageBitmap {
            pixels: vec![WHITE; 100 * 100],
            width: 100,
            height: 100,
        };
        let err = locate_finders(&bm, 50, 50).unwrap_err();
        assert!(matches!(err, FinderError::NoTopLeft));
    }
}
