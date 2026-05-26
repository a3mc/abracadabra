//! Key-performance-indicator strip: one `Paragraph` row with multiple
//! `label: value` cells separated by mid-dots.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::theme;

#[derive(Debug, Clone)]
pub struct Kpi<'a> {
    pub label: &'a str,
    pub value: String,
    pub style: Style,
}

impl<'a> Kpi<'a> {
    pub const fn new(label: &'a str, value: String, style: Style) -> Self {
        Self {
            label,
            value,
            style,
        }
    }
}

pub fn render(frame: &mut Frame<'_>, area: Rect, title: &str, kpis: &[Kpi<'_>]) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_owned())
        .title_style(theme::title_style());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut spans = Vec::with_capacity(kpis.len() * 3);
    for (i, k) in kpis.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  |  ", theme::label_style()));
        }
        spans.push(Span::styled(format!("{} ", k.label), theme::label_style()));
        spans.push(Span::styled(k.value.clone(), k.style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);
}
