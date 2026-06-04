//! Ratatui rendering for the chain pane.
//!
//! Free functions that take immutable references to pane state
//! ([`super::state::ChainPane`]) and a [`Frame`] sink. The split keeps
//! all `ratatui::*` use out of the state module.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::live::animation::spinner_frame;
use crate::tui::theme;

use super::format::StagePercentiles;
use super::state::ChainPane;

pub const PANE_HEIGHT: u16 = 6;

/// Render the entire pane (border + 6-row composition) inside `area`.
pub(super) fn render(pane: &ChainPane, frame: &mut Frame<'_>, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" chain ")
        .title_style(theme::title_style())
        .border_style(theme::title_style());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 20 || inner.height < 4 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // top spacer
            Constraint::Length(1), // spinner + tip slot
            Constraint::Length(1), // blank
            Constraint::Length(1), // "live timing (p50 / p95)" label
            Constraint::Min(1),    // timing table
            Constraint::Length(1), // snapshot
        ])
        .split(inner);

    render_tip(pane, frame, chunks[1]);
    render_section_label(frame, chunks[3], "live timing  (p50 / p95)");
    render_timing_table(pane, frame, chunks[4]);
    render_snapshot(pane, frame, chunks[5]);
}

fn render_tip(pane: &ChainPane, frame: &mut Frame<'_>, area: Rect) {
    let spinner = spinner_frame(pane.event_count, pane.last_event_at);
    let tip = pane
        .tip_slot()
        .map_or_else(|| "—".to_owned(), |s| s.to_string());
    let line = Line::from(vec![
        Span::raw("  "),
        Span::styled(
            spinner.to_owned(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            tip,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  tip slot", theme::label_style()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_section_label(frame: &mut Frame<'_>, area: Rect, label: &str) {
    let line = Line::from(Span::styled(format!("  {label}"), theme::label_style()));
    frame.render_widget(Paragraph::new(line), area);
}

fn render_timing_table(pane: &ChainPane, frame: &mut Frame<'_>, area: Rect) {
    if area.height == 0 {
        return;
    }
    let table = pane.timing_table();
    // Order: cluster (network cadence), then assembly → consensus
    // → lifecycle (stage breakdown matching Windows-tab semantics).
    let rows: [(&str, StagePercentiles); 4] = [
        ("slot cadence", table.cluster),
        ("assembly", table.assembly),
        ("consensus", table.consensus),
        ("lifecycle", table.lifecycle),
    ];
    let max = area.height as usize;
    for (i, (label, pct)) in rows.iter().enumerate().take(max) {
        let y = area.y + i as u16;
        let row = Rect::new(area.x, y, area.width, 1);
        let line = match pct {
            Some((p50, p95)) => Line::from(vec![
                Span::styled(format!("    {label:<14}"), theme::label_style()),
                Span::styled(format!("p50 {p50}ms"), theme::value_style()),
                Span::styled("   ", theme::label_style()),
                Span::styled(format!("p95 {p95}ms"), theme::value_style()),
            ]),
            None => Line::from(vec![
                Span::styled(format!("    {label:<14}"), theme::label_style()),
                Span::styled("—", Style::default().fg(Color::DarkGray)),
            ]),
        };
        frame.render_widget(Paragraph::new(line), row);
    }
}

fn render_snapshot(pane: &ChainPane, frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(Paragraph::new(snapshot_line(pane)), area);
}

/// Compose the snapshot `Line`. Extracted from `render_snapshot`
/// for direct test coverage of the label vocabulary (no frame
/// roundtripping required).
pub(super) fn snapshot_line(pane: &ChainPane) -> Line<'static> {
    let (canonical_skips, indeterminate) = pane.skip_tallies();
    let forks = pane.fork_count();

    let range = match (pane.slots.front(), pane.slots.back()) {
        (Some(f), Some(l)) if f.slot != l.slot => format!("{}..{}", f.slot, l.slot),
        (Some(f), _) => format!("{}", f.slot),
        _ => "—".to_owned(),
    };

    let cskip_style = if canonical_skips > 0 {
        theme::bad_style().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let fork_style = if forks > 0 {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    // `CSKIP` matches the Slots tab vocabulary (see slots.rs:303,384
    // and tui/view.rs status_str — single token across the TUI).
    let mut spans = vec![
        Span::styled(" slots ", theme::label_style()),
        Span::styled(
            range,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        sep(),
        Span::styled(canonical_skips.to_string(), cskip_style),
        Span::styled(" CSKIP", theme::label_style()),
        sep(),
        Span::styled(
            indeterminate.to_string(),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(" indet", theme::label_style()),
        sep(),
        Span::styled(forks.to_string(), fork_style),
        Span::styled(" forks", theme::label_style()),
    ];
    // Walk-back anomalies are only surfaced when nonzero — silence
    // is the correct default for a healthy stream (no-explanatory-UX
    // policy). When nonzero, surface as a red-bold counter so the
    // operator notices upstream parser regressions.
    if pane.walk_back_anomalies > 0 {
        spans.push(sep());
        spans.push(Span::styled(
            pane.walk_back_anomalies.to_string(),
            theme::bad_style().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" anom", theme::label_style()));
    }
    Line::from(spans)
}

fn sep() -> Span<'static> {
    Span::styled(
        "  ·  ",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    )
}
