//! `OverallStats::standstill_ranges` build-up + `compute_tcl_outside_standstill`.
//!
//! Exercises `StandstillExtending` / `StandstillEnded` event handling
//! (range close, EOS-orphan close, the boundary semantics of the
//! inclusive `>=` / `<=` range check), and confirms a bare `Ended`
//! event without a preceding `Extending` is still recorded.

use super::super::*;
use super::parse_and_ingest;

#[test]
fn standstill_range_built_and_tcl_excluded() {
    // Two TCLs: one inside a closed standstill range, one outside.
    // After `analyze`, only the outside one should be counted in
    // `timeout_crashed_leaders_outside_standstill`, while
    // `timeout_crashed_leaders` retains the raw count.
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:00.000000000Z INFO  agave_votor::event_handler] \
         PK: TimeoutCrashedLeader 1000",
    );
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:01.000000000Z INFO  agave_votor::event_handler] \
         PK: Extending timeouts starting at slot 2000",
    );
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:02.000000000Z INFO  agave_votor::event_handler] \
         PK: TimeoutCrashedLeader 2050",
    );
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:03.000000000Z INFO  agave_votor::event_handler] \
         PK: Standstill initially detected at slot=2000 has ended at \
         slot=2100. Ending timeout extension",
    );
    analyze(&mut state);
    assert_eq!(state.overall.timeout_crashed_leaders, 2);
    assert_eq!(state.overall.timeout_crashed_leaders_outside_standstill, 1);
    assert_eq!(state.overall.standstill_ranges, vec![(2000, 2100)]);
    assert!(state.overall.open_standstill_entry.is_none());
}

#[test]
fn standstill_open_at_eos_closes_to_last_slot() {
    // Extending without a matching Ended at EOS: `analyze` should close
    // the range using the highest known slot.
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:00.000000000Z INFO  agave_votor::event_handler] \
         PK: TimeoutCrashedLeader 3500",
    );
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:01.000000000Z INFO  agave_votor::event_handler] \
         PK: Extending timeouts starting at slot 3000",
    );
    analyze(&mut state);
    // Closing range is (3000, max_slot_seen). max_slot = 3500 from the TCL.
    assert_eq!(state.overall.standstill_ranges, vec![(3000, 3500)]);
    // TCL at 3500 is inside the closed range → excluded.
    assert_eq!(state.overall.timeout_crashed_leaders, 1);
    assert_eq!(state.overall.timeout_crashed_leaders_outside_standstill, 0);
}

#[test]
fn no_standstill_means_excluded_equals_total() {
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:00.000000000Z INFO  agave_votor::event_handler] \
         PK: TimeoutCrashedLeader 100",
    );
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:01.000000000Z INFO  agave_votor::event_handler] \
         PK: TimeoutCrashedLeader 200",
    );
    analyze(&mut state);
    assert!(state.overall.standstill_ranges.is_empty());
    assert_eq!(state.overall.timeout_crashed_leaders, 2);
    assert_eq!(state.overall.timeout_crashed_leaders_outside_standstill, 2);
}

#[test]
fn tcl_at_standstill_range_boundary_is_included_in_range() {
    // Range endpoints are inclusive on both sides — a TCL exactly at
    // `entry` or `exit` lands INSIDE the range and is therefore
    // excluded from `timeout_crashed_leaders_outside_standstill`.
    let mut state = State::default();
    // TCL at the exact entry slot.
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:00.000000000Z INFO  agave_votor::event_handler] \
         PK: TimeoutCrashedLeader 2000",
    );
    // TCL at the exact exit slot.
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:01.000000000Z INFO  agave_votor::event_handler] \
         PK: TimeoutCrashedLeader 2100",
    );
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:02.000000000Z INFO  agave_votor::event_handler] \
         PK: Extending timeouts starting at slot 2000",
    );
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:03.000000000Z INFO  agave_votor::event_handler] \
         PK: Standstill initially detected at slot=2000 has ended at \
         slot=2100. Ending timeout extension",
    );
    analyze(&mut state);
    assert_eq!(state.overall.standstill_ranges, vec![(2000, 2100)]);
    assert_eq!(state.overall.timeout_crashed_leaders, 2);
    // Both TCLs are at boundary → both inside (inclusive) → none outside.
    assert_eq!(state.overall.timeout_crashed_leaders_outside_standstill, 0);
}

#[test]
fn standstill_ended_without_extending_still_records_range() {
    // A bare `StandstillEnded` (no preceding `StandstillExtending`,
    // e.g. log cut mid-window or restart between the two emits) still
    // pushes the payload range and is harmless.
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:00.000000000Z INFO  agave_votor::event_handler] \
         PK: TimeoutCrashedLeader 5050",
    );
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:01.000000000Z INFO  agave_votor::event_handler] \
         PK: Standstill initially detected at slot=5000 has ended at \
         slot=5100. Ending timeout extension",
    );
    analyze(&mut state);
    assert_eq!(state.overall.standstill_ranges, vec![(5000, 5100)]);
    assert!(state.overall.open_standstill_entry.is_none());
    // TCL at 5050 falls inside the range → excluded.
    assert_eq!(state.overall.timeout_crashed_leaders, 1);
    assert_eq!(state.overall.timeout_crashed_leaders_outside_standstill, 0);
}
