//! Shred ingress strip — mindmap with arrows, variable-size particles,
//! and semantic colour coding.
//!
//! Layout (half-width pane, ~12 rows tall):
//!
//! ```text
//! ┌─ shred ingress ──────────────────────────┐
//! │                                          │
//! │  network ──→  turbine  357 ●● ─┐         │
//! │                                ├→ verify 877 (8 drop)
//! │  network ──→  repair     5  · ─┘         │
//! │                                          │
//! │              window  771  (3 err)        │
//! │                  │                       │
//! │                  ▼                       │
//! │  blockstore  inserted 601 · 184 FEC · 0 repair
//! │                                          │
//! └──────────────────────────────────────────┘
//! ```
//!
//! Semantic colours (used everywhere — the entire Live tab will adopt
//! the same palette):
//!
//! - **Turbine flow** — `Color::Cyan`. Cool, steady, normal.
//! - **Repair flow** — `Color::Yellow`. Warm, attention. Repair means
//!   "Turbine didn't deliver, we had to ask for it." Small amounts
//!   are normal; persistent yellow means an upstream issue.
//! - **FEC recovery** — `Color::LightBlue`. Clever reconstruction.
//! - **Drops / errors** — `Color::Red`. Bad. Any non-zero is loud.
//! - **Successful insert** — `Color::Green`. The shred made it home.
//!
//! Visual differentiation between Turbine and Repair is the central
//! design point: they used to be mirrored Braille streams, which made
//! the operational asymmetry invisible. Now Turbine is a calm dense
//! stream (`Marker::Braille`, dim cyan); Repair is a few prominent
//! pulses (`Marker::Block`, bright yellow). They look *different*
//! because they *are* different.

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
pub const PANE_HEIGHT: u16 = 12;

/// Particle lifespan from spawn (left) to blockstore (right) in seconds.
const TRAVERSAL_SECS: f64 = 2.5;

/// Soft cap on total particles. Excess dropped oldest-first.
const PARTICLE_CAP: usize = 384;

/// Chart x-axis upper bound. Particle x grows from 0 to this across
/// the lifespan.
const X_MAX: f64 = 100.0;

// Semantic palette. Reused by future strips so the whole Live tab
// reads as one connected mindmap.
const COL_TURBINE: Color = Color::Cyan;
const COL_REPAIR: Color = Color::Yellow;
const COL_FEC: Color = Color::LightBlue;
const COL_INSERTED: Color = Color::Green;
const COL_ERROR: Color = Color::Red;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaneKind {
    Turbine,
    Repair,
}

/// Particle intensity tier, derived from the source event's count.
/// Drives marker style and visual emphasis so the operator can tell
/// "small steady" from "big burst" at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intensity {
    /// `< 50` shreds. Single Braille dot. Most common.
    Small,
    /// `50..=200`. Slightly brighter / larger marker.
    Medium,
    /// `> 200`. Most prominent. Bursts read as "big payload arrived".
    Large,
}

#[derive(Debug, Clone, Copy)]
struct Particle {
    spawn_at: Instant,
    lane: LaneKind,
    intensity: Intensity,
}

#[derive(Debug, Default, Clone, Copy)]
struct LatestNumbers {
    fetch: Option<u64>,
    repair_fetch: Option<u64>,
    sigverify_packets: Option<u64>,
    sigverify_discards: u64,
    window_received: Option<u64>,
    window_errors: u64,
    blockstore_inserted: Option<u64>,
    blockstore_recovered: u64,
    blockstore_repair: u64,
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

    const fn classify(shred_count: u64) -> Intensity {
        match shred_count {
            0..=49 => Intensity::Small,
            50..=200 => Intensity::Medium,
            _ => Intensity::Large,
        }
    }

    /// Spawn one particle representing this whole batch. Batches are
    /// already discrete events in the metrics stream; we honour that
    /// 1:1 instead of fragmenting one batch into many same-frame
    /// particles. Intensity carries the magnitude visually.
    fn spawn_batch(&mut self, lane: LaneKind, shred_count: u64) {
        if self.particles.len() >= PARTICLE_CAP {
            self.particles.remove(0);
        }
        self.particles.push(Particle {
            spawn_at: self.now,
            lane,
            intensity: Self::classify(shred_count),
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
                self.spawn_batch(LaneKind::Turbine, *shred_count);
            }
            MetricEvent::ShredFetchRepair { shred_count } => {
                self.numbers.repair_fetch = Some(*shred_count);
                self.spawn_batch(LaneKind::Repair, *shred_count);
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
        let lifetime = Duration::from_secs_f64(TRAVERSAL_SECS);
        self.particles
            .retain(|p| now.saturating_duration_since(p.spawn_at) < lifetime);
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" shred ingress ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < 30 || inner.height < 6 {
            return;
        }

        // Vertical split: chart on top (the particle highway),
        // mindmap text below.
        let chart_rows = inner.height.saturating_sub(7).max(2);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(chart_rows), Constraint::Min(6)])
            .split(inner);

