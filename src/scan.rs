// Scan-style decoder primitives. Per FORMAT-V1.md §7 and
// `Decoder.cpp:42-602`. The synthetic decoder in `crate::decoder`
// trusts the caller's `PageGeometry`; this module is the scaffolding
// for the *real* scan-decode path that infers geometry from a
// scanner-fed bitmap by histogram peak detection.
//
// Three pieces land in this commit (the bottom of the pipeline):
//   - find_peaks: the histogram peak detector. Mirrors `Findpeaks`
//     at Decoder.cpp:49-155.
//   - find_grid_position: bitmap → approximate raster bounding box.
//     Mirrors `Getgridposition` at Decoder.cpp:259-319.
//   - estimate_intensity: cmin/cmax/sharpness factor. Mirrors
//     `Getgridintensity` at Decoder.cpp:322-386.
//
// The angle finders (Getxangle, Getyangle) and per-block decode
// (Decodeblock with its 8 orientations × 9 dot-shifts × ... search)
// land in the next commit, since they consume the outputs of these
// three.
//
// Empirical magic numbers from PAPERBAK-HACKS.md §3.2-§3.4 stay as
// they are in the C source. Replacing them with cleaner heuristics
// is post-parity work.

use crate::block::NDOT;

/// Maximum histogram length the peak finder will consider. Mirrors
/// `NHYST = 1024` from Decoder.cpp:42. Inputs longer than this are
/// truncated to the first NHYST bins.
pub const NHYST: usize = 1024;

/// Maximum number of peaks recorded by the peak finder before it
/// stops accumulating. Mirrors `NPEAK = 32` from Decoder.cpp:43.
pub const NPEAK: usize = 32;

/// Result of a successful peak fit on a 1D intensity histogram.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PeakInfo {
    /// Centroid of the first detected peak in fractional histogram-bin
    /// coordinates. Pass to upstream geometry logic as the base offset
    /// of the grid.
    pub peak: f32,
    /// Estimated bin spacing between consecutive peaks. Used as the
    /// grid step.
    pub step: f32,
    /// Quality / weight of the fit; higher means more peaks agreed
    /// on the spacing. Callers comparing several candidate fits pick
    /// the one with the largest `weight`.
    pub weight: f32,
}

/// Find regularly-spaced peaks in a 1D intensity histogram and
/// estimate their position and step. Faithful port of `Findpeaks`
/// from Decoder.cpp:49-155.
///
/// Returns `None` when:
/// - `histogram.len() < 16` (too short to fit any sensible step)
/// - fewer than 2 peaks survive the 3/4-of-max threshold
/// - no consistent pairwise distance can be derived from the peaks
/// - the regression denominator collapses to zero
///
/// Empirical magic-number recap (per PAPERBAK-HACKS.md §3.4):
/// the threshold is fixed at 75% of peak-relief amplitude — works
/// in roughly 90% of real-world scans. The 3% dispersion tolerance
/// for grouping peak distances is the `i / 33 + 1` window.
#[must_use]
pub fn find_peaks(histogram: &[i32]) -> Option<PeakInfo> {
    let n = histogram.len().min(NHYST);
    if n < 16 {
        return None;
    }
    let h = &histogram[..n];

    // --- Min / max amplitude --------------------------------------
    let mut amin = h[0];
    let mut amax = h[0];
    for &v in &h[1..] {
        if v < amin {
            amin = v;
        }
        if v > amax {
            amax = v;
        }
    }

    // --- Two-pass shadowing -> inverted relief --------------------
    // Forward then backward passes with a sliding-floor decay rate
    // strip out slow-changing intensity gradients (illumination
    // unevenness in the scan). The second pass also subtracts the
    // raw histogram, so peaks turn into positive humps in `l[]`.
    let d = (amax - amin + 16) / 32;
    let mut l = vec![0i32; n];
    let mut ampl = h[0];
    for i in 0..n {
        ampl = (ampl - d).max(h[i]);
        l[i] = ampl;
    }
    let mut amax_relief = 0i32;
    for i in (0..n).rev() {
        ampl = (ampl - d).max(l[i]);
        l[i] = ampl - h[i];
        if l[i] > amax_relief {
            amax_relief = l[i];
        }
    }

    let mut limit = amax_relief * 3 / 4;
    if limit == 0 {
        limit = 1;
    }

    // --- Walk peaks, computing centroids and weights --------------
    let mut peaks: Vec<f32> = Vec::with_capacity(NPEAK);
    let mut weights: Vec<f32> = Vec::with_capacity(NPEAK);
    let mut heights: Vec<i32> = Vec::with_capacity(NPEAK);

    let mut i = 0usize;
    // Skip the first incomplete peak (might be cut off at the start
    // of the histogram, throwing off centroid math).
    while i < n && l[i] > limit {
        i += 1;
    }

    while i < n && peaks.len() < NPEAK {
        // Skip below-threshold region between peaks.
        while i < n && l[i] <= limit {
            i += 1;
        }
        if i >= n {
            break;
        }
        // Accumulate: integrate area, first moment, max amplitude.
        let mut area = 0f32;
        let mut moment = 0f32;
        let mut peak_max = 0i32;
        while i < n && l[i] > limit {
            let a = l[i] - limit;
            area += a as f32;
            moment += (a as f32) * (i as f32);
            if l[i] > peak_max {
                peak_max = l[i];
            }
            i += 1;
        }
        if i >= n {
            // Reached histogram end mid-peak — incomplete, drop it.
            break;
        }
        // Filter against predecessor: if this peak is 8x weaker
        // than the previous one it's noise; if 8x stronger the
        // predecessor was noise.
        if let Some(&prev) = heights.last() {
            if peak_max * 8 < prev {
                continue;
            }
            if peak_max > prev * 8 {
                peaks.pop();
                weights.pop();
                heights.pop();
            }
        }
        peaks.push(moment / area);
        weights.push(area);
        heights.push(peak_max);
    }

    if peaks.len() < 2 {
        return None;
    }

    // --- Find most common pairwise distance -----------------------
    // Distances under 16 bins are too short to be a real grid step
    // (would imply NDOT = 32 dots in fewer than 16 sample bins, so
    // grid resolution is too coarse to register).
    let mut dist_hist = vec![0i32; n];
    for a in 0..peaks.len() - 1 {
        for b in a + 1..peaks.len() {
            let dist = (peaks[b] - peaks[a]) as usize;
            if dist < n {
                dist_hist[dist] += 1;
            }
        }
    }
    let mut bestdist = 0usize;
    let mut bestcount = 0i32;
    for i in 16..n {
        if dist_hist[i] == 0 {
            continue;
        }
        // Sum a 3% tolerance window above i.
        let upper = (i + i / 33 + 2).min(n);
        let sum: i32 = dist_hist[i..upper].iter().sum();
        if sum > bestcount {
            bestdist = i;
            bestcount = sum;
        }
    }
    if bestdist == 0 {
        return None;
    }

    // --- Linear regression on consistent peak pairs --------------
    // Fit a line through pairs (peaks[i], peaks[j]) where the
    // distance lies within the bestdist window. Each pair contributes
    // two points to the regression; `k` is a virtual ordinal of the
    // point in the inferred sequence, computed from the running fit.
    let mut sn = 0f32;
    let mut sx = 0f32;
    let mut sy = 0f32;
    let mut sxx = 0f32;
    let mut sxy = 0f32;
    let mut moment_sum = 0f32;
    let upper_dist = bestdist + bestdist / 33 + 2;
    for a in 0..peaks.len() - 1 {
        for b in a + 1..peaks.len() {
            let dist = (peaks[b] - peaks[a]) as i32;
            if dist < bestdist as i32 || dist >= upper_dist as i32 {
                continue;
            }
            let k = if sn == 0.0 {
                0i32
            } else {
                let denom = sx * sx - sn * sxx;
                if denom == 0.0 {
                    0
                } else {
                    let x0 = (sx * sxy - sxx * sy) / denom;
                    let step_est = (sx * sy - sn * sxy) / denom;
                    if step_est == 0.0 {
                        0
                    } else {
                        ((peaks[a] - x0 + step_est / 2.0) / step_est) as i32
                    }
                }
            };
            sn += 2.0;
            sx += (k * 2 + 1) as f32;
            sy += peaks[a] + peaks[b];
            sxx += (k * k + (k + 1) * (k + 1)) as f32;
            sxy += peaks[a] * (k as f32) + peaks[b] * ((k + 1) as f32);
            moment_sum += (heights[a] + heights[b]) as f32;
        }
    }
    if sn == 0.0 {
        return None;
    }
    let denom = sx * sx - sn * sxx;
    if denom == 0.0 {
        return None;
    }
    let peak = (sx * sxy - sxx * sy) / denom;
    let step = (sx * sy - sn * sxy) / denom;
    let weight = moment_sum / sn;
    Some(PeakInfo { peak, step, weight })
}

