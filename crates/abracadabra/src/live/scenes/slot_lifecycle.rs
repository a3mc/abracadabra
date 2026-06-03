//! Slot outcomes — single-paragraph rows, stable scale.
//!
//! One row per lane (fast / slow / skip / fec). Each row is a
//! single `Line` of styled spans built once per frame and rendered
//! as one `Paragraph` — see `shred_ingress` for the rationale.
//!
//! Scaling per lane uses [`stable_max`] (`2 × mean(nonzero buckets)`)
//! so single peaks don't reshape every other cell. Snapshot row
//! keeps rolling percentages over the last 64 slots.

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

pub const PANE_HEIGHT: u16 = 10;

const BUCKET_DURATION: Duration = Duration::from_millis(250);
const LANE_COUNT: u16 = 4;
const LABEL_COL_WIDTH: u16 = 10;
const HISTORY_CAPACITY: usize = 256;
const MIN_LANE_MAX: u64 = 1;

/// Cells between two adjacent card dividers (20 cells × 250 ms = 5 s).
const CELLS_PER_CARD: u16 = 20;
const CARD_DIVIDER: &str = "┊";

const BLOCK_BARS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// Dot-size glyphs for discrete event lanes (slow, skip). Three
/// visible levels read as event marks rather than gradient bars.
const MARK_BARS: [&str; 9] = [" ", "·", "·", "•", "•", "•", "●", "●", "●"];

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

const LANES: [Lane; 4] = [Lane::Fast, Lane::Slow, Lane::Skip, Lane::Fec];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Fast,
    Slow,
    Skip,
}

#[derive(Debug)]
struct LaneSpark {
    history: VecDeque<u32>,
    current: u32,
    current_started: Instant,
}

impl LaneSpark {
    fn new(now: Instant) -> Self {
        Self {
            history: VecDeque::with_capacity(HISTORY_CAPACITY),
            current: 0,
            current_started: now,
        }
    }

    const fn accumulate(&mut self, v: u32) {
        self.current = self.current.saturating_add(v);
    }

    fn advance(&mut self, now: Instant) {
        while now.saturating_duration_since(self.current_started) >= BUCKET_DURATION {
            self.history.push_back(self.current);
            self.current = 0;
            self.current_started += BUCKET_DURATION;
            while self.history.len() > HISTORY_CAPACITY {
                self.history.pop_front();
            }
        }
    }
}

pub struct SlotLifecyclePane {
    fast: LaneSpark,
    slow: LaneSpark,
    skip: LaneSpark,
    fec: LaneSpark,
    history: VecDeque<Outcome>,
    last_fec_per_slot: Option<u64>,
    now: Instant,
}

impl SlotLifecyclePane {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            fast: LaneSpark::new(now),
            slow: LaneSpark::new(now),
            skip: LaneSpark::new(now),
            fec: LaneSpark::new(now),
            history: VecDeque::with_capacity(ROLLING_WINDOW),
            last_fec_per_slot: None,
            now,
        }
    }

    const fn lane_ref(&self, lane: Lane) -> &LaneSpark {
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
        self.fast.advance(now);
        self.slow.advance(now);
        self.skip.advance(now);
        self.fec.advance(now);
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" slot outcomes ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < LABEL_COL_WIDTH + 8 || inner.height < LANE_COUNT + 2 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(LANE_COUNT),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(inner);

        self.render_lane_rows(frame, chunks[1]);
        self.render_snapshot(frame, chunks[3]);
    }
}

impl SlotLifecyclePane {
    fn render_lane_rows(&self, frame: &mut Frame<'_>, area: Rect) {
        for (i, lane) in LANES.iter().enumerate() {
            let y = area.y + i as u16;
            let row = Rect::new(area.x, y, area.width, 1);
            self.render_lane_row(frame, row, *lane);
        }
    }

