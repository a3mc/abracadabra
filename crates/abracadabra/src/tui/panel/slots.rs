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

use crate::model::slot::SlotStatus;
use crate::tui::app::{App, SlotFilters};
use crate::tui::theme;
use crate::tui::view::SlotViewRow;
use crate::tui::widget::commas;

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
            Constraint::Percentage(55), // table
            Constraint::Percentage(45), // reference (bumped from 40 -> 45 after
                                        // dropping the validator-info footer to
                                        // give the legend breathing room).
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
    // Use unique-slot counts (populated by classify_skips) instead of
    // per-event counters. Event counts double-count slots that are
    // both Finalized and voted-skip (canonical skips). The subtraction
    // formula `total - fin - skip` would saturate PEND to a misleading
    // zero — see audit/2026-05-27-tui-vs-alpenglow/TRIAGE.md item B7.
    let fin = ov.finalized_slot_count;
    let skip = ov.skipped_slot_count;
    let pend = ov.pending_slot_count;
    let canon = ov
        .canonical_skips_direct
        .saturating_add(ov.canonical_skips_ancestry);
    // Fast-finalize share computed from event counts (these don't
    // double-count between fast and slow; a slot has one or the other).
    let fin_fast = ov.finalized_fast;
    let fin_slow = ov.finalized_slow;
    let fin_events = fin_fast.saturating_add(fin_slow);
    // Pre-counted once in `App::new` to avoid a full BTreeMap scan per frame.
    let leader = app.leader_slot_count;

    let fin_pct = pct(fin, total);
    let fast_share = pct(fin_fast, fin_events);
    let skip_pct = pct(skip, total);
    let canon_pct = pct(canon, skip);
    let fin_style = theme::band_higher_better(fin_pct, theme::FIN_GOOD_PCT, theme::FIN_WARN_PCT);
    let skip_style = theme::band_lower_better(
        skip_pct,
        theme::VOTE_SKIP_WARN_PCT,
        theme::VOTE_SKIP_BAD_PCT,
    );
    let canon_style = theme::band_lower_better(
        canon_pct,
        theme::CANONICAL_SKIP_WARN_PCT,
        theme::CANONICAL_SKIP_BAD_PCT,
    );
    // Lower-bound marker when indeterminate skips exist: the displayed
    // canonical-skip share is a floor, not a point estimate. Same
    // convention as header.rs:81, overview.rs:249, windows.rs:143,
    // runner.rs:181 — operators flipping tabs must see one story.
    let canon_bound = if ov.indeterminate_skips > 0 {
        "≥"
    } else {
        ""
    };

    // Read pre-computed lifecycle percentiles instead of re-sorting
    // ~179k entries per frame (see `App::latency` / `LatencySnapshot`).
    let (p50_us, p95_us, p99_us, max_us) = app.latency.lifecycle_pcts_us;
    let p50 = p50_us / 1000;
    let p95 = p95_us / 1000;
    let p99 = p99_us / 1000;
    let max_ms = max_us / 1000;
    let max_style = if max_ms >= 1000 {
        theme::bad_style()
    } else {
        theme::warn_style()
    };
    // p95 (lifecycle) health: good if ≤ LIFECYCLE_WARN_MS (matches the
    // band shown in the right-panel reference). Replaces the buried
    // `p95 (lifecycle) X ms [✓]` line that used to live in the Latency
    // bands section — same threshold, same colour mapping.
    let p95_style = if p95 <= theme::LIFECYCLE_WARN_MS as i64 {
        theme::good_style()
    } else {
        theme::warn_style()
    };

    let pipe = || Span::styled("  |  ", theme::label_style());

    // Line 1 — dataset identity & outcome split.
    //
    // `vote-skip` is how often this validator cast a Skip vote
    // (distinct from Solana's block-production "skip" — operator
    // mental model differs). `canonical-skip` is the subset that
    // proved wrong (we voted skip on a slot that became canonical).
    // Line 1 carries the dataset + outcome split. `leader N` and
    // `our slot share %` both moved to line 2 (next to the lifecycle
    // percentiles) so this row stays compact and line 2 has the full
    // leadership context grouped.
    let line1 = Line::from(vec![
        Span::styled("slots ", theme::label_style()),
        Span::styled(commas(total), theme::value_style()),
        pipe(),
        Span::styled("FIN ", theme::label_style()),
        Span::styled(format!("{fin_pct:.1}%"), fin_style),
        Span::styled(
            format!(" (fast {fast_share:.0}% of FIN)"),
            theme::label_style(),
        ),
        pipe(),
        Span::styled("vote-skip ", theme::label_style()),
        Span::styled(format!("{skip_pct:.1}%"), skip_style),
        Span::styled(format!(" ({} slots, ", commas(skip)), theme::label_style()),
        Span::styled("canonical-skip ", theme::label_style()),
        Span::styled(format!("{canon_bound}{canon_pct:.2}%"), canon_style),
        Span::styled(format!(" = {} slots)", commas(canon)), theme::label_style()),
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

    // Line 2 — lifecycle latency percentiles + leadership context.
    // p50 in accent so the headline reads first; tails neutral; max
    // coloured by health band. Trailing `leader N` + `our slot share`
    // grouped together (relocated 2026-05-28 from line 1 to keep line
    // 1 compact and give the leadership info its own visible cluster).
    // `our slot share` = leader_slots / total_slots over the log
    // window (window-relative, not stake — see TRIAGE B6).
    let line2 = Line::from(vec![
        Span::styled("lifecycle ", theme::label_style()),
        Span::styled("p50 ", theme::label_style()),
        Span::styled(format!("{p50} ms"), theme::accent_style()),
        pipe(),
        Span::styled("p95 ", theme::label_style()),
        Span::styled(format!("{p95} ms"), p95_style),
        pipe(),
        Span::styled("p99 ", theme::label_style()),
        Span::styled(format!("{p99} ms"), theme::value_style()),
        pipe(),
        Span::styled("max ", theme::label_style()),
        Span::styled(format!("{max_ms} ms"), max_style),
        pipe(),
        Span::styled("leader ", theme::label_style()),
        Span::styled(commas(leader), theme::value_style()),
        pipe(),
        Span::styled("our slot share ", theme::label_style()),
        Span::styled(format!("{:.2}%", pct(leader, total)), theme::value_style()),
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
    // Validator-share metric used to live in a buried footer section
    // here; moved into the `slot stats` line 2 (see B6 fix 2026-05-28)
    // so it has actual visibility. This panel now hosts only the
    // latency bands and the legend, with breathing room between.
    //
    // Legend uses `Min(12)` (not `Length(12)`) so it grows to absorb
    // wrap-induced extra lines on narrow terminals — several legend
    // entries (status, S2N, S2S) have descriptions wider than the
    // available column and wrap onto a second visual line. With a
    // fixed `Length(12)` those wraps would push the footer entries
    // (`vote`, `[c] clear`) off the bottom edge.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // top pad — breathing room above "Latency bands:"
            Constraint::Length(4), // latency content (1 title + 1 spacer + 2 bands)
            Constraint::Length(1), // gap
            Constraint::Min(12),   // legend — grows to fit wrap (title + spacer + 12 entries)
        ])
        .split(inner);

    render_latency_reference(app, frame, chunks[1]);
    render_legend(app.slot_filters, frame, chunks[3]);
}

fn render_latency_reference(_app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    // Per-stage threshold bands. The current-value `p95 (lifecycle)`
    // row that used to live here is now colour-banded in the `slot
    // stats` KPI line above (`p95 NNN ms` styled by health), so this
    // section is purely the reference table.
    let lines = vec![
        section_title("Latency bands:"),
        Line::from(""),
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
        section_title("Filters available:"),
        Line::from(""),
        // status filters (description of FIN/CSKIP/VSKIP/PEND values
        // is in the static reference block at the bottom of the panel).
        // VSKIP and CSKIP toggles OR together — press both for the
        // old "both buckets" view.
        Line::from(vec![
            Span::styled("  status  ", theme::label_style()),
            mark(filters.vskip_only),
            Span::styled("v ", theme::accent_style()),
            Span::styled("VSKIP", theme::warn_style()),
            Span::styled(
                "  vote-skip rows (no canonical evidence)",
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled(TAG_INDENT, theme::label_style()),
            mark(filters.canonical_skip_only),
            Span::styled("c ", theme::accent_style()),
            Span::styled("CSKIP", theme::bad_style()),
            Span::styled("  canonical skips (proven via log)", theme::label_style()),
        ]),
        // ---- path — column shows the CLUSTER's finalization path, not
        // ours. Important distinction for CSKIP rows: F there means
        // "we missed a slot the cluster fast-finalized" (worse for us).
        Line::from(vec![
            Span::styled("  path    ", theme::label_style()),
            mark(filters.fast_only),
            Span::styled("f ", theme::accent_style()),
            Span::styled("F", theme::good_style()),
            Span::styled("  cluster fast-finalized (80% Notar)", theme::label_style()),
        ]),
        Line::from(vec![
            Span::styled(TAG_INDENT, theme::label_style()),
            mark(filters.slow_only),
            Span::styled("s ", theme::accent_style()),
            Span::styled("S", theme::accent_style()),
            Span::styled(
                "  cluster slow-finalized (60% Notar + 60% Final)",
                theme::label_style(),
            ),
        ]),
        // ---- ldr — tickable via 'l' to filter our leader slots only
        Line::from(vec![
            Span::styled("  ldr     ", theme::label_style()),
            mark(filters.leader),
            Span::styled("l ", theme::accent_style()),
            Span::styled("[*]", theme::title_style()),
            Span::styled("  this validator was leader", theme::label_style()),
        ]),
        // ---- events — TCL/S2S/S2N tickable via t/p/n. S2S above S2N:
        // shorter description first; if either wraps, the longer S2N
        // pushes only against the utility footer below.
        Line::from(vec![
            Span::styled("  events  ", theme::label_style()),
            mark(filters.tcl),
            Span::styled("t ", theme::accent_style()),
            Span::styled("TCL", theme::warn_style()),
            Span::styled(
                "  TimeoutCrashedLeader — leader missed window",
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled(TAG_INDENT, theme::label_style()),
            mark(filters.s2s),
            Span::styled("p ", theme::accent_style()),
            Span::styled("S2S", theme::warn_style()),
            Span::styled(
                "  SafeToSkip — stake fragmented; hedged SkipFallback",
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled(TAG_INDENT, theme::label_style()),
            mark(filters.s2n),
            Span::styled("n ", theme::accent_style()),
            Span::styled("S2N", theme::warn_style()),
            Span::styled(
                "  SafeToNotar — sibling block past safety; hedged NotarizeFallback",
                theme::label_style(),
            ),
        ]),
        // ---- footer: clear-all utility, then the static column-value
        // reference rows at the absolute bottom (status descriptions,
        // vote pattern, consensus inverted glyph). These describe what
        // column values mean — they are not filter toggles — so they
        // sit below the [c] separator. Each subgroup gets a blank
        // line above it for visual grouping.
        Line::from(""),
        Line::from(vec![
            Span::styled(TAG_INDENT, theme::label_style()),
            Span::styled("[x] ", theme::accent_style()),
            Span::styled("clear all filters", theme::label_style()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  status  ", theme::label_style()),
            Span::styled("FIN", theme::good_style()),
            Span::styled(" finalized   ", theme::label_style()),
            Span::styled("CSKIP", theme::bad_style()),
            Span::styled(" we voted skip on canonical", theme::label_style()),
        ]),
        Line::from(vec![
            Span::styled(TAG_INDENT, theme::label_style()),
            Span::styled("VSKIP", theme::warn_style()),
            Span::styled(" we voted skip, outcome unknown", theme::label_style()),
        ]),
        Line::from(vec![
            Span::styled(TAG_INDENT, theme::label_style()),
            Span::styled("PEND", theme::label_style()),
            Span::styled("  pending", theme::label_style()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  vote    ", theme::label_style()),
            Span::styled("N", theme::value_style()),
            Span::styled(" notarize  ", theme::label_style()),
            Span::styled("F", theme::value_style()),
            Span::styled(" finalize  ", theme::label_style()),
            Span::styled("S", theme::value_style()),
            Span::styled(" skip", theme::label_style()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  consensus ", theme::label_style()),
            Span::styled("↶", theme::accent_style()),
            Span::styled(
                "  cluster finalized before local replay",
                theme::label_style(),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn section_title(s: &str) -> Line<'_> {
    // 2-space leading indent so the bold heading aligns with the
    // `  status  ` / `  path    ` etc. data rows below, instead of
    // jamming flush against the panel border.
    Line::from(Span::styled(
        format!("  {s}"),
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
    // Total slot count is already shown in the `slot stats` KPI strip
    // above (`slots 179,016`), so the panel title drops the redundant
    // `N total` field and shows only the cursor position. The filtered
    // variant still needs the `M of N` count to disambiguate.
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
            " slots (cursor {} / {}) ",
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
    let mut chips: Vec<&'static str> = Vec::with_capacity(8);
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
    if f.vskip_only {
        chips.push("vskip");
    }
    if f.canonical_skip_only {
        chips.push("cskip");
    }
    if chips.is_empty() {
        String::new()
    } else {
        format!("filter: {}", chips.join(" + "))
    }
}

fn row_for(s: &SlotViewRow) -> Row<'_> {
    // Color-banding for the status cell:
    //   FastFinalized / SlowFinalized → green (healthy outcome)
    //   Skipped + CanonicalSkip (proven bad) → red (real failure)
    //   Skipped + Indeterminate/NotSkipped → yellow (unverified;
    //                                              could be right or canonical)
    //   Pending → gray (no terminal state yet)
    let status_style = match s.status {
        SlotStatus::FastFinalized | SlotStatus::SlowFinalized => theme::good_style(),
        SlotStatus::Skipped if s.skip_classification.is_canonical_skip() => theme::bad_style(),
        SlotStatus::Skipped => theme::warn_style(),
        SlotStatus::Pending => theme::label_style(),
    };
    // Path column gets its own coloring so fast vs slow are visually
    // distinct (the status column collapses both into "FIN" + green).
    // Slow uses accent (cyan) — still successful, but not optimal,
    // and avoids overloading yellow which already marks SKIP / S2N
    // / S2S elsewhere.
    let path_style = match (s.status, s.fast) {
        (SlotStatus::FastFinalized, _) | (_, Some(true)) => theme::good_style(),
        (SlotStatus::SlowFinalized, _) | (_, Some(false)) => theme::accent_style(),
        _ => theme::label_style(),
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

    // Consensus cell: when the cert beat local replay (`consensus_inverted`),
    // render `↶` in accent colour instead of the plain `-` used for
    // missing-data rows. Right-padded into the column to match the
    // `NNN.N ms` width of the data path.
    let (consensus_text, consensus_style) = if s.consensus_inverted {
        ("        ↶".to_owned(), theme::accent_style())
    } else {
        (fmt_ms(s.consensus_ms), theme::value_style())
    };

    Row::new(vec![
        Line::from(Span::styled(commas(s.slot), theme::value_style())),
        Line::from(Span::styled(s.status_str(), status_style)),
        Line::from(Span::styled(s.fast_str(), path_style)),
        Line::from(Span::styled(leader_mark, theme::title_style())),
        Line::from(Span::styled(fmt_ms(s.assembly_ms), asm_style)),
        Line::from(Span::styled(consensus_text, consensus_style)),
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
