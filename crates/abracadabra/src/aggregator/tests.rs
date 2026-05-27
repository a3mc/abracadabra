//! Aggregator unit tests. Split from `mod.rs` to keep that file under
//! the 800-LOC strong-warn threshold; semantics unchanged from the
//! previous inline `mod tests` block.

use super::*;
use crate::model::state::MAX_GROUP_TIMESTAMPS;
use crate::parser::line::Level;
use crate::parser::{self, Parsed};

fn parse_and_ingest(state: &mut State, line: &str) {
    let parsed = parser::parse(line).expect("parse");
    if let Parsed::Event(ev) = parsed {
        ingest(state, ev);
    }
}

fn parse_and_ingest_issue(state: &mut State, line: &str) {
    let parsed = parser::parse(line).expect("parse");
    if let Parsed::Issue {
        ts,
        level,
        module,
        body,
    } = parsed
    {
        ingest_issue(state, ts, level, module, body);
    } else {
        panic!("expected Parsed::Issue, got {parsed:?}");
    }
}

#[test]
fn ingest_lifecycle_round_trip() {
    // PARSE-08: parser regexes now require Base58 hashes of length
    // 32-48 (real Solana hashes are 43-44 chars). Synthetic shortened
    // hashes were rejected; the fixture below uses production-shaped
    // strings to round-trip parser -> aggregator.
    let mut state = State::default();
    let lines = [
        "[2026-05-23T16:00:07.187019566Z INFO  agave_votor::event_handler] \
         ALNSCya: First shred 1028070",
        "[2026-05-23T16:00:07.257045933Z INFO  agave_votor::event_handler] \
         ALNSCya: Block (1028070, EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv) \
         parent (1028069, CdJR4iF3xpkfSH62aMfBfJqKdpTR55KvFnHN93kPDUaW)",
        "[2026-05-23T16:00:07.257052546Z INFO  agave_votor::event_handler] \
         ALNSCya: Voting notarize for 1028070 EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv",
        "[2026-05-23T16:00:07.301219441Z INFO  agave_votor::event_handler] \
         ALNSCya: Block Notarized (1028070, EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv)",
        "[2026-05-23T16:00:07.301228498Z INFO  agave_votor::event_handler] \
         ALNSCya: Voting finalize for 1028070",
        "[2026-05-23T16:00:07.339120015Z INFO  agave_votor::event_handler] \
         ALNSCya: Finalized (1028070, EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv) fast: true",
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
         parent_block: (1028247, GG5ybXkSgf97V5BWgRFQKkweMMvabhaMy16XPsNtjwbB), \
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
fn block_notar_fallback_persists_hash() {
    // Regression for AGG-02: the fallback handler must promote `hash`
    // into SlotRecord.block_id so fork-tracking views see it.
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:07.301219441Z INFO  agave_votor::event_handler] \
         ALNSCya: Block notar-fallback (1028070, EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv)",
    );
    let rec = &state.slots[&1_028_070];
    assert!(rec.notar_fallback_at.is_some());
    assert_eq!(
        rec.block_id.as_deref(),
        Some("EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv")
    );
    assert_eq!(state.overall.block_notar_fallback_count, 1);
}

#[test]
fn safe_to_notar_persists_hash() {
    // Regression for AGG-02: SafeToNotar must also persist hash.
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:07.301219441Z INFO  agave_votor::event_handler] \
         ALNSCya: SafeToNotar (1051172, DTBC1p4b31RH7hRZFZxg4pSxwrsyE4ycmZrTKcTc6ygz)",
    );
    let rec = &state.slots[&1_051_172];
    assert!(rec.safe_to_notar_at.is_some());
    assert_eq!(
        rec.block_id.as_deref(),
        Some("DTBC1p4b31RH7hRZFZxg4pSxwrsyE4ycmZrTKcTc6ygz")
    );
    assert_eq!(state.overall.safe_to_notar, 1);
}

