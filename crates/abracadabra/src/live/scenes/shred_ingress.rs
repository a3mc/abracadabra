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

// Semantic palette (shared with [`super::slot_outcomes`]).
const COL_TURBINE: Color = Color::Cyan;
const COL_REPAIR: Color = Color::Yellow;
const COL_BAD: Color = Color::Red;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    Turbine,
    Repair,
    Drop,
    Err,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intensity {
    Small,
    Medium,
    Large,
}

#[derive(Debug, Clone, Copy)]
struct Particle {
    spawn_at: Instant,
    lane: Lane,
    intensity: Intensity,
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

    const fn classify(count: u64) -> Intensity {
        match count {
            0..=20 => Intensity::Small,
            21..=100 => Intensity::Medium,
            _ => Intensity::Large,
        }
    }

    fn spawn(&mut self, lane: Lane, intensity: Intensity) {
        if self.particles.len() >= PARTICLE_CAP {
            self.particles.remove(0);
        }
        self.particles.push(Particle {
            spawn_at: self.now,
            lane,
            intensity,
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
                self.spawn(Lane::Turbine, Self::classify(*shred_count));
            }
            MetricEvent::ShredFetchRepair { shred_count } => {
                self.numbers.repair = Some(*shred_count);
                if *shred_count > 0 {
                    self.spawn(Lane::Repair, Self::classify(*shred_count));
                }
            }
            MetricEvent::ShredSigverify { num_discards, .. } => {
                self.numbers.drop = Some(*num_discards);
                if *num_discards > 0 {
                    self.spawn(Lane::Drop, Self::classify(*num_discards));
                }
            }
            MetricEvent::RecvWindowInsert { num_errors, .. } => {
                self.numbers.err = Some(*num_errors);
                if *num_errors > 0 {
                    self.spawn(Lane::Err, Self::classify(*num_errors));
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

        if inner.width < 30 || inner.height < 5 {
            return;
        }

        // Vertical split: streams area (above) + snapshot row (1 row).
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(inner);

        self.render_streams(frame, chunks[0]);
        self.render_snapshot(frame, chunks[1]);
    }
}

impl ShredIngressPane {
    fn render_streams(&self, frame: &mut Frame<'_>, area: Rect) {
        // Partition by (lane × intensity). Bigger intensity = bigger marker.
        let mut turbine_s: Vec<(f64, f64)> = Vec::new();
        let mut turbine_m: Vec<(f64, f64)> = Vec::new();
        let mut turbine_l: Vec<(f64, f64)> = Vec::new();
        let mut repair: Vec<(f64, f64)> = Vec::new();
        let mut drop: Vec<(f64, f64)> = Vec::new();
        let mut err: Vec<(f64, f64)> = Vec::new();

        for p in &self.particles {
            let age = self.now.saturating_duration_since(p.spawn_at).as_secs_f64();
            if age >= TRAVERSAL_SECS {
                continue;
            }
            let x = (age / TRAVERSAL_SECS) * X_MAX;
            let y = match p.lane {
                Lane::Turbine => Y_TURBINE,
                Lane::Repair => Y_REPAIR,
                Lane::Drop => Y_DROP,
                Lane::Err => Y_ERR,
            };
            match (p.lane, p.intensity) {
                (Lane::Turbine, Intensity::Small) => turbine_s.push((x, y)),
                (Lane::Turbine, Intensity::Medium) => turbine_m.push((x, y)),
                (Lane::Turbine, Intensity::Large) => turbine_l.push((x, y)),
                (Lane::Repair, _) => repair.push((x, y)),
                (Lane::Drop, _) => drop.push((x, y)),
                (Lane::Err, _) => err.push((x, y)),
            }
        }

        let datasets = vec![
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(COL_TURBINE).add_modifier(Modifier::DIM))
                .data(&turbine_s),
            Dataset::default()
                .marker(Marker::Dot)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(COL_TURBINE))
                .data(&turbine_m),
            Dataset::default()
                .marker(Marker::Block)
                .graph_type(GraphType::Scatter)
                .style(
                    Style::default()
                        .fg(COL_TURBINE)
                        .add_modifier(Modifier::BOLD),
                )
                .data(&turbine_l),
            Dataset::default()
                .marker(Marker::Block)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(COL_REPAIR).add_modifier(Modifier::BOLD))
                .data(&repair),
            Dataset::default()
                .marker(Marker::Block)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(COL_BAD).add_modifier(Modifier::BOLD))
                .data(&drop),
            Dataset::default()
                .marker(Marker::Block)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(COL_BAD).add_modifier(Modifier::BOLD))
                .data(&err),
        ];

        let chart = Chart::new(datasets)
            .x_axis(Axis::default().bounds([0.0, X_MAX]))
            .y_axis(Axis::default().bounds([0.0, 4.0]));
        frame.render_widget(chart, area);

        // Lane labels overlaid on the left edge — one per lane.
        render_lane_label(frame, area, "turbine", Y_TURBINE, COL_TURBINE);
        render_lane_label(frame, area, "repair", Y_REPAIR, COL_REPAIR);
        render_lane_label(frame, area, "drop", Y_DROP, COL_BAD);
        render_lane_label(frame, area, "err", Y_ERR, COL_BAD);
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
                    Style::default().fg(COL_BAD).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(" drop  ", theme::label_style()),
            Span::styled(
                err,
                if self.numbers.err.unwrap_or(0) > 0 {
                    Style::default().fg(COL_BAD).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(" err", theme::label_style()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }
}

fn fmt_opt(v: Option<u64>) -> String {
    v.map_or_else(|| "—".to_owned(), |n| format!("{n}"))
}

/// Paint a small label `text` at the left edge of `area` on the row
/// corresponding to chart-y `y` (chart bounds [0, 4], screen flipped).
fn render_lane_label(frame: &mut Frame<'_>, area: Rect, text: &str, y_chart: f64, fg: Color) {
    const Y_MAX: f64 = 4.0;
    let clamped = y_chart.clamp(0.0, Y_MAX);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let raw = ((1.0 - clamped / Y_MAX) * f64::from(area.height.saturating_sub(1))) as u16;
    let y = area.y + raw.min(area.height.saturating_sub(1));
    let w = text.chars().count() as u16;
    if w + 1 > area.width {
        return;
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            text.to_owned(),
            Style::default().fg(fg).add_modifier(Modifier::BOLD),
        )),
        Rect::new(area.x + 1, y, w, 1),
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
    fn intensity_tiering() {
        assert_eq!(ShredIngressPane::classify(5), Intensity::Small);
        assert_eq!(ShredIngressPane::classify(50), Intensity::Medium);
        assert_eq!(ShredIngressPane::classify(500), Intensity::Large);
    }

    #[test]
    fn non_metric_events_ignored() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::FirstShred { slot: 1 }));
        assert!(p.particles.is_empty());
    }
}
