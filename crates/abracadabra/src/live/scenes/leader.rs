//! Block production — leader-window tracker.
//!
//! Surfaces three things in the same compact pane:
//!
//! 1. The most recent `ProduceWindow` event with a status relative
//!    to the latest observed slot — `in window` / `next window N
//!    slots away` / `last window N slots ago`.
//! 2. Mean slot time, computed as the rolling mean of inter-arrival
//!    times between consecutive `BankFrozen` events (one event per
//!    finalised slot). Out-of-order or skip-bridged arrivals are
//!    divided by the slot delta so individual large gaps don't poison
//!    the mean.
//! 3. A Braille spinner that ticks at a fixed cadence so the pane
//!    visibly "lives" between events.
//!
//! `BankFrozen` arrives ~once per slot, so the inter-arrival sample
//! is a good proxy for cluster slot time as observed locally.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use time::OffsetDateTime;

use crate::live::animation::Pane;
use crate::parser::{Event, EventKind};
use crate::tui::theme;

pub const PANE_HEIGHT: u16 = 5;

/// Braille spinner frames. Single Braille cell, rotating dot
/// pattern — the same animation Cargo uses.
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Inter-arrival samples retained for the rolling mean slot time.
const RECENT_SLOT_TIMES_CAPACITY: usize = 128;
/// Tracked leader windows kept in memory.
const RECENT_WINDOWS_CAPACITY: usize = 16;
/// Maximum tolerated `end - start` span on a `ProduceWindow` event.
/// Mirrors the aggregator's `MAX_LEADER_WINDOW_SPAN` defence against
/// truncated log lines that would otherwise materialise a huge range.
const MAX_WINDOW_SPAN: u64 = 32;
/// Inter-arrival deltas spanning more than this many slots are
/// treated as gaps (skip runs, log truncation) and not used for the
/// mean. Keeps a real slot-time estimate from being dragged down by
/// "we missed 200 slots in a row" events.
const MAX_SLOT_GAP: u64 = 8;

#[derive(Debug, Clone, Copy)]
struct ProduceWindowInfo {
    start: u64,
    end: u64,
}

pub struct LeaderPane {
    recent_windows: VecDeque<ProduceWindowInfo>,
    /// `(slot, ev.ts)` of the most recent `BankFrozen` event. The
    /// timestamp is the parsed log timestamp, not wall-clock, so
    /// inter-arrival samples reflect cluster cadence regardless of
    /// playback speed.
    last_bank_frozen: Option<(u64, OffsetDateTime)>,
    /// Per-slot duration samples (already divided by the slot gap).
    recent_slot_times: VecDeque<Duration>,
    /// Animation-only; do not use for sample timing. Drives the
    /// Braille spinner via `Instant::elapsed`.
    now: Instant,
}

impl LeaderPane {
    pub fn new() -> Self {
        Self {
            recent_windows: VecDeque::with_capacity(RECENT_WINDOWS_CAPACITY),
            last_bank_frozen: None,
            recent_slot_times: VecDeque::with_capacity(RECENT_SLOT_TIMES_CAPACITY),
            now: Instant::now(),
        }
    }

    fn mean_slot_time(&self) -> Option<Duration> {
        if self.recent_slot_times.is_empty() {
            return None;
        }
        let total_micros: u128 = self.recent_slot_times.iter().map(Duration::as_micros).sum();
        let n = self.recent_slot_times.len() as u128;
        let avg_micros = total_micros / n;
        Some(Duration::from_micros(avg_micros as u64))
    }

    fn latest_window(&self) -> Option<ProduceWindowInfo> {
        self.recent_windows.back().copied()
    }

    fn current_slot(&self) -> Option<u64> {
        self.last_bank_frozen.map(|(s, _)| s)
    }
}

impl Default for LeaderPane {
    fn default() -> Self {
        Self::new()
    }
}

