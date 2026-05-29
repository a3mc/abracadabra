//! Per-slot lifecycle record.
//!
//! One `SlotRecord` per observed slot. Fields are populated as events arrive;
//! `status()` derives the slot's classification from which timestamps are set.

use time::OffsetDateTime;

/// Classification of a slot's observed lifecycle outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    /// Event(s) seen but slot has not yet been finalized or skipped.
    Pending,
    /// Finalized via single-round 80% Notarize path.
    FastFinalized,
    /// Finalized via two-round 60% Notarize + 60% Finalize path.
    SlowFinalized,
    /// Local node cast a skip vote for this slot.
    ///
    /// Whether this skip landed on a canonical slot (a participation
    /// failure) or on a slot the cluster also skipped (correct behavior)
    /// is a separate axis tracked by `SkipClassification`.
    Skipped,
}

/// How we know a slot was canonical even though we voted skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalSkipEvidence {
    /// We observed a `Finalized` event for this slot in our own log.
    /// Definitive — the cluster reached a finalization cert here.
    DirectFinalize,
    /// Some descendant of this slot was finalized, and walking parent
    /// pointers from that descendant reaches this slot. The cluster used
    /// this slot as part of the rooted chain. Equally definitive.
    Ancestry,
}

/// Per-slot skip classification — orthogonal to `SlotStatus`.
///
/// Stage 1 (log only) produces `NotSkipped`, `CanonicalSkip(...)`, or
/// `Indeterminate`. Stage 2 (RPC enrichment, not yet implemented) will
/// additionally produce `RightSkip` for skips confirmed non-canonical
/// via `getBlocks`, and may upgrade `Indeterminate` accordingly.
///
/// "Canonical skip" = we voted skip on a slot that became canonical.
/// The slot is canonical; our skip vote landed on it incorrectly. The
/// term is operator-facing; user docs and the TUI use the same wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkipClassification {
    /// This node did not cast a skip vote for this slot.
    #[default]
    NotSkipped,
    /// We voted skip on a slot that became canonical (real participation
    /// failure). Evidence describes how we know the slot is canonical.
    CanonicalSkip(CanonicalSkipEvidence),
    /// We voted skip and no evidence from the log proves the slot's
    /// canonical status either way. Could be a right skip (cluster
    /// also skipped) or an unverified canonical skip — log alone
    /// cannot say. Resolvable via Stage 2 RPC enrichment.
    Indeterminate,
}

impl SkipClassification {
    /// True iff this is a confirmed canonical skip via either evidence
    /// type. This is the operator-facing "did we fail" indicator.
    pub const fn is_canonical_skip(&self) -> bool {
        matches!(self, Self::CanonicalSkip(_))
    }
}

/// All observed events for a single slot.
///
/// Fields are intentionally flat `Option<OffsetDateTime>` to mirror the log
/// emit sites one-to-one. Derived fields live in methods.
#[derive(Debug, Clone, Default)]
pub struct SlotRecord {
    pub slot: u64,

    pub block_id: Option<String>,
    pub parent: Option<(u64, String)>,

    // Local-node lifecycle markers.
    pub first_shred_at: Option<OffsetDateTime>,
    pub block_emitted_at: Option<OffsetDateTime>,
    pub voted_notarize_at: Option<OffsetDateTime>,
    pub block_notarized_at: Option<OffsetDateTime>,
    pub voted_finalize_at: Option<OffsetDateTime>,
    pub notar_fallback_at: Option<OffsetDateTime>,
    pub finalized_at: Option<OffsetDateTime>,
    pub setting_root_at: Option<OffsetDateTime>,
    pub new_root_at: Option<OffsetDateTime>,

    // Timer events.
    pub timeout_at: Option<OffsetDateTime>,
    pub timeout_crashed_leader_at: Option<OffsetDateTime>,
    pub voted_skip_at: Option<OffsetDateTime>,
    pub safe_to_notar_at: Option<OffsetDateTime>,
    pub safe_to_skip_at: Option<OffsetDateTime>,

    pub fast_finalize: Option<bool>,

    /// Number of signed transactions in this slot's bank, harvested from
    /// the `solana_runtime::bank] bank frozen: SLOT hash: H signature_count:
    /// N ...` log line via `EventKind::BankFrozen`. Includes both user
    /// txs and vote txs (votes are ~2/validator/slot, so the *delta*
    /// above baseline = real cluster load). Drives the tx-pressure
    /// time-series card.
    pub signature_count: Option<u64>,

    /// True iff our validator was the leader for this slot's window
    /// (derived from ProduceWindow events).
    pub we_are_leader: bool,

    /// Populated by `aggregator::classify_skips` after `ingest` finishes.
    /// Default `NotSkipped` is the unclassified state — meaningful
    /// values land only after the classifier pass runs.
    pub skip_classification: SkipClassification,
}

impl SlotRecord {
    pub const fn new(slot: u64) -> Self {
        Self {
            slot,
            block_id: None,
            parent: None,
            first_shred_at: None,
            block_emitted_at: None,
            voted_notarize_at: None,
            block_notarized_at: None,
            voted_finalize_at: None,
            notar_fallback_at: None,
            finalized_at: None,
            setting_root_at: None,
            new_root_at: None,
            timeout_at: None,
            timeout_crashed_leader_at: None,
            voted_skip_at: None,
            safe_to_notar_at: None,
            safe_to_skip_at: None,
            fast_finalize: None,
            signature_count: None,
            we_are_leader: false,
            skip_classification: SkipClassification::NotSkipped,
        }
    }

