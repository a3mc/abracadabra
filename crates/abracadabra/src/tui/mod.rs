//! Interactive ratatui dashboard.
//!
//! Layout (single dashboard, no tabs in v0.1):
//!
//! ```text
//! ┌─ header: file info, time range, headline verdicts ─┐
//! ├──────────────────────────────────────────────────────┤
//! │ time-series sparklines (6 metrics over time)         │
//! ├──────────────────────────────────────────────────────┤
//! │ histogram │ top-slowest + top-resume   │ alerts      │
//! ├──────────────────────────────────────────────────────┤
//! │ status bar: keys                                     │
//! └──────────────────────────────────────────────────────┘
//! ```
//!
//! Keys: `q`/`Esc` quit; everything else ignored for v0.1 (focus stays on
//! the whole screen — no inner panel focus yet).

mod app;
mod panel;
mod theme;
mod view;
mod widget;

use std::io;

use thiserror::Error;

use crate::model::buckets::TimeBuckets;
use crate::model::state::State;

#[derive(Debug, Error)]
pub enum TuiError {
    #[error("terminal I/O: {0}")]
    Io(#[from] io::Error),
}

/// Enter the dashboard. Blocks until the user quits.
///
/// `bucket_secs` is the time-series bucket size (validated by the CLI
/// parser; bounds enforced there).
pub fn run(state: &State, bucket_secs: i64) -> Result<(), TuiError> {
    let buckets = TimeBuckets::from_state(state, bucket_secs);
    app::run(state, buckets.as_ref(), bucket_secs)
}
