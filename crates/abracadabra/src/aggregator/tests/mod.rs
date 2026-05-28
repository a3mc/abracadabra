//! Aggregator unit tests. Split across focused sub-modules so no
//! single file crosses the ~500 LOC soft target; helpers live here
//! and submodules pull them in via `use super::...`.
//!
//! Sub-modules:
//!   - `ingest`         — basic event-handler ingest paths + inline alerts
//!   - `standstill`     — `standstill_ranges` build-up + TCL filter
//!   - `classify_skips` — `aggregator::classify_skips` paths
//!   - `log_patterns`   — `ingest_issue` / `record_log_pattern` /
//!     `surface_log_pattern_alerts` family
//!   - `produce_window` — `EventKind::ProduceWindow` corruption guards
//!   - `analyze_alerts` — `analyze` post-pass: sort order, dedup,
//!     local-leader-summary gating

mod analyze_alerts;
mod classify_skips;
mod ingest;
mod log_patterns;
mod produce_window;
mod standstill;

use super::*;
use crate::parser::{self, Parsed};

/// Parse a log line and ingest the resulting `Event` if one was produced.
/// Silently ignores `Parsed::Continuation` / `Parsed::Ignored` /
/// `Parsed::Issue` — submodules that care about the latter use
/// `parse_and_ingest_issue` directly.
pub(super) fn parse_and_ingest(state: &mut State, line: &str) {
    let parsed = parser::parse(line).expect("parse");
    if let Parsed::Event(ev) = parsed {
        ingest(state, ev);
    }
}

/// Parse a log line that is expected to yield a `Parsed::Issue` and
/// route it through `ingest_issue`. Panics if the parse did not
/// produce an Issue — this is a test helper, the asymmetry vs
/// `parse_and_ingest` is intentional.
pub(super) fn parse_and_ingest_issue(state: &mut State, line: &str) {
    let parsed = parser::parse(line).expect("parse");
    if let Parsed::Issue {
        ts,
        level,
        module,
        body,
    } = parsed
    {
        ingest_issue(state, ts, level, module, body);
    } else {
        panic!("expected Parsed::Issue, got {parsed:?}");
    }
}
