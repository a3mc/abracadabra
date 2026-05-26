//! Reusable TUI widgets above what ratatui ships with.

pub mod hbar;
pub mod kpi;

/// Resample `data` so it has exactly `width` points. Used to stretch
/// sparklines across the full width of their panel — ratatui's `Sparkline`
/// draws one column per input point, so a 100-bucket series in a 200-wide
/// panel only fills half the area without this.
///
/// Uses nearest-neighbour sampling: simple, no smoothing, preserves peaks.
#[must_use]
pub fn fit_to_width(data: &[u64], width: usize) -> Vec<u64> {
    if data.is_empty() || width == 0 {
        return Vec::new();
    }
    (0..width).map(|i| data[i * data.len() / width]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_empty_data() {
        assert!(fit_to_width(&[], 10).is_empty());
    }

    #[test]
    fn fit_zero_width() {
        assert!(fit_to_width(&[1, 2, 3], 0).is_empty());
    }

    #[test]
    fn fit_upsample_repeats_buckets() {
        // 3 input points -> 9 output points, each input repeated 3 times.
        let out = fit_to_width(&[10, 20, 30], 9);
        assert_eq!(out, vec![10, 10, 10, 20, 20, 20, 30, 30, 30]);
    }

    #[test]
    fn fit_downsample_picks_evenly() {
        let out = fit_to_width(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], 5);
        assert_eq!(out, vec![0, 2, 4, 6, 8]);
    }

    #[test]
    fn fit_identity_when_equal() {
        let out = fit_to_width(&[1, 2, 3, 4], 4);
        assert_eq!(out, vec![1, 2, 3, 4]);
    }
}
