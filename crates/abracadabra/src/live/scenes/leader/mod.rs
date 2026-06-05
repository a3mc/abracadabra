//! Block production — per-slot detail for our own leader windows.
//!
//! The pane joins the event streams keyed on our `ProduceWindow`
//! ranges to produce honest per-slot status:
//!
//! - `Block (N, hash) parent (…)` — our block emitted
//! - `First shred N`              — first shred for the slot retransmitted
//! - `bank frozen N hash` … sig=K — block banked, K transactions
//! - `Finalized (N, hash) fast`   — finalization arrived (with fast/slow tag)
//! - `Voting skip for N`          — local validator cast a skip vote
//! - `Voting skip-fallback for N` — fallback-round skip vote
//! - `Unable to produce window N-M, skipping window: <reason>` —
//!   validator left its own leader window; the trailing `<reason>`
//!   string is preserved verbatim and displayed as the slot's reason
//!   (e.g. `PohRecorder`).
//!
//! **No inferred reasons.** `Timeout`, `TimeoutCrashedLeader`, and
//! `SafeToSkip` events that fire around our skipped slots are NOT
//! correlated with our skip votes here — the log does not state any
//! of them as the reason for a `Voting skip`, and inferring causality
//! would be unsafe to publish (the operator runs a public validator).
//! The only "reason" surfaced is the verbatim string Solana's own
//! code prints on the `Unable to produce window` line.
//!
//! Per-slot status is derived on render from the captured fields; no
//! status field is stored, so the rule is in one place and adding a
//! new event only requires extending the capture (not a state machine).
//!
//! Layout (top → bottom):
//!
//! - 1 row: spinner + headline. When at least one window has been
//!   observed, the headline shows `bank avg <ms> · sig max <Nk> · sh
//!   max <Nk>` over `Produced | Banked` slots in the retained
//!   windows. No slot counts are surfaced — the cards below carry
//!   the per-slot status directly.
//! - N rows of cards: each card = one window's 4 slots, with a
//!   shared column header `slot bank sigs bcast sh tx`. Status
//!   icons + colours convey produced / banked / banking / skipped /
//!   abandoned / pending.
//!
//! Per-slot row format (fixed widths so multi-digit values do not
//! shift the columns to their right):
//!
//! ```text
//!  [✓] 1234567   45ms  12k   393ms 3k  16k    ← produced
//!  [A] 1234568   —ms   —    —ms   —   —       ← abandoned (stats: no metric)
//!      ⚠ skipped — PoH past slot 1234568      ← card footer, once per window
//! ```
//!
//! Bank time per slot is read directly from the validator's
//! `leader-slot-start-to-cleared-elapsed-ms` metric datapoint, which
//! reports the authoritative leader-slot duration. We do NOT derive
//! it from `First shred N` event timestamps because that event only
//! fires when we *receive* a first shred for slot N (i.e. somebody
//! else produced it); when we are the leader, no such event exists.
//!
//! Spinner advances on event arrival (not wall-clock elapsed), so a
//! stalled stream visibly stops the spin — making it a real liveness
//! signal rather than a screensaver.
//!
//! ## Module layout
//!
//! - [`state`] — `OurSlot`, `OurWindow`, `SlotOutcome`, the
//!   [`LeaderPane`] struct, and its event-observation surface.
//! - [`render`] — ratatui rendering (border, headline, card grid).
//! - [`format`] — per-slot detail formatting and column-width
//!   constants shared between render and tests.

mod format;
mod render;
mod state;

use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::live::animation::Pane;
use crate::parser::Event;

pub use state::LeaderPane;

impl Pane for LeaderPane {
    fn on_event(&mut self, ev: &Event) {
        self.observe_event(ev);
    }

    fn tick(&mut self, _now: Instant) {
        // No wall-clock state to advance; spinner derives from
        // `event_count` / `last_event_at`, both updated in `on_event`.
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        render::render(self, frame, area);
    }
}

