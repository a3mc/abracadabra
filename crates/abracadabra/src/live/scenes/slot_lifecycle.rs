//! Slot lifecycle strip — per-slot timing bars from real metrics.
//!
//! ```text
//! ┌─ slot lifecycle (last 4) ────────────────────────────────────────────┐
//! │   2070551  ▓▓▓░░░░░░ 137ms  fast   (96 shreds · 0 repair · 44 fec)   │
//! │   2070552  ▓▓░░░░░░░  92ms  fast   (94 shreds · 0 repair · 22 fec)   │
//! │   2070553  ▓▓▓▓░░░░░ 144ms  fast   (95 shreds · 0 repair · 44 fec)   │
//! │            ▲shred → vote_notarize → finalized                        │
//! └──────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! One row per recently-completed slot, newest at the bottom. Data
//! comes from two `solana_metrics::metrics` datapoints joined by slot
//! number:
//!
//! - `event_handler_slot_tracking` → `first_shred_us`, `vote_notarize_us`,
//!   `finalized_us`, `is_fast_finalization` (lifecycle timing)
//! - `shred_insert_is_full` → `last_index`, `num_repaired`, `num_recovered`
//!   (per-slot shred-source breakdown)
//!
//! A slot is "ready to display" once both metrics have arrived. The
//! pane keeps the most recent [`MAX_SLOTS_SHOWN`] ready slots; older
//! ones drop off the top as new ones complete.

use std::collections::BTreeMap;
use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::live::animation::Pane;
use crate::parser::{Event, EventKind, MetricEvent};
use crate::tui::theme;

/// Pane row height when laid out by [`crate::live::scenes::SceneEngine`].
pub const PANE_HEIGHT: u16 = 7;

/// Maximum slot rows displayed at once. Older ready rows are evicted.
pub const MAX_SLOTS_SHOWN: usize = 4;

/// Aggregated record for one slot, partially populated by each metric
/// type. A slot is "ready" once both `tracking` and `insert_full` are
/// present.
#[derive(Debug, Default, Clone, Copy)]
struct SlotRow {
    tracking: Option<Tracking>,
    insert_full: Option<InsertFull>,
}

#[derive(Debug, Clone, Copy)]
struct Tracking {
    first_shred_us: u64,
    vote_notarize_us: u64,
    finalized_us: u64,
    is_fast_finalization: bool,
}

#[derive(Debug, Clone, Copy)]
struct InsertFull {
    last_index: u64,
    num_repaired: u64,
    num_recovered: u64,
}

impl SlotRow {
    const fn is_ready(&self) -> bool {
        self.tracking.is_some() && self.insert_full.is_some()
    }
}

pub struct SlotLifecyclePane {
    /// All currently-known slots, ordered by slot number. We hold all
    /// of them so partially-populated rows can be completed when the
    /// second metric for that slot arrives, regardless of order. The
    /// render pass filters this down to [`MAX_SLOTS_SHOWN`] ready rows.
    slots: BTreeMap<u64, SlotRow>,
}

impl SlotLifecyclePane {
    pub const fn new() -> Self {
        Self {
            slots: BTreeMap::new(),
        }
    }

    /// Drop oldest slots once the ready-set is comfortably above the
    /// display cap. Bounded growth even if one metric type never
    /// arrives for some slot — the partial entries get evicted as
    /// newer slots fill the cap.
    fn evict_old(&mut self) {
        let max_keep = MAX_SLOTS_SHOWN.saturating_mul(4);
        while self.slots.len() > max_keep {
            let Some((&oldest, _)) = self.slots.iter().next() else {
                break;
            };
            self.slots.remove(&oldest);
        }
    }
}

impl Default for SlotLifecyclePane {
    fn default() -> Self {
        Self::new()
    }
}

