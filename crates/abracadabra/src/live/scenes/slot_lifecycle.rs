//! Slot lifecycle strip — cards drifting horizontally.
//!
//! Layout (half-width pane, ~12 rows tall):
//!
//! ```text
//! ┌─ recent slots ─────────────────────────────────────┐
//! │                                                    │
//! │   ┌─ 2070551 ─┐  ┌─ 2070552 ─┐  ┌─ 2070553 ─┐      │
//! │   │ 137ms  ⚡ │  │  92ms  ⚡ │  │ 144ms  ⚡ │      │
//! │   │ 96 shr    │  │ 94 shr    │  │ 95 shr    │      │
//! │   │ ▰▰▰▰▰▰▰░░│  │ ▰▰▰▰▰░░░░│  │ ▰▰▰▰▰▰░░░│      │
//! │   │ T96 R0 F44│  │ T72 R0 F22│  │ T51 R0 F44│      │
//! │   └───────────┘  └───────────┘  └───────────┘      │
//! │                                                    │
//! │           ←  newer slots push from the right       │
//! └────────────────────────────────────────────────────┘
//! ```
//!
//! Each card shows ONE finalized slot. New cards spawn on the right;
//! existing cards drift left as new ones push in; cards exit off the
//! left edge after ~30s. Visible card count adapts to the pane width
//! (roughly `width / 16`).
//!
//! Data per card:
//!
//! - `<slot>` — the slot number.
//! - `<total>ms` — `first_shred_us + vote_notarize_us + finalized_us`
//!   from `event_handler_slot_tracking`, divided by 1000.
//! - `⚡` if `is_fast_finalization == true` (NotarizeFast path);
//!   blank otherwise (slow Notarize+Finalize path).
//! - `N shr` — total shred count for the slot (`last_index + 1`
//!   from `shred_insert_is_full`).
//! - T/R/F bar — proportional split of shreds by source:
//!   - `T` (Turbine, green) = inserted shreds (this is what we want
//!     to see fill the bar)
//!   - `R` (Repair, yellow) = `num_repaired` (any yellow = upstream
//!     Turbine had gaps for this slot)
//!   - `F` (FEC, light blue) = `num_recovered` (clever reconstruction)

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::live::animation::Pane;
use crate::parser::{Event, EventKind, MetricEvent};
use crate::tui::theme;

/// Pane row height when laid out by [`crate::live::scenes::SceneEngine`].
pub const PANE_HEIGHT: u16 = 12;

/// Cell width of one card (incl. its borders). 13 inner + 2 borders = 15
/// reads as ~3 cards on a 50-col half-width pane.
const CARD_WIDTH: u16 = 15;

/// Horizontal gap between cards.
const CARD_GAP: u16 = 1;

/// How long a card stays visible after the slot's data lands. Old cards
/// drift left and exit off the left edge.
const CARD_LIFESPAN: Duration = Duration::from_secs(30);

/// Cells per second the cards drift leftward.
const DRIFT_CELLS_PER_SEC: f64 = 1.0;

/// Maximum slots we track in `slots`. Older entries evict regardless of
/// readiness so growth is bounded even if one metric never arrives.
const MAX_TRACKED: usize = 64;

// Semantic palette — shared with [`crate::live::scenes::shred_ingress`].
// Turbine colour is exported via the bar's "T" label which is the
// `inserted` count (everything that wasn't repair or FEC came via
// Turbine, by construction).
const COL_REPAIR: Color = Color::Yellow;
const COL_FEC: Color = Color::LightBlue;
const COL_INSERTED: Color = Color::Green;

#[derive(Debug, Default, Clone, Copy)]
struct SlotRow {
    tracking: Option<Tracking>,
    insert_full: Option<InsertFull>,
    ready_at: Option<Instant>,
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
    /// All currently-tracked slots, ordered by slot number. Capped at
    /// [`MAX_TRACKED`] to bound growth.
    slots: BTreeMap<u64, SlotRow>,
    now: Instant,
}

impl SlotLifecyclePane {
    pub fn new() -> Self {
        Self {
            slots: BTreeMap::new(),
            now: Instant::now(),
        }
    }

    const fn ready_now_if_needed(row: &mut SlotRow, now: Instant) {
        if row.is_ready() && row.ready_at.is_none() {
            row.ready_at = Some(now);
        }
    }