#[cfg(test)]
mod tests {
    use super::format::{
        bank_ms, broadcast_ms, format_count_compact, slot_detail_compact, summarize_abandon_reason,
        CARD_ROW_WIDTH, COLUMN_HEADER, DETAIL_WIDTH,
    };
    use super::render::{
        card_alert_line, card_slot_line, slot_icon, MIN_ONE_CARD_WIDTH, MIN_TWO_CARD_WIDTH,
    };
    use super::state::{LeaderPane, OurSlot, SlotOutcome, RECENT_WINDOWS_CAPACITY};
    use crate::live::animation::{
        spinner_frame, Pane, SPINNER_EVENTS_PER_FRAME, SPINNER_FRAME_COUNT, SPINNER_LIVE_WINDOW,
    };
    use crate::parser::{Event, EventKind, MetricEvent};
    use ratatui::style::Color;
    use ratatui::text::Line;
    use std::time::{Duration, Instant};

    fn mk(kind: EventKind) -> Event {
        Event {
            ts: time::OffsetDateTime::UNIX_EPOCH,
            kind,
        }
    }

    fn pw(start: u64, end: u64) -> EventKind {
        EventKind::ProduceWindow {
            start,
            end,
            parent_slot: start.saturating_sub(1),
            parent_hash: "x".into(),
        }
    }

