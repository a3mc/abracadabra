//! Tab 4: per-slot stats strip + dense scrollable table with selection.
//!
//! Layout:
//! ```text
//! ┌─ slot stats: pipe-separated KPI lines ─────────────┐
//! ├─ slots table (60%) ─────────┬─ reference & legend ─┤
//! │  scrollable, cursor at top  │  thresholds + legend │
//! └─────────────────────────────┴──────────────────────┘
//! ```

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::model::analysis;
use crate::model::slot::SlotStatus;
use crate::tui::app::{App, SlotFilters};
use crate::tui::theme;
use crate::tui::view::SlotViewRow;

pub fn render(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // KPI strip (full width)
            Constraint::Min(10),   // table + reference split
        ])
        .split(area);

    render_kpi(app, frame, chunks[0]);

    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(60), // table
            Constraint::Percentage(40), // reference
        ])
        .split(chunks[1]);
    render_table(app, frame, bottom[0]);
    render_reference(app, frame, bottom[1]);
}

// ---------- KPI strip ----------

fn render_kpi(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    let state = app.state;
    let ov = &state.overall;
    let total = state.slots.len() as u64;
    let fin_fast = ov.finalized_fast;
    let fin_slow = ov.finalized_slow;
    let fin = fin_fast.saturating_add(fin_slow);
    let skip = ov.votes_skip;
    let pend = total.saturating_sub(fin).saturating_sub(skip);
    let leader: u64 = state.slots.values().filter(|s| s.we_are_leader).count() as u64;

    let fin_pct = pct(fin, total);
    let fast_share = pct(fin_fast, fin);
    let skip_pct = pct(skip, total);
    let fin_style = theme::band_higher_better(fin_pct, theme::FIN_GOOD_PCT, theme::FIN_WARN_PCT);
    let skip_style = theme::band_lower_better(
        skip_pct,
        theme::VOTE_SKIP_WARN_PCT,
        theme::VOTE_SKIP_BAD_PCT,
    );

    let lats = analysis::lifecycle_latencies(state);
    let mut us: Vec<i64> = lats.iter().map(|r| r.us).collect();
    us.sort_unstable();
    let p50 = analysis::percentile(&us, 0.50).unwrap_or(0) / 1000;
    let p95 = analysis::percentile(&us, 0.95).unwrap_or(0) / 1000;
    let p99 = analysis::percentile(&us, 0.99).unwrap_or(0) / 1000;
    let max_ms = us.last().copied().unwrap_or(0) / 1000;
    let max_style = if max_ms >= 1000 {
        theme::bad_style()
    } else {
        theme::warn_style()
    };

    let pipe = || Span::styled("  |  ", theme::label_style());

    // Line 1 — dataset identity & outcome split.
    let line1 = Line::from(vec![
        Span::styled("slots ", theme::label_style()),
        Span::styled(commas(total), theme::value_style()),
        pipe(),
        Span::styled("leader ", theme::label_style()),
        Span::styled(commas(leader), theme::value_style()),
        Span::styled(
            format!(" ({:.2}%)", pct(leader, total)),
            theme::label_style(),
        ),
        pipe(),
        Span::styled("FIN ", theme::label_style()),
        Span::styled(format!("{fin_pct:.1}%"), fin_style),
        Span::styled(
            format!(" (fast {fast_share:.0}% of FIN)"),
            theme::label_style(),
        ),
        pipe(),
        Span::styled("SKIP ", theme::label_style()),
        Span::styled(format!("{skip_pct:.1}%"), skip_style),
        pipe(),
        Span::styled("PEND ", theme::label_style()),
        Span::styled(
            commas(pend),
            if pend == 0 {
                theme::good_style()
            } else {
                theme::warn_style()
            },
        ),
    ]);

    // Line 2 — lifecycle latency percentiles. p50 in accent so the
    // headline reads first; tails neutral; max coloured by health band.
    let line2 = Line::from(vec![
        Span::styled("lifecycle ", theme::label_style()),
        Span::styled("p50 ", theme::label_style()),
        Span::styled(format!("{p50} ms"), theme::accent_style()),
        pipe(),
        Span::styled("p95 ", theme::label_style()),
        Span::styled(format!("{p95} ms"), theme::value_style()),
        pipe(),
        Span::styled("p99 ", theme::label_style()),
        Span::styled(format!("{p99} ms"), theme::value_style()),
        pipe(),
        Span::styled("max ", theme::label_style()),
        Span::styled(format!("{max_ms} ms"), max_style),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" slot stats ")
        .title_style(theme::title_style());
    frame.render_widget(Paragraph::new(vec![line1, line2]).block(block), area);
}

// ---------- Reference & legend panel ----------

fn render_reference(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(" reference & legend ")
        .title_style(theme::title_style());
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    // Three sub-sections distributed vertically with proportional
    // Fill-gaps between them. On taller panels the extra height is
    // shared equally across the three gaps instead of accumulating at
    // the bottom — sections "float" with breathing room rather than
    // clustering at the top. Wrap on every Paragraph keeps content
    // readable on narrow / zoomed viewports.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),  // latency content (1 title + 3 bands + 1 p95)
            Constraint::Fill(1),    // gap
            Constraint::Length(10), // legend content (1 title + 9 entries)
            Constraint::Fill(1),    // gap
            Constraint::Length(2),  // validator content (1 title + 1 line)
            Constraint::Fill(1),    // bottom gap
        ])
        .split(inner);

    render_latency_reference(app, frame, chunks[0]);
    render_legend(app.slot_filters, frame, chunks[2]);
    render_validator_info(app, frame, chunks[4]);
}

