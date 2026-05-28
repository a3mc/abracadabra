//! Tab 3: Crashed-leader recoveries analytical dashboard.
//!
//! Layout:
//! ```text
//! ┌─ stats: events, percentiles, severity breakdown, rate ─┐
//! ├─ distribution histogram (recovery time buckets) ───────┤
//! ├─ recoveries-per-time-bucket sparkline ─────────────────┤
//! └─ scrollable list of incidents (longest first) ────────┘
//! ```

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Bar, BarChart, BarGroup, Block, Borders, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use crate::model::analysis;
use crate::tui::app::App;
use crate::tui::theme;
use crate::tui::view::VoteResumeViewRow;
use crate::tui::widget::{commas, fit_to_width, hbar, kpi};

const DIST_BUCKETS_S: &[(f64, f64)] = &[
    (0.0, 0.5),
    (0.5, 1.0),
    (1.0, 1.5),
    (1.5, 2.0),
    (2.0, 3.0),
    (3.0, 4.0),
    (4.0, 5.0),
    (5.0, f64::INFINITY),
];

pub fn render(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // stats KPIs
            Constraint::Length(11), // distribution histogram
            Constraint::Length(8),  // per-bucket trend (was 6; 2 extra
                                    // rows give the bar area 4 rows
                                    // instead of 2 — visible spikes
                                    // even on a baseline-flat series)
            Constraint::Min(8),     // list + band reference (split horizontally)
        ])
        .split(area);

    render_kpi(app, frame, chunks[0]);
    render_distribution(app, frame, chunks[1]);
    render_trend(app, frame, chunks[2]);

    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(60), // list
            Constraint::Percentage(40), // band reference
        ])
        .split(chunks[3]);
    render_list(app, frame, bottom[0]);
    render_band_reference(app, frame, bottom[1]);
}

fn render_band_reference(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    // Read percentiles + severity counts from the cached snapshot
    // instead of recomputing `vote_resumes_after_tcl` per frame.
    let total = app.latency.resume_total;
    let (normal, elevated, severe) = app.latency.resume_severity_counts;
    let (p50, p95, p99, max) = app.latency.resume_pcts_us;

    // Style for glossary terms — bold white, distinct from both the
    // dim label-grey of explanation text and the bold-cyan of section
    // titles. The visual hierarchy is title > term > explanation.
    let term_style = theme::value_style().add_modifier(Modifier::BOLD);

    let lines = vec![
        section_title("Severity bands"),
        // Single line per band: colored label + threshold + count
        // + percentage + short interpretation. Wraps cleanly via the
        // Paragraph's Wrap config on narrow terminals.
        Line::from(vec![
            Span::styled("  NORMAL   ", theme::good_style()),
            Span::styled("< 1.5 s    ", theme::label_style()),
            Span::styled(commas(normal), theme::value_style()),
            Span::styled(
                format!(" ({:.1}%)  — majority of events on healthy clusters", pct(normal, total)),
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled("  ELEVATED ", theme::warn_style()),
            Span::styled("1.5 – 3.0 s ", theme::label_style()),
            Span::styled(commas(elevated), theme::value_style()),
            Span::styled(
                format!(" ({:.1}%)  — slow recovery / next-leader delay", pct(elevated, total)),
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled("  SEVERE   ", theme::bad_style()),
            Span::styled("≥ 3.0 s    ", theme::label_style()),
            Span::styled(commas(severe), theme::value_style()),
            Span::styled(
                format!(" ({:.1}%)  — multi-window outage or stretched-timeout standstill", pct(severe, total)),
                theme::label_style(),
            ),
        ]),
        Line::raw(""),
        section_title("Observed percentiles"),
        kv_time_highlight("  median", p50),
        kv_time("  p95   ", p95),
        kv_time("  p99   ", p99),
        kv_time("  max   ", max),
        Line::raw(""),
        // Glossary anchors at the bottom — read the incidents table
        // on the left, then look down-right for column meanings.
        // One-line-per-term keeps the section ~7 rows tall so it
        // survives narrow / zoomed terminals without overflowing.
        // The Paragraph's Wrap config reflows any line that exceeds
        // the panel's actual width.
        section_title("What this measures"),
        Line::from(vec![
            Span::styled("  leader timeout", term_style),
            Span::styled(
                "  TCL event — timer fires ~1.4 s into window if no shreds arrive.",
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled("  resume time   ", term_style),
            Span::styled(
                "  wall-clock TCL → next Voting notarize (list sorted by this).",
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled("  slot gap      ", term_style),
            Span::styled(
                "  slots elapsed from TCL until we cast next Voting notarize.",
                theme::label_style(),
            ),
        ]),
    ];

    // Render the outer block + title across the full panel area, then
    // render the Paragraph into a padded inner rect: 1 row top
    // padding, 3 cols left padding, 2 cols right padding. Stops the
    // "Severity bands" title from butting against the top border and
    // gives every line some breathing room on both sides so it
    // matches the other widgets' visual rhythm.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" band reference ")
        .title_style(theme::title_style());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let padded = Rect {
        x: inner.x.saturating_add(3),
        y: inner.y.saturating_add(1),
        width: inner.width.saturating_sub(5),
        height: inner.height.saturating_sub(1),
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), padded);
}

fn section_title(s: &str) -> Line<'_> {
    Line::from(Span::styled(
        s.to_owned(),
        theme::title_style().add_modifier(Modifier::BOLD),
    ))
}

fn kv_time(label: &str, us: i64) -> Line<'_> {
    Line::from(vec![
        Span::styled(label.to_owned(), theme::label_style()),
        Span::raw(" "),
        Span::styled(
            format!("{:>6.2} s", us as f64 / 1_000_000.0),
            theme::value_style(),
        ),
    ])
}

/// Same as `kv_time` but tints the value cyan (gentle accent) so the
/// median row reads as the headline without competing with the green
/// severity-band markers above it.
fn kv_time_highlight(label: &str, us: i64) -> Line<'_> {
    Line::from(vec![
        Span::styled(label.to_owned(), theme::label_style()),
        Span::raw(" "),
        Span::styled(
            format!("{:>6.2} s", us as f64 / 1_000_000.0),
            theme::accent_style(),
        ),
    ])
}

