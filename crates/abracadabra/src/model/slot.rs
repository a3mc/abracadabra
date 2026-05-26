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
    Skipped,
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

    /// True iff our validator was the leader for this slot's window
    /// (derived from ProduceWindow events).
    pub we_are_leader: bool,
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
            we_are_leader: false,
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

    /// Microseconds elapsed from `start` to `end`, if both timestamps are set.
    #[allow(clippy::cast_possible_truncation)]
    pub fn delta_us(start: Option<OffsetDateTime>, end: Option<OffsetDateTime>) -> Option<i64> {
        let (a, b) = (start?, end?);
        Some((b - a).whole_microseconds() as i64)
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
        // Defensive: should never happen in practice, but if both timestamps
        // are set we treat Skipped as authoritative.
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
}
