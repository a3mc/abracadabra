//! Shred streams — turbine as a particle stream, others as
//! sparklines.
//!
//! Turbine is a real time-based particle simulation, not a bucket
//! sparkline. The validator emits `ShredFetch` metric datapoints
//! infrequently (sometimes once per ~25 seconds on a live node), so
//! a per-bucket aggregation leaves the row almost entirely empty.
//! Instead, each event *spawns* particles that persist for ~25 s
//! and drift left at constant velocity; the chart stays visually
//! populated even when source events are sparse, and when events
//! come fast the row densifies into a real flow.
//!
//! - Each `ShredFetch` spawns `count / SHREDS_PER_PARTICLE` particles
//!   (capped at [`MAX_PARTICLES_PER_EVENT`]), with random vertical
//!   position and [`PARTICLE_STAGGER`] between them.
//! - Particles drift left 1 sub-pixel per [`SUBPIXEL_DURATION`]
//!   (8 sub-pixels per second). At chart width ≈ 100 cells and 2
//!   sub-pixels per Braille cell, one event's trail spans the
//!   chart over [`PARTICLE_LIFETIME`].
//! - Turbine occupies 2 chart rows (Braille glyphs have 4 vertical
//!   sub-pixels per cell × 2 rows = 8 vertical positions).
//!
//! Other lanes (repair / drop / err) keep the stable mean-based
//! sparkline rendering. They are sparse-event lanes; bucket sums
//! read clearly as block bars.
//!
//! BUCKET_DURATION is 250 ms (was 500 ms): twice the cells, twice
//! the scroll speed.

#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
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
const LABEL_COL_WIDTH: u16 = 10;
const HISTORY_CAPACITY: usize = 256;
const MIN_LANE_MAX: u64 = 1;

const CELLS_PER_CARD: u16 = 20; // 20 cells × 250 ms = 5 s per slot
const CARD_DIVIDER: &str = "┊";

const BLOCK_BARS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// Dot-size glyphs for discrete event lanes. Three visible levels
/// (·, •, ●) read as event marks rather than a gradient bar — the
/// shape signals "an event happened" with rough magnitude, instead
/// of treating sparse events like a continuous quantity.
const MARK_BARS: [&str; 9] = [" ", "·", "·", "•", "•", "•", "●", "●", "●"];

const COL_TURBINE: Color = Color::Cyan;
const COL_REPAIR: Color = Color::Yellow;
const COL_DROP: Color = Color::LightMagenta;
const COL_ERR: Color = Color::Red;

const TURBINE_ROWS: u16 = 2;
const SPARK_LANES: usize = 3; // repair, drop, err

/// One Braille cell has 4 vertical dots × 2 horizontal columns.
/// A `TURBINE_ROWS`-tall stack has `TURBINE_ROWS × 4` vertical
/// sub-pixels; sub-pixel widths are 2 per cell.
const SUBPIXEL_DURATION: Duration = Duration::from_millis(125);
const SUBPIXELS_PER_CELL: usize = 2;
const Y_PER_ROW: u8 = 4;

/// How long a particle is visible. Sized so a particle drifts the
/// width of any reasonable terminal before being pruned:
/// 120 s × 8 sub-pixels/s = 960 sub-pixels = 480 cells. At 2 sub-
/// pixels per Braille cell, this covers fullscreen on any terminal
/// up to ~480 columns wide.
const PARTICLE_LIFETIME: Duration = Duration::from_secs(120);
/// Each particle represents this many shreds. Big events become
/// dense bursts; small events become single particles.
const SHREDS_PER_PARTICLE: u32 = 16;
/// Cap to keep storage and rendering bounded for huge bursts.
const MAX_PARTICLES_PER_EVENT: u32 = 48;
/// Inter-particle release delay within a single burst. Spreads
/// particles horizontally so a burst looks like a comet, not a
/// single bright column.
const PARTICLE_STAGGER: Duration = Duration::from_millis(30);
/// Upper bound on retained bursts. Each burst expands to up to
/// `MAX_PARTICLES_PER_EVENT` particles at render time. 2048 ≈ 17 s
/// of bursts at one burst per 8 ms, well past pathological replay
/// rates.
const MAX_RETAINED_BURSTS: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    Turbine,
    Repair,
    Drop,
    Err,
}

