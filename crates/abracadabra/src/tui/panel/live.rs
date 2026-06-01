//! Live tab — real-time log following surface.
//!
//! Three render states gated by the activity classification from
//! [`crate::live::detect`]:
//!
//! - [`Activity::Static`] — log is rotated, stale, or otherwise frozen.
//!   Panel is grayed; reason text states why following is unavailable.
//! - [`Activity::Active`] without `following` — log is being written.
//!   Panel prompts `Press SPACEBAR to start following`.
//! - [`Activity::Active`] with `following` — placeholder for the
//!   animation surface (filled in by LIVE-4 / LIVE-5). Currently shows
//!   `live following · animation engine pending`.
//!
//! No tail thread runs in this stage; SPACEBAR only flips the
//! `following` flag on `App`. The actual tailing is LIVE-3.

use std::path::Path;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::live::detect::{Activity, StaticReason};
use crate::tui::theme;

/// Render the Live tab into `area`.
///
/// Pure function over its arguments — does not touch `App` directly so
/// it can be unit-tested in isolation once a rendering test harness is
/// in place. Callers pass the activity classification (built once at
/// startup), the source file path (for the prompt), and the current
/// follow flag.
pub fn render(
    activity: &Activity,
    file_path: &Path,
    following: bool,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let title = match (activity, following) {
        (Activity::Active, true) => " Live · following ",
        (Activity::Active, false) => " Live · ready ",
        (Activity::Static(_), _) => " Live · unavailable ",
    };

    let border_style = if matches!(activity, Activity::Active) {
        theme::title_style()
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(theme::title_style())
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Vertical layout: blank, prompt, blank, supporting line, fill.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    let path_str = file_path.display().to_string();
    let (primary, supporting) = lines_for_state(activity, &path_str, following);

    frame.render_widget(
        Paragraph::new(primary).alignment(Alignment::Center),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(supporting).alignment(Alignment::Center),
        chunks[3],
    );
}

/// Build the two centered lines that describe the current Live-tab
/// state. Split out so the wording can be unit-tested without spinning
/// up a `Frame`.
fn lines_for_state<'a>(
    activity: &'a Activity,
    path_str: &'a str,
    following: bool,
) -> (Line<'a>, Line<'a>) {
    match (activity, following) {
        (Activity::Active, false) => (
            Line::from(vec![
                Span::styled("Press ", theme::label_style()),
                Span::styled(
                    "SPACEBAR",
                    theme::accent_style().add_modifier(Modifier::BOLD),
                ),
                Span::styled(" to start following", theme::label_style()),
            ]),
            Line::from(Span::styled(path_str, theme::value_style())),
        ),
        (Activity::Active, true) => (
            Line::from(Span::styled(
                "live following · animation engine pending",
                theme::label_style(),
            )),
            Line::from(Span::styled(path_str, theme::value_style())),
        ),
        (Activity::Static(reason), _) => (
            Line::from(Span::styled(
                "Live mode unavailable",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                static_reason_text(reason, path_str),
                Style::default().fg(Color::DarkGray),
            )),
        ),
    }
}

/// Human-readable supporting line for each `StaticReason` variant.
fn static_reason_text(reason: &StaticReason, path_str: &str) -> String {
    match reason {
        StaticReason::RotatedFilename => {
            format!("rotated / archived filename — {path_str}")
        }
        StaticReason::StaleMtime { age_secs } => {
            format!("mtime is {age_secs}s old — {path_str}")
        }
        StaticReason::NoSizeGrowth => {
            format!("no size growth observed — {path_str}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_idle_prompts_for_spacebar() {
        let (primary, _) = lines_for_state(&Activity::Active, "/tmp/x.log", false);
        let rendered = primary
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("SPACEBAR"), "got: {rendered}");
    }

    #[test]
    fn active_following_shows_placeholder() {
        let (primary, _) = lines_for_state(&Activity::Active, "/tmp/x.log", true);
        let rendered = primary
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("following"), "got: {rendered}");
    }

    #[test]
    fn static_rotated_reason_in_supporting_line() {
        let (_, supporting) = lines_for_state(
            &Activity::Static(StaticReason::RotatedFilename),
            "/var/log/validator.log.3",
            false,
        );
        let rendered = supporting
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("rotated"), "got: {rendered}");
        assert!(rendered.contains("validator.log.3"), "got: {rendered}");
    }

    #[test]
    fn static_stale_mtime_includes_age() {
        let (_, supporting) = lines_for_state(
            &Activity::Static(StaticReason::StaleMtime { age_secs: 3600 }),
            "/var/log/validator.log",
            false,
        );
        let rendered = supporting
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("3600"), "got: {rendered}");
    }

    #[test]
    fn static_no_growth_supporting_line() {
        let (_, supporting) = lines_for_state(
            &Activity::Static(StaticReason::NoSizeGrowth),
            "/var/log/validator.log",
            false,
        );
        let rendered = supporting
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("no size growth"), "got: {rendered}");
    }
}
