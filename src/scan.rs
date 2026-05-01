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
