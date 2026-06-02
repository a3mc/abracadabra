//! Shred streams — multi-lane particle visualization.
//!
//! Four stacked lanes, each showing one real event source as
//! particles flowing left → right. Calm = healthy. Yellow / red
//! particles appearing = look at the snapshot row.
//!
//! ```text
//! ┌─ shred streams ──────────────────────────────────────┐
//! │  turbine    ·   ·    · ·   ·    · ·    ·   ·         │  cyan
//! │  repair         ▪                  ▪                 │  yellow
//! │  drop                                                │  red (empty when healthy)
//! │  err                                                 │  red (empty when healthy)
//! │                                                      │
//! │  357 shreds  ·  5 repair  ·  0 drop  ·  0 err        │  snapshot row
//! └──────────────────────────────────────────────────────┘
//! ```
//!
//! Particle sources:
//!
//! - **turbine** — one particle per `ShredFetch` batch. Always-on
//!   when the cluster is producing. Marker tier by `shred_count`
//!   (Small/Medium/Large). Cyan.
//! - **repair** — one particle per `ShredFetchRepair` batch with
//!   `shred_count > 0`. Bright yellow Block marker; the lane should
//!   be visibly louder than turbine because repair is operationally
//!   meaningful.
//! - **drop** — one particle per `ShredSigverify` sample with
//!   `num_discards > 0`. Red. Empty stream = no sigverify drops.
//! - **err** — one particle per `RecvWindowInsert` sample with
//!   `num_errors > 0`. Red. Empty stream = no window errors.
//!
//! Snapshot row is the *most recent sample value* from each source.
//! It changes with every datapoint; the streams above show the
//! *history* so the operator can spot trends and bursts.

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

/// Pane row height when laid out by [`crate::live::scenes::SceneEngine`].
pub const PANE_HEIGHT: u16 = 9;

const TRAVERSAL_SECS: f64 = 3.0;
const PARTICLE_CAP: usize = 512;
const X_MAX: f64 = 100.0;

// Lane y-coordinates inside chart bounds [0, 4]. Higher y = higher row
// in chart coords, which maps to UPPER row on screen.
const Y_TURBINE: f64 = 3.5;
const Y_REPAIR: f64 = 2.5;
const Y_DROP: f64 = 1.5;
const Y_ERR: f64 = 0.5;

/// Width reserved on the left of each pane for lane labels. Charts
/// render to the right of this column.
const LABEL_COL_WIDTH: u16 = 9;

// Semantic palette (shared with [`super::slot_outcomes`]).
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

#[derive(Debug, Clone, Copy)]
struct Particle {
    spawn_at: Instant,
    lane: Lane,
}

#[derive(Debug, Default, Clone, Copy)]
struct LatestNumbers {
    fetch: Option<u64>,
    repair: Option<u64>,
    drop: Option<u64>,
    err: Option<u64>,
}

pub struct ShredIngressPane {
    particles: Vec<Particle>,
    numbers: LatestNumbers,
    now: Instant,
}

impl ShredIngressPane {
    pub fn new() -> Self {
        Self {
            particles: Vec::new(),
            numbers: LatestNumbers::default(),
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
                self.spawn(Lane::Turbine);
            }
            MetricEvent::ShredFetchRepair { shred_count } => {
                self.numbers.repair = Some(*shred_count);
                if *shred_count > 0 {
                    self.spawn(Lane::Repair);
                }
            }
            MetricEvent::ShredSigverify { num_discards, .. } => {
                self.numbers.drop = Some(*num_discards);
                if *num_discards > 0 {
                    self.spawn(Lane::Drop);
                }
            }
            MetricEvent::RecvWindowInsert { num_errors, .. } => {
                self.numbers.err = Some(*num_errors);
                if *num_errors > 0 {
                    self.spawn(Lane::Err);
                }
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
            .title(" shred streams ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < 30 || inner.height < 6 {
            return;
        }

        // Vertical: 1 row top breathing, 4 rows chart (== 4 lanes ==
        // y bounds [0, 4], so each row corresponds to exactly one
        // chart unit and labels align cleanly), then leftover gap,
        // then 1 row snapshot.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(4),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(inner);

        self.render_streams(frame, chunks[1]);
        self.render_snapshot(frame, chunks[3]);
    }
}

impl ShredIngressPane {
    fn render_streams(&self, frame: &mut Frame<'_>, area: Rect) {
        // Horizontal split: leftmost LABEL_COL_WIDTH cells for lane
        // labels, rest for the particle chart. Labels render
        // separately so they never overlay the moving particles.
        if area.width <= LABEL_COL_WIDTH + 8 {
            return;
        }
        let h_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(LABEL_COL_WIDTH), Constraint::Min(8)])
            .split(area);
        let label_area = h_chunks[0];
        let chart_area = h_chunks[1];

        // Visual hierarchy:
        //   turbine        → Braille DIM (calm noise floor — always-on,
        //                    expected, no attention needed)
        //   repair/drop/err → Block BOLD coloured (loud attention
        //                    events — rare, meaningful)
        // When a healthy log replays, the eye sees a quiet cyan texture
        // along the turbine row and three empty rows below; any bold
        // coloured block on the lower rows pops out of that texture
        // immediately. The intensity-tier classifier was removed
        // because in practice every turbine batch landed in the Large
        // tier (counts ~350) and made the lane indistinguishable from
        // repair / drop / err.
        let mut turbine: Vec<(f64, f64)> = Vec::new();
        let mut repair: Vec<(f64, f64)> = Vec::new();
        let mut drop: Vec<(f64, f64)> = Vec::new();
        let mut err: Vec<(f64, f64)> = Vec::new();

