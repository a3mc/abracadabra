//! Tab 1: Stats-only overview dashboard.
//!
//! No charts here. Time-series visualisation lives on Tab 2, latency and
//! vote-resume distributions live on the Leader-timeouts tab. This page is
//! the digestible summary: file metadata, headline health verdicts, vote
//! and cert totals, latency percentiles, vote-resume percentiles +
//! severity breakdown, alerts.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::model::alerts::Severity;
use crate::model::analysis;
use crate::model::state::State;
use crate::tui::app::App;
use crate::tui::theme;
use crate::tui::widget::{commas, sanitize_for_tui};

pub fn render(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // file & time metadata (4 lines + borders)
            Constraint::Length(5), // headline health (2-column, 3 rows + borders)
            Constraint::Length(6), // vote / cert totals
            Constraint::Length(7), // latency stages (Table: header + 3 rows + borders)
            Constraint::Length(5), // leader-timeout / vote-resume stats
            Constraint::Min(3),    // alerts summary
        ])
        .split(area);

    render_file_meta(app.state, app.bucket_secs, frame, chunks[0]);
    render_headline_health(app.state, frame, chunks[1]);
    render_vote_cert_totals(app.state, frame, chunks[2]);
    render_lifecycle_stats(app, frame, chunks[3]);
    render_resume_stats(app, frame, chunks[4]);
    render_alerts_summary(app.state, frame, chunks[5]);
}

// ---------- File / time metadata ----------

fn render_file_meta(state: &State, bucket_secs: i64, frame: &mut Frame<'_>, area: Rect) {
    let meta = &state.file_meta;
    let mut lines: Vec<Line<'_>> = Vec::with_capacity(4);

    lines.push(Line::from(vec![
        Span::styled("file       ", theme::label_style()),
        Span::styled(meta.path.display().to_string(), theme::value_style()),
        Span::styled(
            format!(
                "    ({:.2} GB · {} lines)",
                meta.size_bytes as f64 / 1_073_741_824.0,
                commas(meta.line_count),
            ),
            theme::label_style(),
        ),
    ]));

    if let Some((lo, hi)) = meta.time_range {
        let d = hi - lo;
        // Headline duration: cyan accent (not bold) so the most important
        // meta-stat reads clearly without shouting.
        lines.push(Line::from(vec![
            Span::styled("time range ", theme::label_style()),
            Span::styled(lo.to_string(), theme::value_style()),
            Span::styled(" -> ", theme::label_style()),
            Span::styled(hi.to_string(), theme::value_style()),
            Span::styled("    log spans ", theme::label_style()),
            Span::styled(
                format!(
                    "{}h {}m {}s",
                    d.whole_hours(),
                    d.whole_minutes() % 60,
                    d.whole_seconds() % 60,
                ),
                theme::accent_style(),
            ),
        ]));
    }

    // Bucket size — promoted to a sibling row of "time range" so the two
    // time-scale knobs sit together at the top of the block. Value uses
    // `title_style` (bold + cyan) so the actively-chosen bucket is
    // unmistakable; the trailing hint stays gray so it reads as context.
    lines.push(Line::from(vec![
        Span::styled("bucket     ", theme::label_style()),
        Span::styled(fmt_bucket(bucket_secs), theme::title_style()),
        Span::styled(
            "                                   drives tabs 2 & 5 trend charts (--bucket to change)",
            theme::label_style(),
        ),
    ]));

    if let Some((min, max)) = slot_range(state) {
        lines.push(Line::from(vec![
            Span::styled("slots      ", theme::label_style()),
            Span::styled(
                format!("{} -> {}", commas(min), commas(max)),
                theme::value_style(),
            ),
            Span::styled(
                format!("    ({} distinct)", commas(state.slots.len() as u64)),
                theme::label_style(),
            ),
        ]));
    }

    paragraph_in_block(frame, area, " overview ", lines);
}

