//! Shred ingress strip — two particle lanes + live numbers.
//!
//! Visualises the Turbine + Repair shred ingress pipeline, grounded
//! entirely in `solana_metrics::metrics` datapoints parsed by
//! [`crate::parser::metrics`]. Layout:
//!
//! ```text
//! ┌─ shred ingress ───────────────────────────────────────────────────────┐
//! │                                                                       │
//! │  turbine ⟫  ·   ·    · ·   ·    ·   · ·    ·  ·  ⟫ blockstore         │
//! │  repair  ⟫       ·             ·           ·                          │
//! │                                                                       │
//! │  fetch 357 · sigverify 877·8 drop · window 771·3 err · insert 601 …   │
//! └───────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Two particle lanes (turbine bright cyan, repair dim gray). Each new
//! `ShredFetch` event spawns turbine particles; each `ShredFetchRepair`
//! spawns repair particles. The number of particles per event is the
//! event's `shred_count` divided by [`PARTICLES_PER_SHRED`] so density
//! stays manageable during bursts.
//!
//! Numbers strip on the bottom carries the most recent sample value
//! for each pipeline stage:
//!
//! - `fetch N` — `ShredFetch.shred_count`
//! - `sigverify N · D drop` — `ShredSigverify.{num_packets, num_discards}`
//! - `window N · E err` — `RecvWindowInsert.{num_shreds_received, num_errors}`
//! - `insert N turbine, F FEC, R repair` —
//!   `BlockstoreInsert.{num_inserted, num_recovered, num_repair}`

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
pub const PANE_HEIGHT: u16 = 6;

/// How many shreds one particle represents. Caps visible density.
const PARTICLES_PER_SHRED: u64 = 10;

/// Lifespan of one particle in seconds (cluster → blockstore).
const TRAVERSAL_SECS: f64 = 2.0;

/// Soft cap on particles per lane. Excess are dropped oldest-first.
const PARTICLE_CAP: usize = 256;

/// Chart x-axis upper bound. Particles spawn at `x=0` and exit at this
/// value; render maps this range across the strip width.
const X_MAX: f64 = 100.0;

#[derive(Debug, Clone, Copy)]
enum LaneKind {
    Turbine,
    Repair,
}

#[derive(Debug, Clone, Copy)]
struct Particle {
    spawn_at: Instant,
    lane: LaneKind,
}

#[derive(Debug, Default, Clone, Copy)]
struct LatestNumbers {
    fetch: Option<u64>,
    sigverify_packets: Option<u64>,
    sigverify_discards: u64,
    window_received: Option<u64>,
    window_errors: u64,
    blockstore_inserted: Option<u64>,
    blockstore_recovered: u64,
    blockstore_repair: u64,
}

/// The shred ingress pane.
pub struct ShredIngressPane {
    particles: Vec<Particle>,
    numbers: LatestNumbers,
    now: Instant,
    frame_seed: u64,
}

impl ShredIngressPane {
    pub fn new() -> Self {
        Self {
            particles: Vec::new(),
            numbers: LatestNumbers::default(),
            now: Instant::now(),
            frame_seed: 0,
        }
    }

