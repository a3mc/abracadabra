//! Time-series tab (Tab 2) — card-grid layout.
//!
//! Each metric gets its own bordered card with a one-line stats summary
//! and a trailing sparkline. The chart visualises variation; the stats
//! line carries the absolute numbers. Shared time axis (bucket count /
//! oldest→newest direction) lives on the outer panel title — printed
//! once instead of once per card.
//!
//! Two visual modes:
//!   * **Single-series sparkline** — count / time metrics. Sparkline is
//!     baseline-subtracted (deviation above p10) so high-baseline
//!     series surface spikes. Stats line carries absolute totals and
//!     `active X/N buckets` so sparse vs sustained is obvious.
//!   * **Dual-series stacked card** — fast-vs-slow finalize breakdown.
//!     Two sparklines (fast top, slow bottom), each baseline-subtracted
//!     to its own p10, so both surfaces show their own variation.
//!     Stats line shows absolute range (`fast 77-95% · slow 5-23%`) so
//!     the deviation chart's magnitude isn't lost.
//!
//! All cards share the 2-column grid so widths line up bucket-for-bucket
//! across cards — eyeballing the same x-position across cards lines up
//! the same time slice.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Sparkline, Widget};
use ratatui::Frame;

use crate::model::buckets::TimeBuckets;
use crate::tui::theme;
use crate::tui::widget::{commas, fit_to_width};