    fn render_lane_row(&self, frame: &mut Frame<'_>, row_area: Rect, lane: Lane) {
        let h_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(LABEL_COL_WIDTH), Constraint::Min(8)])
            .split(row_area);
        let label_area = h_chunks[0];
        let chart_area = h_chunks[1];

        let label_line = Line::from(Span::styled(
            format!(" {} ●", lane.label()),
            Style::default()
                .fg(lane.colour())
                .add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(Paragraph::new(label_line), label_area);

        let line = build_chart_line(self.lane_ref(lane), chart_area.width, lane);
        frame.render_widget(Paragraph::new(line), chart_area);
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
                    "last {} slots · {} ms / cell · stable scale",
                    self.history.len().min(ROLLING_WINDOW),
                    BUCKET_DURATION.as_millis(),
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

/// Stable per-lane scaling max: `2 × mean(nonzero buckets)` across
/// retained history. See `shred_ingress::stable_max` for shared
/// rationale.
fn stable_max(spark: &LaneSpark) -> u64 {
    let mut count = 0u64;
    let mut sum = 0u64;
    for &v in spark.history.iter().chain(std::iter::once(&spark.current)) {
        if v > 0 {
            count = count.saturating_add(1);
            sum = sum.saturating_add(u64::from(v));
        }
    }
    if count == 0 {
        MIN_LANE_MAX
    } else {
        sum.saturating_mul(2)
            .checked_div(count)
            .unwrap_or(MIN_LANE_MAX)
            .max(MIN_LANE_MAX)
    }
}

fn build_visible_cells(spark: &LaneSpark, width: usize) -> Vec<Option<u32>> {
    if width == 0 {
        return Vec::new();
    }
    let history_window = width.saturating_sub(1);
    let history_skip = spark.history.len().saturating_sub(history_window);
    let real: Vec<u32> = spark
        .history
        .iter()
        .skip(history_skip)
        .copied()
        .chain(std::iter::once(spark.current))
        .collect();
    let pad = width.saturating_sub(real.len());
    let mut cells = Vec::with_capacity(width);
    for _ in 0..pad {
        cells.push(None);
    }
    for v in real {
        cells.push(Some(v));
    }
    cells
}

#[allow(clippy::cast_possible_truncation)]
fn level_for(value: u32, max: u64) -> usize {
    if max == 0 {
        return 0;
    }
    let level = (u64::from(value).saturating_mul(8) / max).min(8);
    level as usize
}

fn glyph_for_level(level: usize, bars: &[&'static str; 9]) -> &'static str {
    bars[level.min(8)]
}

/// Per-lane glyph table. High-rate aggregating lanes (fast, fec)
/// keep block bars; sparse event lanes (slow, skip) get dot marks
/// so a single slow finalization or skip stands out as an event.
const fn bars_for(lane: Lane) -> &'static [&'static str; 9] {
    match lane {
        Lane::Fast | Lane::Fec => &BLOCK_BARS,
        Lane::Slow | Lane::Skip => &MARK_BARS,
    }
}

const fn is_divider_offset(offset_from_right: usize) -> bool {
    offset_from_right > 0 && offset_from_right.is_multiple_of(CELLS_PER_CARD as usize)
}

fn build_chart_line(spark: &LaneSpark, chart_width: u16, lane: Lane) -> Line<'static> {
    let max = stable_max(spark);
    let width = chart_width as usize;
    let cells = build_visible_cells(spark, width);
    let bars = bars_for(lane);
    let lane_modifier = if lane.is_attention() {
        Modifier::BOLD
    } else {
        Modifier::DIM
    };
    let lane_style = Style::default()
        .fg(lane.colour())
        .add_modifier(lane_modifier);
    let div_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut buf_is_divider = false;

    for (idx, cell) in cells.iter().enumerate() {
        let offset_from_right = width.saturating_sub(1).saturating_sub(idx);
        let is_div = is_divider_offset(offset_from_right);
        if is_div != buf_is_divider && !buf.is_empty() {
            let style = if buf_is_divider {
                div_style
            } else {
                lane_style
            };
            spans.push(Span::styled(std::mem::take(&mut buf), style));
        }
        buf_is_divider = is_div;
        if is_div {
            buf.push_str(CARD_DIVIDER);
        } else if let Some(value) = cell {
            buf.push_str(glyph_for_level(level_for(*value, max), bars));
        } else {
            buf.push(' ');
        }
    }
    if !buf.is_empty() {
        let style = if buf_is_divider {
            div_style
        } else {
            lane_style
        };
        spans.push(Span::styled(buf, style));
    }
    Line::from(spans)
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
    fn fast_finalize_accumulates_fast_lane_and_records_outcome() {
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
    fn slow_finalize_accumulates_slow_lane() {
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
    fn voting_skip_accumulates_skip_lane() {
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

    #[test]
    fn stable_max_returns_floor_when_idle() {
        let now = Instant::now();
        let spark = LaneSpark::new(now);
        assert_eq!(stable_max(&spark), MIN_LANE_MAX);
    }

    #[test]
    fn stable_max_is_2x_mean_of_nonzero() {
        let now = Instant::now();
        let mut spark = LaneSpark::new(now);
        spark.history.push_back(3);
        spark.history.push_back(9);
        spark.current = 0;
        // mean nonzero = 6 → 2× = 12
        assert_eq!(stable_max(&spark), 12);
    }
}
