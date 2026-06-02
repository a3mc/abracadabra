//! Slot outcomes — bucketed bars.
//!
//! Same time-bucketed model as `shred_ingress`: 250ms windows, one
//! bucket per terminal column, fill height by accumulated value, per
//! lane scale chosen for visibility. Lanes:
//!
//! - **fast**  one Finalized{fast:true}  per slot → cap 3 events/bucket
//! - **slow**  one Finalized{fast:false} per slot → cap 1 (every slow visible)
//! - **skip**  one VotingSkip            per slot → cap 1 (any skip full bar)
//! - **fec**   Σ num_recovered (per-slot)        → cap 50
//!
//! Snapshot row carries rolling percentages and counts (last N slots)
//! so the operator gets both the *flow* (bars sliding left) and the
//! *aggregate* (numbers below).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::live::animation::Pane;
use crate::parser::{Event, EventKind, MetricEvent};
use crate::tui::theme;

pub const PANE_HEIGHT: u16 = 9;

const BUCKET_DURATION: Duration = Duration::from_secs(1);
const CARD_BUCKETS: u16 = 10;
const LABEL_COL_WIDTH: u16 = 9;
const CARD_DIVIDER: &str = "┊";

const FILL_CHARS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

const COL_GOOD: Color = Color::Green;
const COL_WARN: Color = Color::Yellow;
const COL_BAD: Color = Color::Red;
const COL_FEC: Color = Color::LightBlue;

/// Rolling window for the snapshot row's percentages. 64 slots ≈ 25 s
/// of cluster activity at Solana's ~400 ms slot time.
const ROLLING_WINDOW: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    Fast,
    Slow,
    Skip,
    Fec,
}

impl Lane {
    /// Per-lane cap (full bar at this Σ value per 1-s bucket). Caps
    /// rechosen for the 1-second bucket size and 5× replay tolerance.
    /// Skip / slow now have non-trivial gradation so 1 event reads as
    /// a small bar, 4+ saturate (was cap=1 → every event a full bar,
    /// which hid burst magnitude).
    const fn cap(self) -> u32 {
        match self {
            // ~2.5 slots/s × 1 fast = ~2.5/s real, ~12.5 at 5× replay.
            // Cap 15 keeps replay near full without clipping.
            Self::Fast => 15,
            // Slow finalizations are rare; cap 3 gives clear gradation
            // for bursts of 1, 2, 3.
            Self::Slow => 3,
            // Skip: cap 4 turns "1 skip" into a small bar, "4+" into
            // a full bar. Operator can read burst size at a glance.
            Self::Skip => 4,
            // Per-slot num_recovered ~30 × 2.5 slots/s = ~75/s real,
            // ~375/s replay. Cap 500 prevents wall-of-full.
            Self::Fec => 500,
        }
    }

    const fn row(self) -> u16 {
        match self {
            Self::Fast => 0,
            Self::Slow => 1,
            Self::Skip => 2,
            Self::Fec => 3,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Slow => "slow",
            Self::Skip => "skip",
            Self::Fec => "fec",
        }
    }

    const fn colour(self) -> Color {
        match self {
            Self::Fast => COL_GOOD,
            Self::Slow => COL_WARN,
            Self::Skip => COL_BAD,
            Self::Fec => COL_FEC,
        }
    }

    const fn is_attention(self) -> bool {
        matches!(self, Self::Slow | Self::Skip)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Fast,
    Slow,
    Skip,
}

#[derive(Debug)]
struct BucketLane {
    history: VecDeque<u32>,
    current: u32,
    current_started: Instant,
}

impl BucketLane {
    fn new(now: Instant) -> Self {
        Self {
            history: VecDeque::with_capacity(256),
            current: 0,
            current_started: now,
        }
    }

    const fn accumulate(&mut self, v: u32) {
        self.current = self.current.saturating_add(v);
    }