fn render_latency_reference(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    let lats = analysis::lifecycle_latencies(app.state);
    let mut us: Vec<i64> = lats.iter().map(|r| r.us).collect();
    us.sort_unstable();
    let p95 = analysis::percentile(&us, 0.95).unwrap_or(0) / 1000;
    let p95_healthy = p95 <= theme::LIFECYCLE_WARN_MS as i64;

    // Per-stage bands shown on a single line each so both columns are
    // visible side-by-side. Threshold values are styled with the
    // corresponding good/warn/bad colour so the colour↔number mapping
    // is unambiguous.
    let lines = vec![
        section_title("Latency bands  (table colour, by stage)"),
        Line::from(vec![
            Span::styled("  assembly  ", theme::label_style()),
            Span::styled("≤ 500", theme::good_style()),
            Span::styled("  ·  ", theme::label_style()),
            Span::styled("500–600", theme::warn_style()),
            Span::styled("  ·  ", theme::label_style()),
            Span::styled("> 600", theme::bad_style()),
            Span::styled(" ms", theme::label_style()),
        ]),
        Line::from(vec![
            Span::styled("  lifecycle ", theme::label_style()),
            Span::styled("≤ 600", theme::good_style()),
            Span::styled("  ·  ", theme::label_style()),
            Span::styled("600–1000", theme::warn_style()),
            Span::styled("  ·  ", theme::label_style()),
            Span::styled("> 1000", theme::bad_style()),
            Span::styled(" ms", theme::label_style()),
        ]),
        Line::from(vec![
            Span::styled("  p95 (lifecycle)  ", theme::label_style()),
            Span::styled(
                format!("{p95} ms"),
                if p95_healthy {
                    theme::good_style()
                } else {
                    theme::warn_style()
                },
            ),
            Span::raw(" "),
            Span::styled(
                if p95_healthy { "[✓]" } else { "[✗]" },
                if p95_healthy {
                    theme::good_style()
                } else {
                    theme::warn_style()
                },
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_legend(filters: SlotFilters, frame: &mut Frame<'_>, area: Rect) {
    // Tag indent so multi-line subgroups (path, events) line up neatly
    // under their column label.
    const TAG_INDENT: &str = "          "; // 10 spaces = "  status  " width

    // Rows tied to a filter key prepend a `[✓]` / `[ ]` marker (cyan
    // when on, gray when off) so the legend doubles as filter state.
    let mark = |on: bool| -> Span<'static> {
        if on {
            Span::styled("[✓] ", theme::accent_style())
        } else {
            Span::styled("[ ] ", theme::label_style())
        }
    };

    let lines = vec![
        section_title("Column legend  (table columns + event tags · keys toggle filters)"),
        // status — three states inline + tickable filter row for SKIP
        Line::from(vec![
            Span::styled("  status  ", theme::label_style()),
            Span::styled("FIN", theme::good_style()),
            Span::styled(" finalized   ", theme::label_style()),
            Span::styled("SKIP", theme::warn_style()),
            Span::styled(" skipped   ", theme::label_style()),
            Span::styled("PEND", theme::label_style()),
            Span::styled(" pending", theme::label_style()),
        ]),
        Line::from(vec![
            Span::styled(TAG_INDENT, theme::label_style()),
            mark(filters.skipped_only),
            Span::styled("s ", theme::accent_style()),
            Span::styled("SKIP", theme::warn_style()),
            Span::styled("  filter to skipped slots only", theme::label_style()),
        ]),
        // path — F is tickable via 'f' to filter fast-finalized only
        Line::from(vec![
            Span::styled("  path    ", theme::label_style()),
            mark(filters.fast_only),
            Span::styled("f ", theme::accent_style()),
            Span::styled("F", theme::good_style()),
            Span::styled(
                "  fast-finalized — FastFinalize cert (≥80%)",
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled(TAG_INDENT, theme::label_style()),
            mark(filters.slow_only),
            Span::styled("x ", theme::accent_style()),
            Span::styled("s", theme::warn_style()),
            Span::styled(
                "  slow-finalized — Notarize + Finalize 2-round",
                theme::label_style(),
            ),
        ]),
        // ldr — tickable via 'l' to filter our leader slots only
        Line::from(vec![
            Span::styled("  ldr     ", theme::label_style()),
            mark(filters.leader),
            Span::styled("l ", theme::accent_style()),
            Span::styled("[*]", theme::title_style()),
            Span::styled(
                " this validator was leader for the slot",
                theme::label_style(),
            ),
        ]),
        // events — TCL/S2N/S2S tickable via t/n/s
        Line::from(vec![
            Span::styled("  events  ", theme::label_style()),
            mark(filters.tcl),
            Span::styled("t ", theme::accent_style()),
            Span::styled("TCL", theme::warn_style()),
            Span::styled(
                "  TimeoutCrashedLeader — leader missed block window",
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled(TAG_INDENT, theme::label_style()),
            mark(filters.s2n),
            Span::styled("n ", theme::accent_style()),
            Span::styled("S2N", theme::warn_style()),
            Span::styled(
                "  SafeToNotar  — cluster notarized despite local hesitate",
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled(TAG_INDENT, theme::label_style()),
            mark(filters.s2s),
            Span::styled("p ", theme::accent_style()),
            Span::styled("S2S", theme::warn_style()),
            Span::styled(
                "  SafeToSkip   — cluster decided slot safe to skip",
                theme::label_style(),
            ),
        ]),
        // Footer: clear-all hint.
        Line::from(vec![
            Span::styled(TAG_INDENT, theme::label_style()),
            Span::styled("[c] ", theme::accent_style()),
            Span::styled("clear all filters", theme::label_style()),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_validator_info(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    let state = app.state;
    let total = state.slots.len() as u64;
    let leader: u64 = state.slots.values().filter(|s| s.we_are_leader).count() as u64;
    // A leader window = 4 consecutive slots (Solana
    // `NUM_CONSECUTIVE_LEADER_SLOTS`). We divide leader-slot count by 4
    // to derive the window count for display; the aggregator counts
    // `ProduceWindow` events directly in `state.overall.produce_windows`
    // (these should match modulo edge truncation at log boundaries).
    let windows = state.overall.produce_windows;

    let lines = vec![
        section_title("Our validator"),
        Line::from(vec![
            Span::styled("  leader ", theme::label_style()),
            Span::styled(commas(leader), theme::value_style()),
            Span::styled(
                format!(" slots  ≈ {:.2}% stake", pct(leader, total)),
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled("  = ", theme::label_style()),
            Span::styled(commas(windows), theme::value_style()),
            Span::styled(
                " 4-slot windows (NUM_CONSECUTIVE_LEADER_SLOTS)",
                theme::label_style(),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn section_title(s: &str) -> Line<'_> {
    Line::from(Span::styled(
        s.to_owned(),
        theme::title_style().add_modifier(Modifier::BOLD),
    ))
}

// ---------- Table ----------

fn render_table(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    let total_unfiltered = app.slot_rows.len();
    let total = app.slot_indices.len();
    if total_unfiltered == 0 {
        let p = Paragraph::new("(no slots)")
            .style(theme::label_style())
            .block(Block::default().borders(Borders::ALL).title(" slots "));
        frame.render_widget(p, area);
        return;
    }
    if total == 0 {
        // Filters active but no rows match — keep the title showing the
        // active chips so the user understands why the table is empty.
        let title = format!(
            " slots — {}  (no rows match) ",
            filter_chips(app.slot_filters)
        );
        let p = Paragraph::new("(no slots match active filters — press 'c' to clear)")
            .style(theme::label_style())
            .block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(p, area);
        return;
    }

    // Window the rows so we only build `Row` structs for what's visible.
    // Previously this built Rows for every slot (~179k) on every frame,
    // which made the panel hard-lag during navigation. Pattern mirrors
    // `panel::leader_timeouts::render_list`.
    //
    // Inner height = area.height - 2 (borders); subtract 1 more for the
    // header row.
    let visible = area.height.saturating_sub(3) as usize;
    let visible = visible.max(1);
    let start = app.slot_scroll.min(total.saturating_sub(visible));
    let end = (start + visible).min(total);
    let index_window = &app.slot_indices[start..end];

    let header = Row::new(vec![
        "slot",
        "status",
        "path",
        "ldr",
        "assembly",
        "consensus",
        "lifecycle",
        "vote",
        "events",
    ])
    .style(theme::label_style().add_modifier(Modifier::BOLD));

    let rows: Vec<Row<'_>> = index_window
        .iter()
        .map(|&i| row_for(&app.slot_rows[i]))
        .collect();

    let chips = filter_chips(app.slot_filters);
    let title = if app.slot_filters.any_active() {
        format!(
            " slots — {chips}  ({} of {} | cursor {} / {}) ",
            commas(total as u64),
            commas(total_unfiltered as u64),
            commas(app.slot_scroll as u64 + 1),
            commas(total as u64),
        )
    } else {
        format!(
            " slots ({} total | cursor {} / {}) ",
            commas(total as u64),
            commas(app.slot_scroll as u64 + 1),
            commas(total as u64),
        )
    };

    let table = Table::new(
        rows,
        [
            Constraint::Length(11), // slot
            Constraint::Length(7),  // status
            Constraint::Length(5),  // path
            Constraint::Length(4),  // ldr
            Constraint::Length(11), // assembly
            Constraint::Length(11), // consensus
            Constraint::Length(11), // lifecycle
            Constraint::Length(7),  // vote
            Constraint::Min(15),    // events — fills remaining width
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(title))
    .style(Style::default().fg(theme::FG))
    .row_highlight_style(
        Style::default()
            .bg(theme::ACCENT)
            .fg(theme::FG)
            .add_modifier(Modifier::BOLD),
    );

    let mut tstate = TableState::default();
    // Cursor is global (`app.slot_scroll`); within the visible window it
    // sits at `slot_scroll - start`. Mid-list this is always 0 (cursor at
    // top); only when scrolled past the last full page does it drift
    // toward the bottom so the cursor stays visible.
    tstate.select(Some(app.slot_scroll.saturating_sub(start)));
    frame.render_stateful_widget(table, area, &mut tstate);
}

fn filter_chips(f: SlotFilters) -> String {
    let mut chips: Vec<&'static str> = Vec::with_capacity(7);
    if f.tcl {
        chips.push("TCL");
    }
    if f.s2n {
        chips.push("S2N");
    }
    if f.s2s {
        chips.push("S2S");
    }
    if f.leader {
        chips.push("leader");
    }
    if f.fast_only {
        chips.push("fast");
    }
    if f.slow_only {
        chips.push("slow");
    }
    if f.skipped_only {
        chips.push("skipped");
    }
    if chips.is_empty() {
        String::new()
    } else {
        format!("filter: {}", chips.join(" + "))
    }
}

fn row_for(s: &SlotViewRow) -> Row<'_> {
    let status_style = match s.status {
        SlotStatus::FastFinalized => theme::good_style(),
        SlotStatus::SlowFinalized | SlotStatus::Skipped => theme::warn_style(),
        SlotStatus::Pending => theme::label_style(),
    };
    // Per-stage health bands. `None` (pending) -> gray so we don't
    // accidentally paint missing data green.
    let asm_style = s.assembly_ms.map_or_else(theme::label_style, |ms| {
        theme::band_lower_better(ms, theme::ASSEMBLY_WARN_MS, theme::ASSEMBLY_BAD_MS)
    });
    let lat_style = s.lifecycle_ms.map_or_else(theme::label_style, |ms| {
        theme::band_lower_better(ms, theme::LIFECYCLE_WARN_MS, theme::LIFECYCLE_BAD_MS)
    });
    let leader_mark = if s.we_are_leader { "[*]" } else { "" };
    let events = events_str(s);

    Row::new(vec![
        Line::from(Span::styled(commas(s.slot), theme::value_style())),
        Line::from(Span::styled(s.status_str(), status_style)),
        Line::from(Span::styled(s.fast_str(), status_style)),
        Line::from(Span::styled(leader_mark, theme::title_style())),
        Line::from(Span::styled(fmt_ms(s.assembly_ms), asm_style)),
        Line::from(Span::styled(fmt_ms(s.consensus_ms), theme::value_style())),
        Line::from(Span::styled(fmt_ms(s.lifecycle_ms), lat_style)),
        Line::from(Span::styled(s.vote_pattern(), theme::value_style())),
        Line::from(Span::styled(events, theme::warn_style())),
    ])
}

fn events_str(s: &SlotViewRow) -> String {
    let mut tags = Vec::with_capacity(4);
    if s.crashed_leader {
        tags.push("TCL");
    }
    if s.safe_to_notar {
        tags.push("S2N");
    }
    if s.safe_to_skip {
        tags.push("S2S");
    }
    tags.join(" ")
}

fn fmt_ms(v: Option<f64>) -> String {
    v.map_or_else(|| "-".to_owned(), |ms| format!("{ms:>6.1} ms"))
}

fn pct(num: u64, denom: u64) -> f64 {
    if denom == 0 {
        0.0
    } else {
        num as f64 * 100.0 / denom as f64
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