impl Pane for LeaderPane {
    fn on_event(&mut self, ev: &Event) {
        match &ev.kind {
            EventKind::ProduceWindow { start, end, .. } => {
                if *end < *start || end.saturating_sub(*start) > MAX_WINDOW_SPAN {
                    return;
                }
                self.recent_windows.push_back(ProduceWindowInfo {
                    start: *start,
                    end: *end,
                });
                while self.recent_windows.len() > RECENT_WINDOWS_CAPACITY {
                    self.recent_windows.pop_front();
                }
            }
            EventKind::BankFrozen { slot, .. } => {
                let ts = ev.ts;
                if let Some((prev_slot, prev_ts)) = self.last_bank_frozen {
                    if *slot > prev_slot {
                        let raw_delta = ts - prev_ts;
                        // Reject out-of-order log lines outright;
                        // their negative delta would corrupt the mean.
                        if !raw_delta.is_negative() {
                            let delta = raw_delta.unsigned_abs();
                            let gap = *slot - prev_slot;
                            if gap <= MAX_SLOT_GAP && gap > 0 {
                                let per_slot = delta / u32::try_from(gap).unwrap_or(1);
                                self.recent_slot_times.push_back(per_slot);
                                while self.recent_slot_times.len() > RECENT_SLOT_TIMES_CAPACITY {
                                    self.recent_slot_times.pop_front();
                                }
                            }
                        }
                    }
                }
                self.last_bank_frozen = Some((*slot, ts));
            }
            _ => {}
        }
    }

    fn tick(&mut self, now: Instant) {
        self.now = now;
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" block production ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < 10 || inner.height < 3 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);

        self.render_main(frame, chunks[1]);
        self.render_snapshot(frame, chunks[2]);
    }
}

impl LeaderPane {
    fn render_main(&self, frame: &mut Frame<'_>, area: Rect) {
        let spinner_idx = (self.now.elapsed().as_millis() / 100) as usize % SPINNER.len();
        let spinner = SPINNER[spinner_idx];
        let status = self.window_status_text();

        let line = Line::from(vec![
            Span::styled(
                format!(" {spinner} "),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(status, theme::value_style()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn window_status_text(&self) -> String {
        let Some(w) = self.latest_window() else {
            return "no leader window observed yet".to_owned();
        };
        let Some(cur) = self.current_slot() else {
            return format!("window {}..{}", w.start, w.end);
        };
        if cur >= w.start && cur <= w.end {
            format!("in window {}..{} (slot {cur})", w.start, w.end)
        } else if cur < w.start {
            format!(
                "next window {}..{} ({} slots away)",
                w.start,
                w.end,
                w.start - cur
            )
        } else {
            format!(
                "last window {}..{} ({} slots ago)",
                w.start,
                w.end,
                cur - w.end
            )
        }
    }

    fn render_snapshot(&self, frame: &mut Frame<'_>, area: Rect) {
        let mean = self
            .mean_slot_time()
            .map_or_else(|| "—".to_owned(), |d| format!("{} ms", d.as_millis()));
        let samples = self.recent_slot_times.len();
        let windows = self.recent_windows.len();

        let line = Line::from(vec![
            Span::styled(" mean slot ", theme::label_style()),
            Span::styled(
                mean,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" ({samples} samples)"), theme::label_style()),
            sep(),
            Span::styled(
                windows.to_string(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" windows tracked", theme::label_style()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }
}

fn sep() -> Span<'static> {
    Span::styled(
        "  ·  ",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(kind: EventKind) -> Event {
        Event {
            ts: time::OffsetDateTime::UNIX_EPOCH,
            kind,
        }
    }

    fn mk_at(ts: time::OffsetDateTime, kind: EventKind) -> Event {
        Event { ts, kind }
    }

    #[test]
    fn produce_window_event_recorded() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(EventKind::ProduceWindow {
            start: 100,
            end: 103,
            parent_slot: 99,
            parent_hash: "x".into(),
        }));
        assert_eq!(p.recent_windows.len(), 1);
        let w = p.recent_windows[0];
        assert_eq!(w.start, 100);
        assert_eq!(w.end, 103);
    }

    #[test]
    fn malformed_window_rejected() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(EventKind::ProduceWindow {
            start: 200,
            end: 100,
            parent_slot: 99,
            parent_hash: "x".into(),
        }));
        p.on_event(&mk(EventKind::ProduceWindow {
            start: 0,
            end: u64::MAX,
            parent_slot: 0,
            parent_hash: "x".into(),
        }));
        assert_eq!(p.recent_windows.len(), 0);
    }

    #[test]
    fn bank_frozen_updates_last_slot() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(EventKind::BankFrozen {
            slot: 100,
            hash: "a".into(),
            signature_count: 0,
        }));
        assert_eq!(p.current_slot(), Some(100));
    }

    #[test]
    fn bank_frozen_with_huge_gap_does_not_pollute_mean() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(EventKind::BankFrozen {
            slot: 100,
            hash: "a".into(),
            signature_count: 0,
        }));
        // Big gap (200 slots) should be ignored for the mean.
        p.on_event(&mk(EventKind::BankFrozen {
            slot: 300,
            hash: "b".into(),
            signature_count: 0,
        }));
        assert_eq!(p.recent_slot_times.len(), 0);
    }