    fn spawn_particles(&mut self, lane: LaneKind, shred_count: u64) {
        let n = (shred_count / PARTICLES_PER_SHRED).max(1);
        for _ in 0..n {
            if self.particles.len() >= PARTICLE_CAP {
                self.particles.remove(0);
            }
            self.particles.push(Particle {
                spawn_at: self.now,
                lane,
            });
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
                self.spawn_particles(LaneKind::Turbine, *shred_count);
            }
            MetricEvent::ShredFetchRepair { shred_count } => {
                self.spawn_particles(LaneKind::Repair, *shred_count);
            }
            MetricEvent::ShredSigverify {
                num_packets,
                num_discards,
                ..
            } => {
                self.numbers.sigverify_packets = Some(*num_packets);
                self.numbers.sigverify_discards = *num_discards;
            }
            MetricEvent::RecvWindowInsert {
                num_shreds_received,
                num_errors,
            } => {
                self.numbers.window_received = Some(*num_shreds_received);
                self.numbers.window_errors = *num_errors;
            }
            MetricEvent::BlockstoreInsert {
                num_inserted,
                num_recovered,
                num_repair,
                ..
            } => {
                self.numbers.blockstore_inserted = Some(*num_inserted);
                self.numbers.blockstore_recovered = *num_recovered;
                self.numbers.blockstore_repair = *num_repair;
            }
            _ => {}
        }
    }

    fn tick(&mut self, now: Instant) {
        self.now = now;
        self.frame_seed = self.frame_seed.wrapping_add(1);
        let lifetime = Duration::from_secs_f64(TRAVERSAL_SECS);
        self.particles
            .retain(|p| now.saturating_duration_since(p.spawn_at) < lifetime);
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" shred ingress · turbine + repair ⟫ blockstore ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < 30 || inner.height < 4 {
            return;
        }

        // Vertical split:
        //   row 0: top breathing
        //   rows 1..=inner.height-2: chart with two lanes
        //   bottom row: numbers strip
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(2),
                Constraint::Length(1),
            ])
            .split(inner);

        self.render_chart(frame, chunks[1]);
        self.render_numbers(frame, chunks[2]);
    }
}

