//! State mutator: applies each parsed `Event` to the running `State`.
//!
//! Splitting this from `parser` keeps the parser pure (Event-yielding only)
//! and the aggregator focused on bookkeeping. The TUI can read State without
//! understanding the event grammar.

use time::OffsetDateTime;

use crate::model::alerts::{Alert, AlertKind, Severity};
use crate::model::state::{LogIssueGroup, State};
use crate::parser::line::Level;
use crate::parser::{Event, EventKind};

/// Apply a single event to `state`.
pub fn ingest(state: &mut State, event: Event) {
    state.observe_ts(event.ts);

    match event.kind {
        EventKind::Block {
            slot,
            hash,
            parent_slot,
            parent_hash,
        } => {
            let rec = state.slot_mut(slot);
            rec.block_id.get_or_insert(hash);
            rec.parent.get_or_insert((parent_slot, parent_hash));
            rec.block_emitted_at.get_or_insert(event.ts);
        }
        EventKind::FirstShred { slot } => {
            state.slot_mut(slot).first_shred_at.get_or_insert(event.ts);
            state.overall.first_shreds = state.overall.first_shreds.saturating_add(1);
        }
        EventKind::VotingNotarize { slot, hash } => {
            let rec = state.slot_mut(slot);
            rec.voted_notarize_at.get_or_insert(event.ts);
            rec.block_id.get_or_insert(hash);
            state.overall.votes_notarize = state.overall.votes_notarize.saturating_add(1);
        }
        EventKind::VotingFinalize { slot } => {
            state
                .slot_mut(slot)
                .voted_finalize_at
                .get_or_insert(event.ts);
            state.overall.votes_finalize = state.overall.votes_finalize.saturating_add(1);
        }
        EventKind::VotingSkip { slot } => {
            state.slot_mut(slot).voted_skip_at.get_or_insert(event.ts);
            state.overall.votes_skip = state.overall.votes_skip.saturating_add(1);
        }
        EventKind::BlockNotarized { slot, hash } => {
            let rec = state.slot_mut(slot);
            rec.block_notarized_at.get_or_insert(event.ts);
            rec.block_id.get_or_insert(hash);
            state.overall.block_notarized_count =
                state.overall.block_notarized_count.saturating_add(1);
        }
        EventKind::BlockNotarFallback { slot, .. } => {
            state
                .slot_mut(slot)
                .notar_fallback_at
                .get_or_insert(event.ts);
            state.overall.block_notar_fallback_count =
                state.overall.block_notar_fallback_count.saturating_add(1);
        }
        EventKind::Finalized { slot, hash, fast } => {
            let rec = state.slot_mut(slot);
            rec.finalized_at.get_or_insert(event.ts);
            rec.fast_finalize.get_or_insert(fast);
            rec.block_id.get_or_insert(hash);
            if fast {
                state.overall.finalized_fast = state.overall.finalized_fast.saturating_add(1);
            } else {
                state.overall.finalized_slow = state.overall.finalized_slow.saturating_add(1);
            }
        }
        EventKind::SettingRoot { slot } => {
            state.slot_mut(slot).setting_root_at.get_or_insert(event.ts);
            state.overall.setting_root_count = state.overall.setting_root_count.saturating_add(1);
        }
        EventKind::NewRoot { slot } => {
            state.slot_mut(slot).new_root_at.get_or_insert(event.ts);
            state.overall.new_root_count = state.overall.new_root_count.saturating_add(1);
        }
        EventKind::Timeout { slot } => {
            state.slot_mut(slot).timeout_at.get_or_insert(event.ts);
            state.overall.timeouts = state.overall.timeouts.saturating_add(1);
        }
        EventKind::TimeoutCrashedLeader { slot } => {
            state
                .slot_mut(slot)
                .timeout_crashed_leader_at
                .get_or_insert(event.ts);
            state.overall.timeout_crashed_leaders =
                state.overall.timeout_crashed_leaders.saturating_add(1);
        }
        EventKind::SafeToNotar { slot, .. } => {
            state
                .slot_mut(slot)
                .safe_to_notar_at
                .get_or_insert(event.ts);
            state.overall.safe_to_notar = state.overall.safe_to_notar.saturating_add(1);
        }
        EventKind::SafeToSkip { slot } => {
            state.slot_mut(slot).safe_to_skip_at.get_or_insert(event.ts);
            state.overall.safe_to_skip = state.overall.safe_to_skip.saturating_add(1);
        }
        EventKind::ProduceWindow { start, end, .. } => {
            for s in start..=end {
                state.slot_mut(s).we_are_leader = true;
            }
            state.overall.produce_windows = state.overall.produce_windows.saturating_add(1);
            state.overall.produce_window_timestamps.push(event.ts);
        }
        EventKind::Standstill { slot } => {
            state.overall.standstill_events = state.overall.standstill_events.saturating_add(1);
            state.alerts.push(Alert::new(
                Severity::Warn,
                event.ts,
                AlertKind::StandstillObserved { at_slot: slot },
                format!("Standstill firing at finalized slot {slot}"),
            ));
        }
        EventKind::StandstillExtending { .. } => {
            state.overall.standstill_extending_events =
                state.overall.standstill_extending_events.saturating_add(1);
        }
        EventKind::StandstillEnded { .. } => {
            state.overall.standstill_ended_events =
                state.overall.standstill_ended_events.saturating_add(1);
        }
        EventKind::RefreshingVote => {
            state.overall.refreshing_votes = state.overall.refreshing_votes.saturating_add(1);
        }
        EventKind::SetIdentity => {
            // Operator rotated validator identity — INFO timeline anchor.
            // Pushed inline (not via analyze) so the Alert.at carries
            // the actual identity-change timestamp rather than the log's
            // last-seen time.
            state.alerts.push(Alert::new(
                Severity::Info,
                event.ts,
                AlertKind::IdentityChanged,
                "Operator rotated validator identity (Set identity event)".to_owned(),
            ));
        }
        EventKind::BankFrozen { .. } => {
            state.overall.bank_frozen_count = state.overall.bank_frozen_count.saturating_add(1);
        }
        EventKind::NoEpochMetadata { epoch } => {
            state.overall.no_epoch_metadata = state.overall.no_epoch_metadata.saturating_add(1);
            record_log_pattern(
                state,
                event.ts,
                Severity::Critical,
                CLUSTER_SLOTS_MODULE,
                format!("No epoch_metadata record for epoch {epoch}"),
            );
        }
        EventKind::NoEpochInfoForSlot { slot } => {
            state.overall.no_epoch_info_for_slot =
                state.overall.no_epoch_info_for_slot.saturating_add(1);
            record_log_pattern(
                state,
                event.ts,
                Severity::Critical,
                CLUSTER_SLOTS_MODULE,
                format!("No epoch info for slot {slot}"),
            );
        }
        EventKind::UpdatingEpochMetadata { .. } => {
            // INFO-level — counter-only, no alert.
            state.overall.updating_epoch_metadata =
                state.overall.updating_epoch_metadata.saturating_add(1);
        }
        EventKind::EvictingEpochMetadata { .. } => {
            // INFO-level — counter-only, no alert.
            state.overall.evicting_epoch_metadata =
                state.overall.evicting_epoch_metadata.saturating_add(1);
        }
        EventKind::ClusterSlotsStopped => {
            state.overall.cluster_slots_service_stopped = true;
            state.alerts.push(Alert::new(
                Severity::Info,
                event.ts,
                AlertKind::ClusterSlotsShutdownObserved,
                "ClusterSlotsService has stopped — validator entered FullAlpenglowEpoch".to_owned(),
            ));
        }
        EventKind::InvalidClusterSlotsUpdate => {
            state.overall.invalid_cluster_slots_update =
                state.overall.invalid_cluster_slots_update.saturating_add(1);
            record_log_pattern(
                state,
                event.ts,
                Severity::Warn,
                CLUSTER_SLOTS_MODULE,
                "Invalid update call to ClusterSlots, can not roll time backwards!".to_owned(),
            );
        }
        EventKind::EventHandlerStats { .. } | EventKind::BlockCommitmentCache { .. } => {
            // v0.3: parse selective datapoints into model. No-op for v0.1.
        }
    }
}