/// Input tuple for a paired secondary series on a `CardSpec`:
/// `(label, color, per-bucket values)`. Aliased to keep the
/// `with_secondary` signature readable (clippy::type_complexity).
type SecondarySeriesInput = (&'static str, Color, Vec<u64>);

#[derive(Debug, Clone, Copy)]
enum Kind {
    /// Per-bucket count (events). Stats line shows total / peak / avg.
    Count,
    /// Per-bucket time value in milliseconds. Stats line shows min /
    /// avg / max.
    Time,
    /// Per-bucket rate value (e.g. tx/s). Stats line shows min / avg
    /// / peak with the unit suffix. No "total" because summing a rate
    /// across buckets is meaningless; no "/bkt" suffix because each
    /// value already is a per-second rate, not a per-bucket count.
    Rate,
}

/// Per-bucket secondary series rendered as a second sparkline below
/// the main one. Used by the vote-skip card to surface the
/// canonical-skip subset on the same time axis — the operator can
/// see WHERE in the log the bad skips clustered, not just the
/// aggregate count.
///
/// Carries its own `data` (raw per-bucket counts), `deviation` (data
/// minus its own p10 baseline) and `dev_peak` so the second sparkline
/// scales independently of the primary series. The primary's peak
/// can be 188/bucket while canonical is 1-2/bucket; sharing a scale
/// would flatten the secondary to invisibility.
struct SecondarySeries {
    label: &'static str,
    color: Color,
    /// Sum across all buckets — shown in the stats line as
    /// `<label> <total>`.
    total: u64,
    deviation: Vec<u64>,
    dev_peak: u64,
}

struct Metric {
    label: &'static str,
    kind: Kind,
    color: Color,
    data: Vec<u64>,
    /// Per-bucket value minus the p10 baseline — the series the
    /// sparkline actually draws. Precomputed in `build_cards` so the
    /// render path doesn't clone `data` and re-sort for the baseline
    /// each frame. See PERF-05.
    deviation: Vec<u64>,
    /// `max(deviation)` clamped to at least 1 so `Sparkline::max` is
    /// always positive (avoids a divide-by-zero in ratatui's scaler).
    dev_peak: u64,
    /// Optional second sparkline (e.g. canonical-skip below total
    /// vote-skip). When present, the chart area is split 50/50.
    secondary: Option<SecondarySeries>,
    /// Optional pre-formatted extra stat appended to the stats line
    /// (e.g. `~14k tx/block` on the tx-pressure card). Lets a card
    /// surface a single derived value without paying for a separate
    /// card slot when the grid is already full.
    subtitle: Option<String>,
}

impl Metric {
    fn new(label: &'static str, kind: Kind, color: Color, data: Vec<u64>) -> Self {
        Self::with_secondary(label, kind, color, data, None)
    }

    fn with_secondary(
        label: &'static str,
        kind: Kind,
        color: Color,
        data: Vec<u64>,
        secondary: Option<SecondarySeriesInput>,
    ) -> Self {
        let (deviation, dev_peak) = baseline_subtract(&data);
        let secondary = secondary.map(|(sec_label, sec_color, sec_data)| {
            let (sec_dev, sec_peak) = baseline_subtract(&sec_data);
            SecondarySeries {
                label: sec_label,
                color: sec_color,
                total: sec_data.iter().sum(),
                deviation: sec_dev,
                dev_peak: sec_peak,
            }
        });
        Self {
            label,
            kind,
            color,
            data,
            deviation,
            dev_peak,
            secondary,
            subtitle: None,
        }
    }

    fn with_subtitle(mut self, subtitle: String) -> Self {
        self.subtitle = Some(subtitle);
        self
    }
}

/// Subtract the p10 baseline so trailing-sparklines surface spikes
/// above a high floor. `select_nth_unstable` is O(n) average, vs the
/// full O(n log n) sort the previous implementation paid for. Returns
/// `(deviation, dev_peak)` where `dev_peak` is clamped to ≥1 to keep
/// ratatui's Sparkline scaler safe.
fn baseline_subtract(data: &[u64]) -> (Vec<u64>, u64) {
    let baseline = if data.is_empty() {
        0
    } else {
        let mut tmp = data.to_vec();
        let idx = tmp.len() / 10;
        tmp.select_nth_unstable(idx);
        tmp[idx]
    };
    let deviation: Vec<u64> = data.iter().map(|v| v.saturating_sub(baseline)).collect();
    let dev_peak = deviation.iter().copied().max().unwrap_or(1).max(1);
    (deviation, dev_peak)
}

/// Dual-series card variant: two sparklines stacked vertically inside a
/// single card body, each baseline-subtracted to its own p10 so both
/// surfaces show their own variation. Used for the fast-vs-slow
/// finalize breakdown — `top` = fast (green), `bottom` = slow (yellow).
struct DualMetric {
    label: &'static str,
    top_color: Color,
    top_data: Vec<u64>,
    bottom_color: Color,
    bottom_data: Vec<u64>,
}

enum CardSpec {
    Single(Metric),
    Dual(DualMetric),
}

/// Full-screen card grid for Tab 2.
pub fn render_detail(buckets: Option<&TimeBuckets>, frame: &mut Frame<'_>, area: Rect) {
    let title = buckets.map_or_else(
        || " time series — (no data) ".to_owned(),
        |b| {
            format!(
                " time series — ← oldest {} buckets newest →   (bucket = {}, shared x-axis across all cards) ",
                b.buckets.len(),
                humanize_dur(b.bucket_size.whole_seconds()),
            )
        },
    );
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(theme::title_style());
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    let Some(b) = buckets else {
        return;
    };

    let cards = build_cards(b);
    if cards.is_empty() {
        return;
    }

    // Uniform 2-column grid. All cards same width and height so x-axis
    // pixels line up across cards — a spike at column N on the
    // SafeToNotar card sits directly above/below the spike at column N
    // on every other card, which is the diagnostic the user needs.
    let rows_needed = cards.len().div_ceil(2);
    let row_constraints: Vec<Constraint> = (0..rows_needed)
        .map(|_| Constraint::Ratio(1, rows_needed as u32))
        .collect();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(inner);

    for (r, pair) in cards.chunks(2).enumerate() {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[r]);
        for (c, card) in pair.iter().enumerate() {
            match card {
                CardSpec::Single(m) => render_card(frame, cols[c], m),
                CardSpec::Dual(d) => render_dual_card(frame, cols[c], d),
            }
        }
    }
}

