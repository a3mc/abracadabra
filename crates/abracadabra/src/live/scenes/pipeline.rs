//! Pipeline pane — shred ingress strip.
//!
//! Single-strip visualisation of the shred stream flowing from the
//! cluster (left) into the validator's INPUT (right). Particles ride a
//! wavy sine line; floating labels mark slot boundaries so the
//! operator can read both *rate* (particle density) and *identity*
//! (which slot is currently shredding) without leaving this pane.
//!
//! Layout (compact, one of many planned strips):
//!
//! ```text
//!  head 2070553   shreds seen 374   7.2/s
//!
//!    2070553              2070554
//!       · ·                  · ·
//!  cluster ⟫⟫⟫⟫⟫·⟫⟫·⟫·⟫⟫⟫⟫⟫⟫·⟫·⟫⟫⟫⟫⟫⟫·⟫·⟫⟫·⟫⟫ INPUT
//!
//! ```
//!
//! Subsequent strips (voting, bank assembly, ledger, leader window,
//! cluster info) will stack below this one — each implemented as its
//! own `Pane`. Keep this strip tight on vertical space so the others
//! get room.
//!
//! Animation comes from:
//!
//! - `wave_phase` advances each tick — the wave drifts.
//! - Jitter re-rolls per frame from `frame_seed` so particles shimmer.
//! - Buckets roll forward in time, pulling labels along with them.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph};
use ratatui::Frame;

use crate::live::animation::Pane;
use crate::parser::{Event, EventKind};
use crate::tui::theme;

/// Number of historical buckets kept for the stream. Wider history =
/// longer comet tail trailing left of the present moment.
const STREAM_BUCKETS: usize = 240;

/// Wall-clock duration each bucket spans. At ~60 FPS render and ~50ms
/// bucket size, the right-most ~5 buckets reflect the freshest data
/// and the left edge is ~12 seconds in the past.
const BUCKET_INTERVAL: Duration = Duration::from_millis(50);

/// Wave amplitude (in chart-y units) applied to the stream's mean line.
/// Picked to keep the stream visually well within the 0..=6 y-range
/// chosen for the chart bounds.
const WAVE_AMPLITUDE: f64 = 1.4;

/// Wave frequency in radians per bucket. Lower = lazier wave.
const WAVE_FREQUENCY: f64 = 0.18;

/// Y-axis centre for the stream's wave. Sits in the middle of the 0..=6
/// chart range so the wave has room above and below.
const WAVE_BASE_Y: f64 = 3.0;

/// Maximum dot count per bucket. Caps the visual density during bursts
/// so a 100-shred bucket does not paint a solid bar. Picked low for a
/// delicate look — shreds are tiny things, the visual respects that.
const MAX_DOTS_PER_BUCKET: u32 = 4;

/// Minimum cell-distance between two adjacent floating slot labels on
/// the stream. Adjacent buckets that report the same new slot collapse
/// to a single label; consecutive *different* slots that land within
/// this many cells of each other also drop the later one. Prevents
/// label pile-up during heavy bursts.
const SLOT_LABEL_MIN_GAP_CELLS: u16 = 10;

/// One historical bucket: how many shreds landed during this
/// `BUCKET_INTERVAL` window, and which slot's shreds they were. The
/// slot is the most recent `FirstShred` seen during the window — a
/// single bucket can in theory observe multiple slots but in practice
/// FirstShreds for a given slot arrive in tight clusters, so the
/// "latest seen" is the operationally meaningful one.
#[derive(Debug, Clone, Copy, Default)]
struct Bucket {
    count: u32,
    latest_slot: Option<u64>,
}

/// The spike pane. Owns a rolling history of FirstShred counts plus a
/// wave phase that advances each tick.
pub struct PipelinePane {
    /// Newest bucket at the back; oldest at the front.
    history: VecDeque<Bucket>,
    /// The bucket currently being filled (not yet pushed to history).
    /// Rolls into `history` and a fresh one is started every
    /// `BUCKET_INTERVAL`.
    current_bucket: Bucket,
    /// Wall-clock instant when `current_bucket` started accumulating.
    current_bucket_started: Instant,
    /// Wave phase in radians; advances each tick at `wave_speed`.
    wave_phase: f64,
    /// Wave phase speed in radians per second.
    wave_speed: f64,
    /// Monotonic frame counter, used as a seed for per-frame jitter so
    /// the cluster shimmers without an RNG dependency.
    frame_seed: u64,
    /// Most recent shredded slot number, for the headline strip.
    head_slot: Option<u64>,
    /// Total events seen since the pane started, for a sanity counter.
    total_events: u64,
}

