//! Stage-percentile types and helpers for the chain pane's timing
//! table.
//!
//! Definitions match [`crate::model::analysis::LatencyStages`] exactly
//! so the live numbers are directly comparable to the Windows-tab
//! snapshot.

use time::OffsetDateTime;

/// Result of [`super::state::ChainPane::timing_table`]: p50/p95 (ms)
/// for each stage-delta family. `None` if no samples retained.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct TimingTable {
    pub(super) cluster: StagePercentiles,
    pub(super) assembly: StagePercentiles,
    pub(super) consensus: StagePercentiles,
    pub(super) lifecycle: StagePercentiles,
}

/// `(p50_ms, p95_ms)` from a stage-sample slice.
pub(super) type StagePercentiles = Option<(i64, i64)>;

/// Whole-microsecond delta `end - start` when both timestamps are
/// present and the delta is non-negative. Used to harvest stage
/// samples from per-slot timing fields.
pub(super) fn stage_delta_us(
    start: Option<OffsetDateTime>,
    end: Option<OffsetDateTime>,
) -> Option<i64> {
    let (s, e) = (start?, end?);
    let raw = e - s;
    if raw.is_negative() {
        return None;
    }
    i64::try_from(raw.whole_microseconds()).ok()
}

/// Sort `samples` in place and return `(p50_ms, p95_ms)` derived from
/// integer positional percentiles. Inputs are microseconds; output is
/// milliseconds. `None` when the input is empty.
///
/// Caller must guarantee `samples.len() <= HISTORY_CAPACITY` (512) —
/// the `n as f64` cast assumes a small N so no truncation occurs.
pub(super) fn percentiles_ms(samples: &mut [i64]) -> StagePercentiles {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let pick = |frac: f64| -> i64 {
        let n = samples.len();
        // Invariant: `n <= HISTORY_CAPACITY` (512), so `n as f64` is
        // exact and the `f64 → usize` cast cannot truncate.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let idx = ((frac * n as f64).ceil() as usize)
            .saturating_sub(1)
            .min(n - 1);
        samples[idx] / 1000
    };
    Some((pick(0.50), pick(0.95)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    fn ts(ms: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::milliseconds(ms)
    }

    #[test]
    fn stage_delta_us_returns_none_when_either_endpoint_missing() {
        // Both start and end are required; missing either is treated
        // as "no sample" rather than zero, so empty stages stay
        // empty in the percentile table.
        assert_eq!(stage_delta_us(None, Some(ts(10))), None);
        assert_eq!(stage_delta_us(Some(ts(10)), None), None);
        assert_eq!(stage_delta_us(None, None), None);
    }

    #[test]
    fn stage_delta_us_returns_non_negative_microsecond_delta() {
        // 10 ms apart → 10_000 µs.
        let delta = stage_delta_us(Some(ts(0)), Some(ts(10))).expect("ordered pair yields delta");
        assert_eq!(delta, 10_000);
    }

    #[test]
    fn stage_delta_us_filters_negative_deltas() {
        // Out-of-order endpoints (end before start) must be filtered
        // rather than producing a negative microsecond reading —
        // negative samples would poison the percentile picker.
        assert_eq!(stage_delta_us(Some(ts(50)), Some(ts(10))), None);
    }

    #[test]
    fn percentiles_ms_returns_none_on_empty_input() {
        let mut empty: [i64; 0] = [];
        assert_eq!(percentiles_ms(&mut empty), None);
    }

    #[test]
    fn percentiles_ms_collapses_to_single_sample_for_p50_and_p95() {
        // With a single sample, p50 == p95 == that sample (in ms).
        // Microsecond input → millisecond output via integer divide.
        let mut samples = [12_345i64];
        let (p50, p95) = percentiles_ms(&mut samples).expect("single sample yields percentiles");
        assert_eq!(p50, 12, "12_345 µs / 1000 = 12 ms (truncated)");
        assert_eq!(p95, 12);
    }

    #[test]
    fn percentiles_ms_truncates_microseconds_to_milliseconds() {
        // 999 µs truncates to 0 ms; 1_999 µs to 1 ms. Integer divide
        // is the documented behaviour — no rounding.
        let mut samples = [999i64, 1_999i64];
        let (p50, p95) = percentiles_ms(&mut samples).expect("two samples yield percentiles");
        // p50: idx = ceil(0.50 * 2) - 1 = 0 → 999 µs / 1000 = 0 ms.
        assert_eq!(p50, 0, "p50 must truncate to 0 ms on 999 µs sample");
        // p95: idx = ceil(0.95 * 2) - 1 = 1 → 1_999 µs / 1000 = 1 ms.
        assert_eq!(p95, 1, "p95 must truncate to 1 ms on 1_999 µs sample");
    }

    #[test]
    fn percentiles_ms_sorts_in_place_before_picking() {
        // Unsorted input must be sorted by the helper — the picker
        // assumes the slice is sorted ascending.
        let mut samples = [5_000i64, 1_000, 3_000, 4_000, 2_000];
        let (p50, p95) =
            percentiles_ms(&mut samples).expect("five-sample input yields percentiles");
        // After sort: [1_000, 2_000, 3_000, 4_000, 5_000].
        // p50 idx = ceil(0.50 * 5) - 1 = 2 → 3_000 µs → 3 ms.
        assert_eq!(p50, 3);
        // p95 idx = ceil(0.95 * 5) - 1 = 4 → 5_000 µs → 5 ms.
        assert_eq!(p95, 5);
        // Side effect anchor: input slice is now sorted.
        assert_eq!(samples, [1_000, 2_000, 3_000, 4_000, 5_000]);
    }
}