    fn evict_old(&mut self) {
        while self.slots.len() > MAX_TRACKED {
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
                Self::ready_now_if_needed(row, self.now);
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
                Self::ready_now_if_needed(row, self.now);
                self.evict_old();
            }
            _ => {}
        }
    }

    fn tick(&mut self, now: Instant) {
        self.now = now;
        // Drop cards whose ready_at is older than CARD_LIFESPAN; they
        // have already drifted off the visible area.
        self.slots.retain(|_, r| {
            r.ready_at
                .is_none_or(|t| now.saturating_duration_since(t) < CARD_LIFESPAN)
        });
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" recent slots · ←  newer push from right ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < CARD_WIDTH + 4 || inner.height < 6 {
            return;
        }

        // Compute card positions: newest card hugs the right inner
        // edge; each older card sits CARD_WIDTH + CARD_GAP to its left,
        // shifted further left by its drift age * DRIFT_CELLS_PER_SEC.
        let visible: Vec<(u64, SlotRow, Instant)> = self
            .slots
            .iter()
            .filter_map(|(&s, r)| r.ready_at.map(|t| (s, *r, t)))
            .collect();

        // Sort newest first by ready_at.
        let mut sorted = visible;
        sorted.sort_by_key(|x| std::cmp::Reverse(x.2));

        for (rank, (slot, row, ready_at)) in sorted.iter().enumerate() {
            let age_secs = self.now.saturating_duration_since(*ready_at).as_secs_f64();
            let drift = (age_secs * DRIFT_CELLS_PER_SEC) as u16;
            let stride = CARD_WIDTH + CARD_GAP;
            #[allow(clippy::cast_possible_truncation)]
            let rank_u16 = rank as u16;
            // Position from the right: rightmost card at inner.right - CARD_WIDTH.
            let from_right = stride.saturating_mul(rank_u16).saturating_add(drift);
            if from_right + CARD_WIDTH > inner.width {
                break; // ran off the left edge
            }
            let x = inner.x + inner.width - CARD_WIDTH - from_right;
            let y = inner.y;
            if y + 6 > inner.y + inner.height {
                break;
            }
            render_card(frame, Rect::new(x, y, CARD_WIDTH, 6), *slot, row);
        }

        // Caption row.
        let cap_y = inner.y + inner.height.saturating_sub(1);
        let caption = "  T=turbine · R=repair · F=fec";
        frame.render_widget(
            Paragraph::new(Span::styled(
                caption,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            )),
            Rect::new(inner.x, cap_y, inner.width, 1),
        );
    }
}

