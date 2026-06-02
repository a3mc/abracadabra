//! Live tab — real-time log following surface.
//!
//! Three render states gated by the activity classification from
//! [`crate::live::detect`] and the presence of a [`TailHandle`]:
//!
//! - [`Activity::Static`] — log is rotated, stale, or otherwise frozen.
//!   Panel is grayed; reason text states why following is unavailable.
//! - [`Activity::Active`] without a tail handle — log is being written
//!   but we are not following yet. Panel prompts `Press SPACEBAR to
//!   start following`.
//! - [`Activity::Active`] with a tail handle — counters render from the
//!   shared [`LiveBuffer`]: total events, total lines, last-read age,
//!   most-recent event line. The animation surface (LIVE-4 / LIVE-5)
//!   replaces this placeholder counter view once the engine lands.

use std::path::Path;
use std::time::Instant;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::live::detect::{Activity, StaticReason};
use crate::live::scenes::SceneEngine;
use crate::live::tail::TailHandle;
use crate::tui::theme;

/// Snapshot of the tail buffer for one render frame.
///
/// Built by [`tail_snapshot`] while the buffer mutex is briefly held;
/// the rest of the render path operates on this owned copy so the
/// lock is never live during widget construction.
#[derive(Debug, Default)]
struct TailFrame {
    total_events: u64,
    total_lines: u64,
    age_secs: Option<u64>,
    last_event_summary: Option<String>,
    last_error: Option<String>,
}

/// Render the Live tab into `area`.
///
/// Pure function over its arguments — does not touch `App` directly so
/// it can be unit-tested in isolation once a rendering test harness is
/// in place. Callers pass the activity classification (built once at
/// startup), the source file path (for the prompt), and the current
/// tail handle (`None` = not following).
pub fn render(
    activity: &Activity,
    file_path: &Path,
    tail: Option<&TailHandle>,
    engine: Option<&SceneEngine>,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let following = tail.is_some();

    // Active log + following + engine present → hand the whole inner
    // area to the scene engine. The placeholder counter view below is
    // only shown in the transient state where following just started
    // (engine == None) and in the idle / static states.
    if matches!(activity, Activity::Active) && following {
        if let Some(engine) = engine {
            engine.render(frame, area);
            return;
        }
    }

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

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    let path_str = file_path.display().to_string();
    let snap = tail.map(tail_snapshot).unwrap_or_default();
    let (primary, supporting, extra) = lines_for_state(activity, &path_str, following, &snap);

    frame.render_widget(
        Paragraph::new(primary).alignment(Alignment::Center),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(supporting).alignment(Alignment::Center),
        chunks[3],
    );
    if let Some(extra) = extra {
        frame.render_widget(
            Paragraph::new(extra).alignment(Alignment::Center),
            chunks[4],
        );
    }
}

/// Snapshot the live buffer into an owned `TailFrame`. Lock is held
/// only for the duration of the copy; render code below never touches
/// the mutex.
fn tail_snapshot(tail: &TailHandle) -> TailFrame {
    let Ok(buf) = tail.buffer.lock() else {
        return TailFrame::default();
    };
    let age_secs = buf
        .last_read_at
        .map(|t| Instant::now().saturating_duration_since(t).as_secs());
    let last_event_summary = buf.recent.back().map(|ev| {
        let ts = ev
            .ts
            .time()
            .format(time::macros::format_description!(
                "[hour]:[minute]:[second].[subsecond digits:3]"
            ))
            .unwrap_or_else(|_| "??:??:??".into());
        format!("{ts} · {}", event_kind_label(&ev.kind))
    });
    TailFrame {
        total_events: buf.total_events,
        total_lines: buf.total_lines,
        age_secs,
        last_event_summary,
        last_error: buf.last_error.clone(),
    }
}

/// One-word label for an `EventKind` variant. Kept short for the live
/// status line; full structural detail belongs in the snapshot tabs.
const fn event_kind_label(kind: &crate::parser::EventKind) -> &'static str {
    use crate::parser::EventKind as E;
    match kind {
        E::Block { .. } => "Block",
        E::VotingNotarize { .. } => "VotingNotarize",
        E::VotingFinalize { .. } => "VotingFinalize",
        E::VotingSkip { .. } => "VotingSkip",
        E::BlockNotarized { .. } => "BlockNotarized",
        E::BlockNotarFallback { .. } => "BlockNotarFallback",
        E::Finalized { .. } => "Finalized",
        E::FirstShred { .. } => "FirstShred",
        E::Timeout { .. } => "Timeout",
        E::TimeoutCrashedLeader { .. } => "TimeoutCrashedLeader",
        E::SafeToNotar { .. } => "SafeToNotar",
        E::SafeToSkip { .. } => "SafeToSkip",
        E::ProduceWindow { .. } => "ProduceWindow",
        E::Standstill { .. } => "Standstill",
        E::StandstillExtending { .. } => "StandstillExtending",
        E::StandstillEnded { .. } => "StandstillEnded",
        E::SetIdentity => "SetIdentity",
        E::RefreshingVote => "RefreshingVote",
        E::TriggeringParentReady { .. } => "TriggeringParentReady",
        E::SettingRoot { .. } => "SettingRoot",
        E::NewRoot { .. } => "NewRoot",
        E::BankFrozen { .. } => "BankFrozen",
        E::NoEpochMetadata { .. } => "NoEpochMetadata",
        E::NoEpochInfoForSlot { .. } => "NoEpochInfoForSlot",
        E::UpdatingEpochMetadata { .. } => "UpdatingEpochMetadata",
        E::EvictingEpochMetadata { .. } => "EvictingEpochMetadata",
        E::ClusterSlotsStopped => "ClusterSlotsStopped",
        E::InvalidClusterSlotsUpdate => "InvalidClusterSlotsUpdate",
        E::EventHandlerStats { .. } => "EventHandlerStats",
        E::BlockCommitmentCache { .. } => "BlockCommitmentCache",
        E::Metric(_) => "Metric",
    }
}

