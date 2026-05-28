//! Derived analyses over `State` that compute things grep can't do trivially:
//!
//! - per-slot lifecycle latency (first_shred → finalized)
//! - crashed-leader recovery time (TimeoutCrashedLeader → next Voting notarize)
//! - voting gaps (consecutive slots with no Notarize/Skip vote cast)

use std::collections::BTreeSet;

use time::OffsetDateTime;

use crate::model::slot::SlotRecord;
use crate::model::state::State;

/// One slot's first-shred → finalized latency.
#[derive(Debug, Clone)]
pub struct LatencyRecord {
    pub slot: u64,
    pub us: i64,
    pub fast: Option<bool>,
}

/// One vote-resume event: the time from `TimeoutCrashedLeader(N)` firing to
/// the next `Voting notarize` we cast at some slot ≥ N.
///
/// Naming: we deliberately avoid the word "recovery" here — that collides
/// with Solana shred recovery (erasure-decoded shred reconstruction), which
/// is a separate mechanism we'll track in its own area.
#[derive(Debug, Clone)]
pub struct VoteResumeRecord {
    /// Slot at which `TimeoutCrashedLeader` fired.
    pub tcl_slot: u64,
    /// Slot at which we next cast `Voting notarize`.
    pub resume_slot: u64,
    /// Microseconds between the two events.
    pub resume_us: i64,
}

/// A contiguous range of slots where we cast no vote.
#[derive(Debug, Clone)]
pub struct VotingGap {
    pub start_slot: u64,
    pub end_slot: u64,
    pub gap_slots: u64,
    pub last_vote_at: OffsetDateTime,
    pub resume_vote_at: OffsetDateTime,
}

/// First-shred → finalized latency for every finalized slot that has both
/// timestamps captured.
pub fn lifecycle_latencies(state: &State) -> Vec<LatencyRecord> {
    state
        .slots
        .values()
        .filter_map(|r| {
            let lat = SlotRecord::delta_us(r.first_shred_at, r.finalized_at)?;
            Some(LatencyRecord {
                slot: r.slot,
                us: lat,
                fast: r.fast_finalize,
            })
        })
        .collect()
}

/// Lifecycle latency broken into stages.
///
/// ```text
/// first_shred  ──┐
///                │  assembly  (shred reception + replay)
/// block_emitted ─┤
///                │  consensus (rounds 1+2: vote gossip + cert formation)
/// finalized    ──┘
///
/// lifecycle = assembly + consensus
/// ```
#[derive(Debug, Default, Clone)]
pub struct LatencyStages {
    /// `first_shred_at` → `block_emitted_at` (microseconds, sorted ascending).
    pub assembly: Vec<i64>,
    /// `block_emitted_at` → `finalized_at` (microseconds, sorted ascending).
    /// This is "pure consensus latency" — what the user usually thinks of as
    /// finalization time, with shred propagation/replay subtracted out.
    pub consensus: Vec<i64>,
    /// `first_shred_at` → `finalized_at` (microseconds, sorted ascending).
    /// = assembly + consensus (modulo slots where one side is missing).
    pub lifecycle: Vec<i64>,
}

impl LatencyStages {
    pub fn compute(state: &State) -> Self {
        let mut out = Self::default();
        for r in state.slots.values() {
            if let Some(us) = SlotRecord::delta_us(r.first_shred_at, r.block_emitted_at) {
                out.assembly.push(us);
            }
            if let Some(us) = SlotRecord::delta_us(r.block_emitted_at, r.finalized_at) {
                out.consensus.push(us);
            }
            if let Some(us) = SlotRecord::delta_us(r.first_shred_at, r.finalized_at) {
                out.lifecycle.push(us);
            }
        }
        out.sort();
        out
    }

    fn sort(&mut self) {
        self.assembly.sort_unstable();
        self.consensus.sort_unstable();
        self.lifecycle.sort_unstable();
    }
}