/// Render one slot card at `rect` (assumes 15 wide × 6 tall).
fn render_card(frame: &mut Frame<'_>, rect: Rect, slot: u64, row: &SlotRow) {
    let (Some(tracking), Some(insert_full)) = (row.tracking, row.insert_full) else {
        return;
    };

    let total_us = tracking
        .first_shred_us
        .saturating_add(tracking.vote_notarize_us)
        .saturating_add(tracking.finalized_us);
    let total_ms = total_us / 1000;

    let total_shreds = insert_full.last_index.saturating_add(1);
    let inserted = total_shreds
        .saturating_sub(insert_full.num_repaired)
        .saturating_sub(insert_full.num_recovered);

    let bar = trf_bar(
        inserted,
        insert_full.num_repaired,
        insert_full.num_recovered,
        11,
    );

    // Border style: warmer (yellow) when repair is non-zero so the
    // card pulls the operator's eye to itself.
    let border_style = if insert_full.num_repaired > 0 {
        Style::default().fg(COL_REPAIR)
    } else if tracking.is_fast_finalization {
        Style::default().fg(COL_INSERTED)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {slot} "))
        .title_style(theme::title_style())
        .border_style(border_style);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    // Row 1: total ms + fast flag
    let ms_line = Line::from(vec![
        Span::styled(
            format!(" {total_ms:>4}ms"),
            theme::value_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if tracking.is_fast_finalization {
                "  ⚡"
            } else {
                "    "
            },
            Style::default()
                .fg(if tracking.is_fast_finalization {
                    COL_INSERTED
                } else {
                    Color::DarkGray
                })
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    if inner.height > 0 {
        frame.render_widget(
            Paragraph::new(ms_line),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
    }

    // Row 2: total shreds
    if inner.height > 1 {
        let line = Line::from(vec![
            Span::styled(" ", theme::label_style()),
            Span::styled(format!("{total_shreds}"), theme::value_style()),
            Span::styled(" shreds", theme::label_style()),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );
    }

    // Row 3: T/R/F bar
    if inner.height > 2 {
        frame.render_widget(
            Paragraph::new(Line::from(bar)),
            Rect::new(inner.x, inner.y + 2, inner.width, 1),
        );
    }

    // Row 4: T/R/F numerics
    if inner.height > 3 {
        let line = Line::from(vec![
            Span::styled(" T", Style::default().fg(COL_INSERTED)),
            Span::styled(
                format!("{inserted}"),
                Style::default()
                    .fg(COL_INSERTED)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" R", Style::default().fg(COL_REPAIR)),
            Span::styled(
                format!("{}", insert_full.num_repaired),
                if insert_full.num_repaired > 0 {
                    Style::default().fg(COL_REPAIR).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(" F", Style::default().fg(COL_FEC)),
            Span::styled(
                format!("{}", insert_full.num_recovered),
                Style::default().fg(COL_FEC),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, inner.y + 3, inner.width, 1),
        );
    }
}

/// Build the T/R/F partition bar as a `Vec<Span>` so each segment
/// carries its own colour. Total cells fixed at `cells`. Segments are
/// proportional to their share of `t + r + f`.
fn trf_bar(t: u64, r: u64, f: u64, cells: u64) -> Vec<Span<'static>> {
    let total = t.saturating_add(r).saturating_add(f).max(1);
    let t_cells = t.saturating_mul(cells) / total;
    let r_cells = r.saturating_mul(cells) / total;
    let f_cells = cells.saturating_sub(t_cells).saturating_sub(r_cells);
    let mut spans = Vec::with_capacity(4);
    if t_cells > 0 {
        spans.push(Span::styled(
            "▰".repeat(t_cells as usize),
            Style::default().fg(COL_INSERTED),
        ));
    }
    if r_cells > 0 {
        spans.push(Span::styled(
            "▰".repeat(r_cells as usize),
            Style::default().fg(COL_REPAIR).add_modifier(Modifier::BOLD),
        ));
    }
    if f_cells > 0 {
        spans.push(Span::styled(
            "▰".repeat(f_cells as usize),
            Style::default().fg(COL_FEC),
        ));
    }
    spans
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
    fn slot_becomes_ready_when_both_metrics_arrive() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::SlotTracking {
            slot: 1,
            first_shred_us: 100,
            vote_notarize_us: 200,
            finalized_us: 300,
            is_fast_finalization: true,
        })));
        assert!(p.slots.get(&1).unwrap().ready_at.is_none());
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredInsertIsFull {
            slot: 1,
            total_time_ms: 5,
            last_index: 50,
            num_repaired: 0,
            num_recovered: 5,
        })));
        assert!(p.slots.get(&1).unwrap().ready_at.is_some());
    }

    #[test]
    fn order_independent_join() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredInsertIsFull {
            slot: 2,
            total_time_ms: 5,
            last_index: 50,
            num_repaired: 1,
            num_recovered: 0,
        })));
        p.on_event(&mk(EventKind::Metric(MetricEvent::SlotTracking {
            slot: 2,
            first_shred_us: 1,
            vote_notarize_us: 2,
            finalized_us: 3,
            is_fast_finalization: false,
        })));
        assert!(p.slots.get(&2).unwrap().ready_at.is_some());
    }

    #[test]
    fn lifespan_drops_old_cards_on_tick() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::SlotTracking {
            slot: 3,
            first_shred_us: 1,
            vote_notarize_us: 1,
            finalized_us: 1,
            is_fast_finalization: false,
        })));
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredInsertIsFull {
            slot: 3,
            total_time_ms: 1,
            last_index: 1,
            num_repaired: 0,
            num_recovered: 0,
        })));
        // Backdate ready_at past CARD_LIFESPAN.
        let row = p.slots.get_mut(&3).unwrap();
        row.ready_at = Instant::now().checked_sub(CARD_LIFESPAN + Duration::from_secs(1));
        p.tick(Instant::now());
        assert!(!p.slots.contains_key(&3));
    }

    #[test]
    fn trf_bar_segments_sum_to_cells() {
        let spans = trf_bar(10, 0, 5, 10);
        let total_chars: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        assert_eq!(total_chars, 10);
    }

    #[test]
    fn trf_bar_handles_zeros() {
        let spans = trf_bar(0, 0, 0, 10);
        let total_chars: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        assert_eq!(total_chars, 10);
    }

    #[test]
    fn tracked_count_capped() {
        let mut p = SlotLifecyclePane::new();
        for s in 0..(MAX_TRACKED * 3) as u64 {
            p.on_event(&mk(EventKind::Metric(MetricEvent::SlotTracking {
                slot: s,
                first_shred_us: 1,
                vote_notarize_us: 1,
                finalized_us: 1,
                is_fast_finalization: false,
            })));
        }
        assert!(p.slots.len() <= MAX_TRACKED);
    }
}
