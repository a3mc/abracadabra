//! `ingest_issue` / `record_log_pattern` / `surface_log_pattern_alerts`
//! behaviour: severity routing, first-sample-wins, lazy body
//! allocation, deterministic sort, no-clone invariant on timestamps,
//! and the `MAX_GROUP_TIMESTAMPS` cap with overflow accounting.

use super::super::*;
use super::parse_and_ingest_issue;
use crate::model::state::MAX_GROUP_TIMESTAMPS;
use crate::parser::line::Level;

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
        super::parse_and_ingest(&mut state, &line);
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
