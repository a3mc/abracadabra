//! Rolling-window comparison statistics.
//!
//! For a static log we anchor windows at `time_range.1` (end of log) and
//! compute the same statistics over `last 1h`, `last 3h`, ... plus `all`.
//! In live-tail mode the anchor would be "now"; the rest of the logic is
//! unchanged.
//!
//! Each `WindowStats` is computed by iterating the slots whose
//! `first_shred_at` falls inside `[start, end)`. We deliberately gate on
//! `first_shred_at` because that's our best per-slot anchor (see
//! `docs/alpenglow/log-strings-reference.md` and the slot-time discussion
//! in `model/slot.rs`).

use time::{Duration, OffsetDateTime};

use crate::model::analysis;
use crate::model::slot::{SkipClassification, SlotRecord};
use crate::model::state::State;

/// Per-window statistics. Times in microseconds where applicable so we can
/// keep integer math; the renderer converts to ms / s for display.
#[derive(Debug, Clone)]
pub struct WindowStats {
    pub label: &'static str,
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
    pub duration: Duration,

    pub slot_count: u64,
    pub slot_rate_per_sec: f64,

    /// Median observed inter-slot duration (microseconds). Counted over
    /// strictly adjacent slot pairs `(n, n+1)` whose `first_shred_at`
    /// timestamps both fall inside the window. Pairs separated by a gap in
    /// the slot stream (skips, timeouts, missing observations) are
    /// excluded.
    pub slot_duration_p50_us: i64,
    /// p95 of the same strictly-adjacent inter-slot duration distribution.
    pub slot_duration_p95_us: i64,

    pub fast_finalize_pct: f64,
    pub vote_skip_rate_pct: f64,
    /// Total vote-skips in window (denominator for canonical-skip %).
    pub vote_skips: u64,
    /// Subset of `vote_skips` proven to have landed on canonical slots
    /// by the Stage 1 classifier (direct Finalized observation or
    /// ancestry walk). The operator-facing failure indicator.
    pub canonical_skips: u64,
    /// Vote-skips with no log-only evidence of cluster outcome. Used
    /// to render the canonical-skip percentage as a lower bound
    /// (`≥ X%`) when non-zero — same convention as the headline strip.
    pub indeterminate_skips: u64,
    /// `canonical_skips / vote_skips * 100`. Zero when `vote_skips`
    /// is zero. The `indeterminate_skips > 0` case turns this into a
    /// lower bound at the renderer.
    pub canonical_skip_pct: f64,
    pub crashed_leaders: u64,
    pub fragmentation: u64,

    /// Assembly (first_shred -> block_emitted) percentiles, microseconds.
    pub assembly_p50_us: i64,
    pub assembly_p95_us: i64,
    /// Consensus (block_emitted -> finalized) percentiles, microseconds.
    pub consensus_p50_us: i64,
    pub consensus_p95_us: i64,
    /// Lifecycle latency (first shred -> finalized) percentiles, microseconds.
    pub lifecycle_p50_us: i64,
    pub lifecycle_p95_us: i64,
    pub lifecycle_p99_us: i64,

    /// Vote-resume time (TCL -> next Voting notarize) percentiles, microseconds.
    pub resume_p50_us: i64,
    pub resume_p95_us: i64,
    pub resume_p99_us: i64,
}

/// Compute window stats for each entry in `windows`, anchored at the end
/// of the observed log. `all` is computed separately as the full-log
/// baseline. Returns the list in display order with `all` first.
pub fn compute(state: &State, windows: &[Duration]) -> Vec<WindowStats> {
    let Some((lo, hi)) = state.file_meta.time_range else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(windows.len() + 1);
    out.push(compute_one(state, lo, hi, "all", hi - lo));
    for w in windows {
        let start = (hi - *w).max(lo);
        out.push(compute_one(state, start, hi, label_for(*w), *w));
    }
    out
}

