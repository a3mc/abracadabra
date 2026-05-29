//! Basic per-event ingest paths: lifecycle round-trip, votes, block /
//! cert hashes, timeouts, parent-ready recovery counter, identity and
//! standstill inline alerts. Cross-event interactions (analyze pass,
//! sort order, log-pattern surfacing) live in sibling modules.

use super::super::*;
use super::parse_and_ingest;

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
fn triggering_parent_ready_increments_counter() {
    // Every `Triggering parent ready` ingest bumps the recovery-path
    // counter on `OverallStats`. Counter is intentionally not
    // surfaced in the TUI yet (reserved for future analysis) — this
    // test guards the ingest write-site against silent regression.
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:14.187459305Z INFO  agave_votor::event_handler] \
         ALNSCyaSLbRDwmFcGoBV1irHDKPgRxZjfNTex9HPvkWu: \
         Triggering parent ready for slot 1028070 with parent 1028069 \
         CdJR4iF3xpkfSH62aMfBfJqKdpTR55KvFnHN93kPDUaW",
    );
    parse_and_ingest(
        &mut state,
        "[2026-05-23T16:00:15.187459305Z INFO  agave_votor::event_handler] \
         ALNSCyaSLbRDwmFcGoBV1irHDKPgRxZjfNTex9HPvkWu: \
         Triggering parent ready for slot 1028074 with parent 1028073 \
         CdJR4iF3xpkfSH62aMfBfJqKdpTR55KvFnHN93kPDUaW",
    );
    assert_eq!(state.overall.parent_ready_recoveries, 2);
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
    assert!(state.alerts.iter().any(|a| matches!(
        a.kind,
        AlertKind::StandstillObserved {
            at_slot: 1_234_567,
            count: 1,
            ..
        }
    )));
}

#[test]
fn repeated_standstill_at_same_slot_merges_into_one_alert() {
    // Sustained cluster halt — `Standstill {slot}` fires every ~10s at
    // the same slot. The aggregator must merge into a single alert
    // (count = N, last_at updated) rather than push N separate alerts.
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-28T17:50:01.000000000Z INFO  agave_votor::event_handler] \
         ALNSCya: Standstill 2063704",
    );
    parse_and_ingest(
        &mut state,
        "[2026-05-28T17:50:12.000000000Z INFO  agave_votor::event_handler] \
         ALNSCya: Standstill 2063704",
    );
    parse_and_ingest(
        &mut state,
        "[2026-05-28T17:50:23.000000000Z INFO  agave_votor::event_handler] \
         ALNSCya: Standstill 2063704",
    );
    // Event counter still tracks every firing.
    assert_eq!(state.overall.standstill_events, 3);
    // Alerts collapse to one.
    let standstill_alerts: Vec<&AlertKind> = state
        .alerts
        .iter()
        .filter(|a| matches!(a.kind, AlertKind::StandstillObserved { .. }))
        .map(|a| &a.kind)
        .collect();
    assert_eq!(standstill_alerts.len(), 1);
    match standstill_alerts[0] {
        AlertKind::StandstillObserved {
            at_slot,
            count,
            last_at,
        } => {
            assert_eq!(*at_slot, 2_063_704);
            assert_eq!(*count, 3);
            assert_eq!(*last_at, time::macros::datetime!(2026-05-28 17:50:23 UTC));
        }
        _ => unreachable!(),
    }
}

#[test]
fn standstills_at_distinct_slots_remain_separate_alerts() {
    // Two cluster halts at different anchor slots — must NOT merge.
    let mut state = State::default();
    parse_and_ingest(
        &mut state,
        "[2026-05-28T17:50:01.000000000Z INFO  agave_votor::event_handler] \
         ALNSCya: Standstill 2063704",
    );
    parse_and_ingest(
        &mut state,
        "[2026-05-28T18:00:00.000000000Z INFO  agave_votor::event_handler] \
         ALNSCya: Standstill 2063750",
    );
    let standstill_count = state
        .alerts
        .iter()
        .filter(|a| matches!(a.kind, AlertKind::StandstillObserved { .. }))
        .count();
    assert_eq!(standstill_count, 2);
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