/// Ingest one WARN/ERROR line from an unparsed module.
///
/// Aggregated by `(severity, module)` so a thousand identical lines
/// collapse to one `LogIssueGroup` entry with `count = 1000`. The first
/// body sample is retained verbatim (already truncated upstream); the
/// last-seen timestamp and the count update on every hit.
pub fn ingest_issue(
    state: &mut State,
    ts: OffsetDateTime,
    level: Level,
    module: String,
    body: String,
) {
    state.observe_ts(ts);
    let severity = match level {
        Level::Error => Severity::Critical,
        Level::Warn => Severity::Warn,
        _ => return, // Only WARN/ERROR are surfaced as issues.
    };
    record_log_pattern(state, ts, severity, &module, body);
}

/// Add (or update) a `LogIssueGroup` keyed by `(severity, module)`.
/// Used both by `ingest_issue` (unparsed lines) and by the known-issue
/// event handlers below — so a structured event with a documented
/// investigation (e.g. `NoEpochMetadata`) still shows up in the same
/// raw `LogPattern` alerts list as the unparsed errors, with a count.
fn record_log_pattern(
    state: &mut State,
    ts: OffsetDateTime,
    severity: Severity,
    module: &str,
    sample_body: String,
) {
    let entry = state
        .overall
        .log_issues
        .entry((severity, module.to_owned()))
        .or_insert_with(|| LogIssueGroup {
            severity,
            module: module.to_owned(),
            count: 0,
            first_at: ts,
            last_at: ts,
            sample_body,
            timestamps: Vec::new(),
        });
    entry.count = entry.count.saturating_add(1);
    entry.last_at = ts;
    entry.timestamps.push(ts);
}