impl Lane {
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

    const fn is_attention(self) -> bool {
        matches!(self, Self::Repair | Self::Drop | Self::Err)
    }
}

/// Flow-arrow marker shown right after every lane name. Uniform
/// across all lanes (both panes) so the markers line up in a single
/// label column for a calm visual rhythm.
const LANE_MARKER: &str = "▶";

const SPARK_LANE_LIST: [Lane; SPARK_LANES] = [Lane::Repair, Lane::Drop, Lane::Err];

#[derive(Debug, Default, Clone, Copy)]
struct LatestNumbers {
    fetch: Option<u64>,
    repair: Option<u64>,
    drop: Option<u64>,
    err: Option<u64>,
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

/// One emitted `ShredFetch` event, stored as a single record. At
/// render time each burst is expanded into [`Self::particle_count`]
/// particles whose Y positions come from `y_seed`; particle X
/// positions are derived from age relative to `spawn_ts`. Storing
/// bursts (not pre-expanded particles) keeps memory ~`48×` smaller
/// for the same visible history, which matters once
/// `PARTICLE_LIFETIME` is long enough to cover fullscreen terminals.
#[derive(Debug, Clone, Copy)]
struct EventBurst {
    spawn_ts: Instant,
    /// Already clamped to `MAX_PARTICLES_PER_EVENT` at construction.
    particle_count: u8,
    /// Seed for deterministic per-particle Y at render time. Mixed
    /// with the particle's index inside the burst.
    y_seed: u64,
}

/// Cache for the turbine particle rasterisation.
///
/// `row_bits` is reused across frames: between successive renders
/// within the same sub-pixel tick and with no new burst, the
/// existing buffer is re-emitted instead of being rebuilt. Sized
/// lazily to `TURBINE_ROWS × chart_width` on first use and any
/// time `chart_width` changes (terminal resize).
#[derive(Debug, Default)]
struct TurbineCache {
    /// `Some(tick)` once the cache has been populated; cleared on
    /// burst arrival to force the next render to re-rasterise.
    subpixel_tick: Option<u64>,
    burst_count: usize,
    chart_width: usize,
    row_bits: Vec<Vec<u8>>,
}

/// Time-based particle stream for turbine. Stores one [`EventBurst`]
/// per `ShredFetch`; particles are expanded inline by the renderer.
#[derive(Debug)]
struct TurbineStream {
    bursts: VecDeque<EventBurst>,
    stream_start: Instant,
    /// Interior-mutable cache populated by the renderer (`render`
    /// is `&self` per the [`Pane`] contract, so the cache lives
    /// behind a [`RefCell`]).
    cache: RefCell<TurbineCache>,
    /// Counts the number of full rasterisation passes. Used by tests
    /// to assert cache reuse across frames.
    #[cfg(test)]
    rasterise_count: Cell<usize>,
}

impl TurbineStream {
    fn new(now: Instant) -> Self {
        Self {
            bursts: VecDeque::with_capacity(256),
            stream_start: now,
            cache: RefCell::new(TurbineCache::default()),
            #[cfg(test)]
            rasterise_count: Cell::new(0),
        }
    }

    fn on_event(&mut self, now: Instant, count: u32) {
        let n = count
            .div_ceil(SHREDS_PER_PARTICLE)
            .clamp(1, MAX_PARTICLES_PER_EVENT);
        let y_seed = now.saturating_duration_since(self.stream_start).as_nanos() as u64;
        self.bursts.push_back(EventBurst {
            spawn_ts: now,
            particle_count: n as u8,
            y_seed,
        });
        self.prune(now);
        // Belt-and-braces: the bursts-count check already catches
        // this, but clearing the tick stamp keeps the cache state
        // unambiguous after every new event.
        self.cache.borrow_mut().subpixel_tick = None;
    }

