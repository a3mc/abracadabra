//! Pre-computed per-row data for the Slots and Recoveries tables.
//!
//! Computing these once at App startup keeps the per-frame render cheap
//! (`Table` only needs the visible slice, but we still want to avoid
//! recomputing latencies on every keystroke).

use crate::model::analysis::{self, LatencyStages, VoteResumeRecord};
use crate::model::slot::{SkipClassification, SlotRecord, SlotStatus};
use crate::model::state::State;

#[derive(Debug, Clone)]
pub struct SlotViewRow {
    pub slot: u64,
    pub status: SlotStatus,
    pub skip_classification: SkipClassification,
    pub fast: Option<bool>,
    pub we_are_leader: bool,
    /// `first_shred_at` → `block_emitted_at` (shred reception + replay).
    pub assembly_ms: Option<f64>,
    /// `block_emitted_at` → `finalized_at` (pure consensus rounds).
    pub consensus_ms: Option<f64>,
    /// `first_shred_at` → `finalized_at` (full lifecycle = assembly + consensus).
    pub lifecycle_ms: Option<f64>,
    pub voted_notarize: bool,
    pub voted_finalize: bool,
    pub voted_skip: bool,
    pub safe_to_notar: bool,
    pub safe_to_skip: bool,
    pub crashed_leader: bool,
}

impl SlotViewRow {
    pub fn from_record(r: &SlotRecord) -> Self {
        let to_ms = |us: i64| us as f64 / 1000.0;
        let assembly_ms = SlotRecord::delta_us(r.first_shred_at, r.block_emitted_at).map(to_ms);
        let consensus_ms = SlotRecord::delta_us(r.block_emitted_at, r.finalized_at).map(to_ms);
        let lifecycle_ms = SlotRecord::delta_us(r.first_shred_at, r.finalized_at).map(to_ms);
        Self {
            slot: r.slot,
            status: r.status(),
            skip_classification: r.skip_classification,
            fast: r.fast_finalize,
            we_are_leader: r.we_are_leader,
            assembly_ms,
            consensus_ms,
            lifecycle_ms,
            voted_notarize: r.voted_notarize_at.is_some(),
            voted_finalize: r.voted_finalize_at.is_some(),
            voted_skip: r.voted_skip_at.is_some(),
            safe_to_notar: r.safe_to_notar_at.is_some(),
            safe_to_skip: r.safe_to_skip_at.is_some(),
            crashed_leader: r.timeout_crashed_leader_at.is_some(),
        }
    }

    /// Vote-pattern string for the Slots table. Surfaces every present
    /// vote — Notarize, Finalize, Skip — concatenated with `+`. The
    /// `(N, _, S)` and `(N, F, S)` combinations are protocol-ambiguous
    /// (validator cast both a Notarize and a Skip on the same slot);
    /// the renderer keeps both flags visible rather than dropping Skip
    /// so the operator can spot the case. See the `mixed_votes` filter
    /// on `SlotFilters` for the matching tab-3 filter binding.
    pub const fn vote_pattern(&self) -> &'static str {
        match (self.voted_notarize, self.voted_finalize, self.voted_skip) {
            (true, true, true) => "N+F+S",
            (true, true, false) => "N+F",
            (true, false, true) => "N+S",
            (true, false, false) => "N",
            (false, _, true) => "S",
            _ => "-",
        }
    }

    /// Status pill string for the slot table.
    ///
    /// For skipped slots, the `SkipClassification` discriminator
    /// determines the operator-visible label:
    ///
    ///   - `BSKIP` — we voted skip on a slot the cluster reached
    ///     canonical agreement on (real participation failure).
    ///   - `SKIP`  — we voted skip; cluster outcome indeterminate from
    ///     log alone (could be a right skip OR an unverified bad skip).
    ///
    /// Until Stage 2 RPC enrichment lands, plain `SKIP` is the
    /// indeterminate bucket — NOT a claim of correctness. The legend
    /// must make that explicit.
    pub const fn status_str(&self) -> &'static str {
        match self.status {
            SlotStatus::FastFinalized | SlotStatus::SlowFinalized => "FIN",
            SlotStatus::Skipped => match self.skip_classification {
                SkipClassification::Bad(_) => "BSKIP",
                _ => "SKIP",
            },
            SlotStatus::Pending => "PEND",
        }
    }

    pub const fn fast_str(&self) -> &'static str {
        match (self.status, self.fast) {
            (SlotStatus::FastFinalized, _) | (_, Some(true)) => "F",
            (SlotStatus::SlowFinalized, _) | (_, Some(false)) => "s",
            _ => " ",
        }
    }
}

