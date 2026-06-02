//! Shred streams — bucketed bars.
//!
//! Time is sliced into 250ms buckets. Each bucket = one terminal
//! column. Each lane's bucket aggregates the relevant metric field
//! (Σ shred_count, Σ num_discards, etc.), scales against a predefined
//! per-lane cap, and renders one of nine Unicode block-fill levels.
//!
//! ```text
//! ┌─ shred streams ───────────────────────────────────────┐
//! │                                                       │
//! │ turbine ▅▆▅▇▅▆▆▅▆▆▅▆▆▅▇▆▅▆▆▆▅▆▅▆▆▅▇▆▅▆▆▆▅▆▅▆▆▅      │   cyan, steady
//! │ repair         ▃                  ▂                   │   yellow, sparse
//! │ drop                                                  │   magenta, empty (healthy)
//! │ err                                                   │   red, empty (healthy)
//! │                                                       │
//! │ 357 sh  ·  5 rep  ·  0 drop  ·  0 err  ·  per-sample  │   snapshot
//! └───────────────────────────────────────────────────────┘
//! ```
//!
//! Newest bucket on the right; older buckets slide left as new ones
//! complete. The visible time window equals `pane_width × BUCKET_MS`
//! (~37s at 150-col half-width and 250ms buckets).
//!
//! Per-lane caps (full-bar threshold) are picked so typical activity
//! shows in the 4–6 pixel range and bursts saturate to a full bar
//! `█`. See [`Lane::cap`] for the rationale per lane.

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

/// Duration of one bucket = one full mini-card.
///
/// The current (rightmost) card actively populates as events arrive;
/// once 10 seconds elapse it "departs" leftward (becomes a static
/// snapshot) and a fresh empty card starts populating on the right.
pub const BUCKET_DURATION: Duration = Duration::from_secs(10);

/// Number of lanes shown side-by-side inside one card.
const LANES_PER_CARD: u16 = 4;

/// Total width of one card slot in cells, including the gap between
/// cards. `LANES_PER_CARD` vertical bars + 1 divider column.
const CARD_SLOT_WIDTH: u16 = LANES_PER_CARD + 1;

/// Bar rows inside each card. Each row holds one terminal cell whose
/// sub-pixel fill is taken from `FILL_CHARS`. Total levels per bar =
/// `CARD_BAR_ROWS * 8`.
const CARD_BAR_ROWS: u16 = 4;

/// Width reserved on the left for lane labels.
const LABEL_COL_WIDTH: u16 = 9;

/// Vertical divider rendered between cards.
const CARD_DIVIDER: &str = "│";

/// Nine block-fill levels. Index 0 = empty space, 8 = full block.
/// Unicode lower-block-fill characters give a clean bottom-up bar.
const FILL_CHARS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

const COL_TURBINE: Color = Color::Cyan;
const COL_REPAIR: Color = Color::Yellow;
const COL_DROP: Color = Color::LightMagenta;
const COL_ERR: Color = Color::Red;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    Turbine,
    Repair,
    Drop,
    Err,
}

impl Lane {
    /// Cap = Σ value over the 10-second card window at which the
    /// lane's bar fills the full bar height (CARD_BAR_ROWS × 8 = 32
    /// sub-pixel levels). Tuned for 5× replay-log testing so typical
    /// activity fills ~30–60 % of the bar and bursts saturate.
    ///
    /// - Turbine: ~3 500 sh/s real-time × 10 s = ~35 000 per card;
    ///   ~175 000 at 5× replay. Cap = 200 000 prevents saturation.
    /// - Repair: ~2 sh/s real-time × 10 s = ~20; ~100 replay.
    ///   Cap = 150 keeps replay bursts visible.
    /// - Drop: ~40/s replay × 10 = ~400. Cap = 1 000 for headroom.
    /// - Err: ~1/s × 10 s = ~10. Cap = 50.
    const fn cap(self) -> u32 {
        match self {
            Self::Turbine => 200_000,
            Self::Repair => 150,
            Self::Drop => 1_000,
            Self::Err => 50,
        }
    }

