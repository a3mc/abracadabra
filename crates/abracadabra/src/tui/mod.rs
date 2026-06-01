//! Interactive ratatui dashboard.
//!
//! Tab layout is decided at startup from the input log's activity
//! classification. Static logs see the original 6 tabs at `1`-`6` with
//! their original key bindings; active logs get an extra `Live` tab
//! prepended at index 0, shifting the rest to `2`-`7`. Static-log
//! workflows are unchanged. The event loop, key dispatch, and `App`
//! state live in `app.rs`; each tab has its own `panel::*` module that
//! takes a `&App` and renders into a sub-rect.
//!
//! Tabs (active layout — static omits `Live`):
//!
//! 1. Live — real-time follow surface (opt-in). Present only when the
//!    input log is currently being written to; `SPACEBAR` starts
//!    following. Animation engine: pending.
//! 2. Overview — stats-only summary: file meta, headline health,
//!    vote/cert totals, latency stages, vote-resume stats, alerts.
//! 3. Time series — 2-column card grid of sparklines, shared x-axis
//!    across cards.
//! 4. Windows — rolling-window comparison table (`all`, 24h, 12h, 6h,
//!    3h, 1h).
//! 5. Slots — KPI strip + dense scrollable slot table with column
//!    filters (`t/n/p` TCL/S2N/S2S, `l/f/s` leader/fast/slow,
//!    `v/c` VSKIP/CSKIP, `x` clear).
//! 6. Leader timeouts — TCL/vote-resume KPIs, distribution histogram,
//!    per-bucket trend, incident list.
//! 7. Alerts — severity rollup + scrollable list + detail pane with
//!    sparkline; `y` yanks current alert to a per-user file.
//!
//! Common keys: `j`/`k` / arrows scroll, `PgUp`/`PgDn` page, `g`/`G` /
//! `Home`/`End` jump. `q` / `Esc` quit. Scroll keys are no-ops on tabs
//! without scrollable lists. Per-tab keys are documented in the bottom
//! status bar (`panel::status_bar`).
//!
//! Default tab is Live when the input log was detected as active at
//! startup; Overview when static (so the user is not stuck on a gray
//! placeholder when running historical analysis).

mod app;
mod panel;
// `theme` exposes operator-facing threshold constants
// (`CANONICAL_SKIP_*_PCT`, `TRUE_FB_ELEVATED_PCT`, latency bands) that
// `runner::print_summary` also consumes to keep the text and TUI
// verdicts in lockstep.
pub(crate) mod theme;
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
pub fn run(
    state: &State,
    bucket_secs: i64,
    activity: crate::live::detect::Activity,
) -> Result<(), TuiError> {
    let buckets = TimeBuckets::from_state(state, bucket_secs);
    app::run(state, buckets.as_ref(), bucket_secs, activity)
}
