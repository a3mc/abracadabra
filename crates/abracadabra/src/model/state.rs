//! Top-level state populated by the aggregator from a stream of events.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use time::OffsetDateTime;

use crate::model::alerts::{Alert, Severity};
use crate::model::slot::SlotRecord;

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
    /// All timestamps for the group, in arrival order. Retained so the
    /// alerts panel can render a per-pattern time-distribution sparkline
    /// ("does this pattern spam at one moment or fire evenly?"). 16
    /// bytes per timestamp; for the largest known group (~137k cluster-
    /// slots ERRORs in a 21h log) this is ~2.2 MB — acceptable for a
    /// post-mortem analyzer.
    pub timestamps: Vec<OffsetDateTime>,
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

    // Standstill activity.
    pub standstill_events: u64,
    pub standstill_extending_events: u64,
    pub standstill_ended_events: u64,
    pub refreshing_votes: u64,

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