impl PipelinePane {
    pub fn new() -> Self {
        let mut history = VecDeque::with_capacity(STREAM_BUCKETS);
        for _ in 0..STREAM_BUCKETS {
            history.push_back(Bucket::default());
        }
        Self {
            history,
            current_bucket: Bucket::default(),
            current_bucket_started: Instant::now(),
            wave_phase: 0.0,
            wave_speed: 1.8,
            frame_seed: 0,
            head_slot: None,
            total_events: 0,
        }
    }
}

impl Default for PipelinePane {
    fn default() -> Self {
        Self::new()
    }
}

impl Pane for PipelinePane {
    fn on_event(&mut self, ev: &Event) {
        if let EventKind::FirstShred { slot } = ev.kind {
            self.current_bucket.count = self.current_bucket.count.saturating_add(1);
            self.current_bucket.latest_slot = Some(slot);
            self.head_slot = Some(slot);
            self.total_events = self.total_events.saturating_add(1);
        }
    }

    fn tick(&mut self, now: Instant) {
        // Wave advances with real wall-clock dt so the visual speed
        // is independent of frame rate.
        let dt = now
            .saturating_duration_since(self.current_bucket_started)
            .as_secs_f64();
        self.wave_phase = self.wave_speed.mul_add(1.0 / 60.0, self.wave_phase);

        // Bucket rotation: if the current bucket has been open longer
        // than BUCKET_INTERVAL, push it to history and start a new one.
        if now.saturating_duration_since(self.current_bucket_started) >= BUCKET_INTERVAL {
            self.history.pop_front();
            self.history.push_back(self.current_bucket);
            self.current_bucket = Bucket::default();
            self.current_bucket_started = now;
        }

        self.frame_seed = self.frame_seed.wrapping_add(1);

        // dt is currently unused outside the bucket check; keeping the
        // binding makes the intent obvious for the next iteration when
        // we add velocity-driven background particles.
        let _ = dt;
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" cluster ⟫ shreds ⟫ INPUT ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < 30 || inner.height < 5 {
            return;
        }

        // Vertical split: 1-row headline, chart fills the rest.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(3)])
            .split(inner);

        self.render_headline(frame, chunks[0]);
        self.render_stream(frame, chunks[1]);
    }
}