    pub const fn status(&self) -> SlotStatus {
        if self.voted_skip_at.is_some() {
            SlotStatus::Skipped
        } else if self.finalized_at.is_some() {
            match self.fast_finalize {
                Some(true) => SlotStatus::FastFinalized,
                Some(false) | None => SlotStatus::SlowFinalized,
            }
        } else {
            SlotStatus::Pending
        }
    }

    /// Microseconds elapsed from `start` to `end`, if both timestamps are set
    /// and `end >= start`. Returns `None` for inverted intervals (clock skew,
    /// out-of-order events) so callers cannot accidentally feed negatives into
    /// `percentile` / `Severity::from_us`.
    #[allow(clippy::cast_possible_truncation)] // safe: log delta fits in i64 (i64::MAX µs ≈ 292kyr)
    pub fn delta_us(start: Option<OffsetDateTime>, end: Option<OffsetDateTime>) -> Option<i64> {
        let (a, b) = (start?, end?);
        let us = (b - a).whole_microseconds();
        if us < 0 {
            return None;
        }
        Some(us as i64)
    }

    /// First-shred → finalized latency (microseconds).
    pub fn lifecycle_us(&self) -> Option<i64> {
        Self::delta_us(self.first_shred_at, self.finalized_at)
    }

    /// `setting_root` → `new root` latency (bank-forks pruning).
    pub fn root_pruning_us(&self) -> Option<i64> {
        Self::delta_us(self.setting_root_at, self.new_root_at)
    }

    /// Inter-slot duration: time from THIS slot's first_shred to `next`'s
    /// first_shred. Used as the observable proxy for "slot duration".
    /// Returns None if either side lacks the timestamp.
    pub fn slot_duration_us(this: &Self, next: &Self) -> Option<i64> {
        Self::delta_us(this.first_shred_at, next.first_shred_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn status_pending_by_default() {
        let r = SlotRecord::new(42);
        assert_eq!(r.status(), SlotStatus::Pending);
    }

    #[test]
    fn status_fast_finalized() {
        let mut r = SlotRecord::new(42);
        r.finalized_at = Some(datetime!(2026-05-23 16:00:07.123456789 UTC));
        r.fast_finalize = Some(true);
        assert_eq!(r.status(), SlotStatus::FastFinalized);
    }

    #[test]
    fn status_slow_finalized() {
        let mut r = SlotRecord::new(42);
        r.finalized_at = Some(datetime!(2026-05-23 16:00:07.123456789 UTC));
        r.fast_finalize = Some(false);
        assert_eq!(r.status(), SlotStatus::SlowFinalized);
    }

    #[test]
    fn status_skipped_wins_over_finalized() {
        // Both timestamps set is the expected observation when this node
        // voted skip on a slot the cluster went on to finalize. Prefer
        // Skipped in the status pill — local behavior is the more
        // interesting signal for a per-validator analyzer, and cluster-level
        // skip outcomes are not yet log-observable (Skip certs emit no
        // info-level line; needs RPC or block-production data to recover).
        let mut r = SlotRecord::new(42);
        r.voted_skip_at = Some(datetime!(2026-05-23 16:00:07.123456789 UTC));
        r.finalized_at = Some(datetime!(2026-05-23 16:00:07.123456789 UTC));
        assert_eq!(r.status(), SlotStatus::Skipped);
    }

    #[test]
    fn lifecycle_us_computes_delta() {
        let mut r = SlotRecord::new(42);
        r.first_shred_at = Some(datetime!(2026-05-23 16:00:07.000000000 UTC));
        r.finalized_at = Some(datetime!(2026-05-23 16:00:07.152000000 UTC));
        assert_eq!(r.lifecycle_us(), Some(152_000));
    }

    #[test]
    fn lifecycle_us_returns_none_when_unset() {
        let r = SlotRecord::new(42);
        assert!(r.lifecycle_us().is_none());
    }

    #[test]
    fn delta_us_zero_for_equal_timestamps() {
        let t = Some(datetime!(2026-05-23 16:00:07.000000000 UTC));
        assert_eq!(SlotRecord::delta_us(t, t), Some(0));
    }

    #[test]
    fn delta_us_none_when_end_before_start() {
        // Inverted interval (e.g. clock skew) must not propagate as negative µs.
        let start = Some(datetime!(2026-05-23 16:00:07.500000000 UTC));
        let end = Some(datetime!(2026-05-23 16:00:07.000000000 UTC));
        assert_eq!(SlotRecord::delta_us(start, end), None);
    }

    #[test]
    fn delta_us_none_when_either_unset() {
        let t = Some(datetime!(2026-05-23 16:00:07.000000000 UTC));
        assert_eq!(SlotRecord::delta_us(None, t), None);
        assert_eq!(SlotRecord::delta_us(t, None), None);
        assert_eq!(SlotRecord::delta_us(None, None), None);
    }
}