#[derive(Debug, Clone)]
pub struct VoteResumeViewRow {
    pub tcl_slot: u64,
    pub resume_slot: u64,
    pub resume_us: i64,
    pub slot_gap: u64,
}

impl VoteResumeViewRow {
    pub const fn from_record(r: VoteResumeRecord) -> Self {
        let slot_gap = r.resume_slot.saturating_sub(r.tcl_slot);
        Self {
            tcl_slot: r.tcl_slot,
            resume_slot: r.resume_slot,
            resume_us: r.resume_us,
            slot_gap,
        }
    }
}

/// Pre-computed latency vectors + percentiles. Built once in `App::new`
/// so per-frame renders don't re-run `analysis::lifecycle_latencies` /
/// `LatencyStages::compute` / `vote_resumes_after_tcl` (each of which
/// scans the full slot map and sorts O(n log n)).
///
/// All `Vec<i64>` fields are sorted ascending in microseconds so
/// `analysis::percentile` is a constant-time index.
#[derive(Debug, Clone, Default)]
pub struct LatencySnapshot {
    /// Per-stage latency vectors (sorted ascending, microseconds).
    /// `stages.lifecycle` is the source of truth for the lifecycle
    /// latency series — read it directly rather than carrying a clone.
    pub stages: LatencyStages,
    /// p50/p95/p99/max for `stages.lifecycle` in microseconds.
    pub lifecycle_pcts_us: (i64, i64, i64, i64),
    /// Vote-resume durations after `TimeoutCrashedLeader` (microseconds,
    /// ascending). Empty when no TCL events occurred in the log.
    pub resume_us_sorted: Vec<i64>,
    /// p50/p95/p99/max in microseconds for vote-resume times.
    pub resume_pcts_us: (i64, i64, i64, i64),
    /// `(normal, elevated, severe)` resume-severity counts.
    pub resume_severity_counts: (u64, u64, u64),
    /// Total number of vote-resume events recorded (matches `resume_us_sorted.len()`).
    pub resume_total: u64,
}