    /// Position of this lane within a card (column 0..LANES_PER_CARD).
    const fn card_col(self) -> u16 {
        match self {
            Self::Turbine => 0,
            Self::Repair => 1,
            Self::Drop => 2,
            Self::Err => 3,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Turbine => "turbine",
            Self::Repair => "repair",
            Self::Drop => "drop",
            Self::Err => "err",
        }
    }

    const fn colour(self) -> Color {
        match self {
            Self::Turbine => COL_TURBINE,
            Self::Repair => COL_REPAIR,
            Self::Drop => COL_DROP,
            Self::Err => COL_ERR,
        }
    }

    /// Whether the lane is a calm baseline (rendered DIM when filled)
    /// or an attention lane (rendered BOLD when filled).
    const fn is_attention(self) -> bool {
        matches!(self, Self::Repair | Self::Drop | Self::Err)
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct LatestNumbers {
    fetch: Option<u64>,
    repair: Option<u64>,
    drop: Option<u64>,
    err: Option<u64>,
}

/// One lane's bucket history. `current` is the in-progress bucket
/// being accumulated; on bucket roll-over it pushes to `history`.
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

    /// Add `v` to the current in-progress bucket.
    const fn accumulate(&mut self, v: u32) {
        self.current = self.current.saturating_add(v);
    }

    /// Advance the bucket clock to `now`. May roll the current bucket
    /// into history one or more times if multiple bucket windows have
    /// elapsed since the last call (e.g. after a pause).
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

pub struct ShredIngressPane {
    turbine: BucketLane,
    repair: BucketLane,
    drop_lane: BucketLane,
    err_lane: BucketLane,
    numbers: LatestNumbers,
    now: Instant,
}

impl ShredIngressPane {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            turbine: BucketLane::new(now),
            repair: BucketLane::new(now),
            drop_lane: BucketLane::new(now),
            err_lane: BucketLane::new(now),
            numbers: LatestNumbers::default(),
            now,
        }
    }

    const fn lane_mut(&mut self, lane: Lane) -> &mut BucketLane {
        match lane {
            Lane::Turbine => &mut self.turbine,
            Lane::Repair => &mut self.repair,
            Lane::Drop => &mut self.drop_lane,
            Lane::Err => &mut self.err_lane,
        }
    }

    const fn lane_ref(&self, lane: Lane) -> &BucketLane {
        match lane {
            Lane::Turbine => &self.turbine,
            Lane::Repair => &self.repair,
            Lane::Drop => &self.drop_lane,
            Lane::Err => &self.err_lane,
        }
    }
}

impl Default for ShredIngressPane {
    fn default() -> Self {
        Self::new()
    }
}

impl Pane for ShredIngressPane {
    fn on_event(&mut self, ev: &Event) {
        let EventKind::Metric(m) = &ev.kind else {
            return;
        };
        match m {
            MetricEvent::ShredFetch { shred_count } => {
                self.numbers.fetch = Some(*shred_count);
                self.lane_mut(Lane::Turbine)
                    .accumulate(u32::try_from(*shred_count).unwrap_or(u32::MAX));
            }
            MetricEvent::ShredFetchRepair { shred_count } => {
                self.numbers.repair = Some(*shred_count);
                self.lane_mut(Lane::Repair)
                    .accumulate(u32::try_from(*shred_count).unwrap_or(u32::MAX));
            }
            MetricEvent::ShredSigverify { num_discards, .. } => {
                self.numbers.drop = Some(*num_discards);
                self.lane_mut(Lane::Drop)
                    .accumulate(u32::try_from(*num_discards).unwrap_or(u32::MAX));
            }
            MetricEvent::RecvWindowInsert { num_errors, .. } => {
                self.numbers.err = Some(*num_errors);
                self.lane_mut(Lane::Err)
                    .accumulate(u32::try_from(*num_errors).unwrap_or(u32::MAX));
            }
            _ => {}
        }
    }