impl ShredIngressPane {
    fn render_chart(&self, frame: &mut Frame<'_>, area: Rect) {
        // Two lanes in y: turbine at y=1.5 (upper), repair at y=0.5 (lower).
        const Y_TURBINE: f64 = 1.5;
        const Y_REPAIR: f64 = 0.5;
        const LIFESPAN: f64 = TRAVERSAL_SECS;

        let mut turbine_pts: Vec<(f64, f64)> = Vec::new();
        let mut repair_pts: Vec<(f64, f64)> = Vec::new();

        for p in &self.particles {
            let age = self.now.saturating_duration_since(p.spawn_at).as_secs_f64();
            if age >= LIFESPAN {
                continue;
            }
            let x = (age / LIFESPAN) * X_MAX;
            match p.lane {
                LaneKind::Turbine => turbine_pts.push((x, Y_TURBINE)),
                LaneKind::Repair => repair_pts.push((x, Y_REPAIR)),
            }
        }

        let datasets = vec![
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM))
                .data(&turbine_pts),
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Scatter)
                .style(
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                )
                .data(&repair_pts),
        ];

        let chart = Chart::new(datasets)
            .x_axis(Axis::default().bounds([0.0, X_MAX]))
            .y_axis(Axis::default().bounds([0.0, 2.0]));
        frame.render_widget(chart, area);

        // Overlay lane anchors.
        let mid_y = area.y + area.height / 2;
        let turbine_label = "turbine ⟫";
        let repair_label = "repair ⟫";
        let blockstore_label = "⟫ blockstore";

        let tw = turbine_label.chars().count() as u16;
        let rw = repair_label.chars().count() as u16;
        let bw = blockstore_label.chars().count() as u16;

        if area.width > tw + bw + 4 {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    turbine_label,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Rect::new(area.x, mid_y.saturating_sub(1), tw, 1),
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    repair_label,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )),
                Rect::new(area.x, mid_y, rw, 1),
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    blockstore_label,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )),
                Rect::new(area.x + area.width - bw, mid_y, bw, 1),
            );
        }
    }

    fn render_numbers(&self, frame: &mut Frame<'_>, area: Rect) {
        let fetch = self
            .numbers
            .fetch
            .map_or_else(|| "—".to_owned(), |n| format!("{n}"));
        let sigverify = self.numbers.sigverify_packets.map_or_else(
            || "—".to_owned(),
            |n| {
                if self.numbers.sigverify_discards > 0 {
                    format!("{n}·{} drop", self.numbers.sigverify_discards)
                } else {
                    format!("{n}")
                }
            },
        );
        let window = self.numbers.window_received.map_or_else(
            || "—".to_owned(),
            |n| {
                if self.numbers.window_errors > 0 {
                    format!("{n}·{} err", self.numbers.window_errors)
                } else {
                    format!("{n}")
                }
            },
        );
        let insert = self.numbers.blockstore_inserted.map_or_else(
            || "—".to_owned(),
            |n| {
                format!(
                    "{n} turbine, {} FEC, {} repair",
                    self.numbers.blockstore_recovered, self.numbers.blockstore_repair
                )
            },
        );

        let line = Line::from(vec![
            Span::styled("fetch ", theme::label_style()),
            Span::styled(fetch, theme::value_style()),
            Span::styled("  ·  sigverify ", theme::label_style()),
            Span::styled(sigverify, theme::value_style()),
            Span::styled("  ·  window ", theme::label_style()),
            Span::styled(window, theme::value_style()),
            Span::styled("  ·  insert ", theme::label_style()),
            Span::styled(insert, theme::value_style()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
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
    fn shred_fetch_spawns_turbine_particles_and_updates_fetch_number() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetch {
            shred_count: 100,
        })));
        assert_eq!(p.numbers.fetch, Some(100));
        // 100 / 10 = 10 particles
        let turbine = p
            .particles
            .iter()
            .filter(|q| matches!(q.lane, LaneKind::Turbine))
            .count();
        assert_eq!(turbine, 10);
    }

    #[test]
    fn shred_fetch_repair_spawns_repair_particles_only() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetchRepair {
            shred_count: 5,
        })));
        // Repair fetch does NOT advance the "fetch" number (which is
        // turbine-specific) but DOES spawn at least one repair particle.
        assert!(p.numbers.fetch.is_none());
        let repair = p
            .particles
            .iter()
            .filter(|q| matches!(q.lane, LaneKind::Repair))
            .count();
        assert!(repair >= 1);
    }

    #[test]
    fn sigverify_updates_packets_and_discards() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredSigverify {
            num_packets: 886,
            num_discards: 11,
            num_duplicates: 0,
            elapsed_micros: 100,
        })));
        assert_eq!(p.numbers.sigverify_packets, Some(886));
        assert_eq!(p.numbers.sigverify_discards, 11);
    }

    #[test]
    fn window_updates_received_and_errors() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::RecvWindowInsert {
            num_shreds_received: 771,
            num_errors: 3,
        })));
        assert_eq!(p.numbers.window_received, Some(771));
        assert_eq!(p.numbers.window_errors, 3);
    }

    #[test]
    fn blockstore_insert_updates_partition() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::BlockstoreInsert {
            num_shreds: 771,
            num_inserted: 601,
            num_repair: 0,
            num_recovered: 184,
            total_elapsed_us: 11_832,
        })));
        assert_eq!(p.numbers.blockstore_inserted, Some(601));
        assert_eq!(p.numbers.blockstore_recovered, 184);
        assert_eq!(p.numbers.blockstore_repair, 0);
    }

    #[test]
    fn non_metric_events_are_ignored() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::FirstShred { slot: 42 }));
        assert!(p.particles.is_empty());
        assert!(p.numbers.fetch.is_none());
    }

    #[test]
    fn particles_drop_after_their_lifespan() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetch {
            shred_count: 50,
        })));
        assert!(!p.particles.is_empty());

        // Advance time past TRAVERSAL_SECS by manipulating spawn_at.
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
    fn particle_cap_evicts_oldest() {
        let mut p = ShredIngressPane::new();
        // Spawn many bursts so the cap is exercised.
        for _ in 0..50 {
            p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetch {
                shred_count: 100, // → 10 particles each
            })));
        }
        assert!(p.particles.len() <= PARTICLE_CAP);
    }
}