/// Approximate bounding box of the data raster within a scanned
/// bitmap. Coordinates are pixel offsets in the input bitmap's
/// top-left-origin system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridBounds {
    pub xmin: u32,
    pub xmax: u32,
    pub ymin: u32,
    pub ymax: u32,
}

/// Find the approximate bounding box of the data raster within a
/// scanned bitmap. Mirrors `Getgridposition` from Decoder.cpp:259-319.
///
/// Algorithm: subsample at most 256 horizontal × 256 vertical lines.
/// At each sample point, take a 5-pixel "fast intensity range" over
/// a 3-by-3 neighborhood (4 corners + center). Sum these per-row and
/// per-column. The data raster has rapid intensity change at every
/// dot pitch; uniform paper margins do not. A 50%-of-max threshold
/// finds the row/column where each axis crosses into the raster.
///
/// Returns `None` when the bitmap is smaller than 3×NDOT in either
/// dimension (too small to fit even one dot grid).
#[must_use]
pub fn find_grid_position(bitmap: &[u8], width: u32, height: u32) -> Option<GridBounds> {
    let sizex = width as usize;
    let sizey = height as usize;
    if sizex <= 3 * NDOT || sizey <= 3 * NDOT {
        return None;
    }
    if bitmap.len() < sizex * sizey {
        return None;
    }
    let stepx = sizex / 256 + 1;
    let nx = ((sizex - 2) / stepx).min(256);
    let stepy = sizey / 256 + 1;
    let ny = ((sizey - 2) / stepy).min(256);

    let mut distrx = vec![0i32; nx];
    let mut distry = vec![0i32; ny];

    // Both axes accumulate the same per-sample delta, so the natural
    // shape is index-on-both-axes; clippy's iter_mut suggestion only
    // covers one. Allow the lint here in exchange for keeping the
    // code parallel to Decoder.cpp:282-293.
    #[allow(clippy::needless_range_loop)]
    for j in 0..ny {
        let row_base = j * stepy * sizex;
        for i in 0..nx {
            let base = row_base + i * stepx;
            // Sample 5 pixels in a 3x3 area: (0,0), (2,0), (1,1),
            // (0,2), (2,2). Mirrors Decoder.cpp:285-289.
            let c0 = i32::from(bitmap[base]);
            let c1 = i32::from(bitmap[base + 2]);
            let c2 = i32::from(bitmap[base + sizex + 1]);
            let c3 = i32::from(bitmap[base + 2 * sizex]);
            let c4 = i32::from(bitmap[base + 2 * sizex + 2]);
            let cmin = c0.min(c1).min(c2).min(c3).min(c4);
            let cmax = c0.max(c1).max(c2).max(c3).max(c4);
            let delta = cmax - cmin;
            distrx[i] += delta;
            distry[j] += delta;
        }
    }

    fn axis_bounds(distr: &[i32], step: usize) -> (u32, u32) {
        let max = distr.iter().copied().max().unwrap_or(0);
        let limit = max / 2;
        let n = distr.len();
        let mut start = 0usize;
        for (i, &v) in distr.iter().enumerate().take(n.saturating_sub(1)) {
            if v >= limit {
                start = i;
                break;
            }
        }
        let mut end = 0usize;
        for i in (1..n).rev() {
            if distr[i] >= limit {
                end = i;
                break;
            }
        }
        ((start * step) as u32, (end * step) as u32)
    }

    let (xmin, xmax) = axis_bounds(&distrx, stepx);
    let (ymin, ymax) = axis_bounds(&distry, stepy);

    Some(GridBounds {
        xmin,
        xmax,
        ymin,
        ymax,
    })
}

/// Intensity statistics over the central data region.
#[derive(Clone, Copy, Debug)]
pub struct GridIntensity {
    /// Mean intensity of the sampled region.
    pub cmean: u8,
    /// 3rd-percentile intensity — used as the "black" reference.
    pub cmin: u8,
    /// 97th-percentile intensity — used as the "white" reference.
    pub cmax: u8,
    /// Initial sharpness factor estimate (per PAPERBAK-HACKS.md §3.2).
    /// Refined later when dot size is known. Range [0.0, 2.0]
    /// after clamping; higher = more aggressive unsharp masking.
    pub sharpfactor: f32,
    /// Coordinates of the sampled NHYST-sized box, useful for the
    /// downstream angle finders that operate on the same window.
    pub searchx0: u32,
    pub searchx1: u32,
    pub searchy0: u32,
    pub searchy1: u32,
}

