//! Tab "Windows": side-by-side comparison of the same metrics over rolling
//! time windows. Columns are `all`, `24h`, `12h`, `6h`, `3h`, `1h`.
//!
//! Best practices applied:
//! - median + p95 + p99 for percentile-style metrics
//! - rates AND counts (e.g., `slots/s` plus absolute `slot count`)
//! - shorter window = more recent state → easy to spot recent drift
//! - "lifecycle p95 as slot-times" derived row makes the ms value
//!   meaningful relative to the 400 ms slot duration

use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Row, Table};
use ratatui::Frame;

use crate::model::state::State;
use crate::model::window::{self, WindowStats};
use crate::tui::theme;

const DEFAULT_MS_PER_SLOT: f64 = 400.0;

pub fn render(state: &State, frame: &mut Frame<'_>, area: Rect) {
    let stats = window::compute(state, &window::default_windows());
    if stats.is_empty() {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" rolling-window comparison (no data) ");
        frame.render_widget(block, area);
        return;
    }

    let header = build_header(&stats);
    let rows = build_rows(&stats);

    // Constraints: 1 label column + 1 per window.
    let mut constraints = vec![Constraint::Length(24)];
    for _ in &stats {
        constraints.push(Constraint::Length(11));
    }

    // Title carries the only piece of "duration" info that's actually useful —
    // the log's total length. Per-window durations are implicit from the
    // header column names (24h / 12h / 6h / 3h / 1h).
    let log_span = stats
        .first()
        .map_or_else(String::new, |s| humanize_dur(s.duration.whole_seconds()));
    let title = format!(" rolling-window comparison — log spans {log_span} ");
    let table = Table::new(rows, constraints)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_style(theme::title_style()),
        )
        .style(Style::default().fg(theme::FG));
    frame.render_widget(table, area);
}