impl PipelinePane {
    /// Single-row headline above the stream. Carries the operationally
    /// meaningful numbers in plain text so the operator does not need
    /// to read motion to know the current head slot.
    fn render_headline(&self, frame: &mut Frame<'_>, area: Rect) {
        let head = self
            .head_slot
            .map_or_else(|| "head —".to_owned(), |s| format!("head {s}"));
        let total = format!("shreds seen {}", self.total_events);
        let window = format!(
            "{:.1}s window",
            STREAM_BUCKETS as f64 * BUCKET_INTERVAL.as_secs_f64(),
        );
        let line = Line::from(vec![
            Span::styled(head, theme::accent_style().add_modifier(Modifier::BOLD)),
            Span::styled("   ", theme::label_style()),
            Span::styled(total, theme::value_style()),
            Span::styled("   ", theme::label_style()),
            Span::styled(window, theme::label_style()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    /// The stream itself: Braille chart of jittered particles plus
    /// `cluster` / `INPUT` endpoint markers and floating slot labels
    /// at each detected slot boundary.
    fn render_stream(&self, frame: &mut Frame<'_>, area: Rect) {
        // Generate jittered points: per bucket with activity, contribute
        // `√count` particles centred on the moving sine wave. Jitter is
        // deterministic from `(frame_seed, bucket_idx, dot_idx)` so it
        // changes per frame but stays bounded.
        let mut points: Vec<(f64, f64)> = Vec::new();
        for (i, bucket) in self.history.iter().enumerate() {
            if bucket.count == 0 {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            let x = i as f64;
            let wave_y = self.wave_at(x);
            let dots = ((bucket.count as f64).sqrt().ceil() as u32).min(MAX_DOTS_PER_BUCKET);
            let activity_spread = (f64::from(bucket.count) * 0.05).min(0.6);
            for dot in 0..dots {
                let jx = hash_jitter(self.frame_seed, i as u64, u64::from(dot), 0x9E37)
                    * activity_spread;
                let jy = hash_jitter(self.frame_seed, i as u64, u64::from(dot), 0xB5AD) * 0.40;
                points.push((x + jx, wave_y + jy));
            }
        }

        let stream_color = Color::Cyan;
        let datasets = vec![Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Scatter)
            .style(
                Style::default()
                    .fg(stream_color)
                    .add_modifier(Modifier::DIM),
            )
            .data(&points)];

        #[allow(clippy::cast_precision_loss)]
        let max_x = STREAM_BUCKETS as f64 - 1.0;
        let chart = Chart::new(datasets)
            .x_axis(Axis::default().bounds([0.0, max_x]))
            .y_axis(Axis::default().bounds([0.0, 6.0]));
        frame.render_widget(chart, area);

        // Overlays painted *after* the chart so they stack on top of
        // it. Order: endpoint labels first (anchor the stream
        // semantically), then per-slot floating labels.
        Self::render_endpoints(frame, area, max_x);
        self.render_slot_labels(frame, area, max_x);
    }

    /// Paint `cluster ⟫` at the left edge and `⟫ INPUT` at the right
    /// edge, vertically aligned to the wave's mean line. Tells the
    /// operator at a glance what direction data is flowing.
    fn render_endpoints(frame: &mut Frame<'_>, area: Rect, max_x: f64) {
        let mean_y = WAVE_BASE_Y;
        let sy = data_y_to_screen(mean_y, area);

        let cluster = "cluster ⟫";
        let input = "⟫ INPUT";
        let cluster_w = cluster.chars().count() as u16;
        let input_w = input.chars().count() as u16;

        if area.width <= cluster_w + input_w {
            return;
        }

        let cluster_rect = Rect::new(area.x, sy, cluster_w, 1);
        frame.render_widget(
            Paragraph::new(Span::styled(
                cluster,
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            )),
            cluster_rect,
        );

        let input_rect = Rect::new(area.x + area.width - input_w, sy, input_w, 1);
        frame.render_widget(
            Paragraph::new(Span::styled(
                input,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )),
            input_rect,
        );

        let _ = max_x;
    }

    /// Paint a small slot-number label above the wave at each detected
    /// slot transition in `history`. A transition is a bucket whose
    /// `latest_slot` differs from the slot last labelled. Labels too
    /// close to a previous one are suppressed (`SLOT_LABEL_MIN_GAP_CELLS`)
    /// to prevent pile-up during bursts.
    fn render_slot_labels(&self, frame: &mut Frame<'_>, area: Rect, max_x: f64) {
        let mut last_seen_slot: Option<u64> = None;
        let mut last_label_screen_x: Option<u16> = None;

        for (i, bucket) in self.history.iter().enumerate() {
            let Some(slot) = bucket.latest_slot else {
                continue;
            };
            if last_seen_slot == Some(slot) {
                continue;
            }
            last_seen_slot = Some(slot);

            #[allow(clippy::cast_precision_loss)]
            let data_x = i as f64;
            let sx = data_x_to_screen(data_x, max_x, area);

            // Suppress labels that would crowd the previous one.
            if let Some(prev) = last_label_screen_x {
                if sx < prev.saturating_add(SLOT_LABEL_MIN_GAP_CELLS) {
                    continue;
                }
            }

            // Render slightly above the wave at this x: take the
            // wave's current y here, add a small offset upward, map to
            // screen.
            let label_data_y = (self.wave_at(data_x) + 1.6).min(5.8);
            let mut sy = data_y_to_screen(label_data_y, area);
            // Don't paint above the top row of the chart.
            sy = sy.max(area.y);

            let label = format!("{slot}");
            let lw = label.chars().count() as u16;
            if sx + lw > area.x + area.width {
                continue;
            }
            let rect = Rect::new(sx, sy, lw, 1);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    label,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::DIM),
                )),
                rect,
            );
            last_label_screen_x = Some(sx);
        }
    }

    fn wave_at(&self, x: f64) -> f64 {
        WAVE_AMPLITUDE.mul_add(
            WAVE_FREQUENCY.mul_add(x, self.wave_phase).sin(),
            WAVE_BASE_Y,
        )
    }
}