fn fmt_bucket(secs: i64) -> String {
    if secs >= 3600 && secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs >= 3600 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h {m}m")
    } else if secs >= 60 && secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

// ---------- Headline health ----------
//
// Two-column layout:
//
//   LEFT  — rate-style metrics (a percentage is the headline; count is
//           context in parentheses)
//   RIGHT — event-count metrics (a count is the headline; verdict or
//           breakdown is the context)
//
// Each column rendered as its own `Paragraph` inside a `Layout`
// horizontal split of the outer block's inner rect.

fn render_headline_health(state: &State, frame: &mut Frame<'_>, area: Rect) {
    let ov = &state.overall;
    let total_final = ov.finalized_fast.saturating_add(ov.finalized_slow);
    let fast_pct = if total_final > 0 {
        ov.finalized_fast as f64 * 100.0 / total_final as f64
    } else {
        0.0
    };
    let total_slots = state.slots.len() as u64;
    let skip_pct = if total_slots > 0 {
        ov.votes_skip as f64 * 100.0 / total_slots as f64
    } else {
        0.0
    };

    // 4-slot leader window assumed (Solana standard).
    let leader_windows_total = total_slots / 4;
    let crashed_pct = if leader_windows_total > 0 {
        ov.timeout_crashed_leaders as f64 * 100.0 / leader_windows_total as f64
    } else {
        0.0
    };

    let (fast_style, fast_mark, fast_verdict) = if fast_pct >= theme::FAST_FIN_GOOD_PCT {
        (theme::good_style(), "[✓]", "healthy (>=80%)")
    } else if fast_pct >= theme::FAST_FIN_WARN_PCT {
        (
            theme::warn_style(),
            "[✗]",
            "DEGRADED (60-80%) — slow path active",
        )
    } else {
        (
            theme::bad_style(),
            "[✗]",
            "CRITICAL (<60%) — slow path dominant",
        )
    };
    let skip_style = theme::band_lower_better(
        skip_pct,
        theme::VOTE_SKIP_WARN_PCT,
        theme::VOTE_SKIP_BAD_PCT,
    );

    let outer = Block::default()
        .borders(Borders::ALL)
        .title(" headline health  (tab 3 windowed · tab 6 alerts) ")
        .title_style(theme::title_style());
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    let left_lines = vec![
        Line::from(vec![
            Span::styled(format!("  {:<16}", "fast-finalize"), theme::label_style()),
            Span::styled(
                format!("{fast_pct:>6.2}%"),
                theme::value_style().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(fast_mark, fast_style),
            Span::raw(" "),
            Span::styled(fast_verdict, fast_style),
        ]),
        Line::from(vec![
            Span::styled(format!("  {:<16}", "vote skip rate"), theme::label_style()),
            Span::styled(format!("{skip_pct:>6.2}%"), skip_style),
            Span::styled(
                format!(
                    "  ({} of {} slots)",
                    commas(ov.votes_skip),
                    commas(total_slots)
                ),
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled(format!("  {:<16}", "crashed leaders"), theme::label_style()),
            Span::styled(format!("{crashed_pct:>6.2}%"), theme::value_style()),
            Span::styled(
                format!(
                    "  ({} of {} windows)",
                    commas(ov.timeout_crashed_leaders),
                    commas(leader_windows_total),
                ),
                theme::label_style(),
            ),
        ]),
    ];

    let right_lines = vec![
        Line::from(vec![
            Span::styled(format!("  {:<16}", "fragmentation"), theme::label_style()),
            Span::styled(
                format!(
                    "{:>6}",
                    commas(ov.safe_to_notar.saturating_add(ov.safe_to_skip)),
                ),
                theme::value_style(),
            ),
            Span::styled("  SafeToNotar ", theme::label_style()),
            Span::styled(commas(ov.safe_to_notar), theme::value_style()),
            Span::styled(" · SafeToSkip ", theme::label_style()),
            Span::styled(commas(ov.safe_to_skip), theme::value_style()),
        ]),
        verdict_line(
            "standstills",
            commas(ov.standstill_events),
            ov.standstill_events == 0,
            "no liveness issues",
            "STANDSTILL OBSERVED",
        ),
        verdict_line(
            "refresh votes",
            commas(ov.refreshing_votes),
            ov.refreshing_votes == 0,
            "no standstill recoveries",
            "resume activity",
        ),
    ];

    frame.render_widget(Paragraph::new(left_lines), cols[0]);
    frame.render_widget(Paragraph::new(right_lines), cols[1]);
}

// ---------- Vote / cert totals ----------

fn render_vote_cert_totals(state: &State, frame: &mut Frame<'_>, area: Rect) {
    let ov = &state.overall;
    let total_final = ov.finalized_fast.saturating_add(ov.finalized_slow);
    let fast_pct = if total_final > 0 {
        ov.finalized_fast as f64 * 100.0 / total_final as f64
    } else {
        0.0
    };
    let true_fb = ov
        .block_notar_fallback_count
        .saturating_sub(ov.block_notarized_count);
    let fb_pct = if ov.block_notar_fallback_count > 0 {
        true_fb as f64 * 100.0 / ov.block_notar_fallback_count as f64
    } else {
        0.0
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("Local votes    ", theme::label_style()),
            Span::styled("Notarize ", theme::label_style()),
            Span::styled(commas(ov.votes_notarize), theme::value_style()),
            Span::styled("   Finalize ", theme::label_style()),
            Span::styled(commas(ov.votes_finalize), theme::value_style()),
            Span::styled("   Skip ", theme::label_style()),
            Span::styled(commas(ov.votes_skip), theme::value_style()),
        ]),
        Line::from(vec![
            Span::styled("Cluster certs  ", theme::label_style()),
            Span::styled("Block Notarized ", theme::label_style()),
            Span::styled(commas(ov.block_notarized_count), theme::value_style()),
            Span::styled("   Block notar-fb ", theme::label_style()),
            Span::styled(commas(ov.block_notar_fallback_count), theme::value_style()),
        ]),
        Line::from(vec![
            Span::styled("Finalized      ", theme::label_style()),
            Span::styled(commas(total_final), theme::value_style()),
            Span::styled(
                format!(
                    "   fast {} / slow {} = {:.2}% fast",
                    commas(ov.finalized_fast),
                    commas(ov.finalized_slow),
                    fast_pct,
                ),
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled("True fallbacks ", theme::label_style()),
            Span::styled(commas(true_fb), theme::value_style()),
            Span::styled(
                format!(
                    "   {fb_pct:.3}% — {}",
                    if fb_pct < 0.5 {
                        "rare/healthy"
                    } else {
                        "ELEVATED"
                    },
                ),
                if fb_pct < 0.5 {
                    theme::good_style()
                } else {
                    theme::warn_style()
                },
            ),
        ]),
    ];
    paragraph_in_block(frame, area, " vote & cert totals  (full log) ", lines);
}

// ---------- Latency stage breakdown ----------

fn render_lifecycle_stats(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    // Read pre-computed stages from the snapshot rather than rebuilding
    // and re-sorting three vectors per frame.
    let stages = &app.latency.stages;
    let (a50, a95, a99, _) = analysis::pcts(&stages.assembly);
    let (c50, c95, c99, _) = analysis::pcts(&stages.consensus);
    let (l50, l95, l99, lmax) = app.latency.lifecycle_pcts_us;

    // Render as a small Table so columns align deterministically.
    // Columns: stage(20) | p50(10) | p95(10) | p99(10) | samples(rest) | description
    let header = Row::new(vec![
        Cell::from(""),
        Cell::from(right_align("p50", 10)),
        Cell::from(right_align("p95", 10)),
        Cell::from(right_align("p99", 10)),
        Cell::from(right_align("samples", 12)),
        Cell::from("anchor"),
    ])
    .style(theme::label_style().add_modifier(Modifier::BOLD));

    // p50 = the median; every p50 cell is tinted cyan (gentle accent) so
    // the typical-value column reads as distinct from p95/p99 without
    // shouting. Green stays reserved for fast-finalize and severity bands.
    let p50_style = theme::accent_style();

    let assembly_row = Row::new(vec![
        Cell::from("assembly").style(theme::value_style()),
        Cell::from(right_align_ms(a50, 10)).style(p50_style),
        Cell::from(right_align_ms(a95, 10)),
        Cell::from(right_align_ms(a99, 10)),
        Cell::from(right_align(&commas(stages.assembly.len() as u64), 12))
            .style(theme::label_style()),
        Cell::from("first_shred → block_emitted").style(theme::label_style()),
    ]);

    let consensus_row = Row::new(vec![
        Cell::from("consensus").style(theme::value_style()),
        Cell::from(right_align_ms(c50, 10)).style(p50_style),
        Cell::from(right_align_ms(c95, 10)),
        Cell::from(right_align_ms(c99, 10)),
        Cell::from(right_align(&commas(stages.consensus.len() as u64), 12))
            .style(theme::label_style()),
        Cell::from("block_emitted → finalized").style(theme::label_style()),
    ]);

    let lmax_style = if lmax >= 1_000_000 {
        theme::bad_style()
    } else {
        theme::warn_style()
    };
    let lifecycle_row = Row::new(vec![
        Cell::from("lifecycle").style(theme::title_style()),
        Cell::from(right_align_ms(l50, 10)).style(p50_style),
        Cell::from(right_align_ms(l95, 10)).style(theme::value_style()),
        Cell::from(right_align_ms(l99, 10)).style(theme::value_style()),
        Cell::from(right_align(&commas(stages.lifecycle.len() as u64), 12))
            .style(theme::label_style()),
        Cell::from(format!("max {} ms", lmax / 1000)).style(lmax_style),
    ]);

    let table = Table::new(
        vec![assembly_row, consensus_row, lifecycle_row],
        [
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(
                " latency stages (ms) — full log  (tab 2 trend · tab 3 windowed · tab 4 per-slot) ",
            )
            .title_style(theme::title_style()),
    );
    frame.render_widget(table, area);
}

fn right_align(s: &str, width: usize) -> String {
    format!("{s:>width$}")
}

fn right_align_ms(us: i64, width: usize) -> String {
    let text = format!("{} ms", us / 1000);
    right_align(&text, width)
}

// ---------- Recovery stats ----------

fn render_resume_stats(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    // Read from the cached snapshot rather than re-running
    // `vote_resumes_after_tcl` per frame.
    let total = app.latency.resume_total;
    let (normal, elevated, severe) = app.latency.resume_severity_counts;
    let (p50, p95, p99, max) = app.latency.resume_pcts_us;
    // True hours covered by the log. `None` (or a degenerate hi == lo)
    // collapses to a non-positive value; we skip the rate projection
    // entirely in that case rather than reporting a per-hour figure
    // inflated by clamping the denominator to 1.0. See COR-02 audit.
    let hours = app
        .state
        .file_meta
        .time_range
        .map(|(lo, hi)| (hi - lo).as_seconds_f64() / 3600.0);
    #[allow(clippy::cast_precision_loss)]
    let rate_label = match hours {
        Some(h) if h > 0.0 => format!("{:.1}/h", total as f64 / h),
        _ => "—".to_owned(),
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("events ", theme::label_style()),
            Span::styled(commas(total), theme::value_style()),
            Span::styled("   rate ", theme::label_style()),
            Span::styled(rate_label, theme::value_style()),
            Span::styled(
                "   TimeoutCrashedLeader -> next Voting notarize",
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled("p50 ", theme::label_style()),
            Span::styled(fmt_s(p50), theme::accent_style()),
            Span::styled("   p95 ", theme::label_style()),
            Span::styled(fmt_s(p95), theme::value_style()),
            Span::styled("   p99 ", theme::label_style()),
            Span::styled(fmt_s(p99), theme::value_style()),
            Span::styled("   max ", theme::label_style()),
            Span::styled(fmt_s(max), theme::bad_style()),
        ]),
        Line::from(vec![
            Span::styled("severity   ", theme::label_style()),
            Span::styled(
                format!("normal {}", pct_count(normal, total)),
                theme::good_style(),
            ),
            Span::styled("   ", theme::label_style()),
            Span::styled(
                format!("elevated {}", pct_count(elevated, total)),
                theme::warn_style(),
            ),
            Span::styled("   ", theme::label_style()),
            Span::styled(
                format!("severe {}", pct_count(severe, total)),
                theme::bad_style(),
            ),
        ]),
    ];
    paragraph_in_block(
        frame,
        area,
        " leader timeouts — vote-resume time  (tab 5 distribution + incidents) ",
        lines,
    );
}

// ---------- Alerts summary ----------

fn render_alerts_summary(state: &State, frame: &mut Frame<'_>, area: Rect) {
    if state.alerts.is_empty() {
        let line = Line::from(vec![Span::styled("(no alerts)", theme::good_style())]);
        paragraph_in_block(frame, area, " alerts ", vec![line]);
        return;
    }
    let lines: Vec<Line<'_>> = state
        .alerts
        .iter()
        .map(|a| {
            let (tag, style) = match a.severity {
                Severity::Info => ("[INFO]", theme::label_style()),
                Severity::Warn => ("[WARN]", theme::warn_style()),
                Severity::Critical => ("[CRIT]", theme::bad_style()),
            };
            Line::from(vec![
                Span::styled(tag, style),
                Span::raw(" "),
                // Alert descriptions can include log-derived bodies
                // (LogPattern groups embed the sample) — strip control
                // bytes before they reach the terminal.
                Span::raw(sanitize_for_tui(&a.description).into_owned()),
            ])
        })
        .collect();
    paragraph_in_block(
        frame,
        area,
        &format!(" alerts ({}) ", state.alerts.len()),
        lines,
    );
}

// ---------- shared helpers ----------

fn paragraph_in_block(frame: &mut Frame<'_>, area: Rect, title: &str, lines: Vec<Line<'_>>) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_owned())
        .title_style(theme::title_style());
    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, area);
}

