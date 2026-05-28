//! Top-level state populated by the aggregator from a stream of events.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use time::OffsetDateTime;

use crate::model::alerts::{Alert, Severity};
use crate::model::slot::SlotRecord;

/// Upper bound on the number of timestamps retained per `LogIssueGroup`.
///
/// Memory bound at 16 bytes per `OffsetDateTime` -> ~16 MB per group at
/// the cap. Well above the worst observed real workload (~137k entries
/// in a 21h reference log); the cap exists to defend against pathological
/// input (e.g. a multi-GB log with one recurring unparsed WARN/ERROR
/// pattern). When the cap is hit, additional timestamps are dropped from
/// the sparkline-input list and counted in `LogIssueGroup.timestamps_dropped`;
/// `LogIssueGroup.count` continues to reflect the true total.
pub const MAX_GROUP_TIMESTAMPS: usize = 1_000_000;

/// Bucket for WARN/ERROR lines from modules with no dedicated parser.
/// Aggregated by `(severity, module)` in `OverallStats::log_issues`.
#[derive(Debug, Clone)]
pub struct LogIssueGroup {
    pub severity: Severity,
    pub module: String,
    pub count: u64,
    pub first_at: OffsetDateTime,
    pub last_at: OffsetDateTime,
    /// First body seen for the group; truncated upstream at the parser.
    pub sample_body: String,
    /// Timestamps in arrival order, capped at `MAX_GROUP_TIMESTAMPS`.
    /// Retained so the alerts panel can render a per-pattern time
    /// distribution sparkline ("does this pattern spam at one moment or
    /// fire evenly?"). 16 bytes per timestamp; at the cap the per-group
    /// footprint is ~16 MB. Worst observed real workload is ~137k
    /// entries in a 21h reference log; the cap defends against
    /// pathological logs that would otherwise grow unbounded. Overflow
    /// is tracked in `timestamps_dropped` — `count` always reflects the
    /// true total.
    pub timestamps: Vec<OffsetDateTime>,
    /// Number of timestamps not pushed because `timestamps.len()` was
    /// already at `MAX_GROUP_TIMESTAMPS`. Zero on the documented dataset.
    pub timestamps_dropped: u64,
}

/// File-level metadata captured during parse.
#[derive(Debug, Default)]
pub struct FileMeta {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub line_count: u64,
    pub time_range: Option<(OffsetDateTime, OffsetDateTime)>,
}

/// Cluster-wide and own-node counters accumulated across the full log.
#[derive(Debug, Default, Clone)]
pub struct OverallStats {
    // Local votes cast.
    pub votes_notarize: u64,
    pub votes_finalize: u64,
    pub votes_skip: u64,

    // Skip classification (populated by aggregator::classify_skips).
    //
    // `votes_skip` above counts every "Voting skip for SLOT" event we
    // observed. These three counters partition that into evidence
    // categories — operator-facing failure indicator.
    /// We voted skip on a slot we also observed `Finalized` for. The
    /// most direct evidence of participation failure.
    pub canonical_skips_direct: u64,
    /// We voted skip on a slot that is an ancestor of a finalized slot
    /// (parent chain from a finalized descendant reaches this slot).
    /// Equally definitive — the slot is on the rooted chain.
    pub canonical_skips_ancestry: u64,
    /// We voted skip on a slot with no canonical-status evidence in the
    /// log. Could be a right skip or an unverified canonical skip —
    /// Stage 1 alone cannot say.
    pub indeterminate_skips: u64,

    // Unique-slot counts (populated by classify_skips). These exist
    // alongside the per-event counters above because the event counts
    // can double-count: a slot with both `voted_skip_at` and
    // `finalized_at` set (a canonical skip) contributes to BOTH
    // `votes_skip` (event) AND `finalized_fast/slow` (event). The
    // subtraction formula `total - fin - skip` therefore underflows
    // on canonical-skip slots and saturates PEND to a misleading zero.
    /// Slots with `finalized_at` set (unique).
    pub finalized_slot_count: u64,
    /// Slots with `voted_skip_at` set (unique).
    pub skipped_slot_count: u64,
    /// Slots with neither `finalized_at` nor `voted_skip_at` — the
    /// honest pending count. May still carry partial signal (we
    /// observed shreds, voted notarize, etc.).
    pub pending_slot_count: u64,

    // Cluster cert outcomes (events we received).
    pub block_notarized_count: u64,
    pub block_notar_fallback_count: u64,
    pub finalized_fast: u64,
    pub finalized_slow: u64,

    // Roots advanced.
    pub setting_root_count: u64,
    pub new_root_count: u64,

    // Network/local signals.
    pub first_shreds: u64,
    pub timeouts: u64,
    pub timeout_crashed_leaders: u64,
    pub safe_to_notar: u64,
    pub safe_to_skip: u64,
    pub produce_windows: u64,
    /// `ProduceWindow` announcement timestamps, in arrival order. Used
    /// by the Alerts panel to render a sparkline of when this
    /// validator's leader windows fell across the log's time range.
    pub produce_window_timestamps: Vec<OffsetDateTime>,
    /// `ProduceWindow` events rejected because `end - start` exceeded
    /// `MAX_LEADER_WINDOW_SPAN`. Corruption-defence counter — a malformed
    /// `end` could otherwise force the aggregator to materialise
    /// `end - start + 1` `SlotRecord`s in the `slots` map.
    pub malformed_produce_window: u64,