/// Convenience: (p50, p95, p99, max) from a sorted ascending slice.
pub fn pcts(sorted: &[i64]) -> (i64, i64, i64, i64) {
    let p50 = percentile(sorted, 0.50).unwrap_or(0);
    let p95 = percentile(sorted, 0.95).unwrap_or(0);
    let p99 = percentile(sorted, 0.99).unwrap_or(0);
    let max = sorted.last().copied().unwrap_or(0);
    (p50, p95, p99, max)
}

/// For each `TimeoutCrashedLeader` event, the time until we next cast a
/// `Voting notarize` for any subsequent slot.
///
/// Inverted pairs (`vn_at < tcl_at`, observable only via clock skew) are
/// dropped via `SlotRecord::delta_us` and never surface to bucket/window
/// aggregators or `Severity::from_us`.
pub fn vote_resumes_after_tcl(state: &State) -> Vec<VoteResumeRecord> {
    let mut resumes = Vec::new();
    for evt in state.slots.values() {
        if evt.timeout_crashed_leader_at.is_none() {
            continue;
        }
        let next = state
            .slots
            .range(evt.slot.saturating_add(1)..)
            .find_map(|(_, candidate)| candidate.voted_notarize_at.map(|ts| (candidate.slot, ts)));
        let Some((resume_slot, vn_at)) = next else {
            continue;
        };
        let Some(us) = SlotRecord::delta_us(evt.timeout_crashed_leader_at, Some(vn_at)) else {
            continue;
        };
        resumes.push(VoteResumeRecord {
            tcl_slot: evt.slot,
            resume_slot,
            resume_us: us,
        });
    }
    resumes
}

/// Find consecutive slots in `state.slots` range where neither a Notarize nor
/// Skip vote was cast. Returns gaps of length ≥ `min_size`.
///
/// "Voting" here means our local node actually cast one of those votes. A slot
/// with only e.g. a `Block Notarized` event from the cluster but no local vote
/// counts as a gap — the validator didn't participate.
pub fn voting_gaps(state: &State, min_size: u64) -> Vec<VotingGap> {
    let voted: BTreeSet<u64> = state
        .slots
        .iter()
        .filter(|(_, r)| r.voted_notarize_at.is_some() || r.voted_skip_at.is_some())
        .map(|(s, _)| *s)
        .collect();

    let Some((&min, &max)) = state
        .slots
        .keys()
        .next()
        .zip(state.slots.keys().next_back())
    else {
        return Vec::new();
    };

    let mut gaps = Vec::new();
    let mut gap_start: Option<u64> = None;
    let mut last_voted_at: Option<OffsetDateTime> = None;

    for slot in min..=max {
        if voted.contains(&slot) {
            if let Some(start) = gap_start.take() {
                let end = slot.saturating_sub(1);
                let size = end.saturating_sub(start).saturating_add(1);
                if size >= min_size {
                    let recovery_ts = state
                        .slots
                        .get(&slot)
                        .and_then(|r| r.voted_notarize_at.or(r.voted_skip_at));
                    if let (Some(last), Some(rec)) = (last_voted_at, recovery_ts) {
                        gaps.push(VotingGap {
                            start_slot: start,
                            end_slot: end,
                            gap_slots: size,
                            last_vote_at: last,
                            resume_vote_at: rec,
                        });
                    }
                }
            }
            if let Some(rec) = state.slots.get(&slot) {
                last_voted_at = rec.voted_notarize_at.or(rec.voted_skip_at);
            }
        } else {
            gap_start.get_or_insert(slot);
        }
    }
    gaps
}

/// Compute a percentile from a SORTED `i64` slice. Index = floor((len-1) * p).
/// `p` is clamped to `[0.0, 1.0]`; non-finite `p` clamps to `0.0`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // safe: idx ∈ [0, len-1] after clamp
pub fn percentile(sorted: &[i64], p: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    // NaN-safe clamp: replace NaN with 0.0, then clamp to [0, 1].
    let p = if p.is_nan() { 0.0 } else { p.clamp(0.0, 1.0) };
    let idx = ((sorted.len() - 1) as f64 * p) as usize;
    sorted.get(idx).copied()
}