impl LatencySnapshot {
    /// Build a snapshot from the state and a precomputed
    /// `vote_resumes_after_tcl` result. Caller passes the resume vector
    /// in so the slice can be consumed exactly once and reused for the
    /// `resume_rows` view-model on `App` — avoids running the full
    /// TCL→next-notarize scan twice.
    pub fn compute(state: &State, resumes: &[VoteResumeRecord]) -> Self {
        let stages = LatencyStages::compute(state);
        let lifecycle_pcts_us = analysis::pcts(&stages.lifecycle);

        let resume_severity_counts = analysis::resume_severity_counts(resumes);
        let resume_total = resumes.len() as u64;
        let mut resume_us_sorted: Vec<i64> = resumes.iter().map(|r| r.resume_us).collect();
        resume_us_sorted.sort_unstable();
        let resume_pcts_us = analysis::pcts(&resume_us_sorted);

        Self {
            stages,
            lifecycle_pcts_us,
            resume_us_sorted,
            resume_pcts_us,
            resume_severity_counts,
            resume_total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::analysis as ana;
    use std::path::PathBuf;
    use time::macros::datetime;

    fn mk_state() -> crate::model::state::State {
        crate::model::state::State::new(PathBuf::from("/tmp/x"), 0)
    }

    #[test]
    fn snapshot_matches_panel_recompute() {
        // PERF-01 regression guard: the snapshot's pre-computed
        // percentiles and severity counts must match what panels
        // would derive by hand from the same source data.
        let mut s = mk_state();
        // Two finalized slots with full timeline.
        {
            let r = s.slot_mut(10);
            r.first_shred_at = Some(datetime!(2026-05-23 16:00:00.000 UTC));
            r.block_emitted_at = Some(datetime!(2026-05-23 16:00:00.300 UTC));
            r.finalized_at = Some(datetime!(2026-05-23 16:00:00.700 UTC));
            r.fast_finalize = Some(true);
        }
        {
            let r = s.slot_mut(11);
            r.first_shred_at = Some(datetime!(2026-05-23 16:00:01.000 UTC));
            r.block_emitted_at = Some(datetime!(2026-05-23 16:00:01.250 UTC));
            r.finalized_at = Some(datetime!(2026-05-23 16:00:01.500 UTC));
            r.fast_finalize = Some(false);
        }
        // One TCL with a downstream notarize 2 s later.
        {
            let r = s.slot_mut(100);
            r.timeout_crashed_leader_at = Some(datetime!(2026-05-23 16:01:00.000 UTC));
        }
        {
            let r = s.slot_mut(101);
            r.voted_notarize_at = Some(datetime!(2026-05-23 16:01:02.000 UTC));
        }

        let resumes = ana::vote_resumes_after_tcl(&s);
        let snap = LatencySnapshot::compute(&s, &resumes);

        // Lifecycle vector exact match.
        let stages_direct = ana::LatencyStages::compute(&s);
        assert_eq!(snap.stages.lifecycle, stages_direct.lifecycle);
        assert_eq!(snap.stages.assembly, stages_direct.assembly);
        assert_eq!(snap.stages.consensus, stages_direct.consensus);
        // Percentiles match a fresh `pcts` call over the same data.
        assert_eq!(snap.lifecycle_pcts_us, ana::pcts(&snap.stages.lifecycle));
        // Resume vector + severity counts match direct call.
        let direct_counts = ana::resume_severity_counts(&resumes);
        assert_eq!(snap.resume_severity_counts, direct_counts);
        assert_eq!(snap.resume_total, resumes.len() as u64);
    }

    #[test]
    fn vote_pattern_surfaces_every_present_vote() {
        // COR-03 regression guard: mixed Notarize+Skip rows must not
        // silently drop the Skip flag. Every present vote appears in
        // the rendered string concatenated with `+`.
        let mk = |n, f, s| SlotViewRow {
            slot: 0,
            status: SlotStatus::Pending,
            skip_classification: SkipClassification::NotSkipped,
            fast: None,
            we_are_leader: false,
            assembly_ms: None,
            consensus_ms: None,
            lifecycle_ms: None,
            voted_notarize: n,
            voted_finalize: f,
            voted_skip: s,
            safe_to_notar: false,
            safe_to_skip: false,
            crashed_leader: false,
        };
        assert_eq!(mk(false, false, false).vote_pattern(), "-");
        assert_eq!(mk(true, false, false).vote_pattern(), "N");
        assert_eq!(mk(true, true, false).vote_pattern(), "N+F");
        assert_eq!(mk(false, false, true).vote_pattern(), "S");
        // The two cases the old match dropped — assert they're now visible.
        assert_eq!(mk(true, false, true).vote_pattern(), "N+S");
        assert_eq!(mk(true, true, true).vote_pattern(), "N+F+S");
        // Finalize without Notarize is not a protocol state we surface
        // distinctly; falls into the "-" bucket like an all-false row.
        // (Renderer follows the column header order N / F / S.)
        assert_eq!(mk(false, true, false).vote_pattern(), "-");
    }

    #[test]
    fn snapshot_empty_state_is_default_safe() {
        let s = mk_state();
        let resumes = ana::vote_resumes_after_tcl(&s);
        let snap = LatencySnapshot::compute(&s, &resumes);
        assert!(snap.stages.lifecycle.is_empty());
        assert!(snap.resume_us_sorted.is_empty());
        assert_eq!(snap.resume_total, 0);
        assert_eq!(snap.resume_severity_counts, (0, 0, 0));
        assert_eq!(snap.lifecycle_pcts_us, (0, 0, 0, 0));
        assert_eq!(snap.resume_pcts_us, (0, 0, 0, 0));
    }
}