impl Pane for SlotLifecyclePane {
    fn on_event(&mut self, ev: &Event) {
        let EventKind::Metric(m) = &ev.kind else {
            return;
        };
        match m {
            MetricEvent::SlotTracking {
                slot,
                first_shred_us,
                vote_notarize_us,
                finalized_us,
                is_fast_finalization,
            } => {
                let row = self.slots.entry(*slot).or_default();
                row.tracking = Some(Tracking {
                    first_shred_us: *first_shred_us,
                    vote_notarize_us: *vote_notarize_us,
                    finalized_us: *finalized_us,
                    is_fast_finalization: *is_fast_finalization,
                });
                self.evict_old();
            }
            MetricEvent::ShredInsertIsFull {
                slot,
                last_index,
                num_repaired,
                num_recovered,
                ..
            } => {
                let row = self.slots.entry(*slot).or_default();
                row.insert_full = Some(InsertFull {
                    last_index: *last_index,
                    num_repaired: *num_repaired,
                    num_recovered: *num_recovered,
                });
                self.evict_old();
            }
            _ => {}
        }
    }

    fn tick(&mut self, _now: Instant) {
        // Pure data-driven pane; no per-frame state evolution.
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" slot lifecycle (last {MAX_SLOTS_SHOWN}) "))
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < 40 || inner.height < 2 {
            return;
        }

        // Most recent ready rows, newest at the back. Walk reverse
        // (newest first) to take the cap, then reverse again so the
        // visual order is oldest-at-top, newest-at-bottom.
        let mut ready: Vec<(u64, SlotRow)> = self
            .slots
            .iter()
            .rev()
            .filter(|(_, r)| r.is_ready())
            .take(MAX_SLOTS_SHOWN)
            .map(|(&s, &r)| (s, r))
            .collect();
        ready.reverse();

        for (i, (slot, row)) in ready.iter().enumerate() {
            let y = inner.y + i as u16;
            if y >= inner.y + inner.height.saturating_sub(1) {
                break;
            }
            let line = format_slot_line(*slot, row, inner.width);
            frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
        }

        // Caption (bottom row): legend for the timing bar segments.
        let cap_y = inner.y + inner.height.saturating_sub(1);
        let caption = "  shred → vote_notarize → finalized";
        let caption_line = Line::from(Span::styled(
            caption,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
        frame.render_widget(
            Paragraph::new(caption_line),
            Rect::new(inner.x, cap_y, inner.width, 1),
        );
    }
}

/// Build one row's `Line`: slot number, timing bar, total ms, fast flag,
/// shred-source partition.
fn format_slot_line(slot: u64, row: &SlotRow, width: u16) -> Line<'static> {
    // Bar width = 10 cells. Each segment proportional to its share of
    // the total. Use ▓ for the lit portion, ░ for unfilled remainder.
    const BAR_CELLS: u64 = 10;

    let (Some(tracking), Some(insert_full)) = (row.tracking, row.insert_full) else {
        return Line::from("");
    };

    let total_us = tracking
        .first_shred_us
        .saturating_add(tracking.vote_notarize_us)
        .saturating_add(tracking.finalized_us);
    let total_ms = total_us / 1000;

    let bar = build_timing_bar(tracking, BAR_CELLS);

    let fast_label = if tracking.is_fast_finalization {
        " fast "
    } else {
        " slow "
    };

    // Right-side detail: shred counts. last_index is the highest index
    // observed, so total shreds ≈ last_index + 1.
    let total_shreds = insert_full.last_index.saturating_add(1);
    let detail = format!(
        "({total_shreds} shreds · {} repair · {} fec)",
        insert_full.num_repaired, insert_full.num_recovered
    );

    let _ = width;
    Line::from(vec![
        Span::styled("  ", theme::label_style()),
        Span::styled(
            format!("{slot}"),
            theme::accent_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", theme::label_style()),
        Span::styled(bar, Style::default().fg(Color::Cyan)),
        Span::styled(" ", theme::label_style()),
        Span::styled(
            format!("{total_ms}ms"),
            theme::value_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            fast_label,
            if tracking.is_fast_finalization {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Yellow)
            },
        ),
        Span::styled("  ", theme::label_style()),
        Span::styled(detail, theme::label_style()),
    ])
}