/// Severity classification for a vote-resume time.
///
/// Band cuts (`1.5 s` Elevated, `3.0 s` Severe) are **provisional** and
/// have NOT been calibrated against a multi-day, multi-validator log
/// corpus. They were chosen by inspection of a single ~7 h log window
/// and reflect "noticeably slow" vs. "obviously slow" rather than any
/// empirical p90 / p99 baseline.
///
/// Re-validation pending: run `scripts/calibrate_resume_thresholds.sh`
/// against a corpus of 5+ validator logs spanning at least 24 h each;
/// adjust the constants to match the observed p90 (Elevated) and p99
/// (Severe) of the `TimeoutCrashedLeader -> next Voting notarize`
/// distribution. The script outputs honest percentile recommendations.
///
/// Do not tighten these bands without that data — false-positive
/// Elevated/Severe verdicts in operator UI erode the signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Normal,   // <1.5 s  (provisional)
    Elevated, // 1.5..3.0 s  (provisional)
    Severe,   // >=3.0 s  (provisional)
}

impl Severity {
    /// Classify a non-negative microsecond duration.
    ///
    /// Negative inputs (which TIME-01 prevents upstream) are treated as
    /// `Severe` rather than silently sliding into `Normal` via integer
    /// division truncation toward zero. `debug_assert` flags the upstream
    /// invariant break in debug builds.
    ///
    /// Band cuts are provisional — see the `Severity` type docstring and
    /// `scripts/calibrate_resume_thresholds.sh` for the re-validation
    /// path.
    pub const fn from_us(us: i64) -> Self {
        debug_assert!(us >= 0, "Severity::from_us called with negative µs");
        if us < 0 {
            return Self::Severe;
        }
        if us >= 3_000_000 {
            Self::Severe
        } else if us >= 1_500_000 {
            Self::Elevated
        } else {
            Self::Normal
        }
    }
}