/// Map a chart-data x value (range `[0, max_x]`) to a screen column
/// within `area`. Clamps at the right edge.
fn data_x_to_screen(data_x: f64, max_x: f64, area: Rect) -> u16 {
    if max_x <= 0.0 {
        return area.x;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let raw = (data_x / max_x * f64::from(area.width.saturating_sub(1))) as u16;
    area.x + raw.min(area.width.saturating_sub(1))
}

/// Map a chart-data y value (range `[0, 6]`) to a screen row within
/// `area`. The chart's y-axis runs bottom-up, so y=6 (top of chart)
/// maps to area.y (top of screen).
fn data_y_to_screen(data_y: f64, area: Rect) -> u16 {
    const Y_MAX: f64 = 6.0;
    let clamped = data_y.clamp(0.0, Y_MAX);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let raw = ((1.0 - clamped / Y_MAX) * f64::from(area.height.saturating_sub(1))) as u16;
    area.y + raw.min(area.height.saturating_sub(1))
}

/// Deterministic float in `[-1.0, 1.0]` from a small int seed mix.
/// Used in place of an RNG so the spike has zero extra dependencies.
fn hash_jitter(frame: u64, bucket: u64, dot: u64, salt: u64) -> f64 {
    // xorshift-style mix. Good enough for visual jitter; not a PRNG.
    let mut h = frame
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(bucket.wrapping_mul(0xBF58_476D_1CE4_E5B9))
        .wrapping_add(dot.wrapping_mul(0x94D0_49BB_1331_11EB))
        .wrapping_add(salt);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    #[allow(clippy::cast_precision_loss)]
    let scaled = (h as f64) / (u64::MAX as f64);
    scaled.mul_add(2.0, -1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_event(kind: EventKind) -> Event {
        Event {
            ts: time::OffsetDateTime::UNIX_EPOCH,
            kind,
        }
    }

    #[test]
    fn first_shred_increments_current_bucket() {
        let mut p = PipelinePane::new();
        assert_eq!(p.current_bucket.count, 0);
        p.on_event(&mk_event(EventKind::FirstShred { slot: 100 }));
        assert_eq!(p.current_bucket.count, 1);
        p.on_event(&mk_event(EventKind::FirstShred { slot: 101 }));
        assert_eq!(p.current_bucket.count, 2);
        assert_eq!(p.head_slot, Some(101));
        assert_eq!(p.total_events, 2);
    }

    #[test]
    fn non_first_shred_event_ignored() {
        let mut p = PipelinePane::new();
        p.on_event(&mk_event(EventKind::NewRoot { slot: 42 }));
        p.on_event(&mk_event(EventKind::BankFrozen {
            slot: 42,
            hash: "h".into(),
            signature_count: 1,
        }));
        assert_eq!(p.current_bucket.count, 0);
        assert_eq!(p.total_events, 0);
        assert!(p.head_slot.is_none());
    }

    #[test]
    fn bucket_rotates_after_interval_elapses() {
        let mut p = PipelinePane::new();
        p.on_event(&mk_event(EventKind::FirstShred { slot: 100 }));
        assert_eq!(p.current_bucket.count, 1);
        // Force the tick to see ≥BUCKET_INTERVAL of wall-clock by
        // backdating the bucket start.
        p.current_bucket_started = Instant::now()
            .checked_sub(BUCKET_INTERVAL + Duration::from_millis(10))
            .unwrap();
        p.tick(Instant::now());
        assert_eq!(p.current_bucket.count, 0);
        // Newest history entry should now hold the prior count.
        assert_eq!(p.history.back().unwrap().count, 1);
    }

    #[test]
    fn history_length_stable_after_many_rotations() {
        let mut p = PipelinePane::new();
        let initial_len = p.history.len();
        for _ in 0..(STREAM_BUCKETS * 3) {
            p.current_bucket_started = Instant::now()
                .checked_sub(BUCKET_INTERVAL + Duration::from_millis(10))
                .unwrap();
            p.tick(Instant::now());
        }
        assert_eq!(p.history.len(), initial_len);
    }

    #[test]
    fn hash_jitter_in_unit_range() {
        for f in 0..100u64 {
            for b in 0..10u64 {
                let v = hash_jitter(f, b, 0, 0xDEAD);
                assert!((-1.0..=1.0).contains(&v), "out of range: {v}");
            }
        }
    }
}