/// Estimate intensity statistics over a centered NHYST × NHYST patch
/// of the bitmap inside the given bounds. Mirrors `Getgridintensity`
/// from Decoder.cpp:322-386.
///
/// Returns `None` when the central patch is uniformly flat (cmax == cmin),
/// which Decoder.cpp:365-366 flags as "No image". Empirical
/// magic-number recap from PAPERBAK-HACKS.md §3.2: sharpness factor
/// is `(cmax - cmin) / (2 * contrast) - 1`, clamped to [0, 2], where
/// `contrast` is the 5th-percentile of |adjacent-pixel-deltas|.
#[must_use]
pub fn estimate_intensity(
    bitmap: &[u8],
    width: u32,
    height: u32,
    bounds: &GridBounds,
) -> Option<GridIntensity> {
    let sizex = width as usize;
    let sizey = height as usize;
    if bitmap.len() < sizex * sizey {
        return None;
    }
    let centerx = (bounds.xmin + bounds.xmax) as usize / 2;
    let centery = (bounds.ymin + bounds.ymax) as usize / 2;
    let half = NHYST / 2;
    let searchx0 = centerx.saturating_sub(half);
    let searchx1 = (searchx0 + NHYST).min(sizex);
    let searchy0 = centery.saturating_sub(half);
    let searchy1 = (searchy0 + NHYST).min(sizey);
    let dx = searchx1 - searchx0;
    let dy = searchy1 - searchy0;
    if dx < 2 || dy < 2 {
        return None;
    }

    // Histogram of pixel intensities in the search window, plus a
    // histogram of |adjacent pixel deltas| in both axes.
    let mut distrc = [0i32; 256];
    let mut distrd = [0i32; 256];
    let mut cmean_acc: u64 = 0;
    let mut sample_count: u64 = 0;
    for j in 0..(dy - 1) {
        let row = (searchy0 + j) * sizex + searchx0;
        for i in 0..(dx - 1) {
            let p = bitmap[row + i];
            distrc[p as usize] += 1;
            cmean_acc += u64::from(p);
            sample_count += 1;
            let right = bitmap[row + i + 1];
            let down = bitmap[row + sizex + i];
            distrd[(p.abs_diff(right)) as usize] += 1;
            distrd[(p.abs_diff(down)) as usize] += 1;
        }
    }
    if sample_count == 0 {
        return None;
    }
    let cmean = (cmean_acc / sample_count) as u8;

    // 3% percentiles. Mirrors Decoder.cpp:358-364.
    let limit_3pct = (sample_count / 33) as i32;
    let mut cmin = 0u8;
    let mut sum = 0i32;
    for c in 0..=255u16 {
        sum += distrc[c as usize];
        if sum >= limit_3pct {
            cmin = c as u8;
            break;
        }
    }
    let mut cmax = 255u8;
    sum = 0;
    for c in (0..=255u16).rev() {
        sum += distrc[c as usize];
        if sum >= limit_3pct {
            cmax = c as u8;
            break;
        }
    }
    if cmax <= cmin {
        return None;
    }

    // Sharpness factor. The "5%" comment from Decoder.cpp:371 is
    // because each pixel's delta is counted twice (once horizontal,
    // once vertical), so 10% of count = 5% of pixels.
    let limit_10pct = (2 * sample_count / 10) as i32;
    let mut contrast = 1u8;
    sum = 0;
    for c in (1..=255u16).rev() {
        sum += distrd[c as usize];
        if sum >= limit_10pct {
            contrast = c as u8;
            break;
        }
    }
    let raw_sharp = f32::from(cmax - cmin) / (2.0 * f32::from(contrast)) - 1.0;
    let sharpfactor = raw_sharp.clamp(0.0, 2.0);

    Some(GridIntensity {
        cmean,
        cmin,
        cmax,
        sharpfactor,
        searchx0: searchx0 as u32,
        searchx1: searchx1 as u32,
        searchy0: searchy0 as u32,
        searchy1: searchy1 as u32,
    })
}

/// Result of an angle-finder fit. The `peak` and `step` fields are in
/// bitmap-pixel units (already shifted into the bitmap's coordinate
/// system); `angle` is radians. Both axes use the same struct.
#[derive(Clone, Copy, Debug)]
pub struct AngleInfo {
    /// Position (in bitmap pixel coordinates) of the first detected
    /// grid line for this axis.
    pub peak: f32,
    /// Step (per cell) in pixels — equal to `(NDOT + 3) * dot_pitch`
    /// for a well-resolved grid.
    pub step: f32,
    /// Tilt angle in radians, ≈ `tan(θ)` for small θ. Positive means
    /// vertical lines tilt right with increasing y (for x-axis) /
    /// horizontal lines tilt down with increasing x (for y-axis).
    pub angle: f32,
    /// Quality / weight of the fit; the angle finder picks the
    /// candidate with the largest weight.
    pub weight: f32,
}