const CLUSTER_SLOTS_MODULE: &str = "solana_core::cluster_slots_service::cluster_slots";

/// Derived alerts computed after the stream has been fully ingested.
///
/// Emits one `LogPattern` alert per `(severity, module)` group and a
/// single `LocalLeaderSummary` INFO alert when the validator was
/// scheduled to lead any windows. The cluster-slots loose-end pattern
/// surfaces via the LogPattern alert for
/// `solana_core::cluster_slots_service` — see
/// `docs/alpenglow/investigations/01-cluster-slots-loose-end.md` for
/// the analytical context.
pub fn analyze(state: &mut State) {
    surface_log_pattern_alerts(state);
    surface_local_leader_summary(state);
}

/// Emit a single INFO alert summarising the validator's leader windows.
/// The Alerts panel renders a sparkline from
/// `state.overall.produce_window_timestamps` showing when leader
/// windows fell across the log — useful for "was I leader during the
/// outage at 04:42?" queries.
fn surface_local_leader_summary(state: &mut State) {
    let window_count = state.overall.produce_windows;
    if window_count == 0 {
        return;
    }
    let slot_count: u64 = state.slots.values().filter(|s| s.we_are_leader).count() as u64;
    let at = state
        .overall
        .produce_window_timestamps
        .first()
        .copied()
        .unwrap_or_else(|| {
            state
                .file_meta
                .time_range
                .map_or_else(time::OffsetDateTime::now_utc, |(lo, _)| lo)
        });
    // Show the math so the relationship is unambiguous in the alerts
    // panel preview: a "window" is a 4-slot burst assigned to one
    // leader (Solana `NUM_CONSECUTIVE_LEADER_SLOTS = 4`).
    let description = format!(
        "Local validator was leader: {slot_count} slots = {window_count} \
         windows × 4 slots/window"
    );
    state.alerts.push(Alert::new(
        Severity::Info,
        at,
        AlertKind::LocalLeaderSummary {
            slot_count,
            window_count,
        },
        description,
    ));
}

/// Emit one alert per `(severity, module)` group of unparsed WARN/ERROR
/// lines. Sorted Critical-first, then by count, so the alerts panel
/// reads worst-first.
fn surface_log_pattern_alerts(state: &mut State) {
    let mut groups: Vec<LogIssueGroup> = state.overall.log_issues.values().cloned().collect();
    if groups.is_empty() {
        return;
    }
    groups.sort_by(|a, b| {
        severity_rank(b.severity)
            .cmp(&severity_rank(a.severity))
            .then(b.count.cmp(&a.count))
    });
    for g in groups {
        let label = match g.severity {
            Severity::Critical => "ERROR",
            Severity::Warn => "WARN",
            Severity::Info => "INFO",
        };
        let plural = if g.count == 1 { "" } else { "s" };
        let description = format!(
            "{label} {} ({} occurrence{plural}): {}",
            g.module, g.count, g.sample_body,
        );
        state.alerts.push(Alert::new(
            g.severity,
            g.first_at,
            AlertKind::LogPattern {
                severity: g.severity,
                module: g.module,
                count: g.count,
            },
            description,
        ));
    }
}

const fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::Critical => 2,
        Severity::Warn => 1,
        Severity::Info => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{self, Parsed};

    fn parse_and_ingest(state: &mut State, line: &str) {
        let parsed = parser::parse(line).expect("parse");
        if let Parsed::Event(ev) = parsed {
            ingest(state, ev);
        }
    }

    #[test]
    fn ingest_lifecycle_round_trip() {
        let mut state = State::default();
        let lines = [
            "[2026-05-23T16:00:07.187019566Z INFO  agave_votor::event_handler] \
             ALNSCya: First shred 1028070",
            "[2026-05-23T16:00:07.257045933Z INFO  agave_votor::event_handler] \
             ALNSCya: Block (1028070, EEZ7rFB) parent (1028069, CdJR4iF3)",
            "[2026-05-23T16:00:07.257052546Z INFO  agave_votor::event_handler] \
             ALNSCya: Voting notarize for 1028070 EEZ7rFB",
            "[2026-05-23T16:00:07.301219441Z INFO  agave_votor::event_handler] \
             ALNSCya: Block Notarized (1028070, EEZ7rFB)",
            "[2026-05-23T16:00:07.301228498Z INFO  agave_votor::event_handler] \
             ALNSCya: Voting finalize for 1028070",
            "[2026-05-23T16:00:07.339120015Z INFO  agave_votor::event_handler] \
             ALNSCya: Finalized (1028070, EEZ7rFB) fast: true",
            "[2026-05-23T16:00:07.339131506Z INFO  agave_votor::root_utils] \
             ALNSCya: setting root 1028070",
            "[2026-05-23T16:00:07.346089002Z INFO  agave_votor::root_utils] \
             ALNSCya: new root 1028070",
        ];
        for line in lines {
            parse_and_ingest(&mut state, line);
        }

        let rec = &state.slots[&1_028_070];
        assert!(rec.first_shred_at.is_some());
        assert!(rec.block_emitted_at.is_some());
        assert!(rec.voted_notarize_at.is_some());
        assert!(rec.block_notarized_at.is_some());
        assert!(rec.voted_finalize_at.is_some());
        assert!(rec.finalized_at.is_some());
        assert!(rec.setting_root_at.is_some());
        assert!(rec.new_root_at.is_some());
        assert_eq!(rec.fast_finalize, Some(true));
        assert_eq!(rec.status(), crate::model::slot::SlotStatus::FastFinalized);

        assert_eq!(state.overall.votes_notarize, 1);
        assert_eq!(state.overall.votes_finalize, 1);
        assert_eq!(state.overall.first_shreds, 1);
        assert_eq!(state.overall.finalized_fast, 1);
        assert_eq!(state.overall.finalized_slow, 0);
        assert_eq!(state.overall.setting_root_count, 1);
        assert_eq!(state.overall.new_root_count, 1);
    }

    #[test]
    fn voting_skip_marks_slot() {
        let mut state = State::default();
        parse_and_ingest(
            &mut state,
            "[2026-05-23T16:00:14.187459305Z INFO  agave_votor::event_handler] \
             ALNSCya: Voting skip for 1028084",
        );
        let rec = &state.slots[&1_028_084];
        assert!(rec.voted_skip_at.is_some());
        assert_eq!(rec.status(), crate::model::slot::SlotStatus::Skipped);
        assert_eq!(state.overall.votes_skip, 1);
    }

    #[test]
    fn produce_window_marks_leader_window() {
        let mut state = State::default();
        parse_and_ingest(
            &mut state,
            "[2026-05-23T16:01:22.065145178Z INFO  agave_votor::event_handler] \
             ALNSCya: ProduceWindow LeaderWindowInfo { \
             start_slot: 1028248, end_slot: 1028251, \
             parent_block: (1028247, GG5ybXkS), \
             block_timer: Instant { tv_sec: 654042, tv_nsec: 317064752 } }",
        );
        for slot in 1_028_248..=1_028_251 {
            assert!(
                state.slots[&slot].we_are_leader,
                "slot {slot} should be marked leader"
            );
        }
        assert_eq!(state.overall.produce_windows, 1);
    }

    #[test]
    fn cluster_slots_errors_surface_as_log_pattern() {
        // The cluster_slots ERROR group surfaces via the LogPattern path
        // (count + sparkline) — the analytical loose-end WARN is no
        // longer emitted because it duplicated the count for the same
        // root cause.
        let mut state = State::default();
        for epoch in 0..150 {
            let line = format!(
                "[2026-05-23T16:00:07.171303148Z ERROR \
                 solana_core::cluster_slots_service::cluster_slots] \
                 No epoch_metadata record for epoch {epoch}"
            );
            parse_and_ingest(&mut state, &line);
        }
        analyze(&mut state);
        assert!(state.alerts.iter().any(|a| matches!(
            &a.kind,
            AlertKind::LogPattern {
                severity: Severity::Critical,
                count: 150,
                ..
            },
        )));
    }

    #[test]
    fn cluster_slots_shutdown_still_observed_separately() {
        // The shutdown event still fires its own INFO marker — it's a
        // distinct signal from the ERROR count.
        let mut state = State::default();
        parse_and_ingest(
            &mut state,
            "[2026-05-23T16:00:07.171303148Z ERROR solana_core::cluster_slots_service::cluster_slots] \
             No epoch_metadata record for epoch 19",
        );
        parse_and_ingest(
            &mut state,
            "[2026-05-21T04:42:15.117334745Z INFO  solana_core::cluster_slots_service] \
             ClusterSlotsService has stopped because we have finished the alpenglow migration epoch",
        );
        analyze(&mut state);
        assert!(state
            .alerts
            .iter()
            .any(|a| matches!(a.kind, AlertKind::ClusterSlotsShutdownObserved)));
        assert!(state.alerts.iter().any(|a| matches!(
            &a.kind,
            AlertKind::LogPattern {
                severity: Severity::Critical,
                count: 1,
                ..
            },
        )));
    }
}
