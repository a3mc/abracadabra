//! Bottom-of-screen status bar.
//!
//! Layout (horizontal split):
//!   `[ hints OR transient message ]                  [ vX.Y.Z ]`
//!
//! The version pulls from `CARGO_PKG_VERSION` at build time so a tag
//! bump in `Cargo.toml` flows into the binary without further plumbing.
//! Transient `status_message` (e.g. yank confirmation) overrides the
//! hints on the left side only; the version stays put on the right.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::theme;

const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

pub fn render(current_tab: usize, status_message: Option<&str>, frame: &mut Frame<'_>, area: Rect) {
    // Reserve right side for ` vX.Y.Z `. +2 for the surrounding spaces.
    let version_width = u16::try_from(VERSION.len() + 2).unwrap_or(10);
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(version_width)])
        .split(area);

    render_left(current_tab, status_message, frame, chunks[0]);

    let version_line = Line::from(Span::styled(format!(" {VERSION} "), theme::label_style()))
        .alignment(Alignment::Right);
    frame.render_widget(Paragraph::new(version_line), chunks[1]);
}

fn render_left(
    current_tab: usize,
    status_message: Option<&str>,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    if let Some(msg) = status_message {
        let line = Line::from(vec![
            Span::styled("›", theme::accent_style()),
            Span::raw(" "),
            Span::styled(msg.to_owned(), theme::value_style()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    let mut spans = vec![
        Span::styled("1-6", theme::title_style()),
        Span::styled(" tabs  ", theme::label_style()),
        Span::styled("Tab", theme::title_style()),
        Span::styled(" next  ", theme::label_style()),
    ];
    if current_tab >= 3 {
        spans.extend([
            Span::styled("j/k", theme::title_style()),
            Span::styled(" scroll  ", theme::label_style()),
            Span::styled("PgUp/PgDn", theme::title_style()),
            Span::styled(" page  ", theme::label_style()),
            Span::styled("g/G", theme::title_style()),
            Span::styled(" top/bottom  ", theme::label_style()),
        ]);
    }
    if current_tab == 3 {
        spans.extend([
            Span::styled("t/n/p", theme::title_style()),
            Span::styled(" TCL/S2N/S2S  ", theme::label_style()),
            Span::styled("l", theme::title_style()),
            Span::styled(" leader  ", theme::label_style()),
            Span::styled("f/x/s", theme::title_style()),
            Span::styled(" fast/slow/skip  ", theme::label_style()),
            Span::styled("c", theme::title_style()),
            Span::styled(" clear  ", theme::label_style()),
        ]);
    }
    if current_tab == 5 {
        spans.extend([
            Span::styled("y", theme::title_style()),
            Span::styled(" yank to /tmp  ", theme::label_style()),
        ]);
    }
    spans.extend([
        Span::styled("q", theme::title_style()),
        Span::styled(" quit", theme::label_style()),
    ]);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