fn pct(n: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        n as f64 * 100.0 / total as f64
    }
}

fn render_kpi(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    // Naming: "leader-timeout event" = a `TimeoutCrashedLeader` log line.
    // "vote-resume time" = how long until we cast the next Voting notarize.
    // (NOT Solana shred recovery — that's a different mechanism.)
    // Read from the cached snapshot (LatencySnapshot computed once in App::new).
    let total = app.latency.resume_total;
    let (normal, elevated, severe) = app.latency.resume_severity_counts;
    let (p50, p95, p99, max) = app.latency.resume_pcts_us;

    // Compute real elapsed hours. When the log carries no time range or
    // collapses to a single instant (hi == lo), skip the rate projection
    // and emit a dash placeholder instead of inflating with a clamped
    // 1.0 denominator. See COR-02 audit.
    let hours = app
        .state
        .file_meta
        .time_range
        .map(|(lo, hi)| (hi - lo).as_seconds_f64() / 3600.0);
    #[allow(clippy::cast_precision_loss)]
    let rate_label = match hours {
        Some(h) if h > 0.0 => format!("{:.1}", total as f64 / h),
        _ => "—".to_owned(),
    };

    let kpis = vec![
        kpi::Kpi::new(
            "TCL events",
            commas(total),
            theme::value_style().add_modifier(Modifier::BOLD),
        ),
        kpi::Kpi::new("resume p50", fmt_s(p50), theme::accent_style()),
        kpi::Kpi::new("p95", fmt_s(p95), theme::value_style()),
        kpi::Kpi::new("p99", fmt_s(p99), theme::value_style()),
        kpi::Kpi::new("max", fmt_s(max), theme::bad_style()),
        kpi::Kpi::new("rate/h", rate_label, theme::value_style()),
        kpi::Kpi::new("normal", pct_count(normal, total), theme::good_style()),
        kpi::Kpi::new("elevated", pct_count(elevated, total), theme::warn_style()),
        kpi::Kpi::new("severe", pct_count(severe, total), theme::bad_style()),
    ];
    kpi::render(
        frame,
        area,
        " leader-timeout events / vote-resume times ",
        &kpis,
    );
}