/// Find the angle and step of vertical grid lines. Mirrors
/// `Getxangle` from Decoder.cpp:389-450.
///
/// Tries a fan of candidate angles in roughly ±5° (NHYST/20 each
/// side, step 2). For each candidate, projects the search area onto
/// a column histogram via affine X transform, then runs [`find_peaks`].
/// Picks the candidate with the highest weight, with a small bias
/// favoring zero angle (`1 / (|a| + 10)`) to break ties on flat-fit
/// histograms — matches Decoder.cpp:432.
///
/// Returns `None` when no candidate yields a non-zero weight or
/// when the best step is below NDOT (can't fit even one block).
#[must_use]
pub fn find_x_angle(
    bitmap: &[u8],
    width: u32,
    height: u32,
    intensity: &GridIntensity,
) -> Option<AngleInfo> {
    let sizex = width as usize;
    let sizey = height as usize;
    if bitmap.len() < sizex * sizey {
        return None;
    }
    let x0 = intensity.searchx0 as i32;
    let y0 = intensity.searchy0 as i32;
    let dx = (intensity.searchx1 - intensity.searchx0) as i32;
    let dy = (intensity.searchy1 - intensity.searchy0) as i32;
    if dx <= 0 || dy <= 0 {
        return None;
    }
    let ystep = (dy / 256).max(1);

    let mut best: Option<AngleInfo> = None;
    let mut max_weight = 0f32;
    let amax = (NHYST as i32 / 20) * 2;
    let mut a = -amax;
    while a <= amax {
        // Build a column histogram with affine X transform.
        let mut h = vec![0i32; dx as usize];
        let mut nh = vec![0i32; dx as usize];
        let mut j = 0;
        while j < dy {
            let y = y0 + j;
            let row_x = x0 + (y0 + j) * a / NHYST as i32;
            #[allow(clippy::needless_range_loop)]
            for i in 0..dx as usize {
                let x = row_x + i as i32;
                if x >= 0 && x < sizex as i32 && y >= 0 && y < sizey as i32 {
                    let idx = y as usize * sizex + x as usize;
                    h[i] += i32::from(bitmap[idx]);
                    nh[i] += 1;
                }
            }
            j += ystep;
        }
        // Normalize the per-column average.
        for (i, slot) in h.iter_mut().enumerate() {
            if nh[i] > 0 {
                *slot /= nh[i];
            }
        }
        // Find peaks; add the small bias toward zero angle.
        if let Some(info) = find_peaks(&h) {
            let bias = 1.0 / (a.unsigned_abs() as f32 + 10.0);
            let weight = info.weight + bias;
            if weight > max_weight {
                max_weight = weight;
                best = Some(AngleInfo {
                    peak: info.peak + x0 as f32,
                    step: info.step,
                    angle: a as f32 / NHYST as f32,
                    weight,
                });
            }
        }
        a += 2;
    }

    let info = best?;
    if info.step < NDOT as f32 {
        return None;
    }
    Some(info)
}

/// Find the angle and step of horizontal grid lines. Mirrors
/// `Getyangle` from Decoder.cpp:453-513. Same pattern as
/// [`find_x_angle`] with axes swapped.
///
/// Has a tighter validity check than X: rejects fits where the
/// detected Y step is more than 2.5x or less than 0.40x the X step,
/// per Decoder.cpp:501-503. Real grids are roughly square; an
/// asymmetric fit indicates a misregistered axis.
#[must_use]
pub fn find_y_angle(
    bitmap: &[u8],
    width: u32,
    height: u32,
    intensity: &GridIntensity,
    x_angle: &AngleInfo,
) -> Option<AngleInfo> {
    let sizex = width as usize;
    let sizey = height as usize;
    if bitmap.len() < sizex * sizey {
        return None;
    }
    let x0 = intensity.searchx0 as i32;
    let y0 = intensity.searchy0 as i32;
    let dx = (intensity.searchx1 - intensity.searchx0) as i32;
    let dy = (intensity.searchy1 - intensity.searchy0) as i32;
    if dx <= 0 || dy <= 0 {
        return None;
    }
    let xstep = (dx / 256).max(1);

    let mut best: Option<AngleInfo> = None;
    let mut max_weight = 0f32;
    let amax = (NHYST as i32 / 20) * 2;
    let mut a = -amax;
    while a <= amax {
        let mut h = vec![0i32; dy as usize];
        let mut nh = vec![0i32; dy as usize];
        let mut i = 0;
        while i < dx {
            let x = x0 + i;
            let col_y = y0 + (x0 + i) * a / NHYST as i32;
            #[allow(clippy::needless_range_loop)]
            for j in 0..dy as usize {
                let y = col_y + j as i32;
                if x >= 0 && x < sizex as i32 && y >= 0 && y < sizey as i32 {
                    let idx = y as usize * sizex + x as usize;
                    h[j] += i32::from(bitmap[idx]);
                    nh[j] += 1;
                }
            }
            i += xstep;
        }
        for (j, slot) in h.iter_mut().enumerate() {
            if nh[j] > 0 {
                *slot /= nh[j];
            }
        }
        if let Some(info) = find_peaks(&h) {
            let bias = 1.0 / (a.unsigned_abs() as f32 + 10.0);
            let weight = info.weight + bias;
            if weight > max_weight {
                max_weight = weight;
                best = Some(AngleInfo {
                    peak: info.peak + y0 as f32,
                    step: info.step,
                    angle: a as f32 / NHYST as f32,
                    weight,
                });
            }
        }
        a += 2;
    }

    let info = best?;
    if info.step < NDOT as f32 {
        return None;
    }
    // Sanity-check Y step against X step.
    if info.step < x_angle.step * 0.40 || info.step > x_angle.step * 2.50 {
        return None;
    }
    Some(info)
}

/// Sample one pixel at (x, y) in the bitmap with bilinear
/// interpolation. Returns 255 (white) for out-of-bounds.
fn sample_bilinear(bitmap: &[u8], width: usize, height: usize, x: f32, y: f32) -> u8 {
    let ix = x.floor();
    let iy = y.floor();
    if ix < 0.0 || iy < 0.0 || ix as i32 >= width as i32 - 1 || iy as i32 >= height as i32 - 1 {
        return 255;
    }
    let xres = x - ix;
    let yres = y - iy;
    let ux = ix as usize;
    let uy = iy as usize;
    let p00 = f32::from(bitmap[uy * width + ux]);
    let p01 = f32::from(bitmap[uy * width + ux + 1]);
    let p10 = f32::from(bitmap[(uy + 1) * width + ux]);
    let p11 = f32::from(bitmap[(uy + 1) * width + ux + 1]);
    let top = p00 * (1.0 - xres) + p01 * xres;
    let bot = p10 * (1.0 - xres) + p11 * xres;
    let p = top * (1.0 - yres) + bot * yres;
    p.clamp(0.0, 255.0) as u8
}

