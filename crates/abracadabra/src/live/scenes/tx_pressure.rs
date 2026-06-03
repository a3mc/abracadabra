//! Transaction pressure — Canvas/Braille thermal area chart.
//!
//! Plots `signature_count` per finalized slot (from `BankFrozen`
//! events) as a smooth line with a thermal RGB gradient fill
//! beneath it. Each `BankFrozen` event is one sample; the chart
//! scrolls left as time advances. signature_count includes both
//! vote and user transactions — most of the baseline is vote
//! signatures (~2 per active validator per slot), so deviation
//! above that floor is user-transaction load.
//!
//! Rendering uses [`ratatui::widgets::canvas::Canvas`] with
//! [`ratatui::symbols::Marker::Braille`], which gives roughly 2×4
//! sub-pixels per terminal cell. The curve and the area fill are
//! both drawn at sub-pixel resolution. Color is interpolated per
//! sample on a three-stop gradient (cool blue at low pressure →
//! warm yellow at moderate → hot red at peak). True-color terminals
//! get the smooth gradient; 256-color terminals fall back via
//! ratatui's color resolution.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine, Points};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::live::animation::Pane;
use crate::parser::{Event, EventKind};
use crate::tui::theme;

pub const PANE_HEIGHT: u16 = 9;

/// Width of the chart in seconds. Matches the visual time window
/// used by the sparkline panes above so the user can read all three
/// against the same wall-clock.
const VISIBLE_WINDOW: Duration = Duration::from_secs(75);

/// Upper bound on retained samples. Solana mainnet runs ~2.5
/// slots/sec, so 75 s ≈ 188 slots; 512 covers replay speeds up to
/// ~3× before truncation starts to bite.
const MAX_SAMPLES: usize = 512;

/// Y-axis floor when there's almost no data — prevents a single
/// tiny bucket from pinning the curve to the top.
const MIN_Y_MAX: f64 = 1000.0;

/// Three-stop thermal gradient anchors (R, G, B). Cyan-blue at low,
/// yellow-orange at mid, red at high.
const COLOR_LOW: (u8, u8, u8) = (40, 130, 200);
const COLOR_MID: (u8, u8, u8) = (220, 200, 80);
const COLOR_HIGH: (u8, u8, u8) = (230, 70, 60);

/// Divisor applied to area-fill RGB so the fill is dimmer than the
/// curve. 3 is empirically the sweet spot — visible but subordinate.
const FILL_DIM_DIVISOR: u8 = 3;

#[derive(Debug, Clone, Copy)]
struct Sample {
    ts: Instant,
    signatures: u64,
}

pub struct TxPressurePane {
    samples: VecDeque<Sample>,
    latest_sig: Option<u64>,
    now: Instant,
}

impl TxPressurePane {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(MAX_SAMPLES),
            latest_sig: None,
            now: Instant::now(),
        }
    }

    fn prune(&mut self, now: Instant) {
        if let Some(cutoff) = now.checked_sub(VISIBLE_WINDOW) {
            while let Some(s) = self.samples.front() {
                if s.ts < cutoff {
                    self.samples.pop_front();
                } else {
                    break;
                }
            }
        }
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
    }

    fn avg(&self) -> Option<u64> {
        if self.samples.is_empty() {
            return None;
        }
        let sum: u64 = self.samples.iter().map(|s| s.signatures).sum();
        Some(sum / self.samples.len() as u64)
    }

    fn peak(&self) -> Option<u64> {
        self.samples.iter().map(|s| s.signatures).max()
    }
}

impl Default for TxPressurePane {
    fn default() -> Self {
        Self::new()
    }
}

impl Pane for TxPressurePane {
    fn on_event(&mut self, ev: &Event) {
        if let EventKind::BankFrozen {
            signature_count, ..
        } = &ev.kind
        {
            self.samples.push_back(Sample {
                ts: self.now,
                signatures: *signature_count,
            });
            self.latest_sig = Some(*signature_count);
            self.prune(self.now);
        }
    }

