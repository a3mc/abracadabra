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
use crate::model::slot::SlotRecord;
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

    /// Median observed inter-slot duration (microseconds).
    pub slot_duration_p50_us: i64,
    /// p95 observed inter-slot duration (microseconds).
    pub slot_duration_p95_us: i64,

    pub fast_finalize_pct: f64,
    pub vote_skip_rate_pct: f64,
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
    // Collect slot records whose first_shred_at lies in [start, end).
    let in_window: Vec<&SlotRecord> = state
        .slots
        .values()
        .filter(|r| r.first_shred_at.is_some_and(|t| t >= start && t < end))
        .collect();

    let slot_count = in_window.len() as u64;
    let secs = duration.as_seconds_f64().max(1.0);
    #[allow(clippy::cast_precision_loss)]
    let slot_rate_per_sec = slot_count as f64 / secs;

    // Inter-slot duration: pair consecutive slots within the window.
    let mut durations: Vec<i64> = in_window
        .windows(2)
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

    // Vote-resume times: collect from TCL-bearing slots inside window where
    // we have a next Voting notarize.
    let mut resumes: Vec<i64> = Vec::new();
    for r in &in_window {
        let Some(tcl_at) = r.timeout_crashed_leader_at else {
            continue;
        };
        let Some((_, next_vn_at)) = state
            .slots
            .range(r.slot.saturating_add(1)..)
            .find_map(|(_, cand)| cand.voted_notarize_at.map(|ts| (cand.slot, ts)))
        else {
            continue;
        };
        let us = (next_vn_at - tcl_at).whole_microseconds();
        #[allow(clippy::cast_possible_truncation)]
        let us_i64 = us as i64;
        if us_i64 >= 0 {
            resumes.push(us_i64);
        }
    }
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
