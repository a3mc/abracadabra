//! Slot outcomes — multi-lane stream of finalization events.
//!
//! Replaces the previous card-flow design. Every visible glyph is one
//! real event:
//!
//! ```text
//! ┌─ slot outcomes ─────────────────────────────────────┐
//! │  fast final  ✓ ✓ ✓ ✓ ✓ ✓ ✓ ✓ ✓                      │  bright green
//! │  slow final         ◇                ◇              │  yellow
//! │  skip                                               │  red (empty when healthy)
//! │  fec recov    ◆  ◆  ◆  ◆  ◆  ◆                      │  light blue
//! │                                                     │
//! │  fast 96%  ·  slow 4%  ·  skip 0  ·  fec/slot 22    │  snapshot row
//! └─────────────────────────────────────────────────────┘
//! ```
//!
//! Event sources:
//!
//! - **fast final** — one particle per `Finalized { fast: true }`
//!   from votor. Bright green. Calm steady = healthy.
//! - **slow final** — one particle per `Finalized { fast: false }`.
//!   Yellow. Slow path is normal but worth flagging when it spikes.
//! - **skip** — one particle per `VotingSkip { slot }` from votor.
//!   Red X. Empty stream when healthy; any glyph is operationally
//!   meaningful (we voted skip on a slot).
//! - **fec recov** — one particle per `ShredInsertIsFull` with
//!   `num_recovered > 0`. Light blue diamond. Steady stream = FEC
//!   doing its job; the higher the count per slot, the more clever
//!   recovery is saving us from needing repair.
//!
//! Snapshot row carries rolling totals (last N seen) so the operator
//! reads both the stream history (recent flow) and the cumulative
//! state (current ratios).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph};
use ratatui::Frame;

use crate::live::animation::Pane;
use crate::parser::{Event, EventKind, MetricEvent};
use crate::tui::theme;

pub const PANE_HEIGHT: u16 = 9;

const TRAVERSAL_SECS: f64 = 3.0;
const PARTICLE_CAP: usize = 512;
const X_MAX: f64 = 100.0;

// Lane y-coordinates inside chart bounds [0, 4].
const Y_FAST: f64 = 3.5;
const Y_SLOW: f64 = 2.5;
const Y_SKIP: f64 = 1.5;
const Y_FEC: f64 = 0.5;

const COL_GOOD: Color = Color::Green;
const COL_WARN: Color = Color::Yellow;
const COL_BAD: Color = Color::Red;
const COL_FEC: Color = Color::LightBlue;

/// Width reserved on the left of the pane for lane labels.
const LABEL_COL_WIDTH: u16 = 9;

/// Glyphs for single-event lanes — chosen to read as discrete events,
/// not as bars carrying a magnitude. FEC alone carries a per-slot
/// count (`num_recovered`) so it stays as a small Braille dot stream.
const GLYPH_FAST: &str = "✓";
const GLYPH_SLOW: &str = "◆";
const GLYPH_SKIP: &str = "✗";

/// Rolling window for the snapshot row's percentages. 64 slots ≈ ~25s
/// of cluster activity at ~2.5 slots/sec; long enough to be stable,
/// short enough to follow real changes.
const ROLLING_WINDOW: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    Fast,
    Slow,
    Skip,
    Fec,
}

#[derive(Debug, Clone, Copy)]
struct Particle {
    spawn_at: Instant,
    lane: Lane,
}

/// One slot's outcome category, recorded in the rolling history for
/// the snapshot ratios.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Fast,
    Slow,
    Skip,
}

pub struct SlotLifecyclePane {
    particles: Vec<Particle>,
    /// Most recent outcome per slot, used to compute the snapshot
    /// percentages. Capped at [`ROLLING_WINDOW`].
    history: VecDeque<Outcome>,
    /// Last seen `num_recovered` per slot — surfaces in the snapshot
    /// row as `fec/slot` so the operator sees per-slot FEC intensity.
    last_fec_per_slot: Option<u64>,
    now: Instant,
}

impl SlotLifecyclePane {
    pub fn new() -> Self {
        Self {
            particles: Vec::new(),
            history: VecDeque::with_capacity(ROLLING_WINDOW),
            last_fec_per_slot: None,
            now: Instant::now(),
        }
    }