    fn tick(&mut self, now: Instant) {
        self.now = now;
        self.prune(now);
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" tx pressure ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height < 3 || inner.width < 10 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // top spacer
                Constraint::Min(2),    // chart
                Constraint::Length(1), // snapshot
            ])
            .split(inner);

        self.render_chart(frame, chunks[1]);
        self.render_snapshot(frame, chunks[2]);
    }
}

#[allow(clippy::cast_precision_loss)]
impl TxPressurePane {
    fn render_chart(&self, frame: &mut Frame<'_>, area: Rect) {
        if self.samples.is_empty() {
            return;
        }

        let window_secs = VISIBLE_WINDOW.as_secs_f64();
        let peak_observed = self.samples.iter().map(|s| s.signatures).max().unwrap_or(0) as f64;
        let y_max = peak_observed.max(MIN_Y_MAX);
        let avg = (self.samples.iter().map(|s| s.signatures).sum::<u64>() as f64)
            / (self.samples.len() as f64);

        // Pre-compute sample coords + per-sample intensity so the
        // Canvas paint closure stays cheap and capture-free.
        let now = self.now;
        let coords: Vec<(f64, f64, f64)> = self
            .samples
            .iter()
            .map(|s| {
                let age = now.saturating_duration_since(s.ts).as_secs_f64();
                let x = (window_secs - age).max(0.0);
                let y = s.signatures as f64;
                let t = (y / y_max).clamp(0.0, 1.0);
                (x, y, t)
            })
            .collect();

        let canvas = Canvas::default()
            .marker(Marker::Braille)
            .x_bounds([0.0, window_secs])
            .y_bounds([0.0, y_max])
            .paint(move |ctx| {
                // 1. Area fill: vertical strips at each sample,
                // dimmed thermal color.
                for &(x, y, t) in &coords {
                    ctx.draw(&CanvasLine {
                        x1: x,
                        y1: 0.0,
                        x2: x,
                        y2: y,
                        color: dim_rgb(thermal_color(t)),
                    });
                }

                // 2. Reference line at rolling average. Drawn after
                // fill so it overlays cleanly.
                ctx.draw(&CanvasLine {
                    x1: 0.0,
                    y1: avg,
                    x2: window_secs,
                    y2: avg,
                    color: Color::DarkGray,
                });

                // 3. Smooth curve: segments between consecutive
                // samples, full-intensity thermal color (averaged
                // across the segment for smoothness).
                for w in coords.windows(2) {
                    let (x1, y1, t1) = w[0];
                    let (x2, y2, t2) = w[1];
                    ctx.draw(&CanvasLine {
                        x1,
                        y1,
                        x2,
                        y2,
                        color: thermal_color((t1 + t2) * 0.5),
                    });
                }

                // 4. "Now" glow: a small cross at the latest sample.
                if let Some(&(x, y, t)) = coords.last() {
                    let glow = thermal_color(t.max(0.6));
                    let dx = window_secs * 0.005;
                    let dy = y_max * 0.025;
                    ctx.draw(&Points {
                        coords: &[(x, y), (x - dx, y), (x + dx, y), (x, y - dy), (x, y + dy)],
                        color: glow,
                    });
                }
            });

        frame.render_widget(canvas, area);
    }