        self.render_chart(frame, chunks[0]);
        self.render_mindmap(frame, chunks[1]);
    }
}

impl ShredIngressPane {
    fn render_chart(&self, frame: &mut Frame<'_>, area: Rect) {
        // Turbine lane sits at y=1.5; Repair lane at y=0.5 — same as
        // before but the visual treatment now differs strongly.
        const Y_TURBINE: f64 = 1.5;
        const Y_REPAIR: f64 = 0.5;
        const LIFESPAN: f64 = TRAVERSAL_SECS;

        // Partition particles by lane × intensity. Each combination
        // gets its own dataset so the marker + colour can differ.
        let mut turbine_small: Vec<(f64, f64)> = Vec::new();
        let mut turbine_med: Vec<(f64, f64)> = Vec::new();
        let mut turbine_large: Vec<(f64, f64)> = Vec::new();
        let mut repair_all: Vec<(f64, f64)> = Vec::new();

        for p in &self.particles {
            let age = self.now.saturating_duration_since(p.spawn_at).as_secs_f64();
            if age >= LIFESPAN {
                continue;
            }
            let x = (age / LIFESPAN) * X_MAX;
            match (p.lane, p.intensity) {
                (LaneKind::Turbine, Intensity::Small) => turbine_small.push((x, Y_TURBINE)),
                (LaneKind::Turbine, Intensity::Medium) => turbine_med.push((x, Y_TURBINE)),
                (LaneKind::Turbine, Intensity::Large) => turbine_large.push((x, Y_TURBINE)),
                (LaneKind::Repair, _) => repair_all.push((x, Y_REPAIR)),
            }
        }

        let datasets = vec![
            // Turbine: Braille for fine motion + DIM for restraint.
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(COL_TURBINE).add_modifier(Modifier::DIM))
                .data(&turbine_small),
            Dataset::default()
                .marker(Marker::Dot)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(COL_TURBINE))
                .data(&turbine_med),
            // Large Turbine bursts: brighter, bold.
            Dataset::default()
                .marker(Marker::Block)
                .graph_type(GraphType::Scatter)
                .style(
                    Style::default()
                        .fg(COL_TURBINE)
                        .add_modifier(Modifier::BOLD),
                )
                .data(&turbine_large),
            // Repair: Block marker, bold yellow. Each repair pulse is
            // visually loud; you can spot one from across the room.
            Dataset::default()
                .marker(Marker::Block)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(COL_REPAIR).add_modifier(Modifier::BOLD))
                .data(&repair_all),
        ];

        let chart = Chart::new(datasets)
            .x_axis(Axis::default().bounds([0.0, X_MAX]))
            .y_axis(Axis::default().bounds([0.0, 2.0]));
        frame.render_widget(chart, area);
    }

    /// Mindmap-style text block below the particle highway. Boxes
    /// connected with arrows; each box carries its most recent
    /// numeric value; colours track the semantic palette.
    fn render_mindmap(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height < 5 {
            return;
        }

        let fetch_text = fmt_opt(self.numbers.fetch);
        let repair_text = fmt_opt(self.numbers.repair_fetch);
        let verify_text = fmt_opt(self.numbers.sigverify_packets);
        let window_text = fmt_opt(self.numbers.window_received);
        let inserted_text = fmt_opt(self.numbers.blockstore_inserted);

        // Lines explicitly laid out so the arrow joins line up.
        let line1 = Line::from(vec![
            Span::styled("  turbine  ", theme::label_style()),
            Span::styled(fetch_text, Style::default().fg(COL_TURBINE)),
            Span::styled("  ──→ ╮", Style::default().fg(Color::Gray)),
        ]);
        let line2 = Line::from(vec![
            Span::styled("                    ", theme::label_style()),
            Span::styled("├──→ verify ", Style::default().fg(Color::Gray)),
            Span::styled(verify_text, Style::default().fg(COL_TURBINE)),
            discards_span(self.numbers.sigverify_discards),
        ]);
        let line3 = Line::from(vec![
            Span::styled("  repair   ", theme::label_style()),
            Span::styled(repair_text, Style::default().fg(COL_REPAIR)),
            Span::styled("  ──→ ╯", Style::default().fg(Color::Gray)),
        ]);
        let line4 = Line::from(vec![
            Span::styled("                              ", theme::label_style()),
            Span::styled("│", Style::default().fg(Color::Gray)),
        ]);
        let line5 = Line::from(vec![
            Span::styled("              window ", theme::label_style()),
            Span::styled(window_text, theme::value_style()),
            errors_span(self.numbers.window_errors),
            Span::styled("  ←──┘", Style::default().fg(Color::Gray)),
        ]);
        let line6 = Line::from(vec![
            Span::styled("                ", theme::label_style()),
            Span::styled("▼", Style::default().fg(Color::Gray)),
        ]);
        let line7 = Line::from(vec![
            Span::styled("  blockstore  ", theme::label_style()),
            Span::styled(
                inserted_text,
                Style::default()
                    .fg(COL_INSERTED)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" turbine  ", theme::label_style()),
            Span::styled(
                format!("{}", self.numbers.blockstore_recovered),
                Style::default().fg(COL_FEC),
            ),
            Span::styled(" FEC  ", theme::label_style()),
            Span::styled(
                format!("{}", self.numbers.blockstore_repair),
                if self.numbers.blockstore_repair > 0 {
                    Style::default().fg(COL_REPAIR).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(" repair", theme::label_style()),
        ]);

        let lines = [line1, line2, line3, line4, line5, line6, line7];
        for (i, line) in lines.into_iter().take(area.height as usize).enumerate() {
            let y = area.y + i as u16;
            if y >= area.y + area.height {
                break;
            }
            frame.render_widget(Paragraph::new(line), Rect::new(area.x, y, area.width, 1));
        }
    }
}

fn fmt_opt(v: Option<u64>) -> String {
    v.map_or_else(|| "—".to_owned(), |n| format!("{n}"))
}

fn discards_span(discards: u64) -> Span<'static> {
    if discards == 0 {
        Span::raw("")
    } else {
        Span::styled(
            format!(" · {discards} drop"),
            Style::default().fg(COL_ERROR).add_modifier(Modifier::BOLD),
        )
    }
}