    fn advance(&mut self, now: Instant, max_history: usize) {
        while now.saturating_duration_since(self.current_started) >= BUCKET_DURATION {
            self.history.push_back(self.current);
            self.current = 0;
            self.current_started += BUCKET_DURATION;
            while self.history.len() > max_history {
                self.history.pop_front();
            }
        }
    }
}

pub struct SlotLifecyclePane {
    fast: BucketLane,
    slow: BucketLane,
    skip: BucketLane,
    fec: BucketLane,
    /// Rolling window of recent slot outcomes for the snapshot row.
    history: VecDeque<Outcome>,
    /// Last seen `num_recovered` per slot — shown as `fec/slot N` in
    /// the snapshot row.
    last_fec_per_slot: Option<u64>,
    now: Instant,
}

impl SlotLifecyclePane {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            fast: BucketLane::new(now),
            slow: BucketLane::new(now),
            skip: BucketLane::new(now),
            fec: BucketLane::new(now),
            history: VecDeque::with_capacity(ROLLING_WINDOW),
            last_fec_per_slot: None,
            now,
        }
    }

    const fn lane_ref(&self, lane: Lane) -> &BucketLane {
        match lane {
            Lane::Fast => &self.fast,
            Lane::Slow => &self.slow,
            Lane::Skip => &self.skip,
            Lane::Fec => &self.fec,
        }
    }

    fn record_outcome(&mut self, o: Outcome) {
        if self.history.len() == ROLLING_WINDOW {
            self.history.pop_front();
        }
        self.history.push_back(o);
    }

    fn ratio_pct(&self, want: Outcome) -> u64 {
        if self.history.is_empty() {
            return 0;
        }
        let hits = self.history.iter().filter(|o| **o == want).count();
        (hits as u64 * 100) / self.history.len() as u64
    }

    fn skip_count(&self) -> u64 {
        self.history.iter().filter(|o| **o == Outcome::Skip).count() as u64
    }
}

impl Default for SlotLifecyclePane {
    fn default() -> Self {
        Self::new()
    }
}

impl Pane for SlotLifecyclePane {
    fn on_event(&mut self, ev: &Event) {
        match &ev.kind {
            EventKind::Finalized { fast: true, .. } => {
                self.fast.accumulate(1);
                self.record_outcome(Outcome::Fast);
            }
            EventKind::Finalized { fast: false, .. } => {
                self.slow.accumulate(1);
                self.record_outcome(Outcome::Slow);
            }
            EventKind::VotingSkip { .. } => {
                self.skip.accumulate(1);
                self.record_outcome(Outcome::Skip);
            }
            EventKind::Metric(MetricEvent::ShredInsertIsFull { num_recovered, .. })
                if *num_recovered > 0 =>
            {
                self.fec
                    .accumulate(u32::try_from(*num_recovered).unwrap_or(u32::MAX));
                self.last_fec_per_slot = Some(*num_recovered);
            }
            _ => {}
        }
    }

    fn tick(&mut self, now: Instant) {
        self.now = now;
        let cap = 1024;
        self.fast.advance(now, cap);
        self.slow.advance(now, cap);
        self.skip.advance(now, cap);
        self.fec.advance(now, cap);
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" slot outcomes ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < 30 || inner.height < 6 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(4),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(inner);

        self.render_chart(frame, chunks[1]);
        self.render_snapshot(frame, chunks[3]);
    }
}

