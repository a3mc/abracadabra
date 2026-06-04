//! `solana_core::block_creation_loop` — leader-side block-creation errors.
//!
//! One ERROR line shape observed to date:
//!
//! | Line                                                                     | Event                  |
//! |--------------------------------------------------------------------------|------------------------|
//! | `Unable to produce window N-M, skipping window: PohRecorder(WindowMovedOn(M))` | `UnableToProduceWindow` |
//!
//! The body arrives with a `<pubkey>: ` prefix (same shape as
//! `agave_votor::event_handler`); we strip it before matching.

use std::sync::OnceLock;

use regex::Regex;

use crate::aggregator::MAX_LEADER_WINDOW_SPAN;
use crate::parser::{must_compile, EventKind, SLOT_DIGITS};

pub fn parse_body(body: &str) -> Option<EventKind> {
    let (_pubkey, event) = body.split_once(": ")?;
    parse_unable_to_produce_window(event)
}

fn parse_unable_to_produce_window(event: &str) -> Option<EventKind> {
    let caps = re_unable_to_produce_window().captures(event)?;
    let start: u64 = caps.get(1)?.as_str().parse().ok()?;
    let end: u64 = caps.get(2)?.as_str().parse().ok()?;
    let reason = caps.get(3)?.as_str().trim().to_owned();
    // Corruption guard: same rationale as `MAX_LEADER_WINDOW_SPAN` on
    // `EventKind::ProduceWindow` in the aggregator. A truncated end digit
    // (e.g. `0-18446744073709551615`) would otherwise let downstream
    // consumers iterate an unbounded `start..=end` range.
    if end < start || end.saturating_sub(start) > MAX_LEADER_WINDOW_SPAN {
        return None;
    }
    Some(EventKind::UnableToProduceWindow { start, end, reason })
}

fn re_unable_to_produce_window() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        must_compile(&format!(
            r"^Unable to produce window ({SLOT_DIGITS})-({SLOT_DIGITS}), skipping window: (.+)$"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PK: &str = "ALNSCyaSLbRDwmFcGoBV1irHDKPgRxZjfNTex9HPvkWu";

    fn body(s: &str) -> String {
        format!("{PK}: {s}")
    }

    #[test]
    fn unable_to_produce_window_poh_recorder() {
        let s = body("Unable to produce window 282580-282583, skipping window: PohRecorder(WindowMovedOn(282583))");
        let ev = parse_body(&s).unwrap();
        match ev {
            EventKind::UnableToProduceWindow { start, end, reason } => {
                assert_eq!(start, 282_580);
                assert_eq!(end, 282_583);
                assert_eq!(reason, "PohRecorder(WindowMovedOn(282583))");
            }
            other => panic!("expected UnableToProduceWindow, got {other:?}"),
        }
    }

    #[test]
    fn unable_to_produce_window_preserves_arbitrary_reason() {
        // The reason text is the verbatim trailing string. Future agave
        // versions may use different error variants; we should still capture them.
        let s = body("Unable to produce window 100-103, skipping window: SomeFutureVariant(Boom)");
        let ev = parse_body(&s).unwrap();
        match ev {
            EventKind::UnableToProduceWindow { reason, .. } => {
                assert_eq!(reason, "SomeFutureVariant(Boom)");
            }
            other => panic!("expected UnableToProduceWindow, got {other:?}"),
        }
    }

    #[test]
    fn inverted_range_rejected() {
        let s = body(
            "Unable to produce window 100-99, skipping window: PohRecorder(WindowMovedOn(99))",
        );
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn unbounded_range_rejected() {
        // Regression for WIN-02. A truncated end digit (`u64::MAX`) on
        // an `Unable to produce window` line must be rejected by the
        // parser. Without this cap, downstream consumers that iterate
        // `start..=end` would block on a 2^64-element range.
        let s = body("Unable to produce window 0-18446744073709551615, skipping window: Boom");
        assert!(parse_body(&s).is_none());
    }

    #[test]
    fn span_at_max_accepted_span_above_max_rejected() {
        // Sanity boundary: a span of exactly `MAX_LEADER_WINDOW_SPAN`
        // is accepted; one slot wider is rejected.
        let s_ok = body(&format!(
            "Unable to produce window 100-{}, skipping window: x",
            100 + MAX_LEADER_WINDOW_SPAN
        ));
        assert!(parse_body(&s_ok).is_some());
        let s_bad = body(&format!(
            "Unable to produce window 100-{}, skipping window: x",
            100 + MAX_LEADER_WINDOW_SPAN + 1
        ));
        assert!(parse_body(&s_bad).is_none());
    }

    #[test]
    fn missing_pubkey_rejected() {
        // No `<pubkey>: ` prefix → parser refuses to descend.
        assert!(parse_body("Unable to produce window 100-103, skipping window: x").is_none());
    }

    #[test]
    fn unrelated_body_returns_none() {
        let s = body("Some other ERROR text we don't model");
        assert!(parse_body(&s).is_none());
    }
}