fn render_card(frame: &mut Frame<'_>, area: Rect, m: &Metric) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", m.label))
        .title_style(theme::title_style());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let min = m.data.iter().copied().min().unwrap_or(0);
    let max = m.data.iter().copied().max().unwrap_or(0);
    let avg = mean(&m.data);
    let total: u64 = m.data.iter().sum();
    let n_buckets = m.data.len();
    let active = m.data.iter().filter(|v| **v > 0).count();

    // Baseline = p10. The trailing sparkline below draws `deviation`
    // (data minus baseline) so spikes are visible above a high floor.
    // Both `deviation` and `dev_peak` are precomputed in `Metric::new`.
    let deviation = m.deviation.as_slice();
    let dev_peak = m.dev_peak;

    // One-line stats — absolute values live here since the trend
    // sparkline below is baseline-subtracted. `active X/N` exposes
    // sparse vs sustained patterns at a glance: 8/127 reads as
    // sparse, 127/127 as sustained.
    let stats_line = match m.kind {
        Kind::Time | Kind::Rate => {
            let mut spans = vec![
                Span::styled("min ", theme::label_style()),
                Span::styled(format_value(min, m.kind), theme::value_style()),
                Span::styled("  avg ", theme::label_style()),
                Span::styled(format_value(avg, m.kind), theme::value_style()),
                Span::styled("  peak ", theme::label_style()),
                Span::styled(format_value(max, m.kind), theme::value_style()),
            ];
            if let Some(sub) = &m.subtitle {
                spans.push(Span::styled("  ·  ", theme::label_style()));
                spans.push(Span::styled(sub.clone(), theme::value_style()));
            }
            spans.push(Span::styled(
                format!("  ·  active {active}/{n_buckets} bkts"),
                theme::label_style(),
            ));
            Line::from(spans)
        }
        Kind::Count => {
            let mut spans = vec![
                Span::styled("total ", theme::label_style()),
                Span::styled(commas(total), theme::value_style()),
            ];
            if let Some(sec) = &m.secondary {
                spans.push(Span::styled(
                    format!("  {} ", sec.label),
                    theme::label_style(),
                ));
                spans.push(Span::styled(
                    commas(sec.total),
                    Style::default().fg(sec.color).add_modifier(Modifier::BOLD),
                ));
            }
            spans.extend([
                Span::styled("  peak ", theme::label_style()),
                Span::styled(format_value(max, m.kind), theme::value_style()),
                Span::styled("/bkt  avg ", theme::label_style()),
                Span::styled(format_value(avg, m.kind), theme::value_style()),
                Span::styled(
                    format!("/bkt  ·  active {active}/{n_buckets} bkts"),
                    theme::label_style(),
                ),
            ]);
            Line::from(spans)
        }
    };

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // stats
            Constraint::Min(2),    // trend sparkline (rest of the space)
        ])
        .split(inner);

    frame.render_widget(Paragraph::new(stats_line), parts[0]);

    if let Some(sec) = &m.secondary {
        // Mirror chart: secondary (canonical, red) hangs from the top,
        // primary (vote-skip, yellow) rises from the bottom — like a
        // population pyramid. They share the same x-axis, so the
        // operator can read across to see when the canonical events
        // clustered relative to overall vote-skip activity. Each side
        // scales to its own peak so the small canonical line isn't
        // flattened by the large vote-skip magnitude.
        let mirror = MirrorSparkline {
            top_data: &sec.deviation,
            top_peak: sec.dev_peak,
            top_color: sec.color,
            bottom_data: deviation,
            bottom_peak: dev_peak,
            bottom_color: m.color,
        };
        frame.render_widget(mirror, parts[1]);
    } else {
        // Single-series path: resample to width, baseline-subtracted
        // sparkline using ratatui's built-in widget.
        let resampled = fit_to_width(deviation, parts[1].width as usize);
        let spark_data: &[u64] = if resampled.is_empty() {
            deviation
        } else {
            &resampled
        };
        let spark = Sparkline::default()
            .data(spark_data)
            .max(dev_peak)
            .style(Style::default().fg(m.color));
        frame.render_widget(spark, parts[1]);
    }
}