#[test]
fn block_notar_fallback_does_not_overwrite_existing_block_id() {
    // get_or_insert: hash arrives once and stays. A later fallback for
    // the same slot must not clobber the original block hash.
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:07.257045933Z INFO  agave_votor::event_handler] \
         ALNSCya: Block (1028070, EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv) \
         parent (1028069, CdJR4iF3xpkfSH62aMfBfJqKdpTR55KvFnHN93kPDUaW)",
    );
    // Hypothetical conflicting fallback hash on the same slot.
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:07.301219441Z INFO  agave_votor::event_handler] \
         ALNSCya: Block notar-fallback (1028070, FFZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv)",
    );
    assert_eq!(
        state.slots[&1_028_070].block_id.as_deref(),
        Some("EEZ7rFBjoTPWcA4wY1Gyxbe5qWMCKfq6A7bM1nRKB3Pv")
    );
}

#[test]
fn timeout_marks_slot_and_counter() {
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:14.187459305Z INFO  agave_votor::event_handler] \
         ALNSCya: Timeout 1028084",
    );
    assert!(state.slots[&1_028_084].timeout_at.is_some());
    assert_eq!(state.overall.timeouts, 1);
}

#[test]
fn timeout_crashed_leader_marks_slot_and_counter() {
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:14.187459305Z INFO  agave_votor::event_handler] \
         ALNSCya: TimeoutCrashedLeader 1028084",
    );
    assert!(state.slots[&1_028_084].timeout_crashed_leader_at.is_some());
    assert_eq!(state.overall.timeout_crashed_leaders, 1);
}

#[test]
fn safe_to_skip_marks_slot_and_counter() {
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:14.187459305Z INFO  agave_votor::event_handler] \
         ALNSCya: SafeToSkip 1113669",
    );
    assert!(state.slots[&1_113_669].safe_to_skip_at.is_some());
    assert_eq!(state.overall.safe_to_skip, 1);
}

#[test]
fn standstill_emits_inline_alert() {
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:14.187459305Z INFO  agave_votor::event_handler] \
         ALNSCya: Standstill 1234567",
    );
    assert_eq!(state.overall.standstill_events, 1);
    assert!(state
        .alerts
        .iter()
        .any(|a| matches!(a.kind, AlertKind::StandstillObserved { at_slot: 1_234_567 })));
}

#[test]
fn identity_changed_emits_inline_alert() {
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:14.187459305Z INFO  agave_votor::event_handler] \
         ALNSCya: SetIdentity",
    );
    assert!(state
        .alerts
        .iter()
        .any(|a| matches!(a.kind, AlertKind::IdentityChanged)));
}

#[test]
fn ingest_issue_error_becomes_severity_critical() {
    // Unparsed ERROR line from an unknown module routes through
    // ingest_issue and lands in log_issues as Critical.
    let mut state = State::default();
    parse_and_ingest_issue(
        &mut state,
        "[2026-05-23T16:00:07.171303148Z ERROR foo::unknown] something broke",
    );
    let key = (Severity::Critical, "foo::unknown".to_owned());
    let group = state.overall.log_issues.get(&key).expect("group present");
    assert_eq!(group.count, 1);
    assert_eq!(group.severity, Severity::Critical);
}

#[test]
fn ingest_issue_warn_becomes_severity_warn() {
    let mut state = State::default();
    parse_and_ingest_issue(
        &mut state,
        "[2026-05-23T16:00:07.171303148Z WARN  foo::unknown] mild thing",
    );
    let key = (Severity::Warn, "foo::unknown".to_owned());
    assert!(state.overall.log_issues.contains_key(&key));
}

#[test]
fn ingest_issue_info_is_dropped() {
    // INFO-level unparsed lines never become Parsed::Issue at the
    // parser layer (only WARN/ERROR do). Drive the aggregator
    // directly to confirm its level filter still rejects INFO.
    let mut state = State::default();
    let ts = time::macros::datetime!(2026-05-23 16:00:07 UTC);
    ingest_issue(
        &mut state,
        ts,
        Level::Info,
        "foo::unknown".to_owned(),
        "informational".to_owned(),
    );
    assert!(state.overall.log_issues.is_empty());
}

