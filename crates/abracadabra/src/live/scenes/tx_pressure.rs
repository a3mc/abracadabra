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

#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine, Points};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use time::OffsetDateTime;

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
/// tiny bucket from pinning the curve to the top. Integer form
/// (`MIN_Y_MAX_SIGS`) drives cache-key bucketing; `MIN_Y_MAX_F` is
/// the same value as `f64` for Canvas y-bounds math.
const MIN_Y_MAX_SIGS: u64 = 1000;
const MIN_Y_MAX_F: f64 = 1000.0;

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
    ts: OffsetDateTime,
    signatures: u64,
}

/// Cache key for the chart's pre-computed `(x, y, t)` coordinate
/// scratch buffer. The chart only changes meaningfully when one of
/// these inputs advances:
///
/// - `sample_count` — a sample was added or pruned;
/// - `latest_sig` — the newest sample's signature count changed
///   (defensive; the same `latest_event_ts` should always carry the
///   same signature payload but be explicit);
/// - `y_max_bucket` — the curve's vertical scale moved;
/// - `area` — terminal resized or layout shifted.
///
/// Wall-clock alone does not invalidate the cache: the X-axis is
/// anchored to `latest_event_ts`, not to `Instant::now()`, so the
/// rendered curve is byte-identical across frames inside a single
/// event-quiescent window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CoordKey {
    sample_count: usize,
    latest_sig: Option<u64>,
    y_max_bucket: u64,
    /// Event-time anchor bucket. Coords scroll left as
    /// `latest_event_ts` advances; without this field the cache
    /// would stay stale across scroll-cell boundaries. Bucketed to
    /// whole seconds so 60 FPS frames within the same second hit
    /// the cache, but each event-second advance forces a recompute.
    now_ts_bucket: i64,
    area_x: u16,
    area_y: u16,
    area_w: u16,
    area_h: u16,
}

/// Pre-computed coords plus the key they were computed from. Lives
/// behind a [`RefCell`] because [`Pane::render`] takes `&self`.
#[derive(Debug, Default)]
struct ChartCache {
    coords: Vec<(f64, f64, f64)>,
    key: Option<CoordKey>,
    /// Cached `y_max` matching `coords`. Re-emitted to the Canvas
    /// widget so the paint closure does not recompute the peak.
    y_max: f64,
}

pub struct TxPressurePane {
    samples: VecDeque<Sample>,
    latest_sig: Option<u64>,
    /// Newest BankFrozen `ev.ts` seen so far. Acts as the chart's
    /// "now" anchor on the X-axis. `None` until the first event.
    latest_event_ts: Option<OffsetDateTime>,
    /// Reused per-frame scratch + cache. `render_chart` consults the
    /// key and refills `coords` only when the inputs changed; the
    /// Canvas paint closure always iterates the cached buffer.
    cache: RefCell<ChartCache>,
    /// Counts the number of full coord-buffer rebuilds. Used by
    /// tests to assert cache reuse across frames.
    #[cfg(test)]
    rebuild_count: Cell<usize>,
}

