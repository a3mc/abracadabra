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

#[derive(Debug, Clone, Copy)]
enum Kind {
    /// Per-bucket count (events). Stats line shows total / peak / avg.
    Count,
    /// Per-bucket time value in milliseconds. Stats line shows min /
    /// avg / max.
    Time,
}

struct Metric {
    label: &'static str,
    kind: Kind,
    color: Color,
    data: Vec<u64>,
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

    // Baseline = p10. The trailing sparkline below subtracts this so
    // spikes become visible.
    let mut sorted = m.data.clone();
    sorted.sort_unstable();
    let baseline = sorted.get(sorted.len() / 10).copied().unwrap_or(0);
    let deviation: Vec<u64> = m.data.iter().map(|v| v.saturating_sub(baseline)).collect();
    let dev_peak = deviation.iter().copied().max().unwrap_or(1).max(1);

    // One-line stats — absolute values live here since the trend
    // sparkline below is baseline-subtracted. `active X/N` exposes
    // sparse vs sustained patterns at a glance: 8/127 reads as
    // sparse, 127/127 as sustained.
    let stats_line = match m.kind {
        Kind::Time => Line::from(vec![
            Span::styled("min ", theme::label_style()),
            Span::styled(format_value(min, m.kind), theme::value_style()),
            Span::styled("  avg ", theme::label_style()),
            Span::styled(format_value(avg, m.kind), theme::value_style()),
            Span::styled("  max ", theme::label_style()),
            Span::styled(format_value(max, m.kind), theme::value_style()),
            Span::styled(
                format!("  ·  active {active}/{n_buckets} bkts"),
                theme::label_style(),
            ),
        ]),
        Kind::Count => Line::from(vec![
            Span::styled("total ", theme::label_style()),
            Span::styled(commas(total), theme::value_style()),
            Span::styled("  peak ", theme::label_style()),
            Span::styled(format_value(max, m.kind), theme::value_style()),
            Span::styled("/bkt  avg ", theme::label_style()),
            Span::styled(format_value(avg, m.kind), theme::value_style()),
            Span::styled(
                format!("/bkt  ·  active {active}/{n_buckets} bkts"),
                theme::label_style(),
            ),
        ]),
    };

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // stats
            Constraint::Min(2),    // trend sparkline (rest of the space)
        ])
        .split(inner);

    frame.render_widget(Paragraph::new(stats_line), parts[0]);

    let spark = Sparkline::default()
        .data(&deviation)
        .max(dev_peak)
        .style(Style::default().fg(m.color));
    frame.render_widget(spark, parts[1]);
}

fn build_cards(b: &TimeBuckets) -> Vec<CardSpec> {
    let (fast_pct, slow_pct) = b.fast_slow_pct();
    let skip = b.skip_count();
    let crashed = b.crashed_leader_count();
    let s2n = b.safe_to_notar_count();
    let s2s = b.safe_to_skip_count();
    let our_leader = b.our_leader_slot_count();
    let final_total = b.finalized_total_count();
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
        CardSpec::Single(Metric {
            label: "vote skip",
            kind: Kind::Count,
            color: theme::SPARK_PROBLEM,
            data: skip,
        }),
        CardSpec::Single(Metric {
            label: "crashed leaders",
            kind: Kind::Count,
            color: theme::SPARK_PROBLEM,
            data: crashed,
        }),
        CardSpec::Single(Metric {
            label: "SafeToNotar",
            kind: Kind::Count,
            color: theme::SPARK_PROBLEM,
            data: s2n,
        }),
        CardSpec::Single(Metric {
            label: "SafeToSkip",
            kind: Kind::Count,
            color: theme::SPARK_PROBLEM,
            data: s2s,
        }),
        CardSpec::Single(Metric {
            label: "leader windows",
            kind: Kind::Count,
            color: theme::SPARK_HEALTH,
            data: our_leader,
        }),
        CardSpec::Single(Metric {
            label: "total finalize",
            kind: Kind::Count,
            color: theme::SPARK_HEALTH,
            data: final_total,
        }),
        CardSpec::Single(Metric {
            label: "lifecycle p95",
            kind: Kind::Time,
            color: theme::SPARK_TIME,
            data: lat_ms,
        }),
        CardSpec::Single(Metric {
            label: "vote-resume p95",
            kind: Kind::Time,
            color: theme::SPARK_TIME,
            data: resume_ms,
        }),
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
                // Empty bucket — leave the column blank.
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
                    // Entirely below boundary -> full green.
                    cell.set_symbol("█");
                    cell.set_fg(self.fast_color);
                } else if row_bot_sub >= boundary {
                    // Entirely above boundary -> full yellow.
                    cell.set_symbol("█");
                    cell.set_fg(self.slow_color);
                } else {
                    // Straddles boundary -> partial fill.
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

fn commas(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}