#[test]
fn record_log_pattern_first_sample_wins() {
    // The sample_body on a (severity, module) group is the body of the
    // first occurrence; subsequent occurrences only update count and
    // last_at.
    let mut state = State::default();
    let ts1 = time::macros::datetime!(2026-05-23 16:00:07 UTC);
    let ts2 = time::macros::datetime!(2026-05-23 16:00:08 UTC);
    ingest_issue(
        &mut state,
        ts1,
        Level::Error,
        "foo::unknown".to_owned(),
        "first".to_owned(),
    );
    ingest_issue(
        &mut state,
        ts2,
        Level::Error,
        "foo::unknown".to_owned(),
        "second".to_owned(),
    );
    let key = (Severity::Critical, "foo::unknown".to_owned());
    let group = state.overall.log_issues.get(&key).expect("group present");
    assert_eq!(group.count, 2);
    assert_eq!(group.sample_body, "first");
    assert_eq!(group.first_at, ts1);
    assert_eq!(group.last_at, ts2);
    assert_eq!(group.timestamps, vec![ts1, ts2]);
    assert_eq!(group.timestamps_dropped, 0);
}

#[test]
fn record_log_pattern_defers_body_alloc_on_update() {
    // Regression for AGG-04: the body closure must NOT run on the
    // update path. A panicking closure on the second call would fire
    // if the implementation eagerly materialised the body.
    let mut state = State::default();
    let ts = time::macros::datetime!(2026-05-23 16:00:07 UTC);
    record_log_pattern(&mut state, ts, Severity::Warn, "foo::module", || {
        "first body".to_owned()
    });
    record_log_pattern(&mut state, ts, Severity::Warn, "foo::module", || {
        panic!("body closure must not run on update path");
    });
    let key = (Severity::Warn, "foo::module".to_owned());
    let group = state.overall.log_issues.get(&key).expect("group present");
    assert_eq!(group.count, 2);
    assert_eq!(group.sample_body, "first body");
}

#[test]
fn surface_log_pattern_alerts_is_deterministic() {
    // Regression for AGG-01: two groups sharing (severity, count)
    // must always emit in the same order. Run analyze repeatedly and
    // confirm the alert sequence is stable.
    fn build_state() -> State {
        let mut state = State::default();
        let ts = time::macros::datetime!(2026-05-23 16:00:07 UTC);
        // Two Critical groups with count = 3 each — modules differ.
        for _ in 0..3 {
            ingest_issue(
                &mut state,
                ts,
                Level::Error,
                "aaa::module".to_owned(),
                "x".to_owned(),
            );
            ingest_issue(
                &mut state,
                ts,
                Level::Error,
                "zzz::module".to_owned(),
                "y".to_owned(),
            );
        }
        // One Warn group, count = 5.
        for _ in 0..5 {
            ingest_issue(
                &mut state,
                ts,
                Level::Warn,
                "bbb::module".to_owned(),
                "w".to_owned(),
            );
        }
        state
    }
    let mut expected: Option<Vec<String>> = None;
    for _ in 0..16 {
        let mut state = build_state();
        analyze(&mut state);
        let order: Vec<String> = state
            .alerts
            .iter()
            .filter_map(|a| match &a.kind {
                AlertKind::LogPattern { module, .. } => Some(module.clone()),
                _ => None,
            })
            .collect();
        // Critical before Warn; within Critical, modules in lexical
        // ascending order (count tied) -> aaa, zzz, then Warn bbb.
        assert_eq!(order, vec!["aaa::module", "zzz::module", "bbb::module"]);
        match &expected {
            None => expected = Some(order),
            Some(prev) => assert_eq!(prev, &order, "ordering must be deterministic"),
        }
    }
}

#[test]
#[should_panic(expected = "analyze() must run at most once per State")]
fn analyze_double_call_panics_in_debug() {
    // Regression for AGG-04 (prior round): double-call duplicates derived alerts.
    // The debug_assert! catches the regression in debug/test builds.
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:01:22.065145178Z INFO  agave_votor::event_handler] \
         ALNSCya: ProduceWindow LeaderWindowInfo { \
         start_slot: 1028248, end_slot: 1028251, \
         parent_block: (1028247, GG5ybXkSgf97V5BWgRFQKkweMMvabhaMy16XPsNtjwbB), \
         block_timer: Instant { tv_sec: 654042, tv_nsec: 317064752 } }",
    );
    analyze(&mut state);
    analyze(&mut state);
}