    fn spawn(&mut self, lane: Lane) {
        if self.particles.len() >= PARTICLE_CAP {
            self.particles.remove(0);
        }
        self.particles.push(Particle {
            spawn_at: self.now,
            lane,
        });
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
                self.spawn(Lane::Fast);
                self.record_outcome(Outcome::Fast);
            }
            EventKind::Finalized { fast: false, .. } => {
                self.spawn(Lane::Slow);
                self.record_outcome(Outcome::Slow);
            }
            EventKind::VotingSkip { .. } => {
                self.spawn(Lane::Skip);
                self.record_outcome(Outcome::Skip);
            }
            EventKind::Metric(MetricEvent::ShredInsertIsFull { num_recovered, .. })
                if *num_recovered > 0 =>
            {
                self.spawn(Lane::Fec);
                self.last_fec_per_slot = Some(*num_recovered);
            }
            _ => {}
        }
    }

    fn tick(&mut self, now: Instant) {
        self.now = now;
        let lifetime = Duration::from_secs_f64(TRAVERSAL_SECS);
        self.particles
            .retain(|p| now.saturating_duration_since(p.spawn_at) < lifetime);
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" slot outcomes ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < 30 || inner.height < 5 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(inner);

        self.render_streams(frame, chunks[0]);
        self.render_snapshot(frame, chunks[1]);
    }
}