fn render_distribution(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    // Walk the pre-sorted resume vector (cached). Linear scan over the
    // bucket table is cheap; the previous per-frame scan re-built the
    // resume vector from scratch.
    let mut counts = vec![0u64; DIST_BUCKETS_S.len()];
    for us in &app.latency.resume_us_sorted {
        #[allow(clippy::cast_precision_loss)]
        let s = *us as f64 / 1_000_000.0;
        if let Some(idx) = DIST_BUCKETS_S
            .iter()
            .position(|(lo, hi)| s >= *lo && s < *hi)
        {
            counts[idx] = counts[idx].saturating_add(1);
        }
    }
    let labels: Vec<String> = DIST_BUCKETS_S
        .iter()
        .map(|(lo, hi)| {
            if hi.is_infinite() {
                format!(">{lo:>3.1}s")
            } else {
                format!("{lo:>3.1}-{hi:>3.1}s")
            }
        })
        .collect();
    let total: u64 = counts.iter().sum();
    let buckets: Vec<hbar::Bucket<'_>> = labels
        .iter()
        .zip(counts.iter())
        .enumerate()
        .map(|(i, (l, c))| {
            let color: Color = if i >= 5 {
                theme::BAD
            } else if i >= 3 {
                theme::WARN
            } else {
                theme::OK
            };
            hbar::Bucket {
                label: l.as_str(),
                count: *c,
                color,
            }
        })
        .collect();
    hbar::render(
        frame,
        area,
        " vote-resume time distribution — how long until we cast Voting notarize after a leader timeout ",
        &buckets,
        total,
    );
}

fn render_trend(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    let title = app.buckets.map_or_else(
        || " leader-timeout events over time (no data) ".to_owned(),
        |b| {
            format!(
                " leader-timeout events over time — {} buckets at {} each (bars show deviation above baseline) ",
                b.buckets.len(),
                humanize_dur(b.bucket_size.whole_seconds()),
            )
        },
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(theme::title_style());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(b) = app.buckets else { return };

    let counts: Vec<u64> = b.buckets.iter().map(|x| x.resume_us.len() as u64).collect();
    let total: u64 = counts.iter().sum();
    let peak = counts.iter().copied().max().unwrap_or(0);
    let min_v = counts.iter().copied().min().unwrap_or(0);
    let avg = if counts.is_empty() {
        0.0
    } else {
        total as f64 / counts.len() as f64
    };
    let nonzero = counts.iter().filter(|c| **c > 0).count() as u64;
    let coverage_pct = if counts.is_empty() {
        0.0
    } else {
        nonzero as f64 * 100.0 / counts.len() as f64
    };

    // Baseline = 10th percentile. Subtract from each bar so the chart shows
    // DEVIATION above typical rather than absolute counts. Otherwise the
    // chart's lower half is dead air and the spikes get squashed into ~30%
    // of the panel height.
    let mut sorted = counts.clone();
    sorted.sort_unstable();
    let baseline: u64 = if sorted.is_empty() {
        0
    } else {
        sorted[sorted.len() / 10]
    };
    let deviation: Vec<u64> = counts.iter().map(|c| c.saturating_sub(baseline)).collect();
    let dev_peak = deviation.iter().copied().max().unwrap_or(0);

    // Layout: stats line | bar chart | time axis labels
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // stats
            Constraint::Min(2),    // bars
            Constraint::Length(1), // time axis
        ])
        .split(inner);

    // Stats line: real absolute values, plus the baseline note.
    let stats = Line::from(vec![
        Span::styled("events/bucket  ", theme::label_style()),
        Span::styled("peak ", theme::label_style()),
        Span::styled(commas(peak), theme::bad_style()),
        Span::styled("  avg ", theme::label_style()),
        Span::styled(format!("{avg:.1}"), theme::value_style()),
        Span::styled("  min ", theme::label_style()),
        Span::styled(commas(min_v), theme::value_style()),
        Span::styled("  active ", theme::label_style()),
        Span::styled(
            format!("{}/{} ({coverage_pct:.0}%)", nonzero, counts.len()),
            theme::value_style(),
        ),
        Span::styled("  total ", theme::label_style()),
        Span::styled(commas(total), theme::value_style()),
        Span::styled("  | chart baseline (p10) ", theme::label_style()),
        Span::styled(commas(baseline), theme::value_style()),
    ]);
    frame.render_widget(Paragraph::new(stats), rows[0]);

    // BarChart with visual gap between bars. Each Bar gets text_value("")
    // so no numeric label rides on top of the bar (those labels overlap the
    // stats line at large values, which is the "covering the legend" issue).
    // Downsample bucket count to panel_width / 3 so each bar has room for a
    // 2-col width + 1-col gap.
    let panel_w = rows[1].width as usize;
    let max_bars = (panel_w / 3).max(8);
    let display = fit_to_width(&deviation, max_bars.min(deviation.len()));
    let bars: Vec<Bar<'_>> = display
        .iter()
        .map(|v| Bar::default().value(*v).text_value(String::new()))
        .collect();
    let group = BarGroup::default().bars(&bars);
    let chart = BarChart::default()
        .data(group)
        .bar_width(2)
        .bar_gap(1)
        .max(dev_peak.max(1))
        .bar_style(Style::default().fg(theme::SPARK_PROBLEM));
    frame.render_widget(chart, rows[1]);

    // Time axis: shows the wall-clock anchors for left-most and right-most bars.
    let (start_label, end_label) = time_anchors(b);
    let axis = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[2]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("← {start_label}"),
            theme::label_style(),
        ))),
        axis[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{end_label} →"),
            theme::label_style(),
        )))
        .alignment(ratatui::layout::Alignment::Right),
        axis[1],
    );
}