fn build_header<'a>(stats: &[WindowStats]) -> Row<'a> {
    let mut cells: Vec<Line<'a>> = Vec::with_capacity(stats.len() + 1);
    cells.push(Line::from(Span::styled(
        "metric",
        theme::title_style().add_modifier(Modifier::BOLD),
    )));
    for s in stats {
        cells.push(
            Line::from(Span::styled(
                s.label.to_owned(),
                theme::title_style().add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Left),
        );
    }
    Row::new(cells)
}

fn build_rows<'a>(stats: &[WindowStats]) -> Vec<Row<'a>> {
    let mut rows = Vec::new();

    // The `duration` row was here; dropped because window column headers
    // (24h, 12h, ...) duplicate it. Total log span moved to the title.

    rows.push(metric_row("slot count", stats, |s| commas(s.slot_count)));
    rows.push(metric_row("slot rate (slots/s)", stats, |s| {
        format!("{:.2}", s.slot_rate_per_sec)
    }));
    rows.push(metric_row("slot dur p50 (ms)", stats, |s| {
        format!("{}", s.slot_duration_p50_us / 1000)
    }));
    rows.push(metric_row("slot dur p95 (ms)", stats, |s| {
        format!("{}", s.slot_duration_p95_us / 1000)
    }));

    rows.push(spacer());
    rows.push(metric_row_styled("fast-finalize %", stats, |s| {
        let v = format!("{:.2}%", s.fast_finalize_pct);
        let style = theme::band_higher_better(
            s.fast_finalize_pct,
            theme::FAST_FIN_GOOD_PCT,
            theme::FAST_FIN_WARN_PCT,
        );
        (v, style)
    }));
    rows.push(metric_row_styled("vote skip rate %", stats, |s| {
        let v = format!("{:.2}%", s.vote_skip_rate_pct);
        let style = theme::band_lower_better(
            s.vote_skip_rate_pct,
            theme::VOTE_SKIP_WARN_PCT,
            theme::VOTE_SKIP_BAD_PCT,
        );
        (v, style)
    }));
    rows.push(metric_row("crashed leaders", stats, |s| {
        commas(s.crashed_leaders)
    }));
    rows.push(metric_row("fragmentation", stats, |s| {
        commas(s.fragmentation)
    }));

    rows.push(spacer());
    // Bold every p50 row across the latency families: assembly, consensus,
    // lifecycle, vote-resume. p50 = median = "what the typical slot
    // experienced" — the headline; p95/p99 are tail context underneath.
    rows.push(highlighted_metric_row("assembly p50 (ms)", stats, |s| {
        format!("{}", s.assembly_p50_us / 1000)
    }));
    rows.push(metric_row("assembly p95 (ms)", stats, |s| {
        format!("{}", s.assembly_p95_us / 1000)
    }));
    rows.push(highlighted_metric_row("consensus p50 (ms)", stats, |s| {
        format!("{}", s.consensus_p50_us / 1000)
    }));
    rows.push(metric_row("consensus p95 (ms)", stats, |s| {
        format!("{}", s.consensus_p95_us / 1000)
    }));
    rows.push(spacer());
    rows.push(highlighted_metric_row("lifecycle p50 (ms)", stats, |s| {
        format!("{}", s.lifecycle_p50_us / 1000)
    }));
    rows.push(metric_row("lifecycle p95 (ms)", stats, |s| {
        format!("{}", s.lifecycle_p95_us / 1000)
    }));
    rows.push(metric_row("lifecycle p99 (ms)", stats, |s| {
        format!("{}", s.lifecycle_p99_us / 1000)
    }));
    rows.push(metric_row("↳ p95 / slot-time", stats, |s| {
        let ms = s.lifecycle_p95_us as f64 / 1000.0;
        format!("{:.2}", ms / DEFAULT_MS_PER_SLOT)
    }));
    rows.push(spacer());
    rows.push(highlighted_metric_row("vote-resume p50 (s)", stats, |s| {
        format!("{:.2}", s.resume_p50_us as f64 / 1_000_000.0)
    }));
    rows.push(metric_row("vote-resume p95 (s)", stats, |s| {
        format!("{:.2}", s.resume_p95_us as f64 / 1_000_000.0)
    }));
    rows.push(metric_row("vote-resume p99 (s)", stats, |s| {
        format!("{:.2}", s.resume_p99_us as f64 / 1_000_000.0)
    }));

    rows
}

fn spacer<'a>() -> Row<'a> {
    Row::new(vec![Line::from("")])
}

fn metric_row<'a, F>(label: &'a str, stats: &[WindowStats], fmt: F) -> Row<'a>
where
    F: Fn(&WindowStats) -> String,
{
    let mut cells: Vec<Line<'a>> = Vec::with_capacity(stats.len() + 1);
    cells.push(Line::from(Span::styled(
        label.to_owned(),
        theme::label_style(),
    )));
    for s in stats {
        cells.push(
            Line::from(Span::styled(fmt(s), theme::value_style())).alignment(Alignment::Left),
        );
    }
    Row::new(cells)
}

/// Same as `metric_row` but tints the value cells with the accent colour
/// (cyan) so the p50 — the median, the headline for each metric family —
/// reads as a distinct row without competing with `fast-finalize %`
/// (which keeps green because it carries a health threshold). Labels
/// stay at the default `label_style` so the table doesn't fatten up.
fn highlighted_metric_row<'a, F>(label: &'a str, stats: &[WindowStats], fmt: F) -> Row<'a>
where
    F: Fn(&WindowStats) -> String,
{
    let mut cells: Vec<Line<'a>> = Vec::with_capacity(stats.len() + 1);
    cells.push(Line::from(Span::styled(
        label.to_owned(),
        theme::label_style(),
    )));
    for s in stats {
        cells.push(
            Line::from(Span::styled(fmt(s), theme::accent_style())).alignment(Alignment::Left),
        );
    }
    Row::new(cells)
}

fn metric_row_styled<'a, F>(label: &'a str, stats: &[WindowStats], fmt: F) -> Row<'a>
where
    F: Fn(&WindowStats) -> (String, Style),
{
    let mut cells: Vec<Line<'a>> = Vec::with_capacity(stats.len() + 1);
    cells.push(Line::from(Span::styled(
        label.to_owned(),
        theme::label_style(),
    )));
    for s in stats {
        let (text, style) = fmt(s);
        cells.push(Line::from(Span::styled(text, style)).alignment(Alignment::Left));
    }
    Row::new(cells)
}

fn humanize_dur(secs: i64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    format!("{h}h {m}m")
}

fn commas(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}
