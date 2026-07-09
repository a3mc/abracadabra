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
    // p95 (lifecycle) health: same three-band mapping as the reference
    // legend so the KPI colour and the band table read consistently
    // (green ≤ WARN, cyan WARN..BAD, red > BAD). Replaces the buried
    // `p95 (lifecycle) X ms [✓]` line that used to live in the Latency
    // bands section — same thresholds, same colour ramp.
    #[allow(clippy::cast_precision_loss)]
    let p95_style = theme::band_latency_soft(
        p95 as f64,
        theme::LIFECYCLE_WARN_MS,
        theme::LIFECYCLE_BAD_MS,
    );

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
            Constraint::Length(1),  // top pad — breathing room above "Latency bands:"
            Constraint::Length(4),  // latency content (1 title + 1 spacer + 2 bands)
            Constraint::Length(1),  // gap
            Constraint::Length(18), // legend — title + spacer + 12 entries + wraps
            Constraint::Length(1),  // gap
            Constraint::Min(0),     // selected-slot detail card fills remaining space
        ])
        .split(inner);

    render_latency_reference(app, frame, chunks[1]);
    render_legend(app.slot_filters, frame, chunks[3]);
    render_slot_detail(app, frame, chunks[5]);
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
            Span::styled("< 250", theme::good_style()),
            Span::styled("  ·  ", theme::label_style()),
            Span::styled("250–300", theme::accent_style()),
            Span::styled("  ·  ", theme::label_style()),
            Span::styled("≥ 300", theme::bad_style()),
            Span::styled(" ms", theme::label_style()),
        ]),
        Line::from(vec![
            Span::styled("  lifecycle ", theme::label_style()),
            Span::styled("< 380", theme::good_style()),
            Span::styled("  ·  ", theme::label_style()),
            Span::styled("380–500", theme::accent_style()),
            Span::styled("  ·  ", theme::label_style()),
            Span::styled("≥ 500", theme::bad_style()),
            Span::styled(" ms", theme::label_style()),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Selected-slot detail card.
///
/// Uses `app.slot_scroll` as the cursor into `app.slot_indices` to
/// find the slot currently focused in the table. Looks up the full
/// `SlotRecord` from `app.state.slots` for direct access to every
/// captured timestamp + `signature_count` + `block_id`.
///
/// **Every displayed field is a literal log-event timestamp or a
/// direct log field.** No inference, no correlation, no derived
/// state. Deltas are simple subtraction from a `t0` anchor:
///
/// - Leader slots: `t0 = block_emitted_at`. `first_shred_at` is
///   unavailable (we don't receive our own first shred), so the
///   `assembly` window is not shown.
/// - Non-leader slots: `t0 = first_shred_at` when present, else
///   `block_emitted_at`. Assembly window (`t0` → `block_emitted_at`)
///   is displayed.
/// - Skipped slots: whichever event fires first is `t0`.
///
/// Rows appear in temporal order (naturally sorted by ascending
/// timestamp). Missing events are omitted entirely — the card
/// shows only what actually happened.
fn render_slot_detail(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    if area.height < 3 || area.width < 30 {
        return;
    }

    let Some(&row_ix) = app.slot_indices.get(app.slot_scroll) else {
        return;
    };
    let Some(row) = app.slot_rows.get(row_ix) else {
        return;
    };
    let Some(rec) = app.state.slots.get(&row.slot) else {
        return;
    };

    let mut lines: Vec<Line<'_>> = Vec::new();

    // Header: slot · status · path/leader tags.
    lines.push(section_title("Selected slot:"));
    lines.push(Line::from(""));
    let path_tag = match (row.status, row.fast) {
        (SlotStatus::FastFinalized, _) | (_, Some(true)) => " fast",
        (SlotStatus::SlowFinalized, _) | (_, Some(false)) => " slow",
        _ => "",
    };
    let ldr_tag = if row.we_are_leader {
        "  ·  leader"
    } else {
        ""
    };
    lines.push(Line::from(vec![
        Span::styled("  slot ", theme::label_style()),
        Span::styled(commas(row.slot), theme::value_style()),
        Span::styled("  ·  ", theme::label_style()),
        Span::styled(row.status_str().to_string(), theme::value_style()),
        Span::styled(path_tag.to_string(), theme::label_style()),
        Span::styled(ldr_tag.to_string(), theme::label_style()),
    ]));

    // block_id (short) + signature_count if present.
    if rec.block_id.is_some() || rec.signature_count.is_some() {
        let mut meta = vec![Span::styled("  ", theme::label_style())];
        if let Some(bid) = &rec.block_id {
            let short = if bid.len() > 8 {
                format!("{}…", &bid[..8])
            } else {
                bid.clone()
            };
            meta.push(Span::styled("block ", theme::label_style()));
            meta.push(Span::styled(short, theme::value_style()));
        }
        if let Some(sigs) = rec.signature_count {
            if rec.block_id.is_some() {
                meta.push(Span::styled("  ·  ", theme::label_style()));
            }
            meta.push(Span::styled("sigs ", theme::label_style()));
            meta.push(Span::styled(commas(sigs), theme::value_style()));
        }
        lines.push(Line::from(meta));
    }

    lines.push(Line::from(""));

    // Timeline.
    // Collect (timestamp, label) pairs for events that actually fired.
    // Then sort ascending and format as "+X ms label" against t0.
    let events: [(Option<time::OffsetDateTime>, &str); 12] = [
        (rec.first_shred_at, "first_shred"),
        (rec.block_emitted_at, "block_emitted"),
        (rec.voted_notarize_at, "voted_notarize"),
        (rec.block_notarized_at, "block_notarized"),
        (rec.notar_fallback_at, "notar_fallback"),
        (rec.voted_finalize_at, "voted_finalize"),
        (rec.voted_skip_at, "voted_skip"),
        (rec.safe_to_notar_at, "safe_to_notar"),
        (rec.safe_to_skip_at, "safe_to_skip"),
        (rec.timeout_at, "timeout"),
        (rec.timeout_crashed_leader_at, "timeout_crashed_leader"),
        (rec.finalized_at, "finalized"),
    ];
    let mut present: Vec<(time::OffsetDateTime, &str)> = events
        .iter()
        .filter_map(|(t, l)| t.map(|ts| (ts, *l)))
        .collect();
    present.sort_by_key(|(t, _)| *t);

    if present.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no events observed for this slot yet",
            theme::label_style(),
        )));
    } else {
        let t0 = present[0].0;
        let anchor = anchor_label(present[0].1, row.we_are_leader);
        for (idx, (ts, label)) in present.iter().enumerate() {
            let delta_ms = (*ts - t0).whole_microseconds() as f64 / 1000.0;
            let delta_str = if idx == 0 {
                anchor.to_owned()
            } else if delta_ms.abs() < 0.05 {
                "     +0 ms".to_owned()
            } else {
                format!("{delta_ms:+7.1} ms")
            };
            let suffix = path_suffix(label, row.fast);
            lines.push(Line::from(vec![
                Span::styled("  ", theme::label_style()),
                Span::styled(format!("{label:<20}"), theme::label_style()),
                Span::styled(delta_str, theme::value_style()),
                Span::styled(suffix.to_string(), theme::label_style()),
            ]));
        }

        // Anchor note: what `t0` means depends on which event fired
        // first. `first_shred_at` is shred-receipt time (non-leader,
        // full lifecycle observed). `block_emitted_at` is a LOCAL
        // timestamp — leader-production for our slots, local
        // replay-complete for repair-fetched non-leader slots. Skipped
        // slots may anchor on a vote or timeout event. Different
        // anchors mean different portions of the round are covered.
        lines.push(Line::from(""));
        let note = anchor_note(present[0].1, row.we_are_leader);
        for text in note {
            lines.push(Line::from(Span::styled(text, theme::label_style())));
        }
    }

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
        // status filters (description of FIN/CSKIP/LSKIP/VSKIP/PEND
        // values is in the static reference block at the bottom of the
        // panel). VSKIP and CSKIP toggles OR together — press both for
        // the old "both buckets" view. LSKIP rows are a subset of VSKIP
        // (`l` filter already isolates them as the leader filter).
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
            Span::styled("LSKIP", theme::bad_style()),
            Span::styled(" our leader slot, we voted skip", theme::label_style()),
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
        "sigs",
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
            Constraint::Length(7),  // sigs — signature_count from `bank frozen`
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
    //   Skipped + CanonicalSkip (proven bad)      → red (real failure)
    //   Skipped + we_are_leader (LSKIP, our skip) → red (we missed
    //                                              our own leader window —
    //                                              operationally bad)
    //   Skipped + Indeterminate/NotSkipped (VSKIP) → yellow (unverified;
    //                                              could be right or canonical)
    //   Pending → gray (no terminal state yet)
    let status_style = match s.status {
        SlotStatus::FastFinalized | SlotStatus::SlowFinalized => theme::good_style(),
        SlotStatus::Skipped if s.skip_classification.is_canonical_skip() => theme::bad_style(),
        SlotStatus::Skipped if s.we_are_leader => theme::bad_style(),
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
        theme::band_latency_soft(ms, theme::ASSEMBLY_WARN_MS, theme::ASSEMBLY_BAD_MS)
    });
    let lat_style = s.lifecycle_ms.map_or_else(theme::label_style, |ms| {
        theme::band_latency_soft(ms, theme::LIFECYCLE_WARN_MS, theme::LIFECYCLE_BAD_MS)
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
        Line::from(Span::styled(
            fmt_sigs(s.signature_count),
            theme::value_style(),
        )),
        Line::from(Span::styled(events, theme::warn_style())),
    ])
}

