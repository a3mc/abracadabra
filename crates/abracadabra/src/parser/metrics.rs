//! `solana_metrics::metrics` datapoint extraction (selective).
//!
//! v0.1 cares about `event_handler_received_event_count_and_timing` and
//! `block-commitment-cache`. Everything else is `None`.
//!
//! Implementation deferred to v0.3.

use crate::parser::EventKind;

pub const fn parse_body(_body: &str) -> Option<EventKind> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v0.1 stub: every input yields `None` until v0.3 lands. Pins the
    /// current contract so the stub cannot quietly grow a partial parser.
    #[test]
    fn stub_returns_none_for_all_inputs() {
        assert!(parse_body("").is_none());
        assert!(parse_body("datapoint: event_handler_received_event_count_and_timing").is_none());
        assert!(parse_body("datapoint: block-commitment-cache").is_none());
        assert!(parse_body("arbitrary nonsense body").is_none());
    }
}