    #[test]
    fn window_status_in_window() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(EventKind::ProduceWindow {
            start: 100,
            end: 103,
            parent_slot: 99,
            parent_hash: "x".into(),
        }));
        p.on_event(&mk(EventKind::BankFrozen {
            slot: 101,
            hash: "a".into(),
            signature_count: 0,
        }));
        let s = p.window_status_text();
        assert!(s.contains("in window 100..103"), "got {s}");
    }

    #[test]
    fn leader_inter_arrival_uses_event_timestamp() {
        let mut p = LeaderPane::new();
        let t0 = time::OffsetDateTime::UNIX_EPOCH;
        let t1 = t0 + time::Duration::milliseconds(400);
        p.on_event(&mk_at(
            t0,
            EventKind::BankFrozen {
                slot: 100,
                hash: "a".into(),
                signature_count: 0,
            },
        ));
        p.on_event(&mk_at(
            t1,
            EventKind::BankFrozen {
                slot: 101,
                hash: "b".into(),
                signature_count: 0,
            },
        ));
        assert_eq!(p.recent_slot_times.len(), 1);
        assert_eq!(
            p.recent_slot_times.back().copied(),
            Some(Duration::from_millis(400))
        );
        assert_eq!(p.mean_slot_time(), Some(Duration::from_millis(400)));
    }

    #[test]
    fn leader_inter_arrival_rejects_out_of_order_timestamps() {
        let mut p = LeaderPane::new();
        let t0 = time::OffsetDateTime::UNIX_EPOCH + time::Duration::milliseconds(1_000);
        let t1 = time::OffsetDateTime::UNIX_EPOCH + time::Duration::milliseconds(500);
        p.on_event(&mk_at(
            t0,
            EventKind::BankFrozen {
                slot: 100,
                hash: "a".into(),
                signature_count: 0,
            },
        ));
        p.on_event(&mk_at(
            t1,
            EventKind::BankFrozen {
                slot: 101,
                hash: "b".into(),
                signature_count: 0,
            },
        ));
        assert!(p.recent_slot_times.is_empty());
    }

    #[test]
    fn window_status_next_window() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(EventKind::ProduceWindow {
            start: 200,
            end: 203,
            parent_slot: 199,
            parent_hash: "x".into(),
        }));
        p.on_event(&mk(EventKind::BankFrozen {
            slot: 180,
            hash: "a".into(),
            signature_count: 0,
        }));
        let s = p.window_status_text();
        assert!(s.contains("next window"), "got {s}");
    }
}