fn build_cards(b: &TimeBuckets) -> Vec<CardSpec> {
    let (fast_pct, slow_pct) = b.fast_slow_pct();
    let skip = b.skip_count();
    let canon_skip = b.canonical_skip_count();
    let crashed = b.crashed_leader_count();
    let s2n = b.safe_to_notar_count();
    let s2s = b.safe_to_skip_count();
    let our_leader = b.our_leader_slot_count();
    let final_total = b.finalized_total_count();
    let tx_rate = b.tx_per_second();
    // Overall avg signatures per block across the log = Σ signature_sum /
    // Σ blocks observed. Pretty-formatted for the tx-pressure subtitle:
    // `~14k tx/block`. Bucket-size-independent, so a 10m bucket and a
    // 1h bucket read on the same scale (unlike per-bucket counts).
    let total_sigs: u64 = b.buckets.iter().map(|x| x.signature_sum).sum();
    let total_blocks: u64 = b.buckets.iter().map(|x| x.signature_sample_count).sum();
    let avg_tx_per_block = total_sigs.checked_div(total_blocks).unwrap_or(0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lat_ms: Vec<u64> = b
        .lifecycle_p95_us()
        .iter()
        .map(|v| (*v / 1000).max(0) as u64)
        .collect();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let resume_ms: Vec<u64> = b
        .resume_p95_us()
        .iter()
        .map(|v| (*v / 1000).max(0) as u64)
        .collect();

    vec![
        CardSpec::Dual(DualMetric {
            label: "finalize % (fast vs slow)",
            top_color: theme::SPARK_HEALTH,
            top_data: fast_pct,
            // Blue (SPARK_ALT_PATH) — slow finalize is still a successful
            // finalization, just via the 2-round path. Yellow would
            // visually equate it with the actual problem-indicator
            // cards (vote skip, crashed leaders, S2N, S2S).
            bottom_color: theme::SPARK_ALT_PATH,
            bottom_data: slow_pct,
        }),
        // Two stacked sparklines in one card: top = total vote-skip
        // (yellow), bottom = canonical-skip subset (red). Both share
        // the same x-axis but scale independently — total can be
        // 100+/bucket while canonical is 0-2/bucket, so a shared
        // y-scale would flatten the secondary to invisibility.
        // Operator sees WHEN the canonical events fell, not just the
        // aggregate count.
        CardSpec::Single(Metric::with_secondary(
            "vote-skip",
            Kind::Count,
            theme::SPARK_PROBLEM,
            skip,
            Some(("canonical", theme::BAD, canon_skip)),
        )),
        // SafeToNotar and SafeToSkip sit next to each other on the
        // same row of the 2-column card grid — they're sibling
        // safety-threshold events from the consensus pool and read
        // together. `crashed leaders` is bumped one slot down.
        CardSpec::Single(Metric::new(
            "SafeToNotar",
            Kind::Count,
            theme::SPARK_PROBLEM,
            s2n,
        )),
        CardSpec::Single(Metric::new(
            "SafeToSkip",
            Kind::Count,
            theme::SPARK_PROBLEM,
            s2s,
        )),
        CardSpec::Single(Metric::new(
            "crashed leaders",
            Kind::Count,
            theme::SPARK_PROBLEM,
            crashed,
        )),
        CardSpec::Single(Metric::new(
            "leader windows",
            Kind::Count,
            theme::SPARK_HEALTH,
            our_leader,
        )),
        CardSpec::Single(Metric::new(
            "total finalize",
            Kind::Count,
            theme::SPARK_HEALTH,
            final_total,
        )),
        CardSpec::Single(Metric::new(
            "lifecycle p95",
            Kind::Time,
            theme::SPARK_TIME,
            lat_ms,
        )),
        CardSpec::Single(Metric::new(
            "vote-resume p95",
            Kind::Time,
            theme::SPARK_TIME,
            resume_ms,
        )),
        // Tx pressure: signed transactions per second, computed from
        // bank-frozen signature_count. Includes both user txs and vote
        // txs — baseline at ~2 votes per active validator per slot
        // means most of the baseline is votes; spikes above baseline
        // are real user load. SPARK_TIME (cyan) — pressure is a neutral
        // input metric, not a health verdict; the operator reads it
        // alongside skip / latency / crashed-leader cards to spot
        // correlations.
        CardSpec::Single(
            Metric::new("tx pressure", Kind::Rate, theme::SPARK_TIME, tx_rate)
                .with_subtitle(format!("{} tx/block (avg)", commas(avg_tx_per_block))),
        ),
    ]
}

/// In-grid render of the dual fast-vs-slow finalize card. Same
/// dimensions as `render_card` so the x-axis lines up bucket-for-
/// bucket with other cards. Two stacked sparklines (fast top, slow
/// bottom), each baseline-subtracted to its own p10 so variation in
/// each surfaces clearly even when one share dominates. Title stats
/// give the absolute ranges so the deviation chart's magnitude isn't
/// lost.
fn render_dual_card(frame: &mut Frame<'_>, area: Rect, m: &DualMetric) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", m.label))
        .title_style(theme::title_style());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let top_max = m.top_data.iter().copied().max().unwrap_or(0);
    let top_min = m.top_data.iter().copied().min().unwrap_or(0);
    let bot_max = m.bottom_data.iter().copied().max().unwrap_or(0);
    let bot_min = m.bottom_data.iter().copied().min().unwrap_or(0);
    let avg_top = mean(&m.top_data);
    let avg_bot = mean(&m.bottom_data);

    // Stats line shows absolute % values; the stacked-bar chart below
    // shows their per-bucket distribution.
    let stats_line = Line::from(vec![
        Span::styled(
            format!("{avg_top}% fast"),
            Style::default()
                .fg(m.top_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" ({top_min}-{top_max}%)"), theme::label_style()),
        Span::styled("  ·  ", theme::label_style()),
        Span::styled(
            format!("{avg_bot}% slow"),
            Style::default()
                .fg(m.bottom_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" ({bot_min}-{bot_max}%)"), theme::label_style()),
    ]);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // stats
            Constraint::Min(2),    // stacked-bar chart
        ])
        .split(inner);

    frame.render_widget(Paragraph::new(stats_line), parts[0]);

    let chart = StackedBars {
        fast: &m.top_data,
        slow: &m.bottom_data,
        fast_color: m.top_color,
        slow_color: m.bottom_color,
    };
    frame.render_widget(chart, parts[1]);
}