/// Path-tag suffix appended to a timeline row's label.
///
/// `notar_fallback` is only tagged `(slow-path)` when the cluster
/// actually took the slow path — the vast majority of NotarFallback
/// events are benign auto-emitted companions of a successful 60%
/// Notarize cert (see `theme.rs::TRUE_FB_ELEVATED_PCT`). Tagging
/// them unconditionally would contradict the `(fast-path)` tag on
/// the same slot's `finalized` row.
fn path_suffix(label: &str, fast: Option<bool>) -> &'static str {
    match (label, fast) {
        ("notar_fallback", Some(false)) => "  (slow-path)",
        ("finalized", Some(true)) => "  (fast-path)",
        ("finalized", Some(false)) => "  (slow-path)",
        _ => "",
    }
}

/// Anchor-column label for the earliest event in the detail-card
/// timeline. Named after the actual event class so the reader can tell
/// what kind of anchor `t0` is from the delta column alone, without
/// having to consult the footer note.
///
/// - `first_shred` → `(start)`: shred-receipt anchor, the operator-
///   observable start of the slot's local lifecycle.
/// - `block_emitted` + leader → `(emit)`: we produced the block.
/// - `block_emitted` + non-leader → `(replay)`: repair-fetched slot,
///   no first-shred event; the anchor is local replay-complete.
/// - anything else → `(anchor)`: generic fallback for skipped-slot
///   anchors on vote or timeout events.
fn anchor_label(t0_label: &str, we_are_leader: bool) -> &'static str {
    match (t0_label, we_are_leader) {
        ("first_shred", _) => "   (start)",
        ("block_emitted", true) => "    (emit)",
        ("block_emitted", false) => "  (replay)",
        _ => "  (anchor)",
    }
}