    fn tick(&mut self, now: Instant) {
        self.now = now;
        // History capacity is generous; the render loop only paints
        // what fits in the chart area, so we cap once at the upper
        // limit and let the renderer slice.
        let cap = 1024;
        self.turbine.advance(now, cap);
        self.repair.advance(now, cap);
        self.drop_lane.advance(now, cap);
        self.err_lane.advance(now, cap);
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" shred streams ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Need enough height for the 4 bar rows + 1 row breathing
        // above + 1 snapshot row at bottom.
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

impl ShredIngressPane {
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
                (Lane::Turbine, self.lane_ref(Lane::Turbine)),
                (Lane::Repair, self.lane_ref(Lane::Repair)),
                (Lane::Drop, self.lane_ref(Lane::Drop)),
                (Lane::Err, self.lane_ref(Lane::Err)),
            ],
        );
    }

    fn render_snapshot(&self, frame: &mut Frame<'_>, area: Rect) {
        let fetch = fmt_opt(self.numbers.fetch);
        let repair = fmt_opt(self.numbers.repair);
        let drop = fmt_opt(self.numbers.drop);
        let err = fmt_opt(self.numbers.err);

        let line = Line::from(vec![
            Span::styled(format!(" {fetch}"), Style::default().fg(COL_TURBINE)),
            Span::styled(" sh", theme::label_style()),
            sep(),
            Span::styled(
                repair,
                if self.numbers.repair.unwrap_or(0) > 0 {
                    Style::default().fg(COL_REPAIR).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(" rep", theme::label_style()),
            sep(),
            Span::styled(
                drop,
                if self.numbers.drop.unwrap_or(0) > 0 {
                    Style::default().fg(COL_DROP).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(" drop", theme::label_style()),
            sep(),
            Span::styled(
                err,
                if self.numbers.err.unwrap_or(0) > 0 {
                    Style::default().fg(COL_ERR).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(" err", theme::label_style()),
            sep(),
            Span::styled(
                format!(
                    "card {}s × {} lanes",
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

fn fmt_opt(v: Option<u64>) -> String {
    v.map_or_else(|| "—".to_owned(), |n| format!("{n}"))
}

fn sep() -> Span<'static> {
    Span::styled(
        "  ·  ",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    )
}

/// Paint the legend in the label column: one row per lane showing
/// `name ●` with the lane's colour, so the operator knows which
/// vertical bar inside each card maps to which lane.
fn render_legend(frame: &mut Frame<'_>, label_area: Rect) {
    if label_area.height < CARD_BAR_ROWS || label_area.width < 4 {
        return;
    }
    let lanes = [Lane::Turbine, Lane::Repair, Lane::Drop, Lane::Err];
    for (row, lane) in lanes.iter().enumerate() {
        let y = label_area.y + row as u16;
        let label = lane.label();
        let line = Line::from(vec![
            Span::styled(
                format!(" {label}"),
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

/// Paint the card flow. Cards line up right-to-left across the chart
/// area; each card has LANES_PER_CARD vertical bars (one per lane)
/// followed by one divider column. The rightmost card is the in-
/// progress (current) bucket — its bars grow as new events arrive.
fn render_card_flow(frame: &mut Frame<'_>, chart_area: Rect, lanes: &LaneBuckets<'_>) {
    if chart_area.width == 0 || chart_area.height < CARD_BAR_ROWS {
        return;
    }
    let rightmost_x = chart_area.x + chart_area.width - 1;
    let stride = CARD_SLOT_WIDTH;
    let n_cards = chart_area.width / stride;

    for card_index in 0..n_cards {
        // card_index 0 = rightmost (current); higher = older.
        let card_right_x = rightmost_x - card_index * stride;
        // The card slot is [card_left_x, card_right_x] where
        // card_left_x = card_right_x - (LANES_PER_CARD - 1). The
        // divider column sits at card_left_x - 1.
        let bars_left_x = card_right_x.saturating_sub(LANES_PER_CARD - 1);

        // Divider column (between THIS card and the OLDER one to its
        // left). Skip if the older card slot wouldn't exist.
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

        // Each lane within the card gets its own column.
        for (lane, bucket) in lanes {
            let value = lookup_bucket_value(bucket, card_index as usize);
            let cap = lane.cap();
            let col_x = bars_left_x + lane.card_col();
            render_vertical_bar(frame, col_x, chart_area.y, value, cap, *lane);
        }
    }
}

/// Look up the bucket value `n_back` cards from the present.
/// `n_back == 0` returns the current (in-progress) bucket;
/// `n_back == 1` returns the most recently committed bucket; etc.
fn lookup_bucket_value(bucket: &BucketLane, n_back: usize) -> u32 {
    if n_back == 0 {
        return bucket.current;
    }
    let idx = bucket.history.len().wrapping_sub(n_back);
    bucket.history.get(idx).copied().unwrap_or(0)
}

/// Render one vertical bar at column `col_x`, starting at `top_y`,
/// spanning [`CARD_BAR_ROWS`] rows. Value/cap is mapped to a total
/// fill of `CARD_BAR_ROWS × 8` sub-pixels, distributed bottom-up.
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
    // Bottom-up: the bottom row of the bar fills first.
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
    fn shred_fetch_accumulates_into_turbine_current() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetch {
            shred_count: 350,
        })));
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetch {
            shred_count: 400,
        })));
        assert_eq!(p.turbine.current, 750);
        assert_eq!(p.numbers.fetch, Some(400));
    }

    #[test]
    fn tick_rolls_current_bucket_into_history_after_duration() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetch {
            shred_count: 100,
        })));
        // Force the elapsed time past one bucket window.
        p.turbine.current_started = Instant::now()
            .checked_sub(BUCKET_DURATION + Duration::from_millis(10))
            .unwrap();
        p.tick(Instant::now());
        assert_eq!(p.turbine.current, 0);
        assert_eq!(p.turbine.history.back().copied(), Some(100));
    }

    #[test]
    fn repair_only_accumulates_when_shred_count_present() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetchRepair {
            shred_count: 0,
        })));
        // Accumulation of 0 is allowed (history will show empty), but
        // the field updates regardless.
        assert_eq!(p.repair.current, 0);
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetchRepair {
            shred_count: 5,
        })));
        assert_eq!(p.repair.current, 5);
    }

    #[test]
    fn drop_lane_accumulates_discards() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredSigverify {
            num_packets: 100,
            num_discards: 8,
            num_duplicates: 0,
            elapsed_micros: 1,
        })));
        assert_eq!(p.drop_lane.current, 8);
    }

    #[test]
    fn err_lane_accumulates_errors() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::RecvWindowInsert {
            num_shreds_received: 100,
            num_errors: 3,
        })));
        assert_eq!(p.err_lane.current, 3);
    }

    #[test]
    fn non_metric_events_ignored() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::FirstShred { slot: 1 }));
        assert_eq!(p.turbine.current, 0);
    }

    #[test]
    fn lane_caps_match_documented_values() {
        assert_eq!(Lane::Turbine.cap(), 200_000);
        assert_eq!(Lane::Repair.cap(), 150);
        assert_eq!(Lane::Drop.cap(), 1_000);
        assert_eq!(Lane::Err.cap(), 50);
    }

    #[test]
    fn card_col_assignment_unique_per_lane() {
        assert_eq!(Lane::Turbine.card_col(), 0);
        assert_eq!(Lane::Repair.card_col(), 1);
        assert_eq!(Lane::Drop.card_col(), 2);
        assert_eq!(Lane::Err.card_col(), 3);
    }
}
