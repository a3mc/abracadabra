//! Top header strip: file name + time range + headline verdicts.
//!
//! Branding: ratatui `Block` accepts multiple titles, each with its own
//! alignment. We use that to put ` overview ` on the top-left of the
//! border and ` [ART3MIS.CLOUD] ` on the top-right — the brand rides
//! the border itself rather than consuming a content row, which keeps
//! the header dense while making the project's owner visible.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::model::state::State;
use crate::tui::theme;
use crate::tui::widget::commas;

pub fn render(state: &State, frame: &mut Frame<'_>, area: Rect) {
    let meta = &state.file_meta;
    let ov = &state.overall;

    let line1 = Line::from(vec![
        Span::styled("abracadabra", theme::title_style()),
        Span::styled("  |  ", theme::label_style()),
        Span::styled(meta.path.display().to_string(), theme::value_style()),
        Span::styled("  |  ", theme::label_style()),
        Span::styled(
            format!("{:.2} GB", meta.size_bytes as f64 / 1_073_741_824.0),
            theme::value_style(),
        ),
        Span::styled("  |  ", theme::label_style()),
        Span::styled(
            format!("{} slots", commas(state.slots.len() as u64)),
            theme::value_style(),
        ),
        Span::styled("  |  ", theme::label_style()),
        Span::styled(duration_str(state), theme::value_style()),
    ]);

    let total_final = ov.finalized_fast.saturating_add(ov.finalized_slow);
    let fast_pct = if total_final > 0 {
        ov.finalized_fast as f64 * 100.0 / total_final as f64
    } else {
        0.0
    };
    let fast_style =
        theme::band_higher_better(fast_pct, theme::FAST_FIN_GOOD_PCT, theme::FAST_FIN_WARN_PCT);
    let skip_pct = if state.slots.is_empty() {
        0.0
    } else {
        ov.votes_skip as f64 * 100.0 / state.slots.len() as f64
    };
    let skip_style = theme::band_lower_better(
        skip_pct,
        theme::VOTE_SKIP_WARN_PCT,
        theme::VOTE_SKIP_BAD_PCT,
    );
    let standstill_style = if ov.standstill_events == 0 {
        theme::good_style()
    } else {
        theme::bad_style()
    };

    let line2 = Line::from(vec![
        Span::styled("fast-fin ", theme::label_style()),
        Span::styled(format!("{fast_pct:.2}%"), fast_style),
        Span::styled("  skip ", theme::label_style()),
        Span::styled(format!("{skip_pct:.2}%"), skip_style),
        Span::styled("  standstills ", theme::label_style()),
        Span::styled(commas(ov.standstill_events), standstill_style),
        Span::styled("  crashed ldrs ", theme::label_style()),
        Span::styled(commas(ov.timeout_crashed_leaders), theme::value_style()),
        Span::styled("  alerts ", theme::label_style()),
        Span::styled(
            state.alerts.len().to_string(),
            if state.alerts.is_empty() {
                theme::good_style()
            } else {
                theme::warn_style()
            },
        ),
    ]);

    let brand = Line::from(Span::styled(
        " [ART3MIS.CLOUD] ",
        Style::default().fg(theme::OK).add_modifier(Modifier::BOLD),
    ))
    .alignment(Alignment::Right);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" overview ")
        .title(brand);
    let para = Paragraph::new(vec![line1, line2]).block(block);
    frame.render_widget(para, area);
}

fn duration_str(state: &State) -> String {
    let Some((lo, hi)) = state.file_meta.time_range else {
        return String::new();
    };
    let d = hi - lo;
    format!("{}h{}m", d.whole_hours(), d.whole_minutes() % 60)
}
