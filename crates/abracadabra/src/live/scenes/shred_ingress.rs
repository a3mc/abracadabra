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

/// Duration of one bucket.
///
/// One second feels readable: updates once per second (slow enough
/// to track each change), aligns with most per-sample metric
/// cadences. With 10 buckets per card (below) each card spans
/// exactly 10 seconds.
pub const BUCKET_DURATION: Duration = Duration::from_secs(1);

/// Buckets per mini-card. Cards are the larger visual unit: 10
/// buckets × 1s = 10 seconds per card. Cards are separated by a
/// dim divider so the operator sees discrete "packets" of time
/// sliding leftward as new ones populate on the right.
const CARD_BUCKETS: u16 = 10;

/// Width reserved on the left for lane labels.
const LABEL_COL_WIDTH: u16 = 9;

/// Divider rendered between cards.
const CARD_DIVIDER: &str = "┊";

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
    /// Predefined per-lane cap (full bar at this Σ value per 1-s
    /// bucket). Caps were rechosen for the 1-second bucket size and
    /// to tolerate the 5× replay-log testing path:
    ///
    /// - Turbine: ~10 shred_fetch batches/s × ~350 sh = ~3500 sh/s
    ///   at real time; ~17 500 at 5× replay. Cap = 20000 keeps both
    ///   in the 1–6 px range with no saturation.
    /// - Repair: ~5 sh per repair fetch, ~0.5 fetches/s = ~2 sh/s
    ///   real time. 5× replay can reach ~30 sh/s. Cap = 50 gives
    ///   1 px for typical real-time, ~5 px for replay bursts.
    /// - Drop: bursts can hit 40 discards/sample × ~1 sample/s =
    ///   ~40 real, ~200 replay. Cap = 200 prevents wall-of-full.
    /// - Err: rarer; 1–3 per event × 0–1 events/s. Cap = 10.
    const fn cap(self) -> u32 {
        match self {
            Self::Turbine => 20_000,
            Self::Repair => 50,
            Self::Drop => 200,
            Self::Err => 10,
        }
    }

    /// Lane order from top to bottom of the chart area.
    const fn row(self) -> u16 {
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

impl ShredIngressPane {
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

        for lane in [Lane::Turbine, Lane::Repair, Lane::Drop, Lane::Err] {
            render_lane_label(frame, label_area, chart_area, lane);
            render_lane_bars(frame, chart_area, lane, self.lane_ref(lane));
        }
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
                    "cards {}s ({}×{}s)",
                    CARD_BUCKETS as u64 * BUCKET_DURATION.as_secs(),
                    CARD_BUCKETS,
                    BUCKET_DURATION.as_secs()
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

    // Each card occupies `CARD_BUCKETS` bucket columns followed by one
    // separator column. The rightmost card holds the in-progress
    // bucket at its rightmost cell. Older cards drift left, separated
    // by dim dividers so the visual reads as discrete "packs" of
    // 10 seconds each.
    let card_stride = CARD_BUCKETS + 1;
    let total_visible_cells = chart_area.width;
    let rightmost_x = chart_area.x + chart_area.width - 1;

    // Walk visible cells from right to left.
    for cell in 0..total_visible_cells {
        let position_in_card = cell % card_stride;
        let x = rightmost_x - cell;
        if position_in_card == CARD_BUCKETS {
            // Divider column between cards.
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM);
            frame.render_widget(
                Paragraph::new(Span::styled(CARD_DIVIDER.to_owned(), style)),
                Rect::new(x, y, 1, 1),
            );
            continue;
        }
        // Map this cell back to a bucket index. Within the rightmost
        // card, the rightmost cell (cell=0) is the in-progress bucket.
        // For older cards, subtract the dividers we've crossed.
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

/// Map a bucket value to a fill level (0..=8).
///
/// `value / cap * 8`, clamped to `[0, 8]`. Values past `cap` saturate
/// at full bar (`█`) so a heavy burst doesn't disappear into
/// numerical territory that the visual cannot represent.
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
    fn fill_level_zero_is_empty() {
        assert_eq!(fill_level(0, 100), 0);
    }

    #[test]
    fn fill_level_at_cap_is_full() {
        assert_eq!(fill_level(100, 100), 8);
    }

    #[test]
    fn fill_level_past_cap_saturates() {
        assert_eq!(fill_level(1000, 100), 8);
    }

    #[test]
    fn fill_level_proportional() {
        // 50 % of 100 → 4 pixels (half block).
        assert_eq!(fill_level(50, 100), 4);
        // 25 % → 2 pixels.
        assert_eq!(fill_level(25, 100), 2);
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
        // Pins the per-lane caps so a future tweak shows up as a
        // failing test rather than a silently shifted scale.
        assert_eq!(Lane::Turbine.cap(), 20_000);
        assert_eq!(Lane::Repair.cap(), 50);
        assert_eq!(Lane::Drop.cap(), 200);
        assert_eq!(Lane::Err.cap(), 10);
    }
}