impl SlotLifecyclePane {
    fn render_streams(&self, frame: &mut Frame<'_>, area: Rect) {
        // Horizontal split: labels on the left, particle area on right.
        if area.width <= LABEL_COL_WIDTH + 8 {
            return;
        }
        let h_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(LABEL_COL_WIDTH), Constraint::Min(8)])
            .split(area);
        let label_area = h_chunks[0];
        let chart_area = h_chunks[1];

        // FEC particles render through a Chart (low-noise Braille) —
        // they have a per-slot count and read as "density of recovery".
        let mut fec: Vec<(f64, f64)> = Vec::new();
        // Fast / slow / skip render as discrete glyphs at computed
        // (x, y) screen positions — each is one event, not a quantity.
        let mut glyph_events: Vec<(f64, Lane)> = Vec::new();

        for p in &self.particles {
            let age = self.now.saturating_duration_since(p.spawn_at).as_secs_f64();
            if age >= TRAVERSAL_SECS {
                continue;
            }
            let x = (age / TRAVERSAL_SECS) * X_MAX;
            match p.lane {
                Lane::Fec => fec.push((x, Y_FEC)),
                _ => glyph_events.push((x, p.lane)),
            }
        }

        // FEC stream as a Chart (Braille = subtle).
        let datasets = vec![Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Scatter)
            .style(Style::default().fg(COL_FEC).add_modifier(Modifier::DIM))
            .data(&fec)];
        let chart = Chart::new(datasets)
            .x_axis(Axis::default().bounds([0.0, X_MAX]))
            .y_axis(Axis::default().bounds([0.0, 4.0]));
        frame.render_widget(chart, chart_area);

        // Lane labels in the label column.
        render_lane_label(frame, label_area, chart_area, "fast", Y_FAST, COL_GOOD);
        render_lane_label(frame, label_area, chart_area, "slow", Y_SLOW, COL_WARN);
        render_lane_label(frame, label_area, chart_area, "skip", Y_SKIP, COL_BAD);
        render_lane_label(frame, label_area, chart_area, "fec", Y_FEC, COL_FEC);

        // Discrete glyph events painted as text at chart-derived cells.
        for (x_chart, lane) in glyph_events {
            let (glyph, style) = match lane {
                Lane::Fast => (
                    GLYPH_FAST,
                    Style::default().fg(COL_GOOD).add_modifier(Modifier::BOLD),
                ),
                Lane::Slow => (GLYPH_SLOW, Style::default().fg(COL_WARN)),
                Lane::Skip => (
                    GLYPH_SKIP,
                    Style::default().fg(COL_BAD).add_modifier(Modifier::BOLD),
                ),
                Lane::Fec => continue, // handled by the Chart above
            };
            let y_chart = lane_y(lane);
            render_glyph_at(frame, chart_area, glyph, style, x_chart, y_chart);
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
            Span::styled(" fast  ", theme::label_style()),
            Span::styled(
                format!("{slow_pct}%"),
                if slow_pct > 0 {
                    Style::default().fg(COL_WARN)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(" slow  ", theme::label_style()),
            Span::styled(
                format!("{skips}"),
                if skips > 0 {
                    Style::default().fg(COL_BAD).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(" skip  ", theme::label_style()),
            Span::styled(fec, Style::default().fg(COL_FEC)),
            Span::styled(" fec (last)  ", theme::label_style()),
            // Window indicator: the rolling ratios above cover the
            // last N slots. At Solana's ~400ms target slot time this
            // is ~25s of recent cluster activity. State the source so
            // operators don't wonder "over what window?".
            Span::styled(
                format!("· last {} slots", self.history.len().min(ROLLING_WINDOW)),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }
}

/// Map a lane to its chart-y coordinate.
const fn lane_y(lane: Lane) -> f64 {
    match lane {
        Lane::Fast => Y_FAST,
        Lane::Slow => Y_SLOW,
        Lane::Skip => Y_SKIP,
        Lane::Fec => Y_FEC,
    }
}

/// Paint a single glyph at the cell corresponding to chart-coords
/// `(x_chart, y_chart)` inside `chart_area`.
fn render_glyph_at(
    frame: &mut Frame<'_>,
    chart_area: Rect,
    glyph: &str,
    style: Style,
    x_chart: f64,
    y_chart: f64,
) {
    const Y_MAX: f64 = 4.0;
    if chart_area.width == 0 || chart_area.height == 0 {
        return;
    }
    let x_norm = (x_chart / X_MAX).clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let x_off = (x_norm * f64::from(chart_area.width.saturating_sub(1))) as u16;
    let x = chart_area.x + x_off;
    let y_clamped = y_chart.clamp(0.0, Y_MAX);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let row = (((Y_MAX - y_clamped) / Y_MAX) * f64::from(chart_area.height)) as u16;
    let y = chart_area.y + row.min(chart_area.height.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(Span::styled(glyph.to_owned(), style)),
        Rect::new(x, y, 1, 1),
    );
}

fn render_lane_label(
    frame: &mut Frame<'_>,
    label_area: Rect,
    chart_area: Rect,
    text: &str,
    y_chart: f64,
    fg: Color,
) {
    const Y_MAX: f64 = 4.0;
    if chart_area.height == 0 {
        return;
    }
    let clamped = y_chart.clamp(0.0, Y_MAX);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let row = (((Y_MAX - clamped) / Y_MAX) * f64::from(chart_area.height)) as u16;
    let row = row.min(chart_area.height.saturating_sub(1));
    let y = chart_area.y + row;
    let w = text.chars().count() as u16;
    if w + 1 > label_area.width {
        return;
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            text.to_owned(),
            Style::default().fg(fg).add_modifier(Modifier::BOLD),
        )),
        Rect::new(label_area.x + 1, y, w, 1),
    );
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
    fn fast_finalize_spawns_fast_lane() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::Finalized {
            slot: 1,
            hash: "h".into(),
            fast: true,
        }));
        assert_eq!(p.particles.len(), 1);
        assert_eq!(p.particles[0].lane, Lane::Fast);
        assert_eq!(p.history.back(), Some(&Outcome::Fast));
    }

    #[test]
    fn slow_finalize_spawns_slow_lane() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::Finalized {
            slot: 1,
            hash: "h".into(),
            fast: false,
        }));
        assert_eq!(p.particles[0].lane, Lane::Slow);
        assert_eq!(p.history.back(), Some(&Outcome::Slow));
    }

    #[test]
    fn voting_skip_spawns_skip_lane() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::VotingSkip { slot: 1 }));
        assert_eq!(p.particles[0].lane, Lane::Skip);
        assert_eq!(p.history.back(), Some(&Outcome::Skip));
    }

    #[test]
    fn shred_insert_is_full_with_recovery_spawns_fec_lane() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredInsertIsFull {
            slot: 1,
            total_time_ms: 10,
            last_index: 100,
            num_repaired: 0,
            num_recovered: 44,
        })));
        assert_eq!(p.particles.len(), 1);
        assert_eq!(p.particles[0].lane, Lane::Fec);
        assert_eq!(p.last_fec_per_slot, Some(44));
    }

    #[test]
    fn shred_insert_is_full_with_zero_recovery_does_not_spawn() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredInsertIsFull {
            slot: 1,
            total_time_ms: 10,
            last_index: 100,
            num_repaired: 0,
            num_recovered: 0,
        })));
        assert!(p.particles.is_empty());
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
    fn particles_drop_after_lifespan() {
        let mut p = SlotLifecyclePane::new();
        p.on_event(&mk(EventKind::Finalized {
            slot: 0,
            hash: "h".into(),
            fast: true,
        }));
        let past = Instant::now()
            .checked_sub(Duration::from_secs_f64(TRAVERSAL_SECS + 0.5))
            .unwrap();
        for q in &mut p.particles {
            q.spawn_at = past;
        }
        p.tick(Instant::now());
        assert!(p.particles.is_empty());
    }
}