/// Sample a single block at cell position `(posx, posy)` and decode
/// its 32×32 dot grid into 128 wire bytes. Uses the affine transform
/// from `(x_angle, y_angle)` to handle small grid rotation, with
/// bilinear interpolation on each dot center plus a configurable
/// pixel-level `(shift_x, shift_y)` offset applied to every dot.
/// Callers can sweep shifts to find the offset that gives a valid
/// CRC — this mirrors Decoder.cpp:716-746 which builds 9 candidate
/// grids per block, one per (-1, 0, +1) × (-1, 0, +1) shift.
///
/// Does NOT try multiple orientations — assumes natural page
/// orientation. The full multi-orientation search from
/// Decoder.cpp:170-216 is left for a follow-on commit; this
/// happy-path version covers ±5° rotation (the angle finders' range)
/// and small noise but not 90° / mirrored input.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn sample_block_at(
    bitmap: &[u8],
    width: u32,
    height: u32,
    posx: u32,
    posy: u32,
    x_angle: &AngleInfo,
    y_angle: &AngleInfo,
    threshold: u8,
    shift_x: f32,
    shift_y: f32,
) -> [u8; crate::block::BLOCK_BYTES] {
    let w = width as usize;
    let h = height as usize;
    // Per-cell origin (top-left dot of the block) in undistorted coords.
    let cell_x = x_angle.peak + x_angle.step * posx as f32;
    let cell_y = y_angle.peak + y_angle.step * posy as f32;
    // 2-dot leading border per FORMAT-V1.md §4.2. The encoder draws
    // each dot at the LEFT edge of its dot-pitch slot (not centered),
    // so the sampling target is just `cell + 2 * pitch` — adding
    // another `pitch/2` for "centering" lands in the white gap
    // between dots.
    let dot_pitch_x = x_angle.step / (NDOT as f32 + 3.0);
    let dot_pitch_y = y_angle.step / (NDOT as f32 + 3.0);
    let first_dot_x = cell_x + 2.0 * dot_pitch_x;
    let first_dot_y = cell_y + 2.0 * dot_pitch_y;

    let mut grid = [0u32; NDOT];
    for (j, slot) in grid.iter_mut().enumerate() {
        let mut row = 0u32;
        for i in 0..NDOT {
            let dot_x = first_dot_x + i as f32 * dot_pitch_x;
            let dot_y = first_dot_y + j as f32 * dot_pitch_y;
            // Apply affine rotation, then the per-call shift offset
            // so the caller can sweep ±1 pixel shifts to find the
            // best CRC match (Decoder.cpp:716-746).
            let bmp_x = dot_x + dot_y * x_angle.angle + shift_x;
            let bmp_y = dot_y + dot_x * y_angle.angle + shift_y;
            // Single bilinear at the dot's predicted center (a
            // half-pixel offset). When the predicted center sits
            // between four pixels, bilinear interpolation already
            // produces the 2x2 average that Decoder.cpp:739-743's
            // dotsize=2 case computes from integer reads — but
            // bounded to those 4 pixels, with no spill into the
            // adjacent dot or gap.
            //
            // Wider sampling windows (e.g. four bilinears at +1
            // offsets) effectively read a 3x3 weighted region, which
            // spills past dot edges. With dot pitch 3 and dot width
            // 2, the gap between dots is only 1 pixel — the spill
            // pulls the average above the black/white threshold for
            // set dots and the decoder fails. Registration error
            // from integer-step angle search is handled instead by
            // the 9-shift sweep in [`sample_block_best_shift`].
            let pixel = sample_bilinear(bitmap, w, h, bmp_x, bmp_y);
            if pixel < threshold {
                row |= 1u32 << i;
            }
        }
        *slot = row;
    }
    crate::dot_grid::dot_grid_to_block(&grid)
}

/// Geometry context derived from a scanned bitmap. The output of the
/// peak / intensity / angle pipeline; downstream cell sampling
/// consumes this struct rather than re-running the analysis.
#[derive(Clone, Copy, Debug)]
pub struct ScanGeometry {
    pub x_angle: AngleInfo,
    pub y_angle: AngleInfo,
    pub threshold: u8,
    pub nposx: u32,
    pub nposy: u32,
}

/// Run the bounds → intensity → angles pipeline and pull peaks back
/// to the first cell. Returns `None` when any stage fails.
#[must_use]
pub fn detect_geometry(bitmap: &[u8], width: u32, height: u32) -> Option<ScanGeometry> {
    let bounds = find_grid_position(bitmap, width, height)?;
    let intensity = estimate_intensity(bitmap, width, height, &bounds)?;
    let xa = find_x_angle(bitmap, width, height, &intensity)?;
    let ya = find_y_angle(bitmap, width, height, &intensity, &xa)?;
    let mut x_peak = xa.peak;
    let mut y_peak = ya.peak;
    while x_peak >= xa.step {
        x_peak -= xa.step;
    }
    while y_peak >= ya.step {
        y_peak -= ya.step;
    }
    let xa = AngleInfo { peak: x_peak, ..xa };
    let ya = AngleInfo { peak: y_peak, ..ya };
    let nposx = ((width as f32 - x_peak) / xa.step) as u32;
    let nposy = ((height as f32 - y_peak) / ya.step) as u32;
    if nposx == 0 || nposy == 0 {
        return None;
    }
    let threshold = ((u16::from(intensity.cmin) + u16::from(intensity.cmax)) / 2) as u8;
    Some(ScanGeometry {
        x_angle: xa,
        y_angle: ya,
        threshold,
        nposx,
        nposy,
    })
}

/// 9-shift matrix from Decoder.cpp:716-728 — try each (-1, 0, +1) ×
/// (-1, 0, +1) integer-pixel offset and accept the first that gives
/// a CRC-verified block after Reed-Solomon correction. The natural
/// shift `(0, 0)` is tried first because Decoder.cpp:751 reports it
/// as "the most probable good candidate".
const SHIFT_MATRIX: [(f32, f32); 9] = [
    (0.0, 0.0),
    (-1.0, 0.0),
    (1.0, 0.0),
    (0.0, -1.0),
    (0.0, 1.0),
    (-1.0, -1.0),
    (1.0, -1.0),
    (-1.0, 1.0),
    (1.0, 1.0),
];

/// Sample one cell at `(posx, posy)`, trying every shift in
/// [`SHIFT_MATRIX`] and accepting the first that decodes to a
/// CRC-verified block (after Reed-Solomon correction). Falls back
/// to the natural-shift sample if no shift verifies — caller's CRC
/// check will then reject it.
fn sample_block_best_shift(
    bitmap: &[u8],
    width: u32,
    height: u32,
    posx: u32,
    posy: u32,
    geometry: &ScanGeometry,
) -> [u8; crate::block::BLOCK_BYTES] {
    let mut natural = sample_block_at(
        bitmap,
        width,
        height,
        posx,
        posy,
        &geometry.x_angle,
        &geometry.y_angle,
        geometry.threshold,
        0.0,
        0.0,
    );
    let _ = crate::ecc::decode8(&mut natural);
    if crate::block::Block::from_bytes(&natural).verify_crc() {
        return natural;
    }
    for &(sx, sy) in &SHIFT_MATRIX[1..] {
        let mut cell = sample_block_at(
            bitmap,
            width,
            height,
            posx,
            posy,
            &geometry.x_angle,
            &geometry.y_angle,
            geometry.threshold,
            sx,
            sy,
        );
        let _ = crate::ecc::decode8(&mut cell);
        if crate::block::Block::from_bytes(&cell).verify_crc() {
            return cell;
        }
    }
    // No shift produced a CRC-verified block. Return the
    // (already-RS-corrected) natural sample so callers can still
    // inspect it; their CRC check will drop it.
    natural
}