impl TxPressurePane {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(MAX_SAMPLES),
            latest_sig: None,
            latest_event_ts: None,
            cache: RefCell::new(ChartCache::default()),
            #[cfg(test)]
            rebuild_count: Cell::new(0),
        }
    }

    fn prune(&mut self, anchor: OffsetDateTime) {
        // `OffsetDateTime::checked_sub` takes `time::Duration`. Convert
        // the std-typed window once; the fallback prevents a panic on
        // a hypothetical pre-`MIN` anchor (test fixtures only).
        let window = time::Duration::try_from(VISIBLE_WINDOW).unwrap_or(time::Duration::ZERO);
        let cutoff = anchor.checked_sub(window).unwrap_or(anchor);
        while let Some(s) = self.samples.front() {
            if s.ts < cutoff {
                self.samples.pop_front();
            } else {
                break;
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
                ts: ev.ts,
                signatures: *signature_count,
            });
            self.latest_sig = Some(*signature_count);
            // Guard against out-of-order log lines: monotonically
            // advance the anchor only.
            self.latest_event_ts = Some(match self.latest_event_ts {
                Some(prev) if prev > ev.ts => prev,
                _ => ev.ts,
            });
            if let Some(anchor) = self.latest_event_ts {
                self.prune(anchor);
            }
        }
    }

    fn tick(&mut self, _now: Instant) {
        // Sample retention is anchored on the newest event ts, not
        // wall-clock. Re-prune in case `MAX_SAMPLES` constraints need
        // to apply between events.
        if let Some(anchor) = self.latest_event_ts {
            self.prune(anchor);
        }
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
        let Some(now_ts) = self.latest_event_ts else {
            return;
        };

        let window_secs = VISIBLE_WINDOW.as_secs_f64();
        let peak_int = self.samples.iter().map(|s| s.signatures).max().unwrap_or(0);
        let peak_observed = peak_int as f64;
        let y_max = peak_observed.max(MIN_Y_MAX_F);

        // Y-axis bucket coarse enough that a single-signature change
        // does not invalidate the cache. 64 signatures ≈ one slot's
        // vote-floor wiggle.
        let y_max_bucket = peak_int.max(MIN_Y_MAX_SIGS) / 64;

        let key = CoordKey {
            sample_count: self.samples.len(),
            latest_sig: self.latest_sig,
            y_max_bucket,
            now_ts_bucket: now_ts.unix_timestamp(),
            area_x: area.x,
            area_y: area.y,
            area_w: area.width,
            area_h: area.height,
        };

        let mut cache = self.cache.borrow_mut();
        if cache.key != Some(key) {
            cache.coords.clear();
            cache.coords.reserve(self.samples.len());
            for s in &self.samples {
                // (now_ts - s.ts) is a `time::Duration` (signed). Clamp
                // age into [0, window_secs] so a freak out-of-order
                // sample older than the window still renders safely
                // even if `prune` somehow missed it.
                let age = (now_ts - s.ts).as_seconds_f64().clamp(0.0, window_secs);
                let x = (window_secs - age).max(0.0);
                let y = s.signatures as f64;
                let t = (y / y_max).clamp(0.0, 1.0);
                cache.coords.push((x, y, t));
            }
            cache.key = Some(key);
            cache.y_max = y_max;
            #[cfg(test)]
            self.rebuild_count.set(self.rebuild_count.get() + 1);
        }

        // Hand the cached coords to the Canvas paint closure as a
        // borrow-free owned copy. The clone is cheap (one `Vec` of
        // `(f64, f64, f64)`) and sidesteps lifetime issues with the
        // `'static`-bound closure ratatui's Canvas takes. The closure
        // body itself avoids any per-frame allocation.
        let coords_snapshot: Vec<(f64, f64, f64)> = cache.coords.clone();
        let cached_y_max = cache.y_max;
        drop(cache);

        let canvas = Canvas::default()
            .marker(Marker::Braille)
            .x_bounds([0.0, window_secs])
            .y_bounds([0.0, cached_y_max])
            .paint(move |ctx| {
                // 1. Area fill: vertical strips at each sample,
                // dimmed thermal color.
                for &(x, y, t) in &coords_snapshot {
                    ctx.draw(&CanvasLine {
                        x1: x,
                        y1: 0.0,
                        x2: x,
                        y2: y,
                        color: dim_rgb(thermal_color(t)),
                    });
                }

                // 2. Smooth curve: segments between consecutive
                // samples, full-intensity thermal color (averaged
                // across the segment for smoothness).
                for w in coords_snapshot.windows(2) {
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

                // 3. "Now" glow: a small cross at the latest sample.
                if let Some(&(x, y, t)) = coords_snapshot.last() {
                    let glow = thermal_color(t.max(0.6));
                    let dx = window_secs * 0.005;
                    let dy = cached_y_max * 0.025;
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
            Span::styled(" now ", theme::label_style()),
            Span::styled(
                latest,
                Style::default()
                    .fg(Color::Rgb(180, 220, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" sig", theme::label_style()),
            sep(),
            Span::styled("avg ", theme::label_style()),
            Span::styled(avg, Style::default().fg(Color::Rgb(220, 200, 80))),
            sep(),
            Span::styled("peak ", theme::label_style()),
            Span::styled(
                peak,
                Style::default()
                    .fg(Color::Rgb(230, 70, 60))
                    .add_modifier(Modifier::BOLD),
            ),
            sep(),
            Span::styled(
                format!("last {} s", VISIBLE_WINDOW.as_secs()),
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

    fn mk_at(kind: EventKind, ts: time::OffsetDateTime) -> Event {
        Event { ts, kind }
    }

    #[test]
    fn bank_frozen_appends_sample() {
        let mut p = TxPressurePane::new();
        let ev = mk(EventKind::BankFrozen {
            slot: 100,
            hash: "abc".into(),
            signature_count: 5000,
        });
        p.on_event(&ev);
        assert_eq!(p.samples.len(), 1);
        assert_eq!(p.samples.back().unwrap().signatures, 5000);
        assert_eq!(p.samples.back().unwrap().ts, ev.ts);
        assert_eq!(p.latest_sig, Some(5000));
        assert_eq!(p.latest_event_ts, Some(ev.ts));
    }

    #[test]
    fn non_bank_frozen_event_ignored() {
        let mut p = TxPressurePane::new();
        p.on_event(&mk(EventKind::FirstShred { slot: 100 }));
        assert_eq!(p.samples.len(), 0);
        assert_eq!(p.latest_sig, None);
        assert!(p.latest_event_ts.is_none());
    }

    #[test]
    fn old_samples_prune_after_visible_window() {
        let mut p = TxPressurePane::new();
        let window_s = i64::try_from(VISIBLE_WINDOW.as_secs()).unwrap_or(i64::MAX);
        let anchor = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(window_s + 100);
        let older = anchor - time::Duration::seconds(window_s + 10);
        p.samples.push_back(Sample {
            ts: older,
            signatures: 1_000,
        });
        p.samples.push_back(Sample {
            ts: anchor,
            signatures: 2_000,
        });
        p.prune(anchor);
        assert_eq!(p.samples.len(), 1);
        assert_eq!(p.samples.back().unwrap().signatures, 2_000);
    }

    #[test]
    fn burst_in_one_cycle_spreads_samples_on_chart() {
        // Three BankFrozen events 200 ms apart, fed in succession
        // without any wall-clock tick between them. The resulting
        // sample timestamps must reflect the event ts spacing, not
        // collapse to a single tick instant.
        let mut p = TxPressurePane::new();
        let t0 = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1_000);
        let t1 = t0 + time::Duration::milliseconds(200);
        let t2 = t1 + time::Duration::milliseconds(200);
        for ts in [t0, t1, t2] {
            p.on_event(&mk_at(
                EventKind::BankFrozen {
                    slot: 0,
                    hash: "h".into(),
                    signature_count: 100,
                },
                ts,
            ));
        }
        assert_eq!(p.samples.len(), 3);
        let stamps: Vec<_> = p.samples.iter().map(|s| s.ts).collect();
        assert_eq!(stamps, vec![t0, t1, t2]);
        assert_eq!(p.latest_event_ts, Some(t2));
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

    #[test]
    fn tx_pressure_chart_cached_until_scroll_tick() {
        // Render the pane twice without any new samples between
        // frames. The coord-buffer rebuild counter must increment
        // exactly once: first frame populates the cache, second
        // frame reuses it. A subsequent `on_event` (new sample)
        // forces a rebuild on the next render.
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut p = TxPressurePane::new();
        let t0 = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(10_000);
        for i in 0_u64..5 {
            p.on_event(&mk_at(
                EventKind::BankFrozen {
                    slot: i,
                    hash: "h".into(),
                    signature_count: 1_000 + i * 250,
                },
                t0 + time::Duration::seconds(i64::try_from(i).unwrap_or(0)),
            ));
        }
        // Reset the rebuild counter to zero before the test renders
        // so we measure only the renders we control.
        p.rebuild_count.set(0);

        let area = Rect::new(0, 0, 80, 12);
        terminal.draw(|f| p.render(f, area)).unwrap();
        let after_first = p.rebuild_count.get();
        assert_eq!(after_first, 1, "first render must rebuild coords");

        terminal.draw(|f| p.render(f, area)).unwrap();
        assert_eq!(
            p.rebuild_count.get(),
            after_first,
            "second render with no new samples must reuse cached coords",
        );

        // Same area, same now_ts, but `latest_sig` advancing must
        // invalidate the cache.
        p.on_event(&mk_at(
            EventKind::BankFrozen {
                slot: 99,
                hash: "h".into(),
                signature_count: 9_999,
            },
            t0 + time::Duration::seconds(6),
        ));
        terminal.draw(|f| p.render(f, area)).unwrap();
        assert_eq!(
            p.rebuild_count.get(),
            after_first + 1,
            "new sample must force a coord rebuild",
        );
    }
}
