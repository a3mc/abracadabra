//! Time-bucketed view of state for sparkline rendering.
//!
//! Slices the observed time window into ~`target` buckets (clamped 1m..1h)
//! and counts events per bucket. Used by the TUI dashboard.

use time::{Duration, OffsetDateTime};

use crate::model::analysis;
use crate::model::slot::SlotRecord;
use crate::model::state::State;

#[derive(Debug, Clone, Default)]
pub struct BucketStats {
    pub start: Option<OffsetDateTime>,
    pub finalized_fast: u64,
    pub finalized_slow: u64,
    pub votes_skip: u64,
    pub crashed_leaders: u64,
    pub safe_to_notar: u64,
    pub safe_to_skip: u64,
    /// Count of slots in this bucket for which the local validator was
    /// the leader (`we_are_leader`).
    pub our_leader_slots: u64,
    /// First_shred -> finalized latencies (microseconds) for slots in this bucket.
    pub lifecycle_us: Vec<i64>,
    /// TimeoutCrashedLeader -> next Voting notarize times (microseconds).
    pub resume_us: Vec<i64>,
}

#[derive(Debug, Clone)]
pub struct TimeBuckets {
    pub start: OffsetDateTime,
    pub bucket_size: Duration,
    pub buckets: Vec<BucketStats>,
}

/// Default bucket size — 10 minutes.
///
/// 24h → 144 buckets, 1h → 6 buckets. Override with `--bucket <DUR>` on
/// the CLI; bounds (`MIN_BUCKET_SECS`..`MAX_BUCKET_SECS`) are enforced at
/// the parser layer, so this function trusts its caller.
pub const DEFAULT_BUCKET_SECS: i64 = 10 * 60;

impl TimeBuckets {
    /// Build buckets from `state`. `bucket_secs` is the bucket width in
    /// seconds; callers are expected to have validated it (CLI parser
    /// rejects values outside the documented range). Values <1 are
    /// clamped to 1 as a defensive guard.
    pub fn from_state(state: &State, bucket_secs: i64) -> Option<Self> {
        let (lo, hi) = state.file_meta.time_range?;
        let total_secs = (hi - lo).whole_seconds().max(60);
        let bucket_secs = bucket_secs.max(1);
        let n = ((total_secs + bucket_secs - 1) / bucket_secs).max(1) as usize;
        let bucket_size = Duration::seconds(bucket_secs);

        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let mut buckets: Vec<BucketStats> = (0..n)
            .map(|i| BucketStats {
                start: Some(lo + bucket_size * (i as i32)),
                ..BucketStats::default()
            })
            .collect();

        // Per-slot pass.
        for record in state.slots.values() {
            let Some(ts) = record
                .first_shred_at
                .or(record.voted_notarize_at)
                .or(record.voted_skip_at)
                .or(record.timeout_crashed_leader_at)
            else {
                continue;
            };
            let idx = bucket_idx(ts, lo, bucket_secs, n);
            let b = &mut buckets[idx];

            match record.fast_finalize {
                Some(true) => b.finalized_fast = b.finalized_fast.saturating_add(1),
                Some(false) => b.finalized_slow = b.finalized_slow.saturating_add(1),
                None => {}
            }
            if record.voted_skip_at.is_some() {
                b.votes_skip = b.votes_skip.saturating_add(1);
            }
            if record.timeout_crashed_leader_at.is_some() {
                b.crashed_leaders = b.crashed_leaders.saturating_add(1);
            }
            if record.safe_to_notar_at.is_some() {
                b.safe_to_notar = b.safe_to_notar.saturating_add(1);
            }
            if record.safe_to_skip_at.is_some() {
                b.safe_to_skip = b.safe_to_skip.saturating_add(1);
            }
            if record.we_are_leader {
                b.our_leader_slots = b.our_leader_slots.saturating_add(1);
            }
            if let Some(lat) = SlotRecord::delta_us(record.first_shred_at, record.finalized_at) {
                b.lifecycle_us.push(lat);
            }
        }

        // Vote-resume events — attributed to the bucket of the TCL timestamp.
        for r in analysis::vote_resumes_after_tcl(state) {
            let Some(tcl_at) = state
                .slots
                .get(&r.tcl_slot)
                .and_then(|s| s.timeout_crashed_leader_at)
            else {
                continue;
            };
            let idx = bucket_idx(tcl_at, lo, bucket_secs, n);
            buckets[idx].resume_us.push(r.resume_us);
        }

        Some(Self {
            start: lo,
            bucket_size,
            buckets,
        })
    }

