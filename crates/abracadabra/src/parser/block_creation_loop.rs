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

use crate::parser::{must_compile, EventKind, SLOT_DIGITS};

pub fn parse_body(body: &str) -> Option<EventKind> {
    let (_pubkey, event) = body.split_once(": ")?;
    parse_unable_to_produce_window(event)
}

fn parse_unable_to_produce_window(event: &str) -> Option<EventKind> {
    let caps = re_unable_to_produce_window().captures(event)?;
    let start = caps.get(1)?.as_str().parse().ok()?;
    let end = caps.get(2)?.as_str().parse().ok()?;
    let reason = caps.get(3)?.as_str().trim().to_owned();
    if end < start {
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