    fn tick(&mut self, now: Instant) {
        let pre = self.bursts.len();
        self.prune(now);
        if self.bursts.len() != pre {
            // Pruned bursts also invalidate the cached raster.
            self.cache.borrow_mut().subpixel_tick = None;
        }
    }

    fn prune(&mut self, now: Instant) {
        if let Some(cutoff) = now.checked_sub(PARTICLE_LIFETIME) {
            while let Some(b) = self.bursts.front() {
                if b.spawn_ts < cutoff {
                    self.bursts.pop_front();
                } else {
                    break;
                }
            }
        }
        while self.bursts.len() > MAX_RETAINED_BURSTS {
            self.bursts.pop_front();
        }
    }

    /// Total live particle count across all retained bursts (used by
    /// the snapshot row to surface the visualisation's working set).
    fn particle_count(&self) -> usize {
        self.bursts
            .iter()
            .map(|b| usize::from(b.particle_count))
            .sum()
    }
}

/// Deterministic Y position for a particle, in
/// `0..(TURBINE_ROWS × Y_PER_ROW)`. Hash-mixed so consecutive
/// particles within a burst don't all stack on the same row.
fn particle_y(base_nanos: u64, idx: u32) -> u8 {
    let h = base_nanos
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(u64::from(idx).wrapping_mul(0xC6BC_2796_92B5_C323));
    let total = u64::from(TURBINE_ROWS) * u64::from(Y_PER_ROW);
    ((h >> 27) % total) as u8
}

pub struct ShredIngressPane {
    turbine: LaneSpark,
    turbine_stream: TurbineStream,
    repair: LaneSpark,
    drop_lane: LaneSpark,
    err_lane: LaneSpark,
    numbers: LatestNumbers,
    now: Instant,
}

impl ShredIngressPane {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            turbine: LaneSpark::new(now),
            turbine_stream: TurbineStream::new(now),
            repair: LaneSpark::new(now),
            drop_lane: LaneSpark::new(now),
            err_lane: LaneSpark::new(now),
            numbers: LatestNumbers::default(),
            now,
        }
    }

    const fn lane_ref(&self, lane: Lane) -> &LaneSpark {
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
                let v = u32::try_from(*shred_count).unwrap_or(u32::MAX);
                self.turbine.accumulate(v);
                // Particle stream spawns proportional to count.
                self.turbine_stream.on_event(self.now, v);
            }
            MetricEvent::ShredFetchRepair { shred_count } => {
                self.numbers.repair = Some(*shred_count);
                self.repair
                    .accumulate(u32::try_from(*shred_count).unwrap_or(u32::MAX));
            }
            MetricEvent::ShredSigverify { num_discards, .. } => {
                self.numbers.drop = Some(*num_discards);
                self.drop_lane
                    .accumulate(u32::try_from(*num_discards).unwrap_or(u32::MAX));
            }
            MetricEvent::RecvWindowInsert { num_errors, .. } => {
                self.numbers.err = Some(*num_errors);
                self.err_lane
                    .accumulate(u32::try_from(*num_errors).unwrap_or(u32::MAX));
            }
            _ => {}
        }
    }

    fn tick(&mut self, now: Instant) {
        self.now = now;
        self.turbine.advance(now);
        self.turbine_stream.tick(now);
        self.repair.advance(now);
        self.drop_lane.advance(now);
        self.err_lane.advance(now);
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" shred streams ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let min_height = 1 + TURBINE_ROWS + SPARK_LANES as u16 + 1;
        if inner.width < LABEL_COL_WIDTH + 8 || inner.height < min_height {
            return;
        }

        // Vertical layout: top spacer / turbine (2 rows) /
        // repair-drop-err (1 row each) / filler / snapshot.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(TURBINE_ROWS),
                Constraint::Length(SPARK_LANES as u16),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(inner);

        self.render_turbine(frame, chunks[1]);
        self.render_spark_lanes(frame, chunks[2]);
        self.render_snapshot(frame, chunks[4]);
    }
}

