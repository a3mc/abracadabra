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

const BUCKET_DURATION: Duration = Duration::from_secs(10);
const LANES_PER_CARD: u16 = 4;
const CARD_SLOT_WIDTH: u16 = LANES_PER_CARD + 1;
const CARD_BAR_ROWS: u16 = 4;
const LABEL_COL_WIDTH: u16 = 9;
const CARD_DIVIDER: &str = "│";

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
    /// Cap = Σ value over the 10-second card window at which the bar
    /// fills the full bar height. Tuned for 5× replay.
    const fn cap(self) -> u32 {
        match self {
            // ~25 fast finalizations per 10s real-time, ~125 at 5×.
            Self::Fast => 150,
            // Slow rare; cap 5 keeps single events visible, 5 fills.
            Self::Slow => 5,
            // Skip rare; same gradation as slow.
            Self::Skip => 5,
            // Per-slot num_recovered ~30 × 25 slots = ~750 real-time,
            // ~3750 at 5×. Cap 4000 prevents wall-of-full.
            Self::Fec => 4_000,
        }
    }

    /// Position of this lane within a card (column 0..LANES_PER_CARD).
    const fn card_col(self) -> u16 {
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

        if inner.width < 30 || inner.height < CARD_BAR_ROWS + 2 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(CARD_BAR_ROWS),
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
        if area.width <= LABEL_COL_WIDTH + CARD_SLOT_WIDTH {
            return;
        }
        let h_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(LABEL_COL_WIDTH), Constraint::Min(8)])
            .split(area);
        let label_area = h_chunks[0];
        let chart_area = h_chunks[1];

        render_legend(frame, label_area);
        render_card_flow(
            frame,
            chart_area,
            &[
                (Lane::Fast, self.lane_ref(Lane::Fast)),
                (Lane::Slow, self.lane_ref(Lane::Slow)),
                (Lane::Skip, self.lane_ref(Lane::Skip)),
                (Lane::Fec, self.lane_ref(Lane::Fec)),
            ],
        );
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
                    "last {} slots · card {}s × {} lanes",
                    self.history.len().min(ROLLING_WINDOW),
                    BUCKET_DURATION.as_secs(),
                    LANES_PER_CARD,
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

fn render_legend(frame: &mut Frame<'_>, label_area: Rect) {
    if label_area.height < CARD_BAR_ROWS || label_area.width < 4 {
        return;
    }
    let lanes = [Lane::Fast, Lane::Slow, Lane::Skip, Lane::Fec];
    for (row, lane) in lanes.iter().enumerate() {
        let y = label_area.y + row as u16;
        let line = Line::from(vec![
            Span::styled(
                format!(" {}", lane.label()),
                Style::default()
                    .fg(lane.colour())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " ●",
                Style::default()
                    .fg(lane.colour())
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(label_area.x, y, label_area.width, 1),
        );
    }
}

type LaneBuckets<'a> = [(Lane, &'a BucketLane); LANES_PER_CARD as usize];

fn render_card_flow(frame: &mut Frame<'_>, chart_area: Rect, lanes: &LaneBuckets<'_>) {
    if chart_area.width == 0 || chart_area.height < CARD_BAR_ROWS {
        return;
    }
    let rightmost_x = chart_area.x + chart_area.width - 1;
    let stride = CARD_SLOT_WIDTH;
    let n_cards = chart_area.width / stride;

    for card_index in 0..n_cards {
        let card_right_x = rightmost_x - card_index * stride;
        let bars_left_x = card_right_x.saturating_sub(LANES_PER_CARD - 1);

        if card_index + 1 < n_cards && bars_left_x > 0 {
            let div_x = bars_left_x - 1;
            for row in 0..CARD_BAR_ROWS {
                let y = chart_area.y + row;
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        CARD_DIVIDER.to_owned(),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    )),
                    Rect::new(div_x, y, 1, 1),
                );
            }
        }

        for (lane, bucket) in lanes {
            let value = lookup_bucket_value(bucket, card_index as usize);
            let cap = lane.cap();
            let col_x = bars_left_x + lane.card_col();
            render_vertical_bar(frame, col_x, chart_area.y, value, cap, *lane);
        }
    }
}

fn lookup_bucket_value(bucket: &BucketLane, n_back: usize) -> u32 {
    if n_back == 0 {
        return bucket.current;
    }
    let idx = bucket.history.len().wrapping_sub(n_back);
    bucket.history.get(idx).copied().unwrap_or(0)
}

fn render_vertical_bar(
    frame: &mut Frame<'_>,
    col_x: u16,
    top_y: u16,
    value: u32,
    cap: u32,
    lane: Lane,
) {
    if cap == 0 {
        return;
    }
    let total_subpixels = u32::from(CARD_BAR_ROWS) * 8;
    let filled = (u64::from(value) * u64::from(total_subpixels) / u64::from(cap))
        .min(u64::from(total_subpixels)) as u32;
    let modifier = if lane.is_attention() {
        Modifier::BOLD
    } else {
        Modifier::DIM
    };
    let style = Style::default().fg(lane.colour()).add_modifier(modifier);
    for row_from_bottom in 0..CARD_BAR_ROWS {
        let consumed_below = u32::from(row_from_bottom) * 8;
        let in_this_row = filled.saturating_sub(consumed_below).min(8) as usize;
        if in_this_row == 0 {
            continue;
        }
        let glyph = FILL_CHARS[in_this_row];
        let y = top_y + (CARD_BAR_ROWS - 1 - row_from_bottom);
        frame.render_widget(
            Paragraph::new(Span::styled(glyph.to_owned(), style)),
            Rect::new(col_x, y, 1, 1),
        );
    }
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
        assert_eq!(Lane::Fast.cap(), 150);
        assert_eq!(Lane::Slow.cap(), 5);
        assert_eq!(Lane::Skip.cap(), 5);
        assert_eq!(Lane::Fec.cap(), 4_000);
    }

    #[test]
    fn card_col_assignment_unique_per_lane() {
        assert_eq!(Lane::Fast.card_col(), 0);
        assert_eq!(Lane::Slow.card_col(), 1);
        assert_eq!(Lane::Skip.card_col(), 2);
        assert_eq!(Lane::Fec.card_col(), 3);
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
