//! `EventKind::ProduceWindow` ingest path. Most logic is in
//! `aggregator::ingest`; tests here exercise the corruption guards
//! (`MAX_LEADER_WINDOW_SPAN` cap, inverted-range rejection) and the
//! happy-path slot marking.

use super::super::*;
use super::parse_and_ingest;

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