#[test]
fn local_leader_summary_skipped_when_no_windows() {
    // No ProduceWindow events -> no LocalLeaderSummary alert emitted.
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:14.187459305Z INFO  agave_votor::event_handler] \
         ALNSCya: Voting skip for 1028084",
    );
    analyze(&mut state);
    assert!(!state
        .alerts
        .iter()
        .any(|a| matches!(a.kind, AlertKind::LocalLeaderSummary { .. })));
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

// ---------- New regression tests (round 2) ----------

#[test]
fn analyze_sorts_alerts_critical_first() {
    // Regression for AGG-01: TUI title promises "CRIT first", but inline
    // INFO alerts (SetIdentity) used to precede sorted CRIT LogPatterns
    // because they were pushed during ingest and analyze appended.
    // After the fix, analyze re-sorts state.alerts globally and the
    // CRIT LogPattern lands ahead of the INFO IdentityChanged.
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:00.000000000Z INFO  agave_votor::event_handler] \
         ALNSCya: SetIdentity",
    );
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:05.000000000Z ERROR solana_core::cluster_slots_service::cluster_slots] \
         No epoch_metadata record for epoch 19",
    );
    analyze(&mut state);

    // Find the indices of the two alerts of interest and assert their
    // relative position.
    let crit_idx = state
        .alerts
        .iter()
        .position(|a| {
            matches!(
                a.kind,
                AlertKind::LogPattern {
                    severity: Severity::Critical,
                    ..
                }
            )
        })
        .expect("CRIT LogPattern present");
    let info_idx = state
        .alerts
        .iter()
        .position(|a| matches!(a.kind, AlertKind::IdentityChanged))
        .expect("INFO IdentityChanged present");
    assert!(
        crit_idx < info_idx,
        "CRIT must precede INFO in state.alerts; got crit={crit_idx} info={info_idx}",
    );
    assert_eq!(state.alerts[0].severity, Severity::Critical);
}

#[test]
fn analyze_sort_ties_break_by_at_then_kind() {
    // Within a severity bucket, ties break by `at asc`. Two INFO alerts
    // with the same severity should sort by their timestamp ascending.
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        // Later timestamp.
        "[2026-05-23T16:05:00.000000000Z INFO  solana_core::cluster_slots_service] \
         ClusterSlotsService has stopped because we have finished the alpenglow migration epoch",
    );
    parse_and_ingest(
        &mut state,
        // Earlier timestamp.
        "[2026-05-23T16:00:00.000000000Z INFO  agave_votor::event_handler] \
         ALNSCya: SetIdentity",
    );
    analyze(&mut state);
    // Both INFO; expect SetIdentity (16:00) before ClusterSlotsShutdown
    // (16:05) per `at asc`.
    let infos: Vec<&AlertKind> = state
        .alerts
        .iter()
        .filter(|a| a.severity == Severity::Info)
        .map(|a| &a.kind)
        .collect();
    let id_pos = infos
        .iter()
        .position(|k| matches!(k, AlertKind::IdentityChanged))
        .expect("IdentityChanged present");
    let shut_pos = infos
        .iter()
        .position(|k| matches!(k, AlertKind::ClusterSlotsShutdownObserved))
        .expect("ClusterSlotsShutdownObserved present");
    assert!(
        id_pos < shut_pos,
        "earlier-at INFO should sort before later-at INFO",
    );
}

#[test]
fn surface_log_pattern_alerts_does_not_clone_timestamps() {
    // Regression for AGG-02: surface_log_pattern_alerts must not move,
    // clone, or otherwise touch the canonical timestamps Vec in the
    // hashmap. Capture the backing pointer before and after analyze.
    let mut state = State::default();
    let ts = time::macros::datetime!(2026-05-23 16:00:07 UTC);
    for _ in 0..1024 {
        ingest_issue(
            &mut state,
            ts,
            Level::Error,
            "foo::module".to_owned(),
            "x".to_owned(),
        );
    }
    let key = (Severity::Critical, "foo::module".to_owned());
    let before = state
        .overall
        .log_issues
        .get(&key)
        .expect("group present")
        .timestamps
        .as_ptr();
    analyze(&mut state);
    let after = state
        .overall
        .log_issues
        .get(&key)
        .expect("group present")
        .timestamps
        .as_ptr();
    assert_eq!(
        before, after,
        "canonical timestamps Vec must not be reallocated by analyze",
    );
}