fn compute_one(
    state: &State,
    start: OffsetDateTime,
    end: OffsetDateTime,
    label: &'static str,
    duration: Duration,
) -> WindowStats {
    // Anchor each slot to the FIRST available timestamp from this
    // priority chain:
    //
    //   first_shred_at → voted_notarize_at → voted_skip_at →
    //   timeout_crashed_leader_at
    //
    // Using `first_shred_at` alone (the previous behaviour) silently
    // dropped skip-cascade slots — slots where TCL fired and
    // `try_skip_window` cast Skip without our node ever observing a
    // shred. Those slots have `first_shred_at == None` but are real
    // log activity (we cast Skip for them), so the vote-skip and
    // canonical-skip counts went off by the cascade-slot count and
    // the windowed `vote-skip rate %` no longer matched the headline.
    //
    // The same fall-back chain is used by `model/buckets.rs` for the
    // time-series tab; keeping them aligned ensures the two views
    // disagree about nothing.
    let anchor = |r: &SlotRecord| -> Option<OffsetDateTime> {
        r.first_shred_at
            .or(r.voted_notarize_at)
            .or(r.voted_skip_at)
            .or(r.timeout_crashed_leader_at)
    };
    let in_window: Vec<&SlotRecord> = state
        .slots
        .values()
        .filter(|r| anchor(r).is_some_and(|t| t >= start && t < end))
        .collect();

    let slot_count = in_window.len() as u64;
    let secs = duration.as_seconds_f64().max(1.0);
    #[allow(clippy::cast_precision_loss)]
    let slot_rate_per_sec = slot_count as f64 / secs;

    // Inter-slot duration: pair *strictly adjacent* slots (n, n+1) within the
    // window. Pairs separated by holes in the BTreeMap (timeouts, skipped
    // slots, missing observations) are excluded so p95 reflects single-slot
    // durations rather than `k × ideal_slot_time` for some `k > 1`.
    let mut durations: Vec<i64> = in_window
        .windows(2)
        .filter(|pair| pair[1].slot == pair[0].slot.saturating_add(1))
        .filter_map(|pair| SlotRecord::slot_duration_us(pair[0], pair[1]))
        .collect();
    durations.sort_unstable();

    // Lifecycle latency.
    let mut lifecycle: Vec<i64> = in_window
        .iter()
        .filter_map(|r| SlotRecord::delta_us(r.first_shred_at, r.finalized_at))
        .collect();
    lifecycle.sort_unstable();
    let mut assembly: Vec<i64> = in_window
        .iter()
        .filter_map(|r| SlotRecord::delta_us(r.first_shred_at, r.block_emitted_at))
        .collect();
    assembly.sort_unstable();
    let mut consensus: Vec<i64> = in_window
        .iter()
        .filter_map(|r| SlotRecord::delta_us(r.block_emitted_at, r.finalized_at))
        .collect();
    consensus.sort_unstable();

    // Status counts inside window.
    let mut fast = 0u64;
    let mut slow = 0u64;
    let mut skip = 0u64;
    let mut canonical = 0u64;
    let mut indeterminate = 0u64;
    let mut crashed = 0u64;
    let mut s2n = 0u64;
    let mut s2s = 0u64;
    for r in &in_window {
        match r.fast_finalize {
            Some(true) => fast += 1,
            Some(false) => slow += 1,
            None => {}
        }
        if r.voted_skip_at.is_some() {
            skip += 1;
            // Stage 1 classifier (populated by aggregator::classify_skips
            // before window stats are computed) partitions skip-voted
            // slots into canonical / indeterminate / right-skip (the
            // last only appears once Stage 2 RPC enrichment lands).
            match r.skip_classification {
                SkipClassification::CanonicalSkip(_) => canonical += 1,
                SkipClassification::Indeterminate => indeterminate += 1,
                SkipClassification::NotSkipped => {}
            }
        }
        if r.timeout_crashed_leader_at.is_some() {
            crashed += 1;
        }
        if r.safe_to_notar_at.is_some() {
            s2n += 1;
        }
        if r.safe_to_skip_at.is_some() {
            s2s += 1;
        }
    }
    let total_fin = fast.saturating_add(slow);
    let fast_finalize_pct = if total_fin > 0 {
        fast as f64 * 100.0 / total_fin as f64
    } else {
        0.0
    };
    let vote_skip_rate_pct = if slot_count > 0 {
        skip as f64 * 100.0 / slot_count as f64
    } else {
        0.0
    };
    let canonical_skip_pct = if skip > 0 {
        canonical as f64 * 100.0 / skip as f64
    } else {
        0.0
    };

    // Vote-resume times: reuse the canonical `analysis::vote_resumes_after_tcl`
    // computation and keep only events whose TCL slot is part of this window.
    // Inverted pairs are already dropped upstream by `SlotRecord::delta_us`.
    let in_window_slots: std::collections::BTreeSet<u64> =
        in_window.iter().map(|r| r.slot).collect();
    let mut resumes: Vec<i64> = analysis::vote_resumes_after_tcl(state)
        .into_iter()
        .filter(|r| in_window_slots.contains(&r.tcl_slot))
        .map(|r| r.resume_us)
        .collect();
    resumes.sort_unstable();

    WindowStats {
        label,
        start,
        end,
        duration,
        slot_count,
        slot_rate_per_sec,
        slot_duration_p50_us: analysis::percentile(&durations, 0.50).unwrap_or(0),
        slot_duration_p95_us: analysis::percentile(&durations, 0.95).unwrap_or(0),
        fast_finalize_pct,
        vote_skip_rate_pct,
        vote_skips: skip,
        canonical_skips: canonical,
        indeterminate_skips: indeterminate,
        canonical_skip_pct,
        crashed_leaders: crashed,
        fragmentation: s2n.saturating_add(s2s),
        assembly_p50_us: analysis::percentile(&assembly, 0.50).unwrap_or(0),
        assembly_p95_us: analysis::percentile(&assembly, 0.95).unwrap_or(0),
        consensus_p50_us: analysis::percentile(&consensus, 0.50).unwrap_or(0),
        consensus_p95_us: analysis::percentile(&consensus, 0.95).unwrap_or(0),
        lifecycle_p50_us: analysis::percentile(&lifecycle, 0.50).unwrap_or(0),
        lifecycle_p95_us: analysis::percentile(&lifecycle, 0.95).unwrap_or(0),
        lifecycle_p99_us: analysis::percentile(&lifecycle, 0.99).unwrap_or(0),
        resume_p50_us: analysis::percentile(&resumes, 0.50).unwrap_or(0),
        resume_p95_us: analysis::percentile(&resumes, 0.95).unwrap_or(0),
        resume_p99_us: analysis::percentile(&resumes, 0.99).unwrap_or(0),
    }
}