fn errors_span(errors: u64) -> Span<'static> {
    if errors == 0 {
        Span::raw("")
    } else {
        Span::styled(
            format!(" · {errors} err"),
            Style::default().fg(COL_ERROR).add_modifier(Modifier::BOLD),
        )
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
    fn intensity_classifies_by_count() {
        assert_eq!(ShredIngressPane::classify(10), Intensity::Small);
        assert_eq!(ShredIngressPane::classify(75), Intensity::Medium);
        assert_eq!(ShredIngressPane::classify(500), Intensity::Large);
    }

    #[test]
    fn shred_fetch_spawns_one_particle_per_batch() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetch {
            shred_count: 100,
        })));
        // ONE particle per batch (we no longer fragment), intensity Medium.
        assert_eq!(p.particles.len(), 1);
        assert_eq!(p.particles[0].lane, LaneKind::Turbine);
        assert_eq!(p.particles[0].intensity, Intensity::Medium);
    }

    #[test]
    fn shred_fetch_repair_spawns_repair_particle() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetchRepair {
            shred_count: 5,
        })));
        assert_eq!(p.particles.len(), 1);
        assert_eq!(p.particles[0].lane, LaneKind::Repair);
        assert_eq!(p.numbers.repair_fetch, Some(5));
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
    fn blockstore_insert_partition_recorded() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::Metric(MetricEvent::BlockstoreInsert {
            num_shreds: 771,
            num_inserted: 601,
            num_repair: 12,
            num_recovered: 184,
            total_elapsed_us: 11_832,
        })));
        assert_eq!(p.numbers.blockstore_inserted, Some(601));
        assert_eq!(p.numbers.blockstore_repair, 12);
        assert_eq!(p.numbers.blockstore_recovered, 184);
    }

    #[test]
    fn discards_span_empty_when_zero() {
        let s = discards_span(0);
        assert!(s.content.is_empty());
    }

    #[test]
    fn discards_span_loud_when_nonzero() {
        let s = discards_span(8);
        assert!(s.content.contains("8 drop"));
        assert_eq!(s.style.fg, Some(COL_ERROR));
    }

    #[test]
    fn non_metric_events_ignored() {
        let mut p = ShredIngressPane::new();
        p.on_event(&mk(EventKind::FirstShred { slot: 1 }));
        assert!(p.particles.is_empty());
    }

    #[test]
    fn particle_cap_evicts_oldest() {
        let mut p = ShredIngressPane::new();
        for _ in 0..(PARTICLE_CAP + 10) {
            p.on_event(&mk(EventKind::Metric(MetricEvent::ShredFetch {
                shred_count: 10,
            })));
        }
        assert!(p.particles.len() <= PARTICLE_CAP);
    }
}
