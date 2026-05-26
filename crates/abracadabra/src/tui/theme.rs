//! Centralised colour and style constants.

use ratatui::style::{Color, Modifier, Style};

pub const FG: Color = Color::White;
pub const DIM: Color = Color::DarkGray;
pub const ACCENT: Color = Color::Cyan;
pub const OK: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const BAD: Color = Color::Red;

// Sparkline / plot palette — semantic, not arbitrary:
//   - `SPARK_HEALTH`     : metrics where HIGH = good (fast-finalize %, leader windows)
//   - `SPARK_TIME`       : neutral time/latency values (lifecycle, vote-resume)
//   - `SPARK_PROBLEM`    : counts that signal trouble (skip cascades, crashed leaders,
//                          fragmentation events)
//   - `SPARK_ALT_PATH`   : "successful but not optimal" — used for the slow finalize
//                          share. Distinct from `SPARK_PROBLEM` (yellow) because slow
//                          finalize is still a successful finalization, just via the
//                          2-round path instead of the FastFinalize cert.
pub const SPARK_HEALTH: Color = Color::Green;
pub const SPARK_TIME: Color = Color::Cyan;
pub const SPARK_PROBLEM: Color = Color::Yellow;
pub const SPARK_ALT_PATH: Color = Color::Blue;

pub fn title_style() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn label_style() -> Style {
    Style::default().fg(DIM)
}

pub fn good_style() -> Style {
    Style::default().fg(OK)
}

pub fn warn_style() -> Style {
    Style::default().fg(WARN)
}

pub fn bad_style() -> Style {
    Style::default().fg(BAD)
}

pub fn value_style() -> Style {
    Style::default().fg(FG)
}

/// Gentle cyan accent — used to mark "headline" values (e.g. p50 across
/// percentile rows, the time-range duration in the Overview header)
/// without the visual weight of bold or the semantic load of green.
/// Green stays reserved for true health/threshold indicators
/// (fast-finalize %, severity::Normal).
pub fn accent_style() -> Style {
    Style::default().fg(ACCENT)
}

// ---------- Health-band thresholds (Alpenglow-specific) ----------
//
// Centralised so every panel agrees on what counts as good/warn/bad.
// Picked against protocol invariants rather than aesthetic guesses:

/// Fast-finalize % of finalized slots. 80% is the `FinalizeFast` cert
/// stake boundary: above it, fast-path finalization dominates. Below
/// 60% the cluster is leaning on slow finalization for most slots,
/// which is a load/reachability signal worth escalating to red.
pub const FAST_FIN_GOOD_PCT: f64 = 80.0;
pub const FAST_FIN_WARN_PCT: f64 = 60.0;

/// Any-path FIN % across all observed slots. ≥90% expected in a
/// healthy cluster; <80% means a meaningful fraction of slots is
/// stalling or being skipped.
pub const FIN_GOOD_PCT: f64 = 90.0;
pub const FIN_WARN_PCT: f64 = 80.0;

/// Local vote-skip rate (this validator) as a share of observed slots.
/// Sustained skip rates above 15% indicate this node is consistently
/// not observing leader blocks in time.
pub const VOTE_SKIP_WARN_PCT: f64 = 5.0;
pub const VOTE_SKIP_BAD_PCT: f64 = 15.0;

/// Per-slot assembly time (first_shred → block_emitted) in ms. Baseline
/// sits ≈ 450 ms in a healthy 21h log; 500 ms is the visible-spike
/// floor (shred propagation / replay bottlenecks); >600 ms is well
/// past the 400 ms slot target and starts crowding the next slot.
pub const ASSEMBLY_WARN_MS: f64 = 500.0;
pub const ASSEMBLY_BAD_MS: f64 = 600.0;

/// Per-slot lifecycle time (first_shred → finalized) in ms. 600 ms is
/// the healthy-cluster line; >1000 ms is degraded.
pub const LIFECYCLE_WARN_MS: f64 = 600.0;
pub const LIFECYCLE_BAD_MS: f64 = 1000.0;

/// Three-tier band for "higher is better" metrics (fast-finalize %,
/// FIN %, anything where larger values are healthier).
pub fn band_higher_better(v: f64, good_at: f64, warn_at: f64) -> Style {
    if v >= good_at {
        good_style()
    } else if v >= warn_at {
        warn_style()
    } else {
        bad_style()
    }
}

/// Three-tier band for "lower is better" metrics (skip rate %, error
/// counts as a percentage of activity). `warn_at` is the green→yellow
/// crossover; `bad_at` is the yellow→red crossover.
pub fn band_lower_better(v: f64, warn_at: f64, bad_at: f64) -> Style {
    if v < warn_at {
        good_style()
    } else if v < bad_at {
        warn_style()
    } else {
        bad_style()
    }
}
