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