    /// Fast-finalize percentage per bucket (0..=100). NaN buckets (no finalize
    /// activity) become 0.0 — these render as flat lows in a sparkline.
    pub fn fast_finalize_pct(&self) -> Vec<f64> {
        self.buckets
            .iter()
            .map(|b| {
                let tot = b.finalized_fast.saturating_add(b.finalized_slow);
                if tot == 0 {
                    0.0
                } else {
                    b.finalized_fast as f64 * 100.0 / tot as f64
                }
            })
            .collect()
    }

    pub fn skip_count(&self) -> Vec<u64> {
        self.buckets.iter().map(|b| b.votes_skip).collect()
    }

    pub fn crashed_leader_count(&self) -> Vec<u64> {
        self.buckets.iter().map(|b| b.crashed_leaders).collect()
    }

    pub fn fragmentation_count(&self) -> Vec<u64> {
        self.buckets
            .iter()
            .map(|b| b.safe_to_notar.saturating_add(b.safe_to_skip))
            .collect()
    }

    pub fn safe_to_notar_count(&self) -> Vec<u64> {
        self.buckets.iter().map(|b| b.safe_to_notar).collect()
    }

    pub fn safe_to_skip_count(&self) -> Vec<u64> {
        self.buckets.iter().map(|b| b.safe_to_skip).collect()
    }

    pub fn our_leader_slot_count(&self) -> Vec<u64> {
        self.buckets.iter().map(|b| b.our_leader_slots).collect()
    }

    /// Total finalized slots per bucket (fast + slow). Preserves the
    /// aggregate signal that the per-path % view alone hides.
    pub fn finalized_total_count(&self) -> Vec<u64> {
        self.buckets
            .iter()
            .map(|b| b.finalized_fast.saturating_add(b.finalized_slow))
            .collect()
    }

    /// Per-bucket (fast %, slow %) where both shares are taken as a
    /// fraction of `fast + slow` (i.e. of finalized slots). Buckets
    /// with no finalize activity return `(0, 0)`. Useful as a dual
    /// series for the Time-series tab's stacked card.
    pub fn fast_slow_pct(&self) -> (Vec<u64>, Vec<u64>) {
        let mut fast = Vec::with_capacity(self.buckets.len());
        let mut slow = Vec::with_capacity(self.buckets.len());
        for b in &self.buckets {
            let tot = b.finalized_fast.saturating_add(b.finalized_slow);
            // `checked_div` returns None when `tot == 0`; that's the
            // "no finalize activity in this bucket" case and we
            // surface it as (0, 0). Spelled this way (rather than
            // `if tot == 0 ... else / tot`) to satisfy the
            // `manual_checked_ops` lint introduced in clippy 1.95.
            fast.push(
                b.finalized_fast
                    .saturating_mul(100)
                    .checked_div(tot)
                    .unwrap_or(0),
            );
            slow.push(
                b.finalized_slow
                    .saturating_mul(100)
                    .checked_div(tot)
                    .unwrap_or(0),
            );
        }
        (fast, slow)
    }

    /// p95 of lifecycle latency in microseconds per bucket (0 if empty).
    pub fn lifecycle_p95_us(&self) -> Vec<i64> {
        self.buckets
            .iter()
            .map(|b| {
                if b.lifecycle_us.is_empty() {
                    return 0;
                }
                let mut v = b.lifecycle_us.clone();
                v.sort_unstable();
                analysis::percentile(&v, 0.95).unwrap_or(0)
            })
            .collect()
    }