/// Build the timing bar string. Each of the three segments takes up
/// cells proportional to its share of the total. Uses ▓ for the
/// shred segment, ▒ for vote_notarize, ░ for finalized.
fn build_timing_bar(t: Tracking, cells: u64) -> String {
    let total = t
        .first_shred_us
        .saturating_add(t.vote_notarize_us)
        .saturating_add(t.finalized_us)
        .max(1);
    let shred_cells = t.first_shred_us.saturating_mul(cells) / total;
    let vote_cells = t.vote_notarize_us.saturating_mul(cells) / total;
    let final_cells = cells.saturating_sub(shred_cells).saturating_sub(vote_cells);

    let mut s = String::with_capacity(cells as usize * 3);
    for _ in 0..shred_cells {
        s.push('▓');
    }
    for _ in 0..vote_cells {
        s.push('▒');
    }
    for _ in 0..final_cells {
        s.push('░');
    }
    s
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

    #[test]
    fn slot_not_ready_until_both_metrics_arrive() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::SlotTracking {
            slot: 100,
            first_shred_us: 1000,
            vote_notarize_us: 2000,
            finalized_us: 3000,
            is_fast_finalization: true,
        })));
        let row = p.slots.get(&100).unwrap();
        assert!(row.tracking.is_some());
        assert!(row.insert_full.is_none());
        assert!(!row.is_ready());

        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredInsertIsFull {
            slot: 100,
            total_time_ms: 40,
            last_index: 95,
            num_repaired: 0,
            num_recovered: 44,
        })));
        let row = p.slots.get(&100).unwrap();
        assert!(row.is_ready());
    }

    #[test]
    fn order_independent_metric_joining() {
        let mut p = SlotLifecyclePane::new();
        // insert_is_full first, tracking second
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredInsertIsFull {
            slot: 200,
            total_time_ms: 50,
            last_index: 80,
            num_repaired: 1,
            num_recovered: 10,
        })));
        p.on_event(&mk(EventKind::Metric(MetricEvent::SlotTracking {
            slot: 200,
            first_shred_us: 100,
            vote_notarize_us: 200,
            finalized_us: 300,
            is_fast_finalization: false,
        })));
        assert!(p.slots.get(&200).unwrap().is_ready());
    }

    #[test]
    fn evict_old_keeps_growth_bounded() {
        let mut p = SlotLifecyclePane::new();
        for s in 0..(MAX_SLOTS_SHOWN * 6) as u64 {
            p.on_event(&mk(EventKind::Metric(MetricEvent::SlotTracking {
                slot: s,
                first_shred_us: 1,
                vote_notarize_us: 1,
                finalized_us: 1,
                is_fast_finalization: true,
            })));
        }
        assert!(p.slots.len() <= MAX_SLOTS_SHOWN * 4);
    }

    #[test]
    fn non_metric_events_are_ignored() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::FirstShred { slot: 1 }));
        assert!(p.slots.is_empty());
    }

    #[test]
    fn timing_bar_segments_add_up_to_cells() {
        let t = Tracking {
            first_shred_us: 1000,
            vote_notarize_us: 2000,
            finalized_us: 3000,
            is_fast_finalization: false,
        };
        let bar = build_timing_bar(t, 12);
        assert_eq!(bar.chars().count(), 12);
    }

    #[test]
    fn timing_bar_handles_zero_total() {
        let t = Tracking {
            first_shred_us: 0,
            vote_notarize_us: 0,
            finalized_us: 0,
            is_fast_finalization: false,
        };
        let bar = build_timing_bar(t, 10);
        assert_eq!(bar.chars().count(), 10);
    }
}