/// Two-line explanatory note about what `t0` means in the detail
/// card's timeline, branched on the label of the earliest observed
/// event and whether we were the leader for the slot.
///
/// - `first_shred` → shred-receipt anchor (non-leader, full lifecycle
///   observed from first hop of shred propagation).
/// - `block_emitted` + leader → we produced the block; deltas span the
///   full consensus round.
/// - `block_emitted` + non-leader → repair-fetched slot with no
///   first-shred event; the anchor is our local replay-complete.
/// - anything else → generic fallback (skipped slots anchored on a
///   vote or timeout event).
fn anchor_note(t0_label: &str, we_are_leader: bool) -> [&'static str; 2] {
    match (t0_label, we_are_leader) {
        ("first_shred", _) => [
            "  t0 = first shred arrived; deltas span shred-receipt",
            "  → finalized (round minus leader production).",
        ],
        ("block_emitted", true) => [
            "  t0 = we produced this slot; deltas span the full",
            "  consensus round (~250 ms target).",
        ],
        ("block_emitted", false) => [
            "  t0 = our local replay complete; leader emitted",
            "  earlier — deltas cover only the tail of the round.",
        ],
        _ => [
            "  t0 = earliest observed event for this slot; deltas",
            "  cover only the portion that fired locally.",
        ],
    }
}

