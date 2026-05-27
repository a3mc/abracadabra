//! Horizontal bar chart / histogram rendered as `Paragraph` lines.
//!
//! ratatui's built-in `BarChart` is column-oriented and finicky about widths;
//! for distributions we want a tidy text-style horizontal bar with label,
//! filled portion, and percentage — like `htop` mem rows. This renders that
//! deterministically using full-block characters.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::commas;
use crate::tui::theme;

#[derive(Debug, Clone)]
pub struct Bucket<'a> {
    pub label: &'a str,
    pub count: u64,
    pub color: Color,
}

/// Render a horizontal-bar histogram. Percentages are computed against
/// `total` (caller decides whether that's sum or max).
pub fn render(frame: &mut Frame<'_>, area: Rect, title: &str, buckets: &[Bucket<'_>], total: u64) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_owned())
        .title_style(theme::title_style());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if buckets.is_empty() || inner.width < 20 {
        return;
    }

    let label_w = buckets.iter().map(|b| b.label.len()).max().unwrap_or(8);
    // layout: "<label_w> <bar> XX.X%  (NNN)"
    // Reserve 6 chars for percentage, 8 for count parens.
    let reserve = (label_w + 1 + 6 + 2 + 7) as u16;
    let bar_w = inner.width.saturating_sub(reserve).max(8);

    let lines: Vec<Line<'_>> = buckets
        .iter()
        .map(|b| build_line(b, total, label_w, bar_w as usize))
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn build_line<'a>(b: &Bucket<'a>, total: u64, label_w: usize, bar_w: usize) -> Line<'a> {
    let pct = if total > 0 {
        b.count as f64 * 100.0 / total as f64
    } else {
        0.0
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let filled = ((pct / 100.0) * bar_w as f64).round().min(bar_w as f64) as usize;
    let empty = bar_w.saturating_sub(filled);
    let bar_filled: String = "█".repeat(filled);
    let bar_empty: String = "░".repeat(empty);

    Line::from(vec![
        Span::styled(
            format!("{:>label_w$}", b.label, label_w = label_w),
            theme::label_style(),
        ),
        Span::raw(" "),
        Span::styled(bar_filled, Style::default().fg(b.color)),
        Span::styled(bar_empty, theme::label_style()),
        Span::styled(format!(" {pct:>5.1}%"), theme::value_style()),
        Span::styled(format!(" ({})", commas(b.count)), theme::label_style()),
    ])
}