/// Stacked-bar chart: each column is one bucket of `fast` + `slow`
/// percentages summing to 100 (or 0 for empty buckets, which render
/// blank). Y-axis is 0-100% bottom-to-top. `fast` (green) fills from
/// the bottom by its share; `slow` (yellow) fills from the top by
/// its share; they meet at the cluster's per-bucket fast/slow
/// boundary.
///
/// Sub-cell precision via the bottom-N-eighths block characters
/// (`▁▂▃▄▅▆▇`) with `fg = fast_color`, `bg = slow_color` so the
/// foreground (lower portion of the cell) reads as fast and the
/// background (upper portion) reads as slow. 8 sub-cells per cell —
/// even a 3-row chart resolves 24 distinct boundary positions, so
/// shares as small as ~4% surface as a visible band rather than
/// being lost to cell-rounding.
///
/// # Invariants (caller must uphold)
///
/// - `fast[i] + slow[i] == 100` for non-empty buckets;
/// - `fast[i] == 0 && slow[i] == 0` exclusively flags the "empty
///   bucket" case (renders blank). Any other configuration with
///   `fast + slow != 100` will misrepresent the data: the visual
///   boundary is derived from `fast_val` alone, and the upper portion
///   paints `slow_color` regardless of `slow_val`.
///
/// The sole caller (`render_dual_card`) wires the output of
/// Mirror / back-to-back sparkline: two count series share a vertical
/// space. The top series (`top_data`) hangs DOWN from the top edge of
/// the area; the bottom series (`bottom_data`) rises UP from the
/// bottom edge. Each side scales to its own peak so a small-magnitude
/// series (e.g. canonical-skip, peak 1-2/bucket) is not flattened by
/// a large one (vote-skip, peak 200/bucket).
///
/// Layout:
///
/// ```text
///   ▔▔▔▔▔ ← top series fills downward (hanging bars)
///   ▔  ▔
///   ─────  ← center line (always blank)
///   ▁  ▁▁
///   ▁▁▁▁▁ ← bottom series fills upward (rising bars)
/// ```
///
/// Sub-cell precision on the bottom side via the standard `▁▂▃▄▅▆▇█`
/// character set. The top side uses `█` for filled cells and the
/// bottom-anchored character set with `bg = top_color` (a "punch-out"
/// effect) for the boundary cell, yielding the same 1/8 resolution.
struct MirrorSparkline<'a> {
    top_data: &'a [u64],
    top_peak: u64,
    top_color: Color,
    bottom_data: &'a [u64],
    bottom_peak: u64,
    bottom_color: Color,
}

