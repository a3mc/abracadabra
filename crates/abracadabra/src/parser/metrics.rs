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
