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

/// Run-length tolerance for the 1:1:3:1:1 detection, as a fraction
/// of the estimated unit size. ±50% is a deliberately wide tolerance
/// — it lets the detector survive mild ink bleed without false-
/// positive on noise.
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FinderError {
    /// Fewer than 3 finder patterns detected. Either the bitmap
    /// doesn't contain a v3 page, the rotation/distortion exceeds
    /// what this slice handles (~±20°), or the page is partially
    /// out of frame.
    InsufficientFinders { found: usize },
    /// More than 3 finder patterns detected after clustering.
    /// Likely false positives in noisy bitmaps; future slices
    /// will pick the 3 most-confident clusters by hit count.
    AmbiguousFinders { found: usize },
    /// Detected finder triple's L-shape proportions don't match
    /// the supplied page geometry — most likely a chance triplet
    /// of false-positive 1:1:3:1:1 matches in non-page content,
    /// or a v3 page from a different `nx × ny` configuration.
    ProportionMismatch { actual: f32, expected: f32 },
    /// Bitmap pixel buffer can't fit the requested geometry.
    BitmapTooSmall,
}

impl core::fmt::Display for FinderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InsufficientFinders { found } => write!(
                f,
                "only {found} finder pattern(s) detected (need 3) — \
                 bitmap may not contain a v3 page or rotation exceeds tolerance"
            ),
            Self::AmbiguousFinders { found } => write!(
                f,
                "{found} finder patterns detected (expected 3) — \
                 likely false positives in noisy input"
            ),
            Self::ProportionMismatch { actual, expected } => write!(
                f,
                "finder L-shape proportions {actual:.3} don't match expected {expected:.3} — \
                 wrong page geometry, or non-page content"
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

/// Collect the run-lengths along row `y` of the bitmap. `threshold`
/// classifies each pixel: `< threshold = black`, `>= threshold =
/// white`. Phase 2.5c plumbs an Otsu-derived threshold through here
/// so finder detection works on faded / shifted-histogram input.
fn row_runs(bitmap: &PageBitmap, y: u32, threshold: u8) -> Vec<Run> {
    let row_start = (y as usize) * (bitmap.width as usize);
    let mut runs = Vec::new();
    let mut prev_black = bitmap.pixels[row_start] < threshold;
    let mut run_start = 0u32;
    for x in 1..bitmap.width {
        let is_black = bitmap.pixels[row_start + x as usize] < threshold;
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
fn column_runs(bitmap: &PageBitmap, x: u32, threshold: u8) -> Vec<Run> {
    let stride = bitmap.width as usize;
    let mut runs = Vec::new();
    let mut prev_black = bitmap.pixels[x as usize] < threshold;
    let mut run_start = 0u32;
    for y in 1..bitmap.height {
        let is_black = bitmap.pixels[(y as usize) * stride + x as usize] < threshold;
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
/// Only used by the lower-level run-detection unit tests; the
/// production all-finders detector inlines `runs.windows(5)`
/// because it wants every match, not just the first.
#[cfg(test)]
fn first_finder_match(runs: &[Run]) -> Option<(f32, f32)> {
    runs.windows(5).find_map(match_finder_runs)
}

/// Find every finder-pattern hit in the bitmap, then cluster
/// nearby hits down to one representative per finder. Each row of
/// the bitmap may contribute multiple raw matches against a single
/// finder (3 of the 7 rows of a finder pattern produce 1:1:3:1:1
/// signatures), so without clustering one finder shows up as 3-9
/// raw hits.
///
/// Returns clusters paired with their raw-hit count, descending —
/// the count is a confidence signal for distinguishing real
/// finders (many supporting hits) from chance 1:1:3:1:1 collisions
/// in random data cell content (typically 1-2 supporting hits).
pub fn find_all_finders(bitmap: &PageBitmap, threshold: u8) -> Vec<(FinderHit, u32)> {
    let mut raw_hits = Vec::new();
    for y in 0..bitmap.height {
        let runs = row_runs(bitmap, y, threshold);
        for window in runs.windows(5) {
            let Some((row_center_x, row_unit)) = match_finder_runs(window) else {
                continue;
            };
            let col_x = row_center_x.round() as u32;
            if col_x >= bitmap.width {
                continue;
            }
            let col_runs = column_runs(bitmap, col_x, threshold);
            for col_window in col_runs.windows(5) {
                let Some((col_center_y, col_unit)) = match_finder_runs(col_window) else {
                    continue;
                };
                // Both unit estimates should agree within ±25% —
                // a chance horizontal-only 1:1:3:1:1 in noisy data
                // is unlikely to also match in the column.
                let unit_avg = (row_unit + col_unit) / 2.0;
                if (row_unit - col_unit).abs() > unit_avg * 0.25 {
                    continue;
                }
                raw_hits.push(FinderHit {
                    center_x: row_center_x,
                    center_y: col_center_y,
                    unit: unit_avg,
                });
            }
        }
    }
    cluster_hits(raw_hits)
}

/// Sample a 7×7 dot area around `hit.center` (using `hit.unit` as
/// the per-dot pixel scale) and count how many of the 49 sampled
/// dots match the [`FINDER_DOTS`] template. Real finders score 47-49
/// out of 49; chance 1:1:3:1:1 collisions in random data score
/// much lower (typically 25-35). This is the load-bearing filter
/// at low `pixels_per_dot` where the run-length detector alone
/// produces enough false positives to fool the cluster-size
/// ranking.
fn template_match(bitmap: &PageBitmap, hit: FinderHit, threshold: u8) -> u32 {
    let mut matches = 0u32;
    for (r, row) in FINDER_DOTS.iter().enumerate() {
        for (c, &expected) in row.iter().enumerate() {
            let dx_dots = c as f32 - 3.0;
            let dy_dots = r as f32 - 3.0;
            let px = hit.center_x + dx_dots * hit.unit;
            let py = hit.center_y + dy_dots * hit.unit;
            let pxi = px as i64;
            let pyi = py as i64;
            if pxi < 0
                || pyi < 0
                || pxi >= bitmap.width as i64
                || pyi >= bitmap.height as i64
            {
                continue;
            }
            let pixel = bitmap.pixels[(pyi as usize) * (bitmap.width as usize) + pxi as usize];
            let actual_black = pixel < threshold;
            let expected_black = expected == 1;
            if actual_black == expected_black {
                matches += 1;
            }
        }
    }
    matches
}

/// Greedy clustering: hits within `3 * unit` pixels of an existing
/// cluster's running average get merged. Returns `(averaged hit,
/// raw-hit count)` per cluster, sorted by count descending so the
/// caller can pick the top-N by confidence.
fn cluster_hits(hits: Vec<FinderHit>) -> Vec<(FinderHit, u32)> {
    // Each cluster: (sum_x, sum_y, sum_unit, count).
    let mut clusters: Vec<(f32, f32, f32, u32)> = Vec::new();
    for hit in hits {
        let merge_dist = hit.unit * 3.0;
        let mut merged = false;
        for cluster in &mut clusters {
            let n = cluster.3 as f32;
            let avg_x = cluster.0 / n;
            let avg_y = cluster.1 / n;
            if (hit.center_x - avg_x).hypot(hit.center_y - avg_y) < merge_dist {
                cluster.0 += hit.center_x;
                cluster.1 += hit.center_y;
                cluster.2 += hit.unit;
                cluster.3 += 1;
                merged = true;
                break;
            }
        }
        if !merged {
            clusters.push((hit.center_x, hit.center_y, hit.unit, 1));
        }
    }
    let mut out: Vec<(FinderHit, u32)> = clusters
        .into_iter()
        .map(|(sx, sy, su, n)| {
            let nf = n as f32;
            (
                FinderHit {
                    center_x: sx / nf,
                    center_y: sy / nf,
                    unit: su / nf,
                },
                n,
            )
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

/// Identify which of three detected finders is the top-left,
/// top-right, and bottom-left corner. Two-step process:
///
///   1. Find the corner of the right-angle "L" — that's TL.
///      Among the three pairwise distances, the longest one is
///      the L's hypotenuse (TR-to-BL diagonal); the vertex NOT
///      on that hypotenuse is TL. Works at any rotation.
///   2. Disambiguate TR from BL via cross product. With image-
///      coordinate Y-down, the order TL → TR → BL traces a
///      clockwise turn, so `cross(TL→TR, TL→BL) > 0`. Whichever
///      assignment satisfies that wins.
fn identify_corners(
    a: FinderHit,
    b: FinderHit,
    c: FinderHit,
) -> (FinderHit, FinderHit, FinderHit) {
    let d_ab = (a.center_x - b.center_x).hypot(a.center_y - b.center_y);
    let d_ac = (a.center_x - c.center_x).hypot(a.center_y - c.center_y);
    let d_bc = (b.center_x - c.center_x).hypot(b.center_y - c.center_y);

    // The vertex opposite the longest side is TL.
    let (tl, p1, p2) = if d_bc >= d_ab && d_bc >= d_ac {
        (a, b, c)
    } else if d_ac >= d_ab && d_ac >= d_bc {
        (b, a, c)
    } else {
        (c, a, b)
    };

    // Cross product of (TL → p1) × (TL → p2). With image Y-down
    // axis, TL → TR → BL is a clockwise turn, which gives a
    // POSITIVE cross product. Whichever assignment matches wins.
    let v1x = p1.center_x - tl.center_x;
    let v1y = p1.center_y - tl.center_y;
    let v2x = p2.center_x - tl.center_x;
    let v2y = p2.center_y - tl.center_y;
    let cross = v1x * v2y - v1y * v2x;
    let (tr, bl) = if cross > 0.0 { (p1, p2) } else { (p2, p1) };

    (tl, tr, bl)
}

/// Locate the three corner finders on a v3 page bitmap. Returns
/// `[top_left, top_right, bottom_left]` in that order, regardless
/// of how the page is rotated within the bitmap.
///
/// Strategy:
///   1. Detect every finder-pattern hit in the bitmap, cluster
///      nearby raw hits down to one representative per finder
///      ([`find_all_finders`]).
///   2. Require exactly 3 distinct clusters. Fewer = page partly
///      out of frame or rotation past tolerance; more = noisy
///      input with false positives that will need cluster-size
///      ranking in a later slice.
///   3. Identify which finder is TL/TR/BL by L-shape geometry +
///      cross product ([`identify_corners`]).
///   4. Verify the L-shape proportions match what the supplied
///      `page_width_dots` × `page_height_dots` predicts. Catches
///      "found 3 finders, but they belong to a v3 page with a
///      different `nx × ny` than the caller supplied."
///
/// `page_width_dots` and `page_height_dots` are no longer used
/// for "expected position" search (the all-finders detector
/// doesn't need them), only for the proportion sanity check at
/// the end.
/// Geometry derived from a v3 page bitmap's finder positions.
/// All three of `nx`, `ny`, `pixels_per_dot` are inferred from
/// the run-length-detected finder centers — the encoder doesn't
/// need to share a hardcoded geometry with the decoder, the
/// information is self-describing in the bitmap itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetectedGeometry {
    pub nx: u32,
    pub ny: u32,
    pub pixels_per_dot: u32,
    pub finders: [FinderHit; 3],
}

/// Find all corner finders on a page, identify which is TL/TR/BL,
/// then derive `nx`, `ny`, and `pixels_per_dot` from their
/// pixel positions.
///
/// Math: TL center sits at page-dot `(3.5, 3.5)`, TR center at
/// `(page_width_dots - 3.5, 3.5)`, BL center at `(3.5,
/// page_height_dots - 3.5)`. The dot-distance between TL and TR
/// centers is therefore `page_width_dots - 7`, which equals
/// `nx * 32 + 9`. So:
///
/// ```text
///   horizontal_dot_distance = (TR.x - TL.x) / unit
///   nx = (horizontal_dot_distance - 9) / 32
/// ```
///
/// (where `unit` is the pixel-per-dot estimate from the finder
/// run-length detection, averaged across the three finders.)
///
/// `pixels_per_dot` is rounded to the nearest integer ≥ 1
/// because the renderer only emits integer scales — fractional
/// values would mean the encoder isn't ours.
pub fn detect_geometry(bitmap: &PageBitmap) -> Result<DetectedGeometry, FinderError> {
    if bitmap.pixels.len() < (bitmap.width as usize) * (bitmap.height as usize) {
        return Err(FinderError::BitmapTooSmall);
    }

    let threshold = crate::v3::threshold::otsu_threshold(&bitmap.pixels);
    let clusters = find_all_finders(bitmap, threshold);
    if clusters.len() < 3 {
        return Err(FinderError::InsufficientFinders { found: clusters.len() });
    }

    let mut scored: Vec<(FinderHit, u32)> = clusters
        .iter()
        .map(|(hit, _)| (*hit, template_match(bitmap, *hit, threshold)))
        .filter(|(_, score)| *score >= 40)
        .collect();
    if scored.len() < 3 {
        return Err(FinderError::InsufficientFinders { found: scored.len() });
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    let (tl, tr, bl) = identify_corners(scored[0].0, scored[1].0, scored[2].0);

    let unit_avg = (tl.unit + tr.unit + bl.unit) / 3.0;
    if unit_avg < 1.0 {
        return Err(FinderError::ProportionMismatch { actual: unit_avg, expected: 1.0 });
    }

    // Pixel distances between finder centers.
    let h_pixel =
        ((tr.center_x - tl.center_x).powi(2) + (tr.center_y - tl.center_y).powi(2)).sqrt();
    let v_pixel =
        ((bl.center_x - tl.center_x).powi(2) + (bl.center_y - tl.center_y).powi(2)).sqrt();

    // Convert to dot distances using the measured unit. These
    // should be integers (page_width_dots - 7) for an axis-
    // aligned page; for rotated/skewed inputs the fractional
    // residual goes into the affine transform later.
    let h_dot = h_pixel / unit_avg;
    let v_dot = v_pixel / unit_avg;
    let pw_dots = (h_dot + 7.0).round();
    let ph_dots = (v_dot + 7.0).round();
    if pw_dots < 16.0 || ph_dots < 16.0 {
        return Err(FinderError::ProportionMismatch {
            actual: pw_dots / ph_dots,
            expected: 1.0,
        });
    }

    // Recover nx, ny from page_dots = nx * 32 + 16. Round and
    // sanity-check that the residual is small — large residual
    // means the bitmap doesn't match a v3 page layout.
    let nx_f = (pw_dots - 16.0) / 32.0;
    let ny_f = (ph_dots - 16.0) / 32.0;
    let nx = nx_f.round() as i64;
    let ny = ny_f.round() as i64;
    if nx < 1 || ny < 1 {
        return Err(FinderError::ProportionMismatch {
            actual: nx_f.min(ny_f),
            expected: 1.0,
        });
    }
    if (nx_f - nx as f32).abs() > 0.3 || (ny_f - ny as f32).abs() > 0.3 {
        return Err(FinderError::ProportionMismatch {
            actual: (nx_f - nx as f32).abs().max((ny_f - ny as f32).abs()),
            expected: 0.0,
        });
    }

    // pixels_per_dot — round to nearest integer ≥ 1. The renderer
    // only emits integer scales; fractional values come from
    // print-then-scan resampling and the affine transform handles
    // them at sample time.
    let pixels_per_dot = unit_avg.round().max(1.0) as u32;

    Ok(DetectedGeometry {
        nx: nx as u32,
        ny: ny as u32,
        pixels_per_dot,
        finders: [tl, tr, bl],
    })
}

pub fn locate_finders(
    bitmap: &PageBitmap,
    page_width_dots: u32,
    page_height_dots: u32,
) -> Result<[FinderHit; 3], FinderError> {
    if bitmap.pixels.len() < (bitmap.width as usize) * (bitmap.height as usize) {
        return Err(FinderError::BitmapTooSmall);
    }

    // Phase 2.5c: Otsu threshold replaces the fixed-128 check
    // from earlier slices. Computed once per bitmap, threaded
    // through all detection helpers.
    let threshold = crate::v3::threshold::otsu_threshold(&bitmap.pixels);

    let clusters = find_all_finders(bitmap, threshold);
    if clusters.len() < 3 {
        return Err(FinderError::InsufficientFinders { found: clusters.len() });
    }

    // Filter + rank by template match. The run-length detector
    // produces enough chance 1:1:3:1:1 collisions in random data
    // cells (especially at pixels_per_dot=1, where each finder
    // only contributes ~3 supporting cluster hits) that
    // cluster-count alone can't reliably pick the real finders.
    // The 7×7 dot template match at each candidate's center is a
    // much stronger filter — real finders score 47-49 out of 49,
    // chance hits typically score below 40.
    let mut scored: Vec<(FinderHit, u32)> = clusters
        .iter()
        .map(|(hit, _)| (*hit, template_match(bitmap, *hit, threshold)))
        .filter(|(_, score)| *score >= 40) // 40/49 ≈ 82%
        .collect();
    if scored.len() < 3 {
        return Err(FinderError::InsufficientFinders { found: scored.len() });
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    let top_three = [scored[0].0, scored[1].0, scored[2].0];

    let (tl, tr, bl) = identify_corners(top_three[0], top_three[1], top_three[2]);

    // Sanity-check the L-shape proportions. The dot-distance from
    // TL center to TR center is `page_width_dots - 7`; from TL to
    // BL is `page_height_dots - 7`. After rotation those distances
    // are preserved, so the ratio (horiz_pixels / vert_pixels)
    // should match `(W - 7) / (H - 7)` regardless of rotation.
    let horiz_pixels = (tr.center_x - tl.center_x).hypot(tr.center_y - tl.center_y);
    let vert_pixels = (bl.center_x - tl.center_x).hypot(bl.center_y - tl.center_y);
    if vert_pixels < 1.0 {
        return Err(FinderError::ProportionMismatch {
            actual: 0.0,
            expected: 1.0,
        });
    }
    let actual_aspect = horiz_pixels / vert_pixels;
    let expected_aspect = (page_width_dots as f32 - FINDER_SIZE_DOTS as f32)
        / (page_height_dots as f32 - FINDER_SIZE_DOTS as f32);
    // ±30% tolerance — generous for a sanity check; tighter would
    // false-positive on legitimate pages with mild scanner stretch.
    if (actual_aspect / expected_aspect - 1.0).abs() > 0.3 {
        return Err(FinderError::ProportionMismatch {
            actual: actual_aspect,
            expected: expected_aspect,
        });
    }

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
        let runs = row_runs(&bm, 5 + 3, 128); // center row of the finder
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
        let runs = row_runs(&bm, 26, 128);
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
    fn fails_with_insufficient_finders_on_blank_bitmap() {
        let bm = PageBitmap {
            pixels: vec![WHITE; 100 * 100],
            width: 100,
            height: 100,
        };
        let err = locate_finders(&bm, 50, 50).unwrap_err();
        assert!(matches!(err, FinderError::InsufficientFinders { found: 0 }));
    }

    #[test]
    fn identifies_corners_when_page_is_rotated() {
        // Build a 50×50-dot synthetic page with finders at TL/TR/BL,
        // then rotate the resulting bitmap by ~5° clockwise inside
        // a larger canvas. The detector should still identify
        // which finder is which via the L-shape + cross-product
        // analysis.
        let pw = 50u32;
        let ph = 50u32;
        let scale = 4u32;
        let inner_w = pw * scale;
        let inner_h = ph * scale;
        let mut inner_pixels = vec![WHITE; (inner_w * inner_h) as usize];
        draw_finder(&mut inner_pixels, inner_w, 0, 0, scale);
        draw_finder(&mut inner_pixels, inner_w, pw - FINDER_SIZE_DOTS, 0, scale);
        draw_finder(&mut inner_pixels, inner_w, 0, ph - FINDER_SIZE_DOTS, scale);

        // Rotate into a larger canvas to avoid clipping.
        let canvas_w = inner_w + 100;
        let canvas_h = inner_h + 100;
        let mut canvas = vec![WHITE; (canvas_w * canvas_h) as usize];
        let theta = 5.0_f32.to_radians();
        let cos_t = theta.cos();
        let sin_t = theta.sin();
        let cx_in = inner_w as f32 / 2.0;
        let cy_in = inner_h as f32 / 2.0;
        let cx_out = canvas_w as f32 / 2.0;
        let cy_out = canvas_h as f32 / 2.0;
        for ny in 0..canvas_h {
            for nx in 0..canvas_w {
                let dx = nx as f32 - cx_out;
                let dy = ny as f32 - cy_out;
                // Inverse rotation: rotate (dx, dy) by -theta to find source pixel.
                let ox = cos_t * dx + sin_t * dy + cx_in;
                let oy = -sin_t * dx + cos_t * dy + cy_in;
                let oxi = ox as i64;
                let oyi = oy as i64;
                if oxi < 0 || oyi < 0 || oxi >= inner_w as i64 || oyi >= inner_h as i64 {
                    continue;
                }
                canvas[(ny * canvas_w + nx) as usize] =
                    inner_pixels[(oyi as usize) * (inner_w as usize) + oxi as usize];
            }
        }
        let bm = PageBitmap { pixels: canvas, width: canvas_w, height: canvas_h };

        let [tl, tr, bl] = locate_finders(&bm, pw, ph).unwrap();
        // After 5° rotation, all three should still be detected
        // and correctly identified. Sanity-check by direction: TR
        // is to the right of TL (positive Δx), BL is below TL
        // (positive Δy). Rotation preserves these for ±45°.
        assert!(
            tr.center_x > tl.center_x,
            "TR should be to the right of TL after small rotation"
        );
        assert!(
            bl.center_y > tl.center_y,
            "BL should be below TL after small rotation"
        );
    }
}