const fn label_for(d: Duration) -> &'static str {
    let secs = d.whole_seconds();
    if secs <= 60 * 60 {
        "1h"
    } else if secs <= 3 * 60 * 60 {
        "3h"
    } else if secs <= 6 * 60 * 60 {
        "6h"
    } else if secs <= 12 * 60 * 60 {
        "12h"
    } else {
        "24h"
    }
}

/// The default window set used by the UI: 1h / 3h / 6h / 12h / 24h plus
/// `all` (added automatically by `compute`).
#[must_use]
pub fn default_windows() -> Vec<Duration> {
    vec![
        Duration::hours(24),
        Duration::hours(12),
        Duration::hours(6),
        Duration::hours(3),
        Duration::hours(1),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use time::macros::datetime;

    fn mk(state_start: OffsetDateTime, state_end: OffsetDateTime) -> State {
        let mut s = State::new(PathBuf::from("/tmp/x"), 0);
        s.observe_ts(state_start);
        s.observe_ts(state_end);
        s
    }

    #[test]
    fn empty_state_yields_empty_vec() {
        let s = State::new(PathBuf::from("/tmp/x"), 0);
        assert!(compute(&s, &default_windows()).is_empty());
    }

    #[test]
    fn single_slot_window_has_zero_duration_pcts() {
        // One in-window slot → no pairs → percentile returns None → fallback 0.
        let lo = datetime!(2026-05-23 00:00:00 UTC);
        let hi = datetime!(2026-05-23 01:00:00 UTC);
        let mut s = mk(lo, hi);
        let r = s.slot_mut(1);
        r.first_shred_at = Some(datetime!(2026-05-23 00:30:00 UTC));
        let stats = compute(&s, &default_windows());
        assert_eq!(stats[0].slot_duration_p50_us, 0);
        assert_eq!(stats[0].slot_duration_p95_us, 0);
    }

    #[test]
    fn slot_duration_excludes_non_adjacent_pairs() {
        // Three observed slots: 1, 2, 100. Only (1, 2) is strictly adjacent;
        // the (2, 100) pair has a 98-slot hole and must be excluded.
        let lo = datetime!(2026-05-23 00:00:00 UTC);
        let hi = datetime!(2026-05-23 01:00:00 UTC);
        let mut s = mk(lo, hi);
        s.slot_mut(1).first_shred_at = Some(datetime!(2026-05-23 00:30:00.000 UTC));
        s.slot_mut(2).first_shred_at = Some(datetime!(2026-05-23 00:30:00.400 UTC));
        s.slot_mut(100).first_shred_at = Some(datetime!(2026-05-23 00:30:45.000 UTC));
        let stats = compute(&s, &default_windows());
        // p95 over the one-element distribution {400_000 µs} == 400_000.
        assert_eq!(stats[0].slot_duration_p50_us, 400_000);
        assert_eq!(stats[0].slot_duration_p95_us, 400_000);
    }

    #[test]
    fn vote_resume_in_window_uses_canonical_path() {
        // TCL at slot 100, voted_notarize at slot 104, both inside window.
        let lo = datetime!(2026-05-23 00:00:00 UTC);
        let hi = datetime!(2026-05-23 02:00:00 UTC);
        let mut s = mk(lo, hi);
        {
            let r = s.slot_mut(100);
            r.first_shred_at = Some(datetime!(2026-05-23 01:00:00.000 UTC));
            r.timeout_crashed_leader_at = Some(datetime!(2026-05-23 01:00:00.000 UTC));
        }
        {
            let r = s.slot_mut(104);
            r.first_shred_at = Some(datetime!(2026-05-23 01:00:01.500 UTC));
            r.voted_notarize_at = Some(datetime!(2026-05-23 01:00:01.500 UTC));
        }
        let stats = compute(&s, &default_windows());
        assert_eq!(stats[0].resume_p50_us, 1_500_000);
    }

    #[test]
    fn slots_outside_window_excluded() {
        let lo = datetime!(2026-05-23 00:00:00 UTC);
        let hi = datetime!(2026-05-23 02:00:00 UTC);
        let mut s = mk(lo, hi);
        // Slot A at 00:30 (in 1h window? hi - 1h = 01:00, so A is OUT)
        {
            let r = s.slot_mut(1);
            r.first_shred_at = Some(datetime!(2026-05-23 00:30:00 UTC));
            r.fast_finalize = Some(true);
        }
        // Slot B at 01:30 (in 1h window? yes, > 01:00)
        {
            let r = s.slot_mut(2);
            r.first_shred_at = Some(datetime!(2026-05-23 01:30:00 UTC));
            r.fast_finalize = Some(true);
        }
        let windows = compute(&s, &[Duration::hours(1)]);
        // First is "all" (both slots), second is "1h" (one slot).
        assert_eq!(windows[0].slot_count, 2);
        assert_eq!(windows[1].slot_count, 1);
    }
}