impl Widget for MirrorSparkline<'_> {
    #[allow(clippy::cast_possible_truncation)]
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Bottom-anchored eighths character set — same as ratatui's
        // `Sparkline`. Used directly for the bottom-up side and via
        // the bg/fg trick for the top-down side.
        const BARS: [&str; 9] = ["", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
        if area.width == 0 || area.height == 0 {
            return;
        }

        let h = area.height as usize;
        // Split the area in half. With odd height the bottom side gets
        // the extra row (vote-skip is usually the larger magnitude and
        // benefits from the extra resolution).
        let top_h = h / 2;
        let bottom_h = h - top_h;
        let width = area.width as usize;

        // Resample each series to the chart width so trailing blanks
        // don't appear when bucket count < width.
        let top_resampled = fit_to_width(self.top_data, width);
        let top_data: &[u64] = if top_resampled.is_empty() {
            self.top_data
        } else {
            &top_resampled
        };
        let bottom_resampled = fit_to_width(self.bottom_data, width);
        let bottom_data: &[u64] = if bottom_resampled.is_empty() {
            self.bottom_data
        } else {
            &bottom_resampled
        };

        let top_peak = self.top_peak.max(1);
        let bottom_peak = self.bottom_peak.max(1);

        for col in 0..(width as u16).min(area.width) {
            let col_idx = col as usize;
            let x = area.x + col;
            let top_val = top_data.get(col_idx).copied().unwrap_or(0);
            let bot_val = bottom_data.get(col_idx).copied().unwrap_or(0);

            // ---- Top side: hanging bars from the top edge ----
            if top_h > 0 && top_val > 0 {
                let sub = (u128::from(top_val) * (top_h as u128 * 8)) / u128::from(top_peak);
                let sub = sub.min((top_h * 8) as u128) as usize;
                let full = sub / 8;
                let rem = sub % 8;

                // Full cells filled from the top edge downward
                for r in 0..full {
                    let y = area.y + r as u16;
                    if let Some(cell) = buf.cell_mut(Position { x, y }) {
                        cell.set_symbol(BARS[8]).set_fg(self.top_color);
                    }
                }
                // Boundary cell: there are no top-anchored eighths
                // characters past 1/8 and 4/8, so we use a
                // bottom-anchored char (BARS[empty]) and flip its
                // rendering with REVERSED. With the modifier, the
                // terminal swaps fg/bg when drawing the glyph: the
                // glyph (covering the bottom `empty` sub-cells) paints
                // in the cell's default bg (transparent), and the
                // non-glyph area (the top `rem` sub-cells) paints in
                // `top_color`. Without REVERSED the glyph would render
                // in terminal-default-fg (usually gray), producing
                // visible stripes at the bottom of the bar.
                if rem > 0 && full < top_h {
                    let empty = 8 - rem;
                    let y = area.y + full as u16;
                    if let Some(cell) = buf.cell_mut(Position { x, y }) {
                        cell.set_symbol(BARS[empty]).set_style(
                            Style::default()
                                .fg(self.top_color)
                                .add_modifier(Modifier::REVERSED),
                        );
                    }
                }
            }

            // ---- Bottom side: rising bars from the bottom edge ----
            if bottom_h > 0 && bot_val > 0 {
                let sub = (u128::from(bot_val) * (bottom_h as u128 * 8)) / u128::from(bottom_peak);
                let sub = sub.min((bottom_h * 8) as u128) as usize;
                let full = sub / 8;
                let rem = sub % 8;

                // Full cells filled from the bottom edge upward
                for r in 0..full {
                    let y = area.y + (h as u16) - 1 - r as u16;
                    if let Some(cell) = buf.cell_mut(Position { x, y }) {
                        cell.set_symbol(BARS[8]).set_fg(self.bottom_color);
                    }
                }
                // Boundary cell uses the natural bottom-up eighths
                // character at fg = bottom_color.
                if rem > 0 && full < bottom_h {
                    let y = area.y + (h as u16) - 1 - full as u16;
                    if let Some(cell) = buf.cell_mut(Position { x, y }) {
                        cell.set_symbol(BARS[rem]).set_fg(self.bottom_color);
                    }
                }
            }
        }
    }
}