fn time_anchors(b: &crate::model::buckets::TimeBuckets) -> (String, String) {
    // `BucketStats::start` is `Option<_>` only because `BucketStats`
    // implements `Default` — every bucket built by `TimeBuckets::from_state`
    // populates it with `Some(lo + offset)`. The fallback path here
    // exists so a degenerate "no buckets" input doesn't surface a
    // dangling `← ` arrow with nothing after it.
    let first = b.buckets.first().and_then(|x| x.start);
    let last = b
        .buckets
        .last()
        .and_then(|x| x.start)
        .map(|t| t + b.bucket_size);
    let fmt = |t: Option<time::OffsetDateTime>| -> String {
        t.map_or_else(
            || "(no data)".to_owned(),
            |ts| {
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}",
                    ts.year(),
                    u8::from(ts.month()),
                    ts.day(),
                    ts.hour(),
                    ts.minute(),
                )
            },
        )
    };
    (fmt(first), fmt(last))
}

fn render_list(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    let total = app.resume_rows.len();
    // Mirror `panel::slots::render_table`: `.max(1)` keeps at least one
    // row visible even when the rect is so tight that subtracting the
    // border + header reservation drops to 0.
    let visible = (area.height.saturating_sub(3) as usize).max(1);
    let start = app.resume_scroll.min(total.saturating_sub(visible));
    let end = (start + visible).min(total);
    let window = &app.resume_rows[start..end];

    let header = Row::new(vec![
        "TCL slot",
        "resume slot",
        "slot gap",
        "resume time",
        "severity",
    ])
    .style(theme::label_style().add_modifier(Modifier::BOLD));

    let rows: Vec<Row<'_>> = window.iter().map(row_for).collect();

    let title = format!(
        " leader-timeout incidents (sorted by longest vote-resume | cursor {} / {}) ",
        commas(app.resume_scroll as u64 + 1),
        commas(total as u64),
    );
    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Min(12), // severity — fills remaining width
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(title))
    .style(Style::default().fg(theme::FG));
    frame.render_widget(table, area);
}

fn row_for(r: &VoteResumeViewRow) -> Row<'_> {
    let sev = analysis::Severity::from_us(r.resume_us);
    let (sev_label, sev_style) = match sev {
        analysis::Severity::Severe => ("severe", theme::bad_style()),
        analysis::Severity::Elevated => ("elevated", theme::warn_style()),
        analysis::Severity::Normal => ("normal", theme::good_style()),
    };
    Row::new(vec![
        Line::from(Span::styled(commas(r.tcl_slot), theme::value_style())),
        Line::from(Span::styled(commas(r.resume_slot), theme::value_style())),
        Line::from(Span::styled(
            format!("+{}", r.slot_gap),
            theme::warn_style(),
        )),
        Line::from(Span::styled(
            format!("{:>6.2} s", r.resume_us as f64 / 1_000_000.0),
            sev_style,
        )),
        Line::from(Span::styled(sev_label, sev_style)),
    ])
}

fn fmt_s(us: i64) -> String {
    format!("{:.2}s", us as f64 / 1_000_000.0)
}

fn pct_count(n: u64, total: u64) -> String {
    if total == 0 {
        "0 (0.0%)".to_owned()
    } else {
        let pct = n as f64 * 100.0 / total as f64;
        format!("{} ({pct:.1}%)", commas(n))
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