fn verdict_line<'a>(
    label: &'a str,
    value: String,
    healthy: bool,
    good: &'a str,
    bad: &'a str,
) -> Line<'a> {
    let mark_style = if healthy {
        theme::good_style()
    } else {
        theme::bad_style()
    };
    let mark = if healthy { "[✓]" } else { "[✗]" };
    // 16-wide label / 6-wide value mirrors the column widths used in
    // render_headline_health so the right-column rows line up
    // vertically with the fragmentation row above them.
    Line::from(vec![
        Span::styled(format!("  {label:<16}"), theme::label_style()),
        Span::styled(format!("{value:>6}"), theme::value_style()),
        Span::raw("  "),
        Span::styled(mark, mark_style),
        Span::raw(" "),
        Span::styled(if healthy { good } else { bad }, mark_style),
    ])
}

fn slot_range(state: &State) -> Option<(u64, u64)> {
    let min = state.slots.keys().next()?;
    let max = state.slots.keys().next_back()?;
    Some((*min, *max))
}

fn fmt_s(us: i64) -> String {
    format!("{:>5.2} s", us as f64 / 1_000_000.0)
}

fn pct_count(n: u64, total: u64) -> String {
    if total == 0 {
        "0 (0.0%)".to_owned()
    } else {
        let pct = n as f64 * 100.0 / total as f64;
        format!("{} ({pct:.1}%)", commas(n))
    }
}