#[test]
fn timestamps_cap_drops_overflow_and_counts_them() {
    // Regression for AGG-06: timestamps Vec capped at MAX_GROUP_TIMESTAMPS.
    // Drive record_log_pattern past the cap and assert the overflow
    // surfaces in `timestamps_dropped` while `count` keeps growing.
    // Using a small synthetic cap would require const-generics; instead
    // we pre-fill the Vec to the cap and add a few more.
    let mut state = State::default();
    let ts = time::macros::datetime!(2026-05-23 16:00:07 UTC);
    let key = (Severity::Critical, "foo::module".to_owned());

    // Seed one entry so the closure runs once.
    record_log_pattern(&mut state, ts, Severity::Critical, "foo::module", || {
        "first".to_owned()
    });
    // Pre-fill the Vec to capacity to avoid pushing 1M timestamps one by
    // one — the cap is enforced on .len() so we just need len() == cap.
    {
        let group = state
            .overall
            .log_issues
            .get_mut(&key)
            .expect("group present");
        group.timestamps.clear();
        group.timestamps.resize(MAX_GROUP_TIMESTAMPS, ts);
        group.count = MAX_GROUP_TIMESTAMPS as u64;
    }
    // Three more — should not be pushed; should bump timestamps_dropped.
    for _ in 0..3 {
        record_log_pattern(&mut state, ts, Severity::Critical, "foo::module", || {
            unreachable!("update path must not call body closure")
        });
    }
    let group = state.overall.log_issues.get(&key).expect("group present");
    assert_eq!(group.timestamps.len(), MAX_GROUP_TIMESTAMPS);
    assert_eq!(group.timestamps_dropped, 3);
    assert_eq!(group.count, MAX_GROUP_TIMESTAMPS as u64 + 3);
}

#[test]
fn produce_window_rejects_oversized_span() {
    // Regression for AGG-07: a malformed ProduceWindow with end - start
    // > MAX_LEADER_WINDOW_SPAN must be rejected entirely (no slots
    // touched) and counted in overall.malformed_produce_window.
    let mut state = State::default();
    let ts = time::macros::datetime!(2026-05-23 16:00:07 UTC);
    ingest(
        &mut state,
        Event {
            ts,
            kind: EventKind::ProduceWindow {
                start: 1_000_000,
                end: 1_000_000 + MAX_LEADER_WINDOW_SPAN + 1,
                parent_slot: 999_999,
                parent_hash: "x".to_owned(),
            },
        },
    );
    assert_eq!(state.overall.malformed_produce_window, 1);
    assert_eq!(state.overall.produce_windows, 0);
    assert!(
        state.slots.is_empty(),
        "no slot records may be materialised for a rejected window",
    );
}

#[test]
fn produce_window_accepts_max_allowed_span() {
    // Sanity boundary: a span of exactly MAX_LEADER_WINDOW_SPAN is
    // accepted and all slots in the inclusive range are marked.
    let mut state = State::default();
    let ts = time::macros::datetime!(2026-05-23 16:00:07 UTC);
    let start = 2_000_000_u64;
    let end = start + MAX_LEADER_WINDOW_SPAN;
    ingest(
        &mut state,
        Event {
            ts,
            kind: EventKind::ProduceWindow {
                start,
                end,
                parent_slot: start.saturating_sub(1),
                parent_hash: "x".to_owned(),
            },
        },
    );
    assert_eq!(state.overall.malformed_produce_window, 0);
    assert_eq!(state.overall.produce_windows, 1);
    for s in start..=end {
        assert!(
            state.slots[&s].we_are_leader,
            "slot {s} should be marked leader",
        );
    }
}

#[test]
fn produce_window_rejects_inverted_range() {
    // end < start is also corruption — reject and count, do not iterate
    // an empty range silently.
    let mut state = State::default();
    let ts = time::macros::datetime!(2026-05-23 16:00:07 UTC);
    ingest(
        &mut state,
        Event {
            ts,
            kind: EventKind::ProduceWindow {
                start: 1_000_010,
                end: 1_000_000,
                parent_slot: 999_999,
                parent_hash: "x".to_owned(),
            },
        },
    );
    assert_eq!(state.overall.malformed_produce_window, 1);
    assert_eq!(state.overall.produce_windows, 0);
}