/// Top-level scan extractor: find grid bounds + intensity + angles,
/// then walk every (posx, posy) cell position and sample the
/// resulting 128 bytes. Each cell is sweep-sampled across the 9
/// integer-shift matrix and Reed-Solomon corrected; the returned
/// bytes are post-RS, so callers should run [`crate::block::Block::verify_crc`]
/// before trusting them.
///
/// Returns `None` when the scan pipeline cannot register the grid
/// (bitmap too small, no contrast, no peaks, asymmetric Y step,
/// etc.). Distinguishes "couldn't find grid" from "grid found but
/// some blocks unreadable" — the latter is normal and individual
/// blocks fail CRC verification later.
#[must_use]
pub fn scan_extract(
    bitmap: &[u8],
    width: u32,
    height: u32,
) -> Option<Vec<[u8; crate::block::BLOCK_BYTES]>> {
    let geometry = detect_geometry(bitmap, width, height)?;
    let mut out = Vec::with_capacity((geometry.nposx * geometry.nposy) as usize);
    for posy in 0..geometry.nposy {
        for posx in 0..geometry.nposx {
            out.push(sample_block_best_shift(
                bitmap, width, height, posx, posy, &geometry,
            ));
        }
    }
    Some(out)
}

/// Scan-decode a list of bitmaps to the original input bytes. Same
/// reassembly contract as `crate::decoder::decode` — SuperBlock-driven
/// metadata, XOR recovery for one missing block per group, optional
/// bzip2 decompression — but uses [`scan_extract`] instead of
/// known-geometry sampling. This is the M6 sub-bullet 1 entry point.
///
/// Each input is `(bitmap_bytes, width, height)`. Returns the
/// original input bytes on success.
pub fn scan_decode(
    pages: &[(&[u8], u32, u32)],
    password: Option<&[u8]>,
) -> Result<Vec<u8>, crate::decoder::DecodeError> {
    use crate::block::{
        Block, NDATA, NGROUP_MAX, NGROUP_MIN, PBM_COMPRESSED, PBM_ENCRYPTED, SuperBlock,
    };
    use crate::decoder::DecodeError;

    let mut superblock: Option<SuperBlock> = None;
    let mut data_blocks: std::collections::BTreeMap<u32, [u8; NDATA]> = Default::default();
    let mut recovery_blocks: Vec<(u32, u8, [u8; NDATA])> = Vec::new();
    let mut metadata_inconsistency = false;

    for &(bitmap, width, height) in pages {
        // scan_extract already runs Reed-Solomon correction on each
        // cell via the 9-shift sampling sweep; just CRC-filter here.
        let cells = scan_extract(bitmap, width, height).ok_or(DecodeError::NoSuperBlock)?;
        for cell in cells {
            let block = Block::from_bytes(&cell);
            if !block.verify_crc() {
                continue;
            }
            if block.is_super() {
                let parsed = match SuperBlock::from_bytes(&cell) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if !parsed.verify_crc() {
                    continue;
                }
                if let Some(existing) = superblock {
                    if existing.datasize != parsed.datasize
                        || existing.origsize != parsed.origsize
                        || existing.mode != parsed.mode
                        || existing.filecrc != parsed.filecrc
                        || existing.name != parsed.name
                    {
                        metadata_inconsistency = true;
                    }
                } else {
                    superblock = Some(parsed);
                }
            } else if block.is_data() {
                data_blocks.entry(block.offset()).or_insert(block.data);
            } else if block.is_recovery() {
                let ngroup = block.ngroup();
                if (NGROUP_MIN..=NGROUP_MAX).contains(&ngroup) {
                    recovery_blocks.push((block.offset(), ngroup, block.data));
                }
            }
        }
    }

    if metadata_inconsistency {
        return Err(DecodeError::InconsistentSuperBlocks);
    }
    let superblock = superblock.ok_or(DecodeError::NoSuperBlock)?;
    let datasize = superblock.datasize;

    let mut buf = vec![0u8; datasize as usize];
    let mut filled = vec![false; (datasize.div_ceil(NDATA as u32)) as usize];
    for (&offset, data) in &data_blocks {
        if offset >= datasize {
            continue;
        }
        let off = offset as usize;
        let copy_len = NDATA.min(buf.len() - off);
        buf[off..off + copy_len].copy_from_slice(&data[..copy_len]);
        let block_index = off / NDATA;
        if block_index < filled.len() {
            filled[block_index] = true;
        }
    }
    for (recovery_offset, ngroup, recovery_data) in &recovery_blocks {
        let group_size = *ngroup as usize;
        let group_first_block = *recovery_offset as usize / NDATA;
        if group_first_block + group_size > filled.len() {
            continue;
        }
        let mut missing_count = 0usize;
        let mut missing_idx = 0usize;
        for k in 0..group_size {
            let bi = group_first_block + k;
            if !filled[bi] {
                missing_count += 1;
                missing_idx = bi;
            }
        }
        if missing_count != 1 {
            continue;
        }
        let mut recovered = *recovery_data;
        for r in &mut recovered {
            *r ^= 0xFF;
        }
        for k in 0..group_size {
            let bi = group_first_block + k;
            if bi == missing_idx {
                continue;
            }
            let off = bi * NDATA;
            let end = (off + NDATA).min(buf.len());
            if end <= off {
                continue;
            }
            for (r, &b) in recovered.iter_mut().zip(buf[off..end].iter()) {
                *r ^= b;
            }
        }
        let off = missing_idx * NDATA;
        let copy_len = NDATA.min(buf.len().saturating_sub(off));
        buf[off..off + copy_len].copy_from_slice(&recovered[..copy_len]);
        filled[missing_idx] = true;
    }
    for (i, &ok) in filled.iter().enumerate() {
        if !ok {
            return Err(DecodeError::UnrecoverableGap {
                offset: (i * NDATA) as u32,
            });
        }
    }
    // Optional AES-192-CBC decrypt; same shape as crate::decoder::decode.
    if superblock.mode & PBM_ENCRYPTED != 0 {
        let password = password.ok_or(DecodeError::PasswordRequired)?;
        let salt: &[u8; 16] = superblock.name[32..48]
            .try_into()
            .expect("16 bytes from a 64-byte slice");
        let iv: &[u8; 16] = superblock.name[48..64]
            .try_into()
            .expect("16 bytes from a 64-byte slice");
        let key = crate::legacy_aes::derive_key_v1(password, salt);
        crate::legacy_aes::decrypt_v1_in_place(&mut buf, &key, iv)
            .map_err(DecodeError::DecryptFailed)?;
        let computed = crate::crc::crc16(&buf);
        if computed != superblock.filecrc {
            return Err(DecodeError::InvalidPassword);
        }
    }
    if superblock.mode & PBM_COMPRESSED != 0 {
        buf = crate::bz::decompress(&buf).map_err(|e| DecodeError::BzipFailed(e.to_string()))?;
    }
    buf.truncate(superblock.origsize as usize);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fewer than 16 bins must always return None (too short to fit
    /// any sensible grid step).
    #[test]
    fn find_peaks_rejects_short_input() {
        assert!(find_peaks(&[]).is_none());
        assert!(find_peaks(&[100; 8]).is_none());
        assert!(find_peaks(&[100; 15]).is_none());
    }

    /// A flat histogram has no peaks and must return None.
    #[test]
    fn find_peaks_rejects_flat() {
        assert!(find_peaks(&[100; 200]).is_none());
    }

    /// Synthetic histogram with regular dips at known positions and
    /// a known step. Find_peaks must recover the step within ±1
    /// (subpixel jitter from the centroid math is fine).
    #[test]
    fn find_peaks_recovers_known_step() {
        // Build a histogram where most of the signal is at intensity
        // 100, with sharp dips down to 0 every 30 bins. The dips are
        // what `Findpeaks` calls "peaks" — it operates on the
        // inverted relief, so a low-intensity dip becomes a high
        // peak in `l[]`.
        let n = 600;
        let step = 30usize;
        let dips = (0..n).step_by(step).skip(2).collect::<Vec<_>>(); // skip first two (avoid edge effect)
        let mut histogram = vec![100i32; n];
        for &d in &dips {
            for offset in 0..3 {
                if d + offset < n {
                    histogram[d + offset] = 0;
                }
            }
        }
        let info = find_peaks(&histogram).expect("expected a fit on regular dips");
        assert!(
            (info.step - step as f32).abs() < 1.0,
            "step {} not within 1 of {step}",
            info.step
        );
    }

    /// Tiny bitmap (smaller than 3*NDOT in either dimension) must
    /// be rejected — there's no room for even one block.
    #[test]
    fn find_grid_position_rejects_tiny_bitmap() {
        let bitmap = vec![255u8; 50 * 50];
        assert!(find_grid_position(&bitmap, 50, 50).is_none());
        let bitmap = vec![255u8; 200 * 50];
        assert!(find_grid_position(&bitmap, 200, 50).is_none());
    }

    /// Synthetic bitmap with a large white margin around a noisy
    /// data region. Bounds should land inside the noisy region.
    #[test]
    fn find_grid_position_locates_noisy_region() {
        let width = 400u32;
        let height = 400u32;
        let mut bitmap = vec![255u8; (width * height) as usize];
        // Noisy data region from x=80..320, y=80..320 (240x240 px).
        // Alternating black/white pixels in a checker pattern.
        for y in 80..320usize {
            for x in 80..320usize {
                bitmap[y * width as usize + x] = if (x + y) % 2 == 0 { 0 } else { 255 };
            }
        }
        let bounds = find_grid_position(&bitmap, width, height).unwrap();
        // The 50%-of-max threshold should land within the data region.
        // Allow some slack from the 256-sample subsampling.
        assert!(
            (60..120).contains(&bounds.xmin),
            "xmin {} not near 80",
            bounds.xmin
        );
        assert!(
            (280..340).contains(&bounds.xmax),
            "xmax {} not near 320",
            bounds.xmax
        );
        assert!(
            (60..120).contains(&bounds.ymin),
            "ymin {} not near 80",
            bounds.ymin
        );
        assert!(
            (280..340).contains(&bounds.ymax),
            "ymax {} not near 320",
            bounds.ymax
        );
    }

    /// Uniform white bitmap has cmin == cmax (no signal); estimate
    /// must return None.
    #[test]
    fn estimate_intensity_rejects_flat() {
        let width = 1100u32;
        let height = 1100u32;
        let bitmap = vec![255u8; (width * height) as usize];
        let bounds = GridBounds {
            xmin: 0,
            xmax: width,
            ymin: 0,
            ymax: height,
        };
        assert!(estimate_intensity(&bitmap, width, height, &bounds).is_none());
    }

    /// Bitmap with a known black/white checkerboard returns
    /// cmin near 0 and cmax near 255.
    #[test]
    fn estimate_intensity_finds_extremes_in_checkerboard() {
        let width = 1100u32;
        let height = 1100u32;
        let mut bitmap = vec![255u8; (width * height) as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                bitmap[y * width as usize + x] = if (x + y) % 2 == 0 { 0 } else { 255 };
            }
        }
        let bounds = GridBounds {
            xmin: 0,
            xmax: width,
            ymin: 0,
            ymax: height,
        };
        let intensity = estimate_intensity(&bitmap, width, height, &bounds).unwrap();
        // 3rd / 97th percentile of a 50/50 mix should be at the extremes.
        assert_eq!(intensity.cmin, 0);
        assert_eq!(intensity.cmax, 255);
    }

    use crate::encoder::{EncodeOptions, FileMeta, encode};
    use crate::page::{BLACK_PAPER, PageGeometry};

    fn scan_geometry() -> PageGeometry {
        PageGeometry {
            ppix: 600,
            ppiy: 600,
            dpi: 200,
            dot_percent: 70,
            // 1500x1500 is bigger than NHYST so estimate_intensity
            // can sample its full window; it also fits a sensible
            // number of cells (14x14 = 196) for round-trip testing.
            width: 1500,
            height: 1500,
            print_border: true,
        }
    }

    fn meta() -> FileMeta<'static> {
        FileMeta {
            name: "scan-test.bin",
            modified: 0,
            attributes: 0x80,
        }
    }

    fn encode_payload(payload: &[u8]) -> (Vec<u8>, u32, u32) {
        let opts = EncodeOptions {
            geometry: scan_geometry(),
            redundancy: 5,
            compress: false,
            black: BLACK_PAPER,
        };
        let pages = encode(payload, &opts, &meta()).unwrap();
        let bitmap = pages[0].bitmap.clone();
        (bitmap, pages[0].width, pages[0].height)
    }

    /// Apply a small affine rotation to a bitmap by sampling the
    /// rotated source coordinates with bilinear interpolation. The
    /// rotation is around the image center; `theta` in radians,
    /// positive rotates counter-clockwise. Edges fill with white
    /// (255) — same convention as a real scanner outside the page.
    fn rotate_bitmap(bitmap: &[u8], width: u32, height: u32, theta: f32) -> Vec<u8> {
        let w = width as usize;
        let h = height as usize;
        let cx = width as f32 / 2.0;
        let cy = height as f32 / 2.0;
        let cos_t = theta.cos();
        let sin_t = theta.sin();
        let mut out = vec![255u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                // Inverse rotation: where in the source did this
                // output pixel come from?
                let sx = dx * cos_t + dy * sin_t + cx;
                let sy = -dx * sin_t + dy * cos_t + cy;
                out[y * w + x] = sample_bilinear(bitmap, w, h, sx, sy);
            }
        }
        out
    }

    /// Apply small additive noise to a bitmap. Uses a deterministic
    /// LCG (no `rand` dep) so test failures are reproducible.
    fn noisy_bitmap(bitmap: &[u8], amplitude: i16, seed: u64) -> Vec<u8> {
        let mut state = seed;
        bitmap
            .iter()
            .map(|&p| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let r = ((state >> 33) as i64 as i32) % (i32::from(amplitude * 2 + 1));
                let r = r - i32::from(amplitude);
                let v = i32::from(p) + r;
                v.clamp(0, 255) as u8
            })
            .collect()
    }

    /// Angle finders return a near-zero angle for an unrotated
    /// encoder bitmap, since the grid is axis-aligned by construction.
    #[test]
    fn angle_finders_detect_zero_on_unrotated_bitmap() {
        let payload: Vec<u8> = (0..500u32).map(|i| (i * 7) as u8).collect();
        let (bitmap, w, h) = encode_payload(&payload);
        let bounds = find_grid_position(&bitmap, w, h).unwrap();
        let intensity = estimate_intensity(&bitmap, w, h, &bounds).unwrap();
        let xa = find_x_angle(&bitmap, w, h, &intensity).unwrap();
        let ya = find_y_angle(&bitmap, w, h, &intensity, &xa).unwrap();
        // |angle| should be small (a few NHYST units / NHYST = a fraction
        // of a degree). Allow up to ±0.02 rad ~= ±1.1°.
        assert!(xa.angle.abs() < 0.02, "x angle {} not near 0", xa.angle);
        assert!(ya.angle.abs() < 0.02, "y angle {} not near 0", ya.angle);
        // Step should equal (NDOT+3) * dx — i.e., one cell pitch.
        // For 600 DPI / 200 dpi, dx = 3, so step = 35*3 = 105.
        assert!(
            (xa.step - 105.0).abs() < 5.0,
            "x step {} not near 105",
            xa.step
        );
        assert!(
            (ya.step - 105.0).abs() < 5.0,
            "y step {} not near 105",
            ya.step
        );
    }

    /// scan_decode round-trips an encoder-produced bitmap. This
    /// exercises the full peak-finder → angle-finder → block-sampler
    /// pipeline end-to-end, even though the unrotated case is the
    /// happy path.
    #[test]
    fn scan_decode_round_trip_unrotated() {
        let payload: Vec<u8> = (0..500u32).map(|i| (i * 31) as u8).collect();
        let (bitmap, w, h) = encode_payload(&payload);
        let recovered = scan_decode(&[(&bitmap, w, h)], None).unwrap();
        assert_eq!(recovered, payload);
    }

    /// Round-trip with a small rotation (~0.57°). The angle finders
    /// detect the rotation, and the 9-shift sampling sweep finds
    /// the integer-pixel offset that reads each dot correctly. RS
    /// then corrects the handful of bit errors from sub-pixel
    /// sampling residual.
    #[test]
    fn scan_decode_round_trip_with_small_rotation() {
        let payload: Vec<u8> = (0..500u32).map(|i| (i * 31) as u8).collect();
        let (bitmap, w, h) = encode_payload(&payload);
        let rotated = rotate_bitmap(&bitmap, w, h, 0.01);
        let recovered = scan_decode(&[(&rotated, w, h)], None).unwrap();
        assert_eq!(recovered, payload);
    }

    /// Round-trip with low-amplitude noise. CRC + ECC will catch
    /// any flipped dots and drop the affected blocks; recovery
    /// blocks fill them in. Amplitude is small enough that the
    /// 0/255 black/white separation survives.
    #[test]
    fn scan_decode_round_trip_with_noise() {
        let payload: Vec<u8> = (0..500u32).map(|i| (i * 31) as u8).collect();
        let (bitmap, w, h) = encode_payload(&payload);
        let noisy = noisy_bitmap(&bitmap, 30, 0xAA55_DEAD_BEEF_F00D);
        let recovered = scan_decode(&[(&noisy, w, h)], None).unwrap();
        assert_eq!(recovered, payload);
    }

    /// Pipeline integration smoke-test: run all three primitives
    /// against an actual encoder bitmap. Bounds should bracket the
    /// data area and intensity should reflect black-ink dots.
    #[test]
    fn pipeline_runs_on_encoder_output() {
        use crate::encoder::{EncodeOptions, FileMeta, encode};
        use crate::page::{BLACK_PAPER, PageGeometry};

        let geometry = PageGeometry {
            ppix: 600,
            ppiy: 600,
            dpi: 200,
            dot_percent: 70,
            // Make the page bigger than NHYST in each axis so the
            // intensity estimator can sample a full window.
            width: 1500,
            height: 1500,
            print_border: true,
        };
        let opts = EncodeOptions {
            geometry,
            redundancy: 5,
            compress: false,
            black: BLACK_PAPER,
        };
        let meta = FileMeta {
            name: "scan-test.bin",
            modified: 0,
            attributes: 0x80,
        };
        let payload: Vec<u8> = (0..1000u32).map(|i| (i * 31) as u8).collect();
        let pages = encode(&payload, &opts, &meta).unwrap();
        let bitmap = &pages[0].bitmap;
        let width = pages[0].width;
        let height = pages[0].height;

        let bounds = find_grid_position(bitmap, width, height).expect("grid bounds");
        // Bounds should be within the bitmap.
        assert!(bounds.xmax > bounds.xmin);
        assert!(bounds.ymax > bounds.ymin);
        assert!(bounds.xmax <= width);
        assert!(bounds.ymax <= height);

        let intensity = estimate_intensity(bitmap, width, height, &bounds).expect("intensity");
        // Black ink + white paper should give a wide range.
        assert!(intensity.cmax > intensity.cmin + 100);
        // Sharp synthetic edges should drive sharpfactor low.
        assert!(intensity.sharpfactor < 1.0);
    }
}