        for p in &self.particles {
            let age = self.now.saturating_duration_since(p.spawn_at).as_secs_f64();
            if age >= TRAVERSAL_SECS {
                continue;
            }
            let x = (age / TRAVERSAL_SECS) * X_MAX;
            match p.lane {
                Lane::Turbine => turbine.push((x, Y_TURBINE)),
                Lane::Repair => repair.push((x, Y_REPAIR)),
                Lane::Drop => drop.push((x, Y_DROP)),
                Lane::Err => err.push((x, Y_ERR)),
            }
        }

        let datasets = vec![
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(COL_TURBINE).add_modifier(Modifier::DIM))
                .data(&turbine),
            Dataset::default()
                .marker(Marker::Block)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(COL_REPAIR).add_modifier(Modifier::BOLD))
                .data(&repair),
            Dataset::default()
                .marker(Marker::Block)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(COL_DROP).add_modifier(Modifier::BOLD))
                .data(&drop),
            Dataset::default()
                .marker(Marker::Block)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(COL_ERR).add_modifier(Modifier::BOLD))
                .data(&err),
        ];

        let chart = Chart::new(datasets)
            .x_axis(Axis::default().bounds([0.0, X_MAX]))
            .y_axis(Axis::default().bounds([0.0, 4.0]));
        frame.render_widget(chart, chart_area);

        // Lane labels in their dedicated column — same row mapping as
        // the chart uses for particles, so labels sit at the start of
        // their lane.
        render_lane_label(
            frame,
            label_area,
            chart_area,
            "turbine",
            Y_TURBINE,
            COL_TURBINE,
        );
        render_lane_label(
            frame, label_area, chart_area, "repair", Y_REPAIR, COL_REPAIR,
        );
        render_lane_label(frame, label_area, chart_area, "drop", Y_DROP, COL_DROP);
        render_lane_label(frame, label_area, chart_area, "err", Y_ERR, COL_ERR);
    }

    fn render_snapshot(&self, frame: &mut Frame<'_>, area: Rect) {
        let fetch = fmt_opt(self.numbers.fetch);
        let repair = fmt_opt(self.numbers.repair);
        let drop = fmt_opt(self.numbers.drop);
        let err = fmt_opt(self.numbers.err);

        let line = Line::from(vec![
            Span::styled(format!(" {fetch}"), Style::default().fg(COL_TURBINE)),
            Span::styled(" sh  ", theme::label_style()),
            Span::styled(
                repair,
                if self.numbers.repair.unwrap_or(0) > 0 {
                    Style::default().fg(COL_REPAIR).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(" rep  ", theme::label_style()),
            Span::styled(
                drop,
                if self.numbers.drop.unwrap_or(0) > 0 {
                    Style::default().fg(COL_DROP).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(" drop  ", theme::label_style()),
            Span::styled(
                err,
                if self.numbers.err.unwrap_or(0) > 0 {
                    Style::default().fg(COL_ERR).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(" err  ", theme::label_style()),
            Span::styled(
                "·  per-sample (~1/s)",
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

/// Paint a small label `text` at the left edge of `area` on the row
/// corresponding to chart-y `y` (chart bounds [0, 4], screen flipped).
/// Paint a small lane label inside `label_area`, at the row that
/// corresponds to chart-y `y_chart` in `chart_area`. The row mapping
/// quantises the chart's [0, Y_MAX] range across the chart area's
/// row count so the label and the particles share the same line.
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
    // Row index inside the chart area: 0 = top row.
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
    fn turbine_lane_fires_on_every_fetch() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetch {
            shred_count: 100,
        })));
        assert_eq!(p.particles.len(), 1);
        assert_eq!(p.particles[0].lane, Lane::Turbine);
        assert_eq!(p.numbers.fetch, Some(100));
    }

    #[test]
    fn repair_lane_only_fires_when_count_nonzero() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetchRepair {
            shred_count: 0,
        })));
        assert!(p.particles.is_empty());
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetchRepair {
            shred_count: 5,
        })));
        assert_eq!(p.particles.len(), 1);
        assert_eq!(p.particles[0].lane, Lane::Repair);
    }

    #[test]
    fn drop_lane_only_fires_when_discards_nonzero() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredSigverify {
            num_packets: 100,
            num_discards: 0,
            num_duplicates: 0,
            elapsed_micros: 1,
        })));
        assert!(p.particles.is_empty());
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredSigverify {
            num_packets: 100,
            num_discards: 8,
            num_duplicates: 0,
            elapsed_micros: 1,
        })));
        assert_eq!(p.particles.len(), 1);
        assert_eq!(p.particles[0].lane, Lane::Drop);
    }

    #[test]
    fn err_lane_only_fires_when_errors_nonzero() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::RecvWindowInsert {
            num_shreds_received: 100,
            num_errors: 0,
        })));
        assert!(p.particles.is_empty());
        p.on_event(&mk(EventKind::Metric(MetricEvent::RecvWindowInsert {
            num_shreds_received: 100,
            num_errors: 3,
        })));
        assert_eq!(p.particles.len(), 1);
        assert_eq!(p.particles[0].lane, Lane::Err);
    }

    #[test]
    fn particles_drop_after_lifespan() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetch {
            shred_count: 10,
        })));
        let past = Instant::now()
            .checked_sub(Duration::from_secs_f64(TRAVERSAL_SECS + 0.5))
            .unwrap();
        for q in &mut p.particles {
            q.spawn_at = past;
        }
        p.tick(Instant::now());
        assert!(p.particles.is_empty());
    }

    #[test]
    fn non_metric_events_ignored() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::FirstShred { slot: 1 }));
        assert!(p.particles.is_empty());
    }
}