impl ShredIngressPane {
    fn render_turbine(&self, frame: &mut Frame<'_>, area: Rect) {
        let h = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(LABEL_COL_WIDTH), Constraint::Min(8)])
            .split(area);
        let label_area = h[0];
        let chart_area = h[1];

        // Label only on the first row of the turbine block. Format
        // pads the lane name to 7 chars so all marker arrows across
        // the four lanes sit at the same column.
        let label_line = Line::from(Span::styled(
            format!(" {:<7} {LANE_MARKER}", Lane::Turbine.label()),
            Style::default()
                .fg(Lane::Turbine.colour())
                .add_modifier(Modifier::BOLD),
        ));
        let label_first = Rect::new(label_area.x, label_area.y, label_area.width, 1);
        frame.render_widget(Paragraph::new(label_line), label_first);

        render_turbine_particles(frame, chart_area, &self.turbine_stream, self.now);
    }

    fn render_spark_lanes(&self, frame: &mut Frame<'_>, area: Rect) {
        for (i, lane) in SPARK_LANE_LIST.iter().enumerate() {
            let y = area.y + i as u16;
            let row = Rect::new(area.x, y, area.width, 1);
            self.render_spark_row(frame, row, *lane);
        }
    }

    fn render_spark_row(&self, frame: &mut Frame<'_>, row_area: Rect, lane: Lane) {
        let h = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(LABEL_COL_WIDTH), Constraint::Min(8)])
            .split(row_area);
        let label_area = h[0];
        let chart_area = h[1];

        let label_line = Line::from(Span::styled(
            format!(" {:<7} {LANE_MARKER}", lane.label()),
            Style::default()
                .fg(lane.colour())
                .add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(Paragraph::new(label_line), label_area);

        let line = build_chart_line(self.lane_ref(lane), chart_area.width, lane);
        frame.render_widget(Paragraph::new(line), chart_area);
    }

    fn render_snapshot(&self, frame: &mut Frame<'_>, area: Rect) {
        let fetch = fmt_opt(self.numbers.fetch);
        let repair = fmt_opt(self.numbers.repair);
        let drop = fmt_opt(self.numbers.drop);
        let err = fmt_opt(self.numbers.err);
        let particles = self.turbine_stream.particle_count();

        // Compact snapshot — half-width laptop pane is ~70 cells of
        // inner width. Drop "latest sample:" prefix and abbreviate
        // "turbine particles in flight" → "particles".
        let line = Line::from(vec![
            Span::styled(" ", theme::label_style()),
            Span::styled(fetch, Style::default().fg(COL_TURBINE)),
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
                format!("{particles} particles"),
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

/// Standard Braille dot bit values per (col, row) — see U+2800 block:
/// dots are numbered 1-8 in a 2×4 grid (col, row):
/// (0,0)=1=0x01 (1,0)=4=0x08
/// (0,1)=2=0x02 (1,1)=5=0x10
/// (0,2)=3=0x04 (1,2)=6=0x20
/// (0,3)=7=0x40 (1,3)=8=0x80
const fn braille_bit(col: u8, row: u8) -> u8 {
    match (col, row) {
        (0, 0) => 0x01,
        (1, 0) => 0x08,
        (0, 1) => 0x02,
        (1, 1) => 0x10,
        (0, 2) => 0x04,
        (1, 2) => 0x20,
        (0, 3) => 0x40,
        (1, 3) => 0x80,
        _ => 0,
    }
}

/// Render the turbine particle field. The chart area is
/// [`TURBINE_ROWS`] rows tall; particles' Y values map into one of
/// `TURBINE_ROWS × Y_PER_ROW` vertical sub-pixels, and the row a
/// particle lands in is `y / Y_PER_ROW`. Card dividers are
/// embedded into the same `Line` as the Braille glyphs.
///
/// The bit-pattern raster is cached on [`TurbineStream`]: while the
/// sub-pixel tick, burst count, and chart width are unchanged the
/// renderer reuses the cached `row_bits` instead of re-expanding
/// every burst. Cache is invalidated by `on_event` (new burst),
/// `tick` (pruned burst), terminal resize, and sub-pixel advance.
fn render_turbine_particles(
    frame: &mut Frame<'_>,
    chart_area: Rect,
    stream: &TurbineStream,
    now: Instant,
) {
    let chart_width = chart_area.width as usize;
    if chart_width == 0 || chart_area.height == 0 {
        return;
    }

    let subpixel_micros = SUBPIXEL_DURATION.as_micros() as u64;
    let elapsed_micros = now
        .saturating_duration_since(stream.stream_start)
        .as_micros() as u64;
    let current_subpixel = elapsed_micros / subpixel_micros.max(1);
    let burst_count = stream.bursts.len();

    let mut cache = stream.cache.borrow_mut();
    let hit = cache.subpixel_tick == Some(current_subpixel)
        && cache.burst_count == burst_count
        && cache.chart_width == chart_width
        && cache.row_bits.len() == TURBINE_ROWS as usize
        && cache.row_bits.iter().all(|r| r.len() == chart_width);

    if !hit {
        rasterise_turbine_row_bits(stream, now, chart_width, &mut cache.row_bits);
        cache.subpixel_tick = Some(current_subpixel);
        cache.burst_count = burst_count;
        cache.chart_width = chart_width;
        #[cfg(test)]
        stream.rasterise_count.set(stream.rasterise_count.get() + 1);
    }

    let particle_style = Style::default()
        .fg(COL_TURBINE)
        .add_modifier(Modifier::BOLD);
    let div_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);

    for (row_idx, bits) in cache.row_bits.iter().enumerate() {
        let line = build_turbine_row_line(bits, chart_width, particle_style, div_style);
        let y = chart_area.y + row_idx as u16;
        let row_rect = Rect::new(chart_area.x, y, chart_area.width, 1);
        frame.render_widget(Paragraph::new(line), row_rect);
    }
}

/// Resize and zero `row_bits` to `TURBINE_ROWS × chart_width`,
/// then expand every retained burst into its constituent particles
/// and OR each particle's Braille bit into the matching cell.
///
/// Reuses the existing allocation: outer `Vec` is resized to
/// `TURBINE_ROWS` rows, each inner `Vec<u8>` is resized to
/// `chart_width` and zero-filled in place.
fn rasterise_turbine_row_bits(
    stream: &TurbineStream,
    now: Instant,
    chart_width: usize,
    row_bits: &mut Vec<Vec<u8>>,
) {
    let total_subpixels = chart_width * SUBPIXELS_PER_CELL;
    let subpixel_micros = SUBPIXEL_DURATION.as_micros() as u64;

    row_bits.resize_with(TURBINE_ROWS as usize, Vec::new);
    for row in row_bits.iter_mut() {
        row.clear();
        row.resize(chart_width, 0u8);
    }

    for burst in &stream.bursts {
        for i in 0..u32::from(burst.particle_count) {
            let spawn_ts = burst.spawn_ts + PARTICLE_STAGGER.saturating_mul(i);
            if spawn_ts > now {
                continue;
            }
            let age_micros = now.duration_since(spawn_ts).as_micros() as u64;
            let drift = (age_micros / subpixel_micros.max(1)) as usize;
            if drift >= total_subpixels {
                continue;
            }
            let sub_x = total_subpixels - 1 - drift;
            let cell_idx = sub_x / SUBPIXELS_PER_CELL;
            let cell_col = (sub_x % SUBPIXELS_PER_CELL) as u8;
            let y = particle_y(burst.y_seed, i);
            let row = (y / Y_PER_ROW) as usize;
            let y_in_row = y % Y_PER_ROW;
            if row >= row_bits.len() || cell_idx >= chart_width {
                continue;
            }
            row_bits[row][cell_idx] |= braille_bit(cell_col, y_in_row);
        }
    }
}

/// Build the styled `Line` for a single rasterised turbine row.
/// Inlines card dividers at the same offsets as the sparkline rows
/// below so the four lanes share a unified divider grid.
fn build_turbine_row_line(
    bits: &[u8],
    chart_width: usize,
    particle_style: Style,
    div_style: Style,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut buf_is_divider = false;
    for (idx, &b) in bits.iter().enumerate() {
        let offset_from_right = chart_width.saturating_sub(1).saturating_sub(idx);
        let is_div = is_divider_offset(offset_from_right);
        if is_div != buf_is_divider && !buf.is_empty() {
            let s = if buf_is_divider {
                div_style
            } else {
                particle_style
            };
            spans.push(Span::styled(std::mem::take(&mut buf), s));
        }
        buf_is_divider = is_div;
        if is_div {
            buf.push_str(CARD_DIVIDER);
        } else {
            let codepoint = 0x2800u32 + u32::from(b);
            buf.push(char::from_u32(codepoint).unwrap_or('⠀'));
        }
    }
    if !buf.is_empty() {
        let s = if buf_is_divider {
            div_style
        } else {
            particle_style
        };
        spans.push(Span::styled(buf, s));
    }
    Line::from(spans)
}

/// Stable per-lane scaling for sparkline rows: `2 × mean(nonzero)`
/// across retained history. See LIVE-21 for rationale.
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

/// Per-lane glyph table. Sparse event lanes (repair, drop, err)
/// use dot marks; turbine is rendered via the particle path so
/// this is unused for it. The match is exhaustive so any future
/// lane gets an explicit decision instead of a silent default.
const fn bars_for(lane: Lane) -> &'static [&'static str; 9] {
    match lane {
        Lane::Turbine => &BLOCK_BARS,
        Lane::Repair | Lane::Drop | Lane::Err => &MARK_BARS,
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
    fn shred_fetch_spawns_turbine_burst_with_expected_particle_count() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetch {
            shred_count: 100,
        })));
        assert_eq!(p.turbine_stream.bursts.len(), 1);
        // 100 / 16 = 7 particles (div_ceil)
        let count = p.turbine_stream.bursts[0].particle_count;
        assert!((6..=8).contains(&count), "got {count}");
        assert_eq!(p.turbine_stream.particle_count(), usize::from(count));
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
        assert_eq!(p.turbine_stream.bursts.len(), 0);
        assert_eq!(p.turbine_stream.particle_count(), 0);
    }

    #[test]
    fn particle_event_caps_count_at_max() {
        let mut p = ShredIngressPane::new();
        // Huge event — particle_count should be clamped to MAX_PARTICLES_PER_EVENT.
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetch {
            shred_count: 100_000,
        })));
        assert_eq!(p.turbine_stream.bursts.len(), 1);
        assert_eq!(
            u32::from(p.turbine_stream.bursts[0].particle_count),
            MAX_PARTICLES_PER_EVENT
        );
    }

    #[test]
    fn build_visible_cells_pads_left_with_none() {
        let now = Instant::now();
        let mut spark = LaneSpark::new(now);
        spark.history.push_back(5);
        spark.history.push_back(10);
        spark.current = 7;
        let cells = build_visible_cells(&spark, 6);
        assert_eq!(cells, vec![None, None, None, Some(5), Some(10), Some(7)]);
    }

    #[test]
    fn stable_max_is_2x_mean_of_nonzero() {
        let now = Instant::now();
        let mut spark = LaneSpark::new(now);
        spark.history.push_back(4);
        spark.history.push_back(8);
        spark.current = 0;
        assert_eq!(stable_max(&spark), 12);
    }

    #[test]
    fn divider_offsets_anchored_to_right_edge() {
        assert!(!is_divider_offset(0));
        assert!(is_divider_offset(CELLS_PER_CARD as usize));
        assert!(is_divider_offset((CELLS_PER_CARD * 2) as usize));
        assert!(!is_divider_offset(CELLS_PER_CARD as usize + 1));
    }

    #[test]
    fn particle_y_within_bounds() {
        for idx in 0..1000 {
            let y = particle_y(idx as u64 * 31, idx as u32);
            assert!(u16::from(y) < TURBINE_ROWS * u16::from(Y_PER_ROW));
        }
    }

    #[test]
    fn turbine_row_bits_cached_until_subpixel_tick() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        let t0 = Instant::now();
        let stream = {
            let mut s = TurbineStream::new(t0);
            s.on_event(t0, 64);
            s
        };

        // Two renders inside the same sub-pixel tick. Drawing area
        // is fixed, no new burst arrives, and `now` advances by
        // less than SUBPIXEL_DURATION (125 ms). The second render
        // must hit the cache.
        let chart_rect = Rect::new(0, 0, 60, TURBINE_ROWS);
        terminal
            .draw(|f| render_turbine_particles(f, chart_rect, &stream, t0))
            .unwrap();
        let after_first = stream.rasterise_count.get();
        assert_eq!(after_first, 1, "first render should rasterise");

        let t1 = t0 + Duration::from_millis(50);
        terminal
            .draw(|f| render_turbine_particles(f, chart_rect, &stream, t1))
            .unwrap();
        assert_eq!(
            stream.rasterise_count.get(),
            after_first,
            "second render within the same sub-pixel tick must reuse cache",
        );

        // Advancing past the sub-pixel boundary forces a re-raster.
        let t2 = t0 + SUBPIXEL_DURATION + Duration::from_millis(1);
        terminal
            .draw(|f| render_turbine_particles(f, chart_rect, &stream, t2))
            .unwrap();
        assert_eq!(
            stream.rasterise_count.get(),
            after_first + 1,
            "crossing a sub-pixel boundary must re-rasterise",
        );
    }

    #[test]
    fn turbine_cache_invalidated_on_new_burst() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        let t0 = Instant::now();
        let mut stream = TurbineStream::new(t0);
        stream.on_event(t0, 32);
        let chart_rect = Rect::new(0, 0, 60, TURBINE_ROWS);
        terminal
            .draw(|f| render_turbine_particles(f, chart_rect, &stream, t0))
            .unwrap();
        let baseline = stream.rasterise_count.get();

        // New burst within the same sub-pixel tick must invalidate.
        stream.on_event(t0, 16);
        terminal
            .draw(|f| render_turbine_particles(f, chart_rect, &stream, t0))
            .unwrap();
        assert_eq!(
            stream.rasterise_count.get(),
            baseline + 1,
            "new burst must invalidate the cache",
        );
    }

    #[test]
    fn turbine_cache_invalidated_on_resize() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(120, 10)).unwrap();
        let t0 = Instant::now();
        let stream = {
            let mut s = TurbineStream::new(t0);
            s.on_event(t0, 64);
            s
        };
        terminal
            .draw(|f| render_turbine_particles(f, Rect::new(0, 0, 60, TURBINE_ROWS), &stream, t0))
            .unwrap();
        let baseline = stream.rasterise_count.get();

        // Same tick, same burst count, different chart width:
        // resize invalidates the cache.
        terminal
            .draw(|f| render_turbine_particles(f, Rect::new(0, 0, 80, TURBINE_ROWS), &stream, t0))
            .unwrap();
        assert_eq!(
            stream.rasterise_count.get(),
            baseline + 1,
            "chart-width change must invalidate the cache",
        );
    }
}
