//! `analyze` post-pass behaviour: double-call guard, local-leader
//! summary gating, alert sort order (severity bucket + at + kind), and
//! the cluster-slots-shutdown vs LogPattern dual-emit case.

use super::super::*;
use super::parse_and_ingest;

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