impl SlotLifecyclePane {
    fn render_chart(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width <= LABEL_COL_WIDTH + 4 {
            return;
        }
        let h_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(LABEL_COL_WIDTH), Constraint::Min(8)])
            .split(area);
        let label_area = h_chunks[0];
        let chart_area = h_chunks[1];

        for lane in [Lane::Fast, Lane::Slow, Lane::Skip, Lane::Fec] {
            render_lane_label(frame, label_area, chart_area, lane);
            render_lane_bars(frame, chart_area, lane, self.lane_ref(lane));
        }
    }

    fn render_snapshot(&self, frame: &mut Frame<'_>, area: Rect) {
        let fast_pct = self.ratio_pct(Outcome::Fast);
        let slow_pct = self.ratio_pct(Outcome::Slow);
        let skips = self.skip_count();
        let fec = self
            .last_fec_per_slot
            .map_or_else(|| "—".to_owned(), |n| format!("{n}"));

        let line = Line::from(vec![
            Span::styled(
                format!(" {fast_pct}%"),
                Style::default().fg(COL_GOOD).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" fast", theme::label_style()),
            sep(),
            Span::styled(
                format!("{slow_pct}%"),
                if slow_pct > 0 {
                    Style::default().fg(COL_WARN)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(" slow", theme::label_style()),
            sep(),
            Span::styled(
                format!("{skips}"),
                if skips > 0 {
                    Style::default().fg(COL_BAD).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(" skip", theme::label_style()),
            sep(),
            Span::styled(fec, Style::default().fg(COL_FEC)),
            Span::styled(" fec (last)", theme::label_style()),
            sep(),
            Span::styled(
                format!(
                    "last {} slots · cards {}s",
                    self.history.len().min(ROLLING_WINDOW),
                    CARD_BUCKETS as u64 * BUCKET_DURATION.as_secs(),
                ),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
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

fn render_lane_label(frame: &mut Frame<'_>, label_area: Rect, chart_area: Rect, lane: Lane) {
    let row = lane.row();
    if row >= chart_area.height {
        return;
    }
    let y = chart_area.y + row;
    let text = lane.label();
    let w = text.chars().count() as u16;
    if w + 1 > label_area.width {
        return;
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            text.to_owned(),
            Style::default()
                .fg(lane.colour())
                .add_modifier(Modifier::BOLD),
        )),
        Rect::new(label_area.x + 1, y, w, 1),
    );
}

fn render_lane_bars(frame: &mut Frame<'_>, chart_area: Rect, lane: Lane, bucket: &BucketLane) {
    let row = lane.row();
    if row >= chart_area.height || chart_area.width == 0 {
        return;
    }
    let y = chart_area.y + row;
    let cap = lane.cap();
    let colour = lane.colour();
    let is_attention = lane.is_attention();

    let card_stride = CARD_BUCKETS + 1;
    let total_visible_cells = chart_area.width;
    let rightmost_x = chart_area.x + chart_area.width - 1;

    for cell in 0..total_visible_cells {
        let position_in_card = cell % card_stride;
        let x = rightmost_x - cell;
        if position_in_card == CARD_BUCKETS {
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM);
            frame.render_widget(
                Paragraph::new(Span::styled(CARD_DIVIDER.to_owned(), style)),
                Rect::new(x, y, 1, 1),
            );
            continue;
        }
        let dividers_crossed = cell / card_stride;
        let bucket_index_from_right = cell - dividers_crossed;
        let value = if bucket_index_from_right == 0 {
            bucket.current
        } else {
            let idx = bucket
                .history
                .len()
                .wrapping_sub(bucket_index_from_right as usize);
            bucket.history.get(idx).copied().unwrap_or(0)
        };
        let pixels = fill_level(value, cap);
        if pixels == 0 {
            continue;
        }
        let glyph = FILL_CHARS[pixels];
        let modifier = if is_attention {
            Modifier::BOLD
        } else {
            Modifier::DIM
        };
        let style = Style::default().fg(colour).add_modifier(modifier);
        frame.render_widget(
            Paragraph::new(Span::styled(glyph.to_owned(), style)),
            Rect::new(x, y, 1, 1),
        );
    }
}

fn fill_level(value: u32, cap: u32) -> usize {
    if cap == 0 {
        return 0;
    }
    let level = (u64::from(value) * 8 / u64::from(cap)) as usize;
    level.min(8)
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
    fn fast_finalize_accumulates_fast_bucket_and_records_outcome() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::Finalized {
            slot: 1,
            hash: "h".into(),
            fast: true,
        }));
        assert_eq!(p.fast.current, 1);
        assert_eq!(p.history.back(), Some(&Outcome::Fast));
    }

    #[test]
    fn slow_finalize_accumulates_slow_bucket() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::Finalized {
            slot: 1,
            hash: "h".into(),
            fast: false,
        }));
        assert_eq!(p.slow.current, 1);
        assert_eq!(p.history.back(), Some(&Outcome::Slow));
    }

    #[test]
    fn voting_skip_accumulates_skip_bucket() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::VotingSkip { slot: 1 }));
        assert_eq!(p.skip.current, 1);
        assert_eq!(p.history.back(), Some(&Outcome::Skip));
    }

    #[test]
    fn shred_insert_is_full_accumulates_fec_only_when_nonzero() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredInsertIsFull {
            slot: 1,
            total_time_ms: 10,
            last_index: 100,
            num_repaired: 0,
            num_recovered: 0,
        })));
        assert_eq!(p.fec.current, 0);
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredInsertIsFull {
            slot: 1,
            total_time_ms: 10,
            last_index: 100,
            num_repaired: 0,
            num_recovered: 44,
        })));
        assert_eq!(p.fec.current, 44);
        assert_eq!(p.last_fec_per_slot, Some(44));
    }

    #[test]
    fn ratio_reflects_history_mix() {
        let mut p = SlotLifecyclePane::new();
        for _ in 0..3 {
            p.on_event(&mk(EventKind::Finalized {
                slot: 0,
                hash: "h".into(),
                fast: true,
            }));
        }
        p.on_event(&mk(EventKind::VotingSkip { slot: 0 }));
        assert_eq!(p.ratio_pct(Outcome::Fast), 75);
        assert_eq!(p.skip_count(), 1);
    }

    #[test]
    fn lane_caps_match_documented_values() {
        assert_eq!(Lane::Fast.cap(), 15);
        assert_eq!(Lane::Slow.cap(), 3);
        assert_eq!(Lane::Skip.cap(), 4);
        assert_eq!(Lane::Fec.cap(), 500);
    }

    #[test]
    fn fill_level_at_or_past_cap_saturates() {
        assert_eq!(fill_level(1, 1), 8);
        assert_eq!(fill_level(100, 1), 8);
    }

    #[test]
    fn rolling_window_caps_history() {
        let mut p = SlotLifecyclePane::new();
        for _ in 0..(ROLLING_WINDOW * 3) {
            p.on_event(&mk(EventKind::Finalized {
                slot: 0,
                hash: "h".into(),
                fast: true,
            }));
        }
        assert_eq!(p.history.len(), ROLLING_WINDOW);
    }
}