    fn render_snapshot(&self, frame: &mut Frame<'_>, area: Rect) {
        let latest = self
            .latest_sig
            .map_or_else(|| "—".to_owned(), |n| format!("{n}"));
        let avg = self
            .avg()
            .map_or_else(|| "—".to_owned(), |n| format!("{n}"));
        let peak = self
            .peak()
            .map_or_else(|| "—".to_owned(), |n| format!("{n}"));

        let line = Line::from(vec![
            Span::styled(
                format!(" {latest}"),
                Style::default()
                    .fg(Color::Rgb(180, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" sig/slot now", theme::label_style()),
            sep(),
            Span::styled(avg, Style::default().fg(Color::Rgb(220, 200, 80))),
            Span::styled(" avg", theme::label_style()),
            sep(),
            Span::styled(
                peak,
                Style::default()
                    .fg(Color::Rgb(230, 70, 60))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" peak", theme::label_style()),
            sep(),
            Span::styled(
                format!(
                    "last ~{} s ({} slots)",
                    VISIBLE_WINDOW.as_secs(),
                    self.samples.len()
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

/// Three-stop thermal gradient. `t` is the normalized pressure
/// in `[0, 1]`. Below 0.5 interpolates `COLOR_LOW → COLOR_MID`;
/// above interpolates `COLOR_MID → COLOR_HIGH`.
fn thermal_color(t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    let (r, g, b) = if t < 0.5 {
        lerp_rgb(COLOR_LOW, COLOR_MID, t / 0.5)
    } else {
        lerp_rgb(COLOR_MID, COLOR_HIGH, (t - 0.5) / 0.5)
    };
    Color::Rgb(r, g, b)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f64) -> (u8, u8, u8) {
    let lerp = |x: u8, y: u8| -> u8 {
        let v = (f64::from(y) - f64::from(x)).mul_add(t, f64::from(x));
        v.clamp(0.0, 255.0) as u8
    };
    (lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

/// Dim an RGB color for the area fill so the curve on top reads
/// against it. Non-RGB colors pass through unchanged.
const fn dim_rgb(c: Color) -> Color {
    if let Color::Rgb(r, g, b) = c {
        Color::Rgb(
            r / FILL_DIM_DIVISOR,
            g / FILL_DIM_DIVISOR,
            b / FILL_DIM_DIVISOR,
        )
    } else {
        c
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
    fn bank_frozen_appends_sample() {
        let mut p = TxPressurePane::new();
        p.on_event(&mk(EventKind::BankFrozen {
            slot: 100,
            hash: "abc".into(),
            signature_count: 5000,
        }));
        assert_eq!(p.samples.len(), 1);
        assert_eq!(p.samples.back().unwrap().signatures, 5000);
        assert_eq!(p.latest_sig, Some(5000));
    }

    #[test]
    fn non_bank_frozen_event_ignored() {
        let mut p = TxPressurePane::new();
        p.on_event(&mk(EventKind::FirstShred { slot: 100 }));
        assert_eq!(p.samples.len(), 0);
        assert_eq!(p.latest_sig, None);
    }

    #[test]
    fn old_samples_prune_after_visible_window() {
        let mut p = TxPressurePane::new();
        let now = Instant::now();
        let older = now
            .checked_sub(VISIBLE_WINDOW + Duration::from_secs(10))
            .unwrap();
        p.samples.push_back(Sample {
            ts: older,
            signatures: 1_000,
        });
        p.samples.push_back(Sample {
            ts: now,
            signatures: 2_000,
        });
        p.now = now;
        p.prune(now);
        assert_eq!(p.samples.len(), 1);
        assert_eq!(p.samples.back().unwrap().signatures, 2_000);
    }

    #[test]
    fn thermal_color_anchors_match_design() {
        let cold = thermal_color(0.0);
        let mid = thermal_color(0.5);
        let hot = thermal_color(1.0);
        assert!(matches!(cold, Color::Rgb(r, _, b) if r < 100 && b > 150));
        assert!(matches!(mid, Color::Rgb(r, g, _) if r > 150 && g > 150));
        assert!(matches!(hot, Color::Rgb(r, _, b) if r > 200 && b < 100));
    }

    #[test]
    fn dim_rgb_reduces_brightness() {
        let bright = Color::Rgb(240, 240, 240);
        let dim = dim_rgb(bright);
        match dim {
            Color::Rgb(r, g, b) => assert!(r < 240 && g < 240 && b < 240),
            _ => panic!("expected RGB"),
        }
    }

    #[test]
    fn avg_and_peak_reflect_samples() {
        let mut p = TxPressurePane::new();
        for v in [1000_u64, 5000, 3000, 7000] {
            p.on_event(&mk(EventKind::BankFrozen {
                slot: 0,
                hash: "h".into(),
                signature_count: v,
            }));
        }
        assert_eq!(p.peak(), Some(7000));
        assert_eq!(p.avg(), Some(4000));
    }
}