    #[test]
    fn produce_window_creates_pending_slots() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        assert_eq!(p.windows.len(), 1);
        let w = &p.windows[0];
        assert_eq!(w.slots.len(), 4);
        for (i, s) in w.slots.iter().enumerate() {
            assert_eq!(s.slot, 100 + i as u64);
            assert!(matches!(s.status(), SlotOutcome::Pending));
        }
    }

    #[test]
    fn malformed_window_rejected() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(200, 100)));
        p.on_event(&mk(pw(0, u64::MAX)));
        assert_eq!(p.windows.len(), 0);
    }

    #[test]
    fn full_produced_path_sets_status_to_produced_fast() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::BankFrozen {
            slot: 100,
            hash: "y".into(),
            signature_count: 42,
        }));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "y".into(),
            fast: true,
        }));
        let s = &p.windows[0].slots[0];
        assert!(matches!(s.status(), SlotOutcome::Produced { fast: true }));
        assert_eq!(s.sig_count, Some(42));
    }

    #[test]
    fn skip_vote_wins_over_bank_frozen_for_status() {
        // We can locally bank a fork block whose slot the network
        // ultimately skipped — the banking pipeline still emits
        // BankFrozen with a sig_count. The skip vote we cast is the
        // ground truth: "this slot did not produce canonically".
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        // Locally bank slot 100 (could be our fork, not canonical).
        p.on_event(&mk(EventKind::BankFrozen {
            slot: 100,
            hash: "fork".into(),
            signature_count: 67_000,
        }));
        // Cast skip-fallback for the same slot — the canonical chain
        // skipped this slot.
        p.on_event(&mk(EventKind::VotingSkipFallback { slot: 100 }));
        assert!(matches!(
            p.windows[0].slots[0].status(),
            SlotOutcome::Skipped { fallback: true }
        ));
    }

    #[test]
    fn unable_to_produce_window_marks_all_slots_abandoned_with_verbatim_reason() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::UnableToProduceWindow {
            start: 100,
            end: 103,
            reason: "PohRecorder(WindowMovedOn(103))".into(),
        }));
        for s in &p.windows[0].slots {
            assert!(matches!(s.status(), SlotOutcome::Abandoned));
            // Reason text is preserved verbatim from the log line.
            assert_eq!(
                s.abandoned_reason.as_deref(),
                Some("PohRecorder(WindowMovedOn(103))")
            );
        }
    }

    /// Concatenate the text content of a [`Line`] for substring asserts
    /// in render-level tests. Ignores styling — assertion targets are
    /// the literal characters the user reads.
    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn abandoned_slot_renders_a_icon_and_stats_columns_in_row_body() {
        // Pure-abandoned slot (no skip vote): icon is `[A]` and the
        // per-slot row still carries the `bank/sigs/bcast/sh/tx` data
        // columns (rendered as `—` placeholders when no metric arrived).
        // The verbatim abandon reason does NOT appear in the row body
        // — it surfaces once at the card alert footer instead, so a
        // 9-digit mainnet slot in the reason text cannot eat the stats
        // columns and the four otherwise-identical rows do not repeat
        // the same string four times.
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::UnableToProduceWindow {
            start: 100,
            end: 103,
            reason: "WindowMovedOn(103)".into(),
        }));
        let line = card_slot_line(&p.windows[0].slots[0]);
        let text = line_text(&line);
        assert!(
            text.contains("[A]"),
            "abandoned row missing [A] icon: {text:?}"
        );
        // Stats columns must still render — `bank` and `bcast` both
        // surface as `—ms` placeholders when no metric datapoint arrived.
        assert!(
            text.contains("—ms"),
            "abandoned row dropped stats placeholder columns: {text:?}"
        );
        assert!(
            !text.contains("WindowMovedOn"),
            "abandoned reason must not appear in row body (moved to footer): {text:?}"
        );
    }

    #[test]
    fn skip_then_abandon_renders_cross_icon_with_stats_columns() {
        // Skip-vote precedence wins for the icon ([✗]) and the row
        // still renders the data columns. The verbatim abandon reason
        // is surfaced once at the card alert footer, never per-row.
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        for slot in 100..=103 {
            p.on_event(&mk(EventKind::VotingSkipFallback { slot }));
        }
        p.on_event(&mk(EventKind::UnableToProduceWindow {
            start: 100,
            end: 103,
            reason: "WindowMovedOn(103)".into(),
        }));
        let s = &p.windows[0].slots[0];
        // Status remains Skipped (skip-vote precedence).
        assert!(matches!(s.status(), SlotOutcome::Skipped { .. }));
        let line = card_slot_line(s);
        let text = line_text(&line);
        assert!(
            text.contains("[✗]"),
            "skip+abandon row should keep [✗] icon: {text:?}"
        );
        assert!(
            !text.contains("[A]"),
            "skip+abandon row must not use [A] icon: {text:?}"
        );
        assert!(
            text.contains("—ms"),
            "skip+abandon row dropped stats placeholder columns: {text:?}"
        );
        assert!(
            !text.contains("WindowMovedOn"),
            "skip+abandon row must not embed the abandon reason: {text:?}"
        );
    }

    #[test]
    fn summarize_abandon_reason_pattern_matches_observed_solana_variants() {
        // The summarizer must round-trip every variant the operator's
        // log empirically contains. Inputs below are real strings
        // observed in log captures against this validator. Every
        // abandon observed so far has been `WindowMovedOn(N)` with
        // `N` equal to the window's last slot; the `MaxHeightReached`
        // variant has not been observed but is kept because it is
        // documented in Solana source and we want a sensible label
        // if it ever fires; if Solana emits a new variant we fall
        // through to verbatim rather than silently losing it.
        assert_eq!(
            summarize_abandon_reason("PohRecorder(WindowMovedOn(336507))"),
            "PoH moved on"
        );
        assert_eq!(
            summarize_abandon_reason("PohRecorder(WindowMovedOn(282583))"),
            "PoH moved on"
        );
        assert_eq!(
            summarize_abandon_reason("PohRecorder(MaxHeightReached)"),
            "max height reached"
        );
        // Other PohRecorder variants — wrapper stripped, inner kept.
        assert_eq!(
            summarize_abandon_reason("PohRecorder(SomeNewVariant)"),
            "SomeNewVariant"
        );
        // Anything outside the PohRecorder wrap is passed through so
        // future non-PoH reason families are still legible.
        assert_eq!(
            summarize_abandon_reason("SomeFutureError"),
            "SomeFutureError"
        );
    }

    #[test]
    fn summarize_abandon_reason_rejects_non_digit_inner_payload() {
        // TEST-03 regression: the `WindowMovedOn(N)` collapse only
        // applies when `N` is the empirically-observed digits-only
        // slot number. A future Solana variant emitting a nested
        // structure (e.g. `WindowMovedOn(Nested(123))`) must NOT
        // resolve to "PoH moved on" — that would be a lie. Fall
        // through to the wrapper-stripped path instead so the
        // operator sees the genuine inner debug text.
        assert_eq!(
            summarize_abandon_reason("PohRecorder(WindowMovedOn(Nested(123)))"),
            "WindowMovedOn(Nested(123))",
            "nested payload must not match the digits-only collapse"
        );
        // Empty inner payload also rejected — N is mandatory.
        assert_eq!(
            summarize_abandon_reason("PohRecorder(WindowMovedOn())"),
            "WindowMovedOn()"
        );
        // Mixed alphanumeric inner — rejected.
        assert_eq!(
            summarize_abandon_reason("PohRecorder(WindowMovedOn(12a3))"),
            "WindowMovedOn(12a3)"
        );
    }

    #[test]
    fn poh_window_moved_on_summarises_to_short_phrase_in_footer() {
        // Solana's `Unable to produce window` log emits the `Debug`
        // form of a PohRecorder error. The footer translates the only
        // variant observed in capture — `WindowMovedOn(N)` — to a
        // short phrase. The slot `N` has always equalled the window's
        // last slot in every abandon observed so far, so we do NOT
        // echo it in the footer: the slot is already on screen in the
        // last row's slot column. Solana's own log line reads
        // "skipping window: <reason>"; we mirror that with "skipped"
        // and never use the internal-only "abandoned" vocabulary.
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(300_123_456, 300_123_459)));
        p.on_event(&mk(EventKind::UnableToProduceWindow {
            start: 300_123_456,
            end: 300_123_459,
            reason: "PohRecorder(WindowMovedOn(300123459))".into(),
        }));
        let w = &p.windows[0];
        let footer = card_alert_line(w).expect("footer must fire for an abandoned window");
        let footer_text = line_text(&footer);
        assert!(
            footer_text.contains("skipped — PoH moved on"),
            "footer should read 'skipped — PoH moved on' for WindowMovedOn: {footer_text:?}"
        );
        assert!(
            !footer_text.contains("PohRecorder"),
            "footer must not leak raw error Debug output: {footer_text:?}"
        );
        assert!(
            !footer_text.contains("300123459"),
            "footer must not echo the redundant WindowMovedOn slot — \
             the row already shows it: {footer_text:?}"
        );
        assert!(
            !footer_text.contains("abandoned"),
            "footer must use Solana 'skipped' vocabulary, not 'abandoned': {footer_text:?}"
        );
    }

    #[test]
    fn drops_counter_footer_uses_descriptive_label_not_bare_drops() {
        // The `num_dropped_on_capacity` field, per the parser's own
        // docstring, counts "txns the scheduler had to drop because
        // its buffer was full". The footer label says exactly that —
        // not the bare "drops" mnemonic the operator complained
        // about, and not a vague "over capacity" that obscures which
        // capacity. Observed values on this validator have stayed
        // well under the `Nk` compaction threshold; this test uses a
        // deliberately small value so the assertion shape matches
        // everyday operation.
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::Metric(MetricEvent::BankingStageCounts {
            slot: 100,
            num_dropped_on_capacity: 187,
            num_finished: 0,
        })));
        let footer = card_alert_line(&p.windows[0])
            .expect("footer must fire on nonzero num_dropped_on_capacity");
        let text = line_text(&footer);
        assert!(
            text.contains("187 tx dropped (scheduler full)"),
            "drops segment should read '187 tx dropped (scheduler full)': {text:?}"
        );
        assert!(
            !text.contains("drops "),
            "footer must not use the bare 'drops' mnemonic: {text:?}"
        );
    }

    #[test]
    fn produced_slot_renders_data_columns_not_reason() {
        // Negative test: a produced slot with no abandon signal must
        // render the normal `bank/sigs/bcast/sh/fin` columns. Catches
        // a refactor that accidentally always-takes the reason branch.
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::Metric(MetricEvent::LeaderSlotElapsed {
            slot: 100,
            elapsed_ms: 400,
        })));
        p.on_event(&mk(EventKind::BankFrozen {
            slot: 100,
            hash: "h".into(),
            signature_count: 12_000,
        }));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "h".into(),
            fast: true,
        }));
        let line = card_slot_line(&p.windows[0].slots[0]);
        let text = line_text(&line);
        assert!(text.contains("[✓]"), "produced row missing icon: {text:?}");
        assert!(
            text.contains("400ms"),
            "produced row missing bank field: {text:?}"
        );
    }

    #[test]
    fn skipped_status_does_not_infer_reason_from_surrounding_events() {
        // Even with Timeout / SafeToSkip / TimeoutCrashedLeader events
        // observed in the surrounding stream, the status is `Skipped`
        // without any reason label. The log does NOT state these as
        // the reason for our vote — claiming them would be inference.
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::Timeout { slot: 101 }));
        p.on_event(&mk(EventKind::VotingSkip { slot: 101 }));
        p.on_event(&mk(EventKind::SafeToSkip { slot: 102 }));
        p.on_event(&mk(EventKind::VotingSkip { slot: 102 }));
        p.on_event(&mk(EventKind::TimeoutCrashedLeader { slot: 103 }));
        p.on_event(&mk(EventKind::VotingSkip { slot: 103 }));
        assert!(matches!(
            p.windows[0].slots[1].status(),
            SlotOutcome::Skipped { fallback: false }
        ));
        assert!(matches!(
            p.windows[0].slots[2].status(),
            SlotOutcome::Skipped { fallback: false }
        ));
        assert!(matches!(
            p.windows[0].slots[3].status(),
            SlotOutcome::Skipped { fallback: false }
        ));
    }

    #[test]
    fn bank_ms_comes_from_leader_slot_elapsed_metric() {
        // The validator emits the authoritative leader-slot duration as
        // a metric datapoint; the per-slot bank time must come from
        // there, not from event-timestamp subtraction (which is
        // impossible for our slots — see [`OurSlot::leader_elapsed_ms`]).
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::Metric(MetricEvent::LeaderSlotElapsed {
            slot: 100,
            elapsed_ms: 400,
        })));
        assert_eq!(bank_ms(&p.windows[0].slots[0]), Some(400));
    }

    #[test]
    fn summary_tracks_bank_avg_and_sig_max_over_produced_slots() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        // Two produced slots with bank ms + sig counts; mean = 350,
        // max sig = 19000.
        for (slot, elapsed, sigs) in [(100u64, 400u64, 12_000u64), (101, 300, 19_000)] {
            p.on_event(&mk(EventKind::Metric(MetricEvent::LeaderSlotElapsed {
                slot,
                elapsed_ms: elapsed,
            })));
            p.on_event(&mk(EventKind::BankFrozen {
                slot,
                hash: "h".into(),
                signature_count: sigs,
            }));
            p.on_event(&mk(EventKind::Finalized {
                slot,
                hash: "h".into(),
                fast: true,
            }));
        }
        let s = p.summary();
        assert!(s.has_windows);
        assert_eq!(s.bank_ms_avg, Some(350));
        assert_eq!(s.sig_max, Some(19_000));
    }

    #[test]
    fn format_count_compact_clamps_to_k_only() {
        assert_eq!(format_count_compact(42), "42");
        assert_eq!(format_count_compact(999), "999");
        assert_eq!(format_count_compact(1_000), "1k");
        assert_eq!(format_count_compact(43_000), "43k");
        assert_eq!(format_count_compact(999_999), "999k");
        // No `m` bucket — million-scale renders as multi-digit `Nk`.
        assert_eq!(format_count_compact(1_500_000), "1500k");
    }

    #[test]
    fn column_header_width_matches_rendered_row_width() {
        // FMT-01 fragility guard: `COLUMN_HEADER` is hand-aligned to
        // the per-slot row format string `{bank}ms {sigs}  {bcast}ms
        // {shreds}  {tx}` plus the row prefix " {icon} {slot_field}".
        // Width constants live at module scope so the single-card
        // fallback in `render_windows` can reuse them. Any change to
        // a field width must be mirrored in the header literal; this
        // test fires loud if the two drift.
        assert_eq!(
            COLUMN_HEADER.chars().count(),
            CARD_ROW_WIDTH,
            "COLUMN_HEADER drifted from the rendered row width — \
             update the literal to match the field-width constants",
        );
    }

    #[test]
    fn voting_skip_fallback_sets_fallback_flag_in_status() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::VotingSkipFallback { slot: 100 }));
        assert!(matches!(
            p.windows[0].slots[0].status(),
            SlotOutcome::Skipped { fallback: true }
        ));
    }

    #[test]
    fn broadcast_shreds_populates_bcast_and_shred_fields() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::Metric(MetricEvent::BroadcastShreds {
            slot: 100,
            broadcast_us: Some(393_182),
            num_data_shreds: 3200,
        })));
        let s = &p.windows[0].slots[0];
        assert_eq!(broadcast_ms(s), Some(393));
        assert_eq!(s.num_data_shreds, Some(3200));
    }

    #[test]
    fn broadcast_interrupted_records_partial_shreds_but_no_broadcast_time() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        // Interrupted variant: `broadcast_us = None` is the sole
        // signal of mid-broadcast abandonment (matches the parser's
        // `slot_broadcast_time=-1` handling).
        p.on_event(&mk(EventKind::Metric(MetricEvent::BroadcastShreds {
            slot: 103,
            broadcast_us: None,
            num_data_shreds: 2240,
        })));
        let s = &p.windows[0].slots[3];
        assert_eq!(broadcast_ms(s), None);
        assert_eq!(s.num_data_shreds, Some(2240));
    }

    #[test]
    fn events_outside_any_window_are_ignored_silently() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(EventKind::Block {
            slot: 9999,
            hash: "h".into(),
            parent_slot: 9998,
            parent_hash: "p".into(),
        }));
        assert!(p.windows.is_empty());
    }

    #[test]
    fn unable_to_produce_outside_window_is_silent_no_op() {
        let mut p = LeaderPane::new();
        // No matching ProduceWindow seen — this can happen if the log
        // tail started mid-stream after the leader window event.
        p.on_event(&mk(EventKind::UnableToProduceWindow {
            start: 100,
            end: 103,
            reason: "x".into(),
        }));
        assert!(p.windows.is_empty());
    }

    #[test]
    fn unable_to_produce_with_oversized_span_is_rejected_no_iteration() {
        // Regression for WIN-02. Even if a corrupted event somehow
        // bypasses the parser (e.g. constructed directly in a future
        // ingest path), the pane must not iterate the `start..=end`
        // range and must not mutate state. The cap is the same as
        // the parser's: `MAX_LEADER_WINDOW_SPAN`. The bounded loop
        // here is a smoke test — if the defence is missing, this test
        // would otherwise hang/spin on a 2^64 iteration.
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        let pre_slot = p.windows[0].slots[0].clone();
        p.on_event(&mk(EventKind::UnableToProduceWindow {
            start: 0,
            end: u64::MAX,
            reason: "x".into(),
        }));
        // No slot mutated; pane state stable.
        assert_eq!(p.windows.len(), 1);
        assert!(p.windows[0].slots[0].abandoned_at.is_none());
        assert!(p.windows[0].slots[0].abandoned_reason.is_none());
        assert_eq!(p.windows[0].slots[0].slot, pre_slot.slot);
    }

    #[test]
    fn windows_overflow_drops_oldest() {
        let mut p = LeaderPane::new();
        for i in 0..(RECENT_WINDOWS_CAPACITY as u64 + 2) {
            let start = 100 + i * 4;
            p.on_event(&mk(pw(start, start + 3)));
        }
        assert_eq!(p.windows.len(), RECENT_WINDOWS_CAPACITY);
        // First retained window's start advanced by 2*4.
        assert_eq!(p.windows.front().unwrap().start, 100 + 2 * 4);
    }

    #[test]
    fn spinner_index_advances_with_events_and_freezes_when_quiet() {
        let mut p = LeaderPane::new();
        for _ in 0..(SPINNER_EVENTS_PER_FRAME * SPINNER_FRAME_COUNT as u64) {
            p.on_event(&mk(EventKind::FirstShred { slot: 1 }));
        }
        assert_eq!(
            p.event_count,
            SPINNER_EVENTS_PER_FRAME * SPINNER_FRAME_COUNT as u64
        );
        // Back-date last_event_at past the live window — spinner frame
        // should pin to the idle position (first frame).
        p.last_event_at = Some(
            Instant::now()
                .checked_sub(SPINNER_LIVE_WINDOW + Duration::from_millis(50))
                .unwrap(),
        );
        // Idle: shared helper returns the first frame.
        let idle = spinner_frame(p.event_count, p.last_event_at);
        let fresh = spinner_frame(0, Some(Instant::now()));
        assert_eq!(idle, fresh, "idle spinner should pin to first frame");
    }

    #[test]
    fn column_header_uses_tx_not_fin() {
        // UX-13 regression: header column for `num_finished` was
        // renamed from `fin` to `tx` to avoid clashing with the
        // dominant codebase meaning of "finalized".
        assert!(
            COLUMN_HEADER.contains(" tx"),
            "header missing `tx` column: {COLUMN_HEADER:?}"
        );
        assert!(
            !COLUMN_HEADER.contains(" fin"),
            "header still carries deprecated `fin` label: {COLUMN_HEADER:?}"
        );
    }

    #[test]
    fn card_row_width_constants_match_header_literal() {
        // UX-01 / FMT-01 cross-check: the module-scope
        // `CARD_ROW_WIDTH` drives both the COLUMN_HEADER width guard
        // and the single-card / two-card fallback thresholds used in
        // `render_windows`. They must stay equal.
        assert_eq!(CARD_ROW_WIDTH, COLUMN_HEADER.chars().count());
        assert_eq!(MIN_ONE_CARD_WIDTH as usize, CARD_ROW_WIDTH);
        assert_eq!(MIN_TWO_CARD_WIDTH as usize, CARD_ROW_WIDTH * 2 + 1);
    }

    fn line_concat_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn alert_footer_labels_bad_handover_not_sad() {
        // UX-05: the display label was `sad` (opaque Solana metric
        // name) and is now `bad handover`. Underlying field name
        // stays `leader_handover_sad` so grep parity holds.
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::Metric(MetricEvent::SlotMetrics {
            slot: 100,
            leader_handover_sad: true,
            replay_is_behind_count: 0,
        })));
        let line = card_alert_line(&p.windows[0]).expect("alert footer should fire");
        let text = line_concat_text(&line);
        assert!(
            text.contains("1 bad handover"),
            "footer missing `bad handover`: {text:?}"
        );
        assert!(
            !text.contains("sad "),
            "footer still uses opaque `sad` label: {text:?}"
        );
    }

    #[test]
    fn slot_detail_compact_matches_detail_width_across_value_shapes() {
        // TEST-01: the per-slot detail body is load-bearing for the
        // 44-col card-row layout (see `CARD_ROW_WIDTH`). Fixed-width
        // sub-fields must stay aligned regardless of value shape.
        //
        // Note `DETAIL_WIDTH` is the *minimum* — 4-digit `bank` /
        // `bcast` values legitimately extend one column wider per the
        // accepted-drift comment on `COLUMN_HEADER`. So we assert
        // `>= DETAIL_WIDTH` rather than equality, with the equality
        // case checked on the steady-state 3-digit shape.
        let all_none = OurSlot::default();
        let three_digit_ms = OurSlot {
            leader_elapsed_ms: Some(456),
            sig_count: Some(19_000),
            broadcast_us: Some(393_000),
            num_data_shreds: Some(3_200),
            num_finished: Some(8_400),
            ..OurSlot::default()
        };
        let four_digit_ms = OurSlot {
            leader_elapsed_ms: Some(9_999),
            sig_count: Some(999_999),
            broadcast_us: Some(9_999_000),
            num_data_shreds: Some(999_999),
            num_finished: Some(999_999),
            ..OurSlot::default()
        };
        // `chars().count()` over `Some(123)` / `None` paths must hit
        // the documented detail width exactly on the steady-state
        // shape — no early truncation, no padding drift.
        assert_eq!(slot_detail_compact(&all_none).chars().count(), DETAIL_WIDTH);
        assert_eq!(
            slot_detail_compact(&three_digit_ms).chars().count(),
            DETAIL_WIDTH
        );
        // 4-digit ms / Nk values are allowed to grow but only by the
        // overflow width — assert each is at least the documented
        // minimum.
        assert!(slot_detail_compact(&four_digit_ms).chars().count() >= DETAIL_WIDTH);
    }

    #[test]
    fn slot_icon_banked_uses_dim_green_for_visual_contrast() {
        // TERM-01: `[~]` Banked was yellow (same as `[…]` Banking),
        // making visual scan ambiguous. Banked now uses dim green —
        // still distinguishable from `[✓]` Produced (bold/bright green)
        // and from `[…]` Banking (yellow).
        let (banked_glyph, banked_style) = slot_icon(SlotOutcome::Banked);
        let (banking_glyph, banking_style) = slot_icon(SlotOutcome::Banking);
        assert_eq!(banked_glyph, "[~]");
        assert_eq!(banking_glyph, "[…]");
        assert_eq!(banked_style.fg, Some(Color::Green));
        assert_eq!(banking_style.fg, Some(Color::Yellow));
    }

    // ---- since-last-block headline timer ---------------------------

    #[test]
    fn summary_last_produced_at_is_none_when_no_slot_produced() {
        // LIVE-60: empty pane and pane with windows-only-but-not-yet-
        // produced both leave `last_produced_at` as `None`. The
        // render path keys off this to omit the segment, sidestepping
        // duplication with the "waiting for first leader window"
        // line that already covers the empty case.
        let empty = LeaderPane::new();
        assert!(empty.summary().last_produced_at.is_none());

        let mut pending = LeaderPane::new();
        pending.on_event(&mk(pw(100, 103)));
        // ProduceWindow alone — no BankFrozen yet — leaves slots in
        // Pending state, so `last_produced_at` stays None.
        assert!(pending.summary().last_produced_at.is_none());
    }

    #[test]
    fn summary_last_produced_at_tracks_latest_bank_frozen_across_slots() {
        // LIVE-60: pick the MAX bank_frozen_at across produced/banked
        // slots so the headline reflects the most-recent block, not
        // an arbitrary one.
        use time::Duration as TimeDuration;
        let t0 = time::OffsetDateTime::UNIX_EPOCH;
        let t_later = t0 + TimeDuration::seconds(30);

        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&Event {
            ts: t0,
            kind: EventKind::BankFrozen {
                slot: 100,
                hash: "a".into(),
                signature_count: 100,
            },
        });
        p.on_event(&Event {
            ts: t_later,
            kind: EventKind::BankFrozen {
                slot: 101,
                hash: "b".into(),
                signature_count: 200,
            },
        });
        assert_eq!(p.summary().last_produced_at, Some(t_later));
    }

    #[test]
    fn format_since_uses_single_unit_per_magnitude() {
        use super::render::format_since;
        assert_eq!(format_since(0), "0s");
        assert_eq!(format_since(59), "59s");
        assert_eq!(format_since(60), "1m");
        assert_eq!(format_since(3_599), "59m");
        assert_eq!(format_since(3_600), "1h");
        assert_eq!(format_since(86_399), "23h");
        assert_eq!(format_since(86_400), "1d");
        // Negative is clamped at the call site; helper should not
        // panic if a negative ever slips through.
        assert_eq!(format_since(-1), "-1s");
    }
}
