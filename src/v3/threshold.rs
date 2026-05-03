// Global thresholding for the Phase 2.5c noise-tolerant parse path.
// Replaces the fixed `< 128` check earlier slices used — real
// scanner output rarely has the bimodal histogram synthetic
// bitmaps do, and a fixed midpoint doesn't survive paper fade or
// scanner gamma drift.
//
// Phase 2.5c first slice ships GLOBAL Otsu — one threshold per
// page bitmap. A future slice may upgrade to per-region Otsu /
// Sauvola when that turns out to matter for some real scan
// profiles, but global Otsu is the standard document-binarization
// baseline (used by Tesseract, ZXing, libdmtx, etc.) and a
// dramatic improvement over fixed-128 on its own.

/// Otsu's method (1979): pick the grayscale threshold that
/// maximizes inter-class variance for the bitmap's pixel
/// histogram.
///
/// Returns the threshold byte. Pixels with value `< threshold`
/// are treated as black; `>= threshold` as white. The convention
/// matches the rest of the v3 parse path.
///
/// Edge cases:
///   - Empty input returns 128 — a safe fallback that wouldn't
///     change behavior for callers that previously hardcoded 128.
///   - All-same-value input (e.g., all-white blank bitmaps) makes
///     every candidate threshold produce zero between-class
///     variance; the algorithm leaves the default 128 in place,
///     which causes the caller to see "no black pixels" and
///     bail out via the existing finder-detection error path
///     rather than silently misbehaving.
#[must_use]
pub fn otsu_threshold(pixels: &[u8]) -> u8 {
    if pixels.is_empty() {
        return 128;
    }

    let mut hist = [0u32; 256];
    for &p in pixels {
        hist[p as usize] += 1;
    }

    let total = pixels.len() as f64;
    let total_sum: f64 = hist
        .iter()
        .enumerate()
        .map(|(i, &c)| (i as f64) * (c as f64))
        .sum();

    let mut sum_b = 0.0f64;
    let mut w_b = 0.0f64;
    let mut max_var = 0.0f64;
    // Optional: only set when at least one t produces non-zero
    // between-class variance. For all-same-value input no t
    // qualifies, so we fall back to 128 (matching the empty-input
    // fallback and the previous fixed-threshold behavior).
    let mut best_t: Option<u8> = None;

    for (t, &count) in hist.iter().enumerate() {
        w_b += count as f64;
        if w_b == 0.0 {
            continue;
        }
        let w_f = total - w_b;
        if w_f <= 0.0 {
            break;
        }
        sum_b += (t as f64) * (count as f64);
        let m_b = sum_b / w_b;
        let m_f = (total_sum - sum_b) / w_f;
        let between_var = w_b * w_f * (m_b - m_f) * (m_b - m_f);
        if between_var > max_var {
            max_var = between_var;
            best_t = Some(t as u8);
        }
    }

    // Standard Otsu picks `t` such that pixels with value ≤ t are
    // class 0 (black). The rest of the v3 parse path uses `pixel
    // < threshold` for "black", so we shift by +1 to align the
    // conventions: a class-0 pixel of value `t` becomes black via
    // `t < t+1`. Saturating add caps at 255 (degenerate but
    // harmless — would mean every pixel is black, which is what
    // the algorithm actually decided).
    best_t
        .map(|t| t.saturating_add(1))
        .unwrap_or(128)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_default_for_empty_input() {
        assert_eq!(otsu_threshold(&[]), 128);
    }

    #[test]
    fn returns_default_for_all_same_value() {
        let all_white = [255u8; 1000];
        // No bimodal split exists; threshold defaults to 128.
        assert_eq!(otsu_threshold(&all_white), 128);
        let all_black = [0u8; 1000];
        assert_eq!(otsu_threshold(&all_black), 128);
    }

    #[test]
    fn picks_separator_for_clean_bimodal() {
        // Half black (0), half white (255). The optimal threshold
        // sits somewhere strictly between 0 and 255 — Otsu's
        // standard result for this distribution puts it at the
        // first non-empty bin above 0 (= 1) since that maximizes
        // between-class variance. Not the human midpoint, but
        // correct by Otsu's definition.
        let pixels: Vec<u8> = (0..1000).map(|i| if i < 500 { 0 } else { 255 }).collect();
        let t = otsu_threshold(&pixels);
        assert!(t > 0 && t < 255, "threshold should be strictly between 0 and 255, got {t}");
    }

    #[test]
    fn picks_separator_for_faded_bimodal() {
        // "Faded paper" simulation: black=100, white=200. Fixed-
        // 128 still works (200 > 128 > 100), but Otsu picks
        // somewhere between 100 and 200 — proves Otsu adapts.
        let pixels: Vec<u8> = (0..1000).map(|i| if i < 500 { 100 } else { 200 }).collect();
        let t = otsu_threshold(&pixels);
        assert!(t > 100 && t <= 200, "threshold should be between 100 and 200, got {t}");
    }

    #[test]
    fn picks_separator_for_doubly_faded_bimodal() {
        // Both classes ABOVE 128: black=140, white=200. Fixed-128
        // would treat all pixels as white and lose all data —
        // this is the failure mode Otsu is here to fix.
        let pixels: Vec<u8> = (0..1000).map(|i| if i < 500 { 140 } else { 200 }).collect();
        let t = otsu_threshold(&pixels);
        assert!(
            t > 140 && t <= 200,
            "threshold {t} should sit between 140 and 200 to separate the two classes"
        );
    }

    #[test]
    fn picks_separator_for_unbalanced_histogram() {
        // 90% white, 10% black — typical document ratio. Threshold
        // should still separate the two classes.
        let mut pixels = vec![230u8; 900];
        pixels.extend(vec![30u8; 100]);
        let t = otsu_threshold(&pixels);
        assert!(t > 30 && t < 230, "threshold {t} not in range [30, 230]");
    }
}