    /// p95 of vote-resume time in microseconds per bucket (0 if empty).
    pub fn resume_p95_us(&self) -> Vec<i64> {
        self.buckets
            .iter()
            .map(|b| {
                if b.resume_us.is_empty() {
                    return 0;
                }
                let mut v = b.resume_us.clone();
                v.sort_unstable();
                analysis::percentile(&v, 0.95).unwrap_or(0)
            })
            .collect()
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn bucket_idx(ts: OffsetDateTime, lo: OffsetDateTime, bucket_secs: i64, n: usize) -> usize {
    let elapsed = (ts - lo).whole_seconds().max(0);
    let idx = (elapsed / bucket_secs) as usize;
    idx.min(n.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use time::macros::datetime;

    fn mk_state(lo: OffsetDateTime, hi: OffsetDateTime) -> State {
        let mut s = State::new(PathBuf::from("/tmp/x"), 0);
        s.observe_ts(lo);
        s.observe_ts(hi);
        s
    }

    #[test]
    fn empty_state_yields_none() {
        let s = State::new(PathBuf::from("/tmp/x"), 0);
        assert!(TimeBuckets::from_state(&s, DEFAULT_BUCKET_SECS).is_none());
    }

    #[test]
    fn default_bucket_size_yields_expected_count() {
        let lo = datetime!(2026-05-23 16:00:00 UTC);
        let hi = datetime!(2026-05-23 21:00:00 UTC); // 5h
        let mut s = mk_state(lo, hi);
        s.slot_mut(1).first_shred_at = Some(lo);
        let b = TimeBuckets::from_state(&s, DEFAULT_BUCKET_SECS).unwrap();
        assert_eq!(b.bucket_size.whole_seconds(), 600);
        assert_eq!(b.buckets.len(), 30); // 5h / 10m = 30
    }

    #[test]
    fn custom_bucket_size_is_honored() {
        let lo = datetime!(2026-05-23 16:00:00 UTC);
        let hi = datetime!(2026-05-23 17:00:00 UTC); // 1h
        let mut s = mk_state(lo, hi);
        s.slot_mut(1).first_shred_at = Some(lo);
        // 5m buckets -> 12 buckets in 1h.
        let b = TimeBuckets::from_state(&s, 5 * 60).unwrap();
        assert_eq!(b.bucket_size.whole_seconds(), 300);
        assert_eq!(b.buckets.len(), 12);
    }

    #[test]
    fn slot_attributed_to_correct_bucket() {
        let lo = datetime!(2026-05-23 16:00:00 UTC);
        let hi = lo + Duration::hours(2);
        let mut s = mk_state(lo, hi);
        // Slot at +25 minutes => bucket 2 (in 10m buckets).
        s.slot_mut(1).first_shred_at = Some(lo + Duration::minutes(25));
        s.slot_mut(1).fast_finalize = Some(true);
        let b = TimeBuckets::from_state(&s, DEFAULT_BUCKET_SECS).unwrap();
        let series = b.fast_finalize_pct();
        assert_eq!(series[2], 100.0);
    }

    #[test]
    fn fast_finalize_pct_mixed() {
        let lo = datetime!(2026-05-23 16:00:00 UTC);
        let hi = lo + Duration::hours(1);
        let mut s = mk_state(lo, hi);
        for i in 0..3u64 {
            let r = s.slot_mut(i);
            r.first_shred_at = Some(lo + Duration::seconds(10));
            r.fast_finalize = Some(true);
        }
        let r = s.slot_mut(10);
        r.first_shred_at = Some(lo + Duration::seconds(10));
        r.fast_finalize = Some(false);

        let b = TimeBuckets::from_state(&s, DEFAULT_BUCKET_SECS).unwrap();
        let series = b.fast_finalize_pct();
        // 3 fast + 1 slow in the first bucket -> 75%
        assert_eq!(series[0], 75.0);
    }
}