/// Group vote-resume events by severity. Returns `(normal, elevated, severe)`.
pub fn resume_severity_counts(recs: &[VoteResumeRecord]) -> (u64, u64, u64) {
    let mut counts = (0, 0, 0);
    for r in recs {
        match Severity::from_us(r.resume_us) {
            Severity::Normal => counts.0 += 1,
            Severity::Elevated => counts.1 += 1,
            Severity::Severe => counts.2 += 1,
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use time::macros::datetime;

    fn mk_state() -> State {
        State::new(PathBuf::from("/tmp/x"), 0)
    }

    #[test]
    fn latencies_only_finalized() {
        let mut s = mk_state();
        // Finalized slot with full timeline
        {
            let r = s.slot_mut(10);
            r.first_shred_at = Some(datetime!(2026-05-23 16:00:00.000 UTC));
            r.finalized_at = Some(datetime!(2026-05-23 16:00:00.152 UTC));
            r.fast_finalize = Some(true);
        }
        // Slot with no finalized timestamp
        {
            let r = s.slot_mut(11);
            r.first_shred_at = Some(datetime!(2026-05-23 16:00:00.500 UTC));
        }
        let lats = lifecycle_latencies(&s);
        assert_eq!(lats.len(), 1);
        assert_eq!(lats[0].slot, 10);
        assert_eq!(lats[0].us, 152_000);
        assert_eq!(lats[0].fast, Some(true));
    }

    #[test]
    fn vote_resume_finds_next_notarize_after_tcl() {
        let mut s = mk_state();
        {
            let r = s.slot_mut(100);
            r.timeout_crashed_leader_at = Some(datetime!(2026-05-23 16:00:14.000 UTC));
        }
        // Slot 104 is the next leader window's first slot, where we voted notarize.
        {
            let r = s.slot_mut(104);
            r.voted_notarize_at = Some(datetime!(2026-05-23 16:00:15.600 UTC));
        }
        let recs = vote_resumes_after_tcl(&s);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].tcl_slot, 100);
        assert_eq!(recs[0].resume_slot, 104);
        assert_eq!(recs[0].resume_us, 1_600_000);
    }

    #[test]
    fn voting_gap_detected() {
        let mut s = mk_state();
        // Voted on 100, gap 101..=103, voted again on 104.
        {
            let r = s.slot_mut(100);
            r.voted_notarize_at = Some(datetime!(2026-05-23 16:00:00.000 UTC));
        }
        for slot in 101..=103 {
            // Insert slot records with no vote captured
            s.slot_mut(slot);
        }
        {
            let r = s.slot_mut(104);
            r.voted_notarize_at = Some(datetime!(2026-05-23 16:00:01.500 UTC));
        }
        let gaps = voting_gaps(&s, 2);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].start_slot, 101);
        assert_eq!(gaps[0].end_slot, 103);
        assert_eq!(gaps[0].gap_slots, 3);
    }

    #[test]
    fn percentile_basic() {
        let xs = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        assert_eq!(percentile(&xs, 0.0), Some(1));
        assert_eq!(percentile(&xs, 0.5), Some(5));
        assert_eq!(percentile(&xs, 1.0), Some(10));
        assert_eq!(percentile(&[], 0.5), None);
    }

    #[test]
    fn percentile_single_element() {
        assert_eq!(percentile(&[42], 0.0), Some(42));
        assert_eq!(percentile(&[42], 0.5), Some(42));
        assert_eq!(percentile(&[42], 1.0), Some(42));
    }

    #[test]
    fn percentile_three_at_p1() {
        assert_eq!(percentile(&[1, 2, 3], 1.0), Some(3));
    }

    #[test]
    fn percentile_clamps_out_of_range() {
        let xs = vec![10, 20, 30, 40, 50];
        // p > 1 saturates at last element.
        assert_eq!(percentile(&xs, 1.5), Some(50));
        // p < 0 saturates at first element.
        assert_eq!(percentile(&xs, -0.5), Some(10));
        // NaN treated as 0.
        assert_eq!(percentile(&xs, f64::NAN), Some(10));
    }

    #[test]
    fn severity_thresholds() {
        // Boundaries: <1.5 s Normal, [1.5, 3.0) Elevated, >=3.0 Severe.
        assert_eq!(Severity::from_us(0), Severity::Normal);
        assert_eq!(Severity::from_us(1_499_999), Severity::Normal);
        assert_eq!(Severity::from_us(1_500_000), Severity::Elevated);
        assert_eq!(Severity::from_us(2_999_999), Severity::Elevated);
        assert_eq!(Severity::from_us(3_000_000), Severity::Severe);
        assert_eq!(Severity::from_us(i64::MAX), Severity::Severe);
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn severity_negative_releases_to_severe() {
        // In release builds the debug_assert is elided; negative input still
        // classifies as Severe rather than silently Normal.
        assert_eq!(Severity::from_us(-1), Severity::Severe);
        assert_eq!(Severity::from_us(-500), Severity::Severe);
    }

    #[test]
    fn voting_gaps_empty_state() {
        let s = mk_state();
        assert!(voting_gaps(&s, 1).is_empty());
    }

    #[test]
    fn vote_resume_skips_inverted_clock_skew() {
        // Voting-notarize timestamp falls *before* the TCL timestamp (clock
        // skew). Must be dropped, not emitted as a negative resume_us.
        let mut s = mk_state();
        {
            let r = s.slot_mut(100);
            r.timeout_crashed_leader_at = Some(datetime!(2026-05-23 16:00:14.000 UTC));
        }
        {
            let r = s.slot_mut(104);
            r.voted_notarize_at = Some(datetime!(2026-05-23 16:00:13.000 UTC));
        }
        assert!(vote_resumes_after_tcl(&s).is_empty());
    }
}