/// `TimeBuckets::fast_slow_pct` which guarantees both invariants by
/// construction. The two-slice signature is preserved (rather than
/// taking only `fast` and deriving `slow = 100 - fast` internally) so
/// the empty-bucket sentinel `(0, 0)` remains distinguishable from
/// `(0, 100)` ("all-slow"). See audit STRUCT-03.
struct StackedBars<'a> {
    fast: &'a [u64],
    slow: &'a [u64],
    fast_color: Color,
    slow_color: Color,
}

impl Widget for StackedBars<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let n = self.fast.len().min(self.slow.len());
        if n == 0 {
            return;
        }
        let cols = area.width as usize;
        let rows = area.height as usize;
        let total_sub = rows * 8;

        for col_idx in 0..cols {
            let sample_i = col_idx * n / cols;
            if sample_i >= n {
                continue;
            }
            let fast_val = self.fast[sample_i].min(100);
            let slow_val = self.slow[sample_i].min(100);
            if fast_val == 0 && slow_val == 0 {
                // Empty bucket — explicitly blank every cell in the
                // column so a previous frame's filled/straddle state
                // doesn't persist through ratatui's diff path.
                for row_idx in 0..rows {
                    let x = area.x + col_idx as u16;
                    let y = area.y + row_idx as u16;
                    if let Some(cell) = buf.cell_mut(Position { x, y }) {
                        cell.set_symbol(" ");
                        cell.set_fg(Color::Reset);
                        cell.set_bg(Color::Reset);
                    }
                }
                continue;
            }

            // Boundary = where fast (from bottom) ends and slow
            // (from top) begins, counted in sub-cells from bottom.
            // fast_val=100 -> boundary=total_sub (all green);
            // fast_val=0   -> boundary=0          (all yellow).
            let mut boundary = (fast_val as usize * total_sub) / 100;
            // Bump 1-subcell minimum so a small non-zero share is
            // still visible (e.g. fast_val=4% at total_sub=24 would
            // round to 0 — bump it to 1 so the green band shows).
            if fast_val > 0 && boundary == 0 {
                boundary = 1;
            }
            if slow_val > 0 && boundary == total_sub {
                boundary = total_sub - 1;
            }

            for row_idx in 0..rows {
                let row_bot_sub = (rows - 1 - row_idx) * 8; // sub-cell at row's bottom
                let row_top_sub = row_bot_sub + 8; // sub-cell at row's top
                let x = area.x + col_idx as u16;
                let y = area.y + row_idx as u16;
                let Some(cell) = buf.cell_mut(Position { x, y }) else {
                    continue;
                };
                if row_top_sub <= boundary {
                    // Entirely below boundary -> full green. Reset bg
                    // explicitly so a previous frame's straddle-cell
                    // (which set bg = slow_color) doesn't bleed through
                    // ratatui's per-cell diff path.
                    cell.set_symbol("█");
                    cell.set_fg(self.fast_color);
                    cell.set_bg(Color::Reset);
                } else if row_bot_sub >= boundary {
                    // Entirely above boundary -> full yellow. Reset bg
                    // for the same reason as the green branch above.
                    cell.set_symbol("█");
                    cell.set_fg(self.slow_color);
                    cell.set_bg(Color::Reset);
                } else {
                    // Straddles boundary -> partial fill. fg = lower
                    // portion = fast; bg = upper portion = slow.
                    let green_sub_in_cell = boundary - row_bot_sub; // 1..=7
                    let glyph = match green_sub_in_cell {
                        1 => "▁",
                        2 => "▂",
                        3 => "▃",
                        4 => "▄",
                        5 => "▅",
                        6 => "▆",
                        7 => "▇",
                        _ => "█",
                    };
                    cell.set_symbol(glyph);
                    cell.set_fg(self.fast_color);
                    cell.set_bg(self.slow_color);
                }
            }
        }
    }
}

fn format_value(v: u64, kind: Kind) -> String {
    match kind {
        Kind::Count => commas(v),
        Kind::Time => format!("{v} ms"),
        Kind::Rate => format!("{} tx/s", commas(v)),
    }
}

fn humanize_dur(secs: i64) -> String {
    if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn mean(xs: &[u64]) -> u64 {
    if xs.is_empty() {
        0
    } else {
        let sum: u64 = xs.iter().sum();
        sum / xs.len() as u64
    }
}