    // Standstill activity.
    pub standstill_events: u64,
    pub standstill_extending_events: u64,
    pub standstill_ended_events: u64,
    pub refreshing_votes: u64,

    /// Closed standstill periods as `(entry_slot, exit_slot)` ranges.
    /// Built incrementally from `StandstillExtending` / `StandstillEnded`
    /// events; unmatched `StandstillExtending` at end-of-stream is closed
    /// off in `aggregator::analyze`.
    pub standstill_ranges: Vec<(u64, u64)>,

    /// Transient: entry slot of the currently-open standstill, set by
    /// `StandstillExtending` and cleared by `StandstillEnded`. Closed off
    /// in `analyze` if non-`None` at the end of ingest.
    pub open_standstill_entry: Option<u64>,

    /// `timeout_crashed_leaders` filtered to TCLs whose slot is not in
    /// any standstill range. Populated by `aggregator::analyze`. Equals
    /// `timeout_crashed_leaders` when `standstill_ranges.is_empty()`.
    pub timeout_crashed_leaders_outside_standstill: u64,

    /// Number of `Triggering parent ready` lines observed — i.e. how many
    /// times `event_handler::add_missing_parent_ready` fired the stuck-
    /// cluster recovery path. Expected to be rare; a spike is a signal
    /// that the validator is repeatedly catching up mid-window.
    pub parent_ready_recoveries: u64,

    // Bank.
    pub bank_frozen_count: u64,

    // Cluster slots loose-end signal counts.
    pub no_epoch_metadata: u64,
    pub no_epoch_info_for_slot: u64,
    pub updating_epoch_metadata: u64,
    pub evicting_epoch_metadata: u64,
    pub invalid_cluster_slots_update: u64,
    pub cluster_slots_service_stopped: bool,

    /// Generic WARN/ERROR lines from modules with no dedicated event
    /// parser. Grouped by `(severity, module)` so a thousand identical
    /// errors collapse to one entry with `count = 1000`.
    pub log_issues: HashMap<(Severity, String), LogIssueGroup>,
}

/// Top-level state. The aggregator owns one of these and mutates it.
///
/// Invariant on `alerts`: after `aggregator::analyze` returns, the vector
/// is sorted by `(severity desc, at asc, kind discriminant asc)`. Until
/// `analyze` runs, `alerts` is in stream-insertion order and that
/// ordering must not be relied on by consumers.
#[derive(Debug, Default)]
pub struct State {
    pub file_meta: FileMeta,
    pub our_pubkey: Option<String>,
    pub slots: BTreeMap<u64, SlotRecord>,
    pub overall: OverallStats,
    pub alerts: Vec<Alert>,
}

impl State {
    pub fn new(path: PathBuf, size_bytes: u64) -> Self {
        Self {
            file_meta: FileMeta {
                path,
                size_bytes,
                line_count: 0,
                time_range: None,
            },
            ..Self::default()
        }
    }

    /// Get-or-insert the SlotRecord for `slot`.
    pub fn slot_mut(&mut self, slot: u64) -> &mut SlotRecord {
        self.slots
            .entry(slot)
            .or_insert_with(|| SlotRecord::new(slot))
    }

    /// Observe a timestamp, advancing `file_meta.time_range`.
    pub fn observe_ts(&mut self, ts: OffsetDateTime) {
        self.file_meta.time_range = Some(match self.file_meta.time_range {
            None => (ts, ts),
            Some((lo, hi)) => (lo.min(ts), hi.max(ts)),
        });
    }

    /// Look up a log-issue group by `(severity, module)` without forcing
    /// the caller to construct a fresh owned `String` key — used by the
    /// alerts panel to fetch timestamps for sparkline rendering.
    pub fn log_issues_get(
        &self,
        severity: crate::model::alerts::Severity,
        module: &str,
    ) -> Option<&LogIssueGroup> {
        // HashMap lookup requires the key type. We construct one with a
        // borrowed clone of `module`; the alternative (custom Borrow
        // impl on a tuple) is more code for no benefit at typical
        // analyzer call rates.
        self.overall.log_issues.get(&(severity, module.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn slot_mut_inserts_default() {
        let mut s = State::new(PathBuf::from("/tmp/x"), 0);
        let rec = s.slot_mut(42);
        assert_eq!(rec.slot, 42);
        assert!(rec.block_id.is_none());
    }

    #[test]
    fn slot_mut_returns_existing() {
        let mut s = State::new(PathBuf::from("/tmp/x"), 0);
        s.slot_mut(42).block_id = Some("HASH".to_owned());
        let rec = s.slot_mut(42);
        assert_eq!(rec.block_id.as_deref(), Some("HASH"));
    }

    #[test]
    fn observe_ts_tracks_extremes() {
        let mut s = State::new(PathBuf::from("/tmp/x"), 0);
        s.observe_ts(datetime!(2026-05-23 16:00:00 UTC));
        s.observe_ts(datetime!(2026-05-24 13:00:00 UTC));
        s.observe_ts(datetime!(2026-05-23 18:00:00 UTC));
        let (lo, hi) = s.file_meta.time_range.unwrap();
        assert_eq!(lo, datetime!(2026-05-23 16:00:00 UTC));
        assert_eq!(hi, datetime!(2026-05-24 13:00:00 UTC));
    }
}