/// Build the centered lines that describe the current Live-tab state.
///
/// Returns `(primary, supporting, extra)`. `extra` is `Some` only when
/// the tail thread has live event detail to surface; static and idle
/// states use just the two main lines.
///
/// Returns `'static` lines — every input is either copied into owned
/// strings (`to_owned`, `format!`, `.clone()`) or is a `'static` literal.
/// Snap and path do not need to outlive the call.
fn lines_for_state(
    activity: &Activity,
    path_str: &str,
    following: bool,
    snap: &TailFrame,
) -> (Line<'static>, Line<'static>, Option<Line<'static>>) {
    let path_owned = path_str.to_owned();
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
            Line::from(Span::styled(path_owned, theme::value_style())),
            None,
        ),
        (Activity::Active, true) => {
            let primary = snap.last_error.as_ref().map_or_else(
                || {
                    let age = snap
                        .age_secs
                        .map_or_else(|| "—".to_owned(), |s| format!("{s}s ago"));
                    Line::from(vec![
                        Span::styled("following · ", theme::label_style()),
                        Span::styled(
                            format!("{} events", snap.total_events),
                            theme::value_style(),
                        ),
                        Span::styled(" · ", theme::label_style()),
                        Span::styled(format!("{} lines", snap.total_lines), theme::value_style()),
                        Span::styled(" · last ", theme::label_style()),
                        Span::styled(age, theme::value_style()),
                    ])
                },
                |err| {
                    Line::from(Span::styled(
                        err.clone(),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ))
                },
            );
            (
                primary,
                Line::from(Span::styled(path_owned, theme::value_style())),
                snap.last_event_summary
                    .as_ref()
                    .map(|s| Line::from(Span::styled(s.clone(), theme::label_style()))),
            )
        }
        (Activity::Static(reason), _) => (
            Line::from(Span::styled(
                "Live mode unavailable",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                static_reason_text(reason, &path_owned),
                Style::default().fg(Color::DarkGray),
            )),
            None,
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

    fn snap() -> TailFrame {
        TailFrame::default()
    }

    fn snap_with(events: u64, lines: u64, age_secs: Option<u64>) -> TailFrame {
        TailFrame {
            total_events: events,
            total_lines: lines,
            age_secs,
            ..TailFrame::default()
        }
    }

    fn join_line(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn active_idle_prompts_for_spacebar() {
        let (primary, _, extra) = lines_for_state(&Activity::Active, "/tmp/x.log", false, &snap());
        assert!(extra.is_none());
        assert!(join_line(&primary).contains("SPACEBAR"));
    }

    #[test]
    fn active_following_with_counters() {
        let (primary, _, _) = lines_for_state(
            &Activity::Active,
            "/tmp/x.log",
            true,
            &snap_with(42, 137, Some(3)),
        );
        let rendered = join_line(&primary);
        assert!(rendered.contains("42 events"), "got: {rendered}");
        assert!(rendered.contains("137 lines"), "got: {rendered}");
        assert!(rendered.contains("3s ago"), "got: {rendered}");
    }

    #[test]
    fn active_following_error_takes_over_primary() {
        let mut s = snap_with(0, 0, None);
        s.last_error = Some("read /x: io".to_owned());
        let (primary, _, _) = lines_for_state(&Activity::Active, "/x", true, &s);
        assert!(join_line(&primary).contains("read /x"));
    }

    #[test]
    fn static_rotated_reason_in_supporting_line() {
        let (_, supporting, extra) = lines_for_state(
            &Activity::Static(StaticReason::RotatedFilename),
            "/var/log/validator.log.3",
            false,
            &snap(),
        );
        assert!(extra.is_none());
        let rendered = join_line(&supporting);
        assert!(rendered.contains("rotated"), "got: {rendered}");
        assert!(rendered.contains("validator.log.3"), "got: {rendered}");
    }

    #[test]
    fn static_stale_mtime_includes_age() {
        let (_, supporting, _) = lines_for_state(
            &Activity::Static(StaticReason::StaleMtime { age_secs: 3600 }),
            "/var/log/validator.log",
            false,
            &snap(),
        );
        assert!(join_line(&supporting).contains("3600"));
    }

    #[test]
    fn static_no_growth_supporting_line() {
        let (_, supporting, _) = lines_for_state(
            &Activity::Static(StaticReason::NoSizeGrowth),
            "/var/log/validator.log",
            false,
            &snap(),
        );
        assert!(join_line(&supporting).contains("no size growth"));
    }
}
