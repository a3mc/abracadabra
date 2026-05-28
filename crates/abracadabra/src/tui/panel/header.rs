//! Top header strip: project brand + file context + headline verdicts.
//!
//! Branding: ratatui `Block` accepts multiple titles, each with its own
//! alignment. We use that to put ` abracadabra ` on the top-left of the
//! border and ` [ART3MIS.CLOUD] ` on the top-right — the brand rides
//! the border itself rather than consuming a content row, which keeps
//! the header dense while making the project's owner visible. The
//! inner content drops the redundant "abracadabra " prefix since the
//! border already names the project.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::model::alerts::Severity;
use crate::model::state::State;
use crate::tui::theme;
use crate::tui::widget::commas;

pub fn render(state: &State, frame: &mut Frame<'_>, area: Rect) {
    let meta = &state.file_meta;
    let ov = &state.overall;

    let line1 = Line::from(vec![
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
        // Headline duration: bold + cyan so the "how much log am I
        // looking at" answer reads at-a-glance. Same format as the
        // `data source` widget below (`log spans …`) for consistency.
        Span::styled(duration_str(state), theme::title_style()),
    ]);

    let total_final = ov.finalized_fast.saturating_add(ov.finalized_slow);
    let fast_pct = if total_final > 0 {
        ov.finalized_fast as f64 * 100.0 / total_final as f64
    } else {
        0.0
    };
    let slow_pct = if total_final > 0 {
        ov.finalized_slow as f64 * 100.0 / total_final as f64
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
    let canon_skips = ov
        .canonical_skips_direct
        .saturating_add(ov.canonical_skips_ancestry);
    let canon_skip_pct = if ov.votes_skip > 0 {
        canon_skips as f64 * 100.0 / ov.votes_skip as f64
    } else {
        0.0
    };
    let canon_skip_style = theme::band_lower_better(
        canon_skip_pct,
        theme::CANONICAL_SKIP_WARN_PCT,
        theme::CANONICAL_SKIP_BAD_PCT,
    );
    let canon_bound = if ov.indeterminate_skips > 0 {
        "≥"
    } else {
        ""
    };
    let standstill_style = if ov.standstill_events == 0 {
        theme::good_style()
    } else {
        theme::bad_style()
    };

    // Actionable alerts only: Info-severity entries (LocalLeaderSummary,
    // IdentityChanged, INFO LogPattern rows) inflate the count without
    // representing problems. Operator wants the headline number to mean
    // "things you should look at."
    let actionable_alerts = state
        .alerts
        .iter()
        .filter(|a| !matches!(a.severity, Severity::Info))
        .count();
    let info_alerts = state.alerts.len().saturating_sub(actionable_alerts);

    let line2 = Line::from(vec![
        Span::styled("fast-fin ", theme::label_style()),
        Span::styled(format!("{fast_pct:.2}%"), fast_style),
        Span::styled(" / slow ", theme::label_style()),
        Span::styled(
            format!("{slow_pct:.2}%"),
            Style::default().fg(theme::SPARK_ALT_PATH),
        ),
        Span::styled("  vote-skip ", theme::label_style()),
        Span::styled(format!("{skip_pct:.2}%"), skip_style),
        Span::styled("  canonical-skip ", theme::label_style()),
        Span::styled(
            format!("{canon_bound}{canon_skip_pct:.2}%"),
            canon_skip_style.add_modifier(Modifier::BOLD),
        ),
        Span::styled("  standstills ", theme::label_style()),
        Span::styled(commas(ov.standstill_events), standstill_style),
        Span::styled(" (full log)", theme::label_style()),
        Span::styled("  crashed ldrs ", theme::label_style()),
        Span::styled(commas(ov.timeout_crashed_leaders), theme::value_style()),
        Span::styled("  alerts ", theme::label_style()),
        Span::styled(
            actionable_alerts.to_string(),
            if actionable_alerts == 0 {
                theme::good_style()
            } else {
                theme::warn_style()
            },
        ),
        Span::styled(
            if info_alerts > 0 {
                format!(" (+{info_alerts} info)")
            } else {
                String::new()
            },
            theme::label_style(),
        ),
    ]);

    let brand = Line::from(Span::styled(
        " [ART3MIS.CLOUD] ",
        Style::default().fg(theme::OK).add_modifier(Modifier::BOLD),
    ))
    .alignment(Alignment::Right);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            " abracadabra ",
            theme::title_style(),
        )))
        .title(brand);
    let para = Paragraph::new(vec![line1, line2]).block(block);
    frame.render_widget(para, area);
}

fn duration_str(state: &State) -> String {
    let Some((lo, hi)) = state.file_meta.time_range else {
        return String::new();
    };
    let d = hi - lo;
    // Format matches the "data source" widget below (`log spans …`)
    // so the two readings of the same duration are visually
    // consistent. Drop a unit only when its value is 0 (so a 21h
    // sample doesn't print `21h 0m 0s`).
    let h = d.whole_hours();
    let m = d.whole_minutes() % 60;
    let s = d.whole_seconds() % 60;
    match (h, m, s) {
        (0, 0, _) => format!("{s}s"),
        (0, _, 0) => format!("{m}m"),
        (0, _, _) => format!("{m}m {s}s"),
        (_, 0, 0) => format!("{h}h"),
        (_, _, 0) => format!("{h}h {m}m"),
        _ => format!("{h}h {m}m {s}s"),
    }
}