/// Compact signature_count formatter matching the Live tab's leader-
/// pane style. Empty when we did not bank the slot.
fn fmt_sigs(v: Option<u64>) -> String {
    let Some(n) = v else {
        return String::new();
    };
    if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
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

#[cfg(test)]
mod tests {
    use super::{anchor_label, anchor_note, fmt_sigs, path_suffix};

    // SLOTS-01 regression guard. The `notar_fallback` suffix must
    // only fire on genuine slow-path finalizations; on the ~99.9% of
    // NotarFallback events that are benign auto-emitted companions
    // (`row.fast == Some(true)` or `None`), the suffix must be empty
    // so it does not contradict a `(fast-path)` tag on the same
    // slot's `finalized` row.
    #[test]
    fn path_suffix_notar_fallback_only_tags_slow_finalized() {
        assert_eq!(path_suffix("notar_fallback", Some(true)), "");
        assert_eq!(path_suffix("notar_fallback", Some(false)), "  (slow-path)");
        assert_eq!(path_suffix("notar_fallback", None), "");
    }

    #[test]
    fn path_suffix_finalized_reflects_fast_flag() {
        assert_eq!(path_suffix("finalized", Some(true)), "  (fast-path)");
        assert_eq!(path_suffix("finalized", Some(false)), "  (slow-path)");
        assert_eq!(path_suffix("finalized", None), "");
    }

    #[test]
    fn path_suffix_other_labels_have_no_tag() {
        assert_eq!(path_suffix("voted_notarize", Some(true)), "");
        assert_eq!(path_suffix("voted_notarize", Some(false)), "");
        assert_eq!(path_suffix("voted_notarize", None), "");
        assert_eq!(path_suffix("first_shred", Some(true)), "");
        assert_eq!(path_suffix("block_emitted", Some(false)), "");
    }

    // SLOTS-02 regression guard. Anchor-note text branches on the
    // label of the earliest observed event, not on `we_are_leader`
    // alone. The pre-fix code claimed "t0 = our local replay
    // complete" for every non-leader slot, which is wrong for
    // non-leader slots with a `first_shred` event (the majority).
    #[test]
    fn anchor_note_first_shred_describes_shred_receipt() {
        for we_are_leader in [true, false] {
            let note = anchor_note("first_shred", we_are_leader);
            assert!(
                note[0].contains("first shred") && note[0].contains("t0"),
                "line 0 = {:?}",
                note[0],
            );
            assert!(note[1].contains("finalized"), "line 1 = {:?}", note[1]);
        }
    }

    #[test]
    fn anchor_note_block_emitted_leader_describes_production() {
        let note = anchor_note("block_emitted", true);
        assert!(note[0].contains("we produced"), "line 0 = {:?}", note[0]);
        assert!(
            note[1].contains("consensus round"),
            "line 1 = {:?}",
            note[1]
        );
    }

    #[test]
    fn anchor_note_block_emitted_non_leader_describes_replay() {
        let note = anchor_note("block_emitted", false);
        assert!(
            note[0].contains("local replay complete"),
            "line 0 = {:?}",
            note[0]
        );
        assert!(
            note[1].contains("tail of the round"),
            "line 1 = {:?}",
            note[1]
        );
    }

    #[test]
    fn anchor_label_first_shred_reads_start() {
        for we_are_leader in [true, false] {
            assert!(
                anchor_label("first_shred", we_are_leader).contains("(start)"),
                "label = {:?}",
                anchor_label("first_shred", we_are_leader),
            );
        }
    }

    #[test]
    fn anchor_label_block_emitted_leader_reads_emit() {
        assert!(
            anchor_label("block_emitted", true).contains("(emit)"),
            "label = {:?}",
            anchor_label("block_emitted", true),
        );
    }

    #[test]
    fn anchor_label_block_emitted_non_leader_reads_replay() {
        assert!(
            anchor_label("block_emitted", false).contains("(replay)"),
            "label = {:?}",
            anchor_label("block_emitted", false),
        );
    }

    #[test]
    fn anchor_label_fallback_reads_anchor() {
        assert!(
            anchor_label("voted_skip", false).contains("(anchor)"),
            "label = {:?}",
            anchor_label("voted_skip", false),
        );
    }

    #[test]
    fn anchor_label_column_width_matches_delta_column() {
        // The anchor label must occupy the same column width (10 chars)
        // as `"     +0 ms"` / `"{:+7.1} ms"` so the delta column stays
        // aligned when the first row is the anchor.
        for (label, ldr) in [
            ("first_shred", true),
            ("first_shred", false),
            ("block_emitted", true),
            ("block_emitted", false),
            ("voted_skip", false),
        ] {
            assert_eq!(
                anchor_label(label, ldr).chars().count(),
                10,
                "label = {:?}",
                anchor_label(label, ldr),
            );
        }
    }

    #[test]
    fn fmt_sigs_none_is_empty() {
        assert_eq!(fmt_sigs(None), "");
    }

    #[test]
    fn fmt_sigs_small_counts_render_as_integer() {
        assert_eq!(fmt_sigs(Some(0)), "0");
        assert_eq!(fmt_sigs(Some(999)), "999");
    }

    #[test]
    fn fmt_sigs_large_counts_render_as_k() {
        assert_eq!(fmt_sigs(Some(1_000)), "1k");
        // 33_747 observed max on cadabra.log — rounds to 34k.
        assert_eq!(fmt_sigs(Some(33_747)), "34k");
    }
}
