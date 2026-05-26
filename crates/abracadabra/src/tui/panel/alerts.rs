//! Tab 6: alerts dashboard.
//!
//! ```text
//! ┌─ KPI strip: severity counts ──────────────────────────────────────────┐
//! ├─ list (60%) ──────────────┬─ detail (40%) ───────────────────────────┤
//! │   severity-tag · count    │   full body / sample                     │
//! │   · module · preview      │   module · count · first/last · span     │
//! │                           │   sparkline of per-bucket counts         │
//! └───────────────────────────┴──────────────────────────────────────────┘
//! ```
//!
//! The sparkline is computed on-the-fly from the selected group's
//! timestamps, bucketed across `state.file_meta.time_range`. So a
//! 1-burst-then-quiet pattern reads visually different from a sustained
//! one — the user's diagnostic question.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Sparkline, Wrap};
use ratatui::Frame;

use crate::model::alerts::{Alert, AlertKind, Severity};
use crate::model::state::{LogIssueGroup, State};
use crate::tui::app::App;
use crate::tui::theme;

/// Tab 6: full-screen alert dashboard with selectable detail pane.
pub fn render_full(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    let state = app.state;
    if state.alerts.is_empty() {
        let p = Paragraph::new("(no alerts in this log)")
            .style(theme::label_style())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" alerts ")
                    .title_style(theme::title_style()),
            );
        frame.render_widget(p, area);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // KPI strip
            Constraint::Min(8),    // list + detail
        ])
        .split(area);

    render_kpi_strip(state, frame, chunks[0]);

    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(60), // list
            Constraint::Percentage(40), // detail
        ])
        .split(chunks[1]);
    render_list(app, frame, bottom[0]);
    render_detail(app, frame, bottom[1]);
}

// ---------- KPI strip ----------

fn render_kpi_strip(state: &State, frame: &mut Frame<'_>, area: Rect) {
    let (crit, warn, info) = severity_counts(state);
    let total = state.alerts.len() as u64;

    // Severities with zero count are dropped from the strip so the
    // rollup highlights what's actually present. CRIT and WARN always
    // render (even at zero) so the rollup is recognisable; INFO is
    // conditional because the only kind currently emitting it
    // (ClusterSlotsShutdownObserved) is sparse.
    let mut spans = vec![
        Span::styled("[CRIT] ", theme::bad_style()),
        Span::styled(
            commas(crit),
            theme::value_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled("    [WARN] ", theme::warn_style()),
        Span::styled(
            commas(warn),
            theme::value_style().add_modifier(Modifier::BOLD),
        ),
    ];
    if info > 0 {
        spans.extend([
            Span::styled("    [INFO] ", theme::label_style()),
            Span::styled(commas(info), theme::value_style()),
        ]);
    }
    spans.extend([
        Span::styled("    total ", theme::label_style()),
        Span::styled(commas(total), theme::value_style()),
        Span::styled(
            "    (unique groups; many lines collapsed per group)",
            theme::label_style(),
        ),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" alerts — severity rollup ")
        .title_style(theme::title_style());
    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn severity_counts(state: &State) -> (u64, u64, u64) {
    let mut crit = 0;
    let mut warn = 0;
    let mut info = 0;
    for a in &state.alerts {
        match a.severity {
            Severity::Critical => crit += 1,
            Severity::Warn => warn += 1,
            Severity::Info => info += 1,
        }
    }
    (crit, warn, info)
}

// ---------- List (left 60%) ----------

fn render_list(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    let state = app.state;
    let items: Vec<ListItem<'_>> = state
        .alerts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let selected = i == app.alert_scroll;
            ListItem::new(row_line(a, selected))
        })
        .collect();
    let title = format!(
        " alerts ({}) — j/k cursor · CRIT first, by count ",
        state.alerts.len(),
    );
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .title_style(theme::title_style()),
    );
    frame.render_widget(list, area);
}

fn row_line(a: &Alert, selected: bool) -> Line<'_> {
    let cursor = if selected { ">" } else { " " };
    let (tag, tag_style) = severity_tag(a.severity);
    let count = alert_count(a);
    let count_str = format!("{:>9}×", commas(count));
    let module_short = alert_module_short(a);
    let preview = alert_preview(a);

    Line::from(vec![
        Span::styled(format!("{cursor} "), theme::title_style()),
        Span::styled(tag, tag_style),
        Span::raw(" "),
        Span::styled(count_str, theme::value_style()),
        Span::raw("  "),
        Span::styled(module_short, theme::accent_style()),
        Span::raw("  "),
        Span::styled(preview, theme::label_style()),
    ])
}

fn severity_tag(s: Severity) -> (&'static str, Style) {
    match s {
        Severity::Critical => ("[CRIT]", theme::bad_style()),
        Severity::Warn => ("[WARN]", theme::warn_style()),
        Severity::Info => ("[INFO]", theme::label_style()),
    }
}

/// Best-effort "count" per alert kind. For `LogPattern` it's the group
/// count; for `LocalLeaderSummary` it's the leader-slot count. Other
/// kinds are singleton events and report `1`.
const fn alert_count(a: &Alert) -> u64 {
    match &a.kind {
        AlertKind::LogPattern { count, .. } => *count,
        AlertKind::LocalLeaderSummary { slot_count, .. } => *slot_count,
        _ => 1,
    }
}

/// Short module label for the list view — last `::` segment for
/// `LogPattern` alerts, a fixed tag for analytical / informational alerts.
fn alert_module_short(a: &Alert) -> String {
    match &a.kind {
        AlertKind::LogPattern { module, .. } => module
            .rsplit("::")
            .next()
            .unwrap_or(module.as_str())
            .to_owned(),
        AlertKind::ClusterSlotsShutdownObserved => "shutdown".to_owned(),
        AlertKind::StandstillObserved { .. } => "standstill".to_owned(),
        AlertKind::LeaderTimeoutCrashed { .. } => "leader-timeout".to_owned(),
        AlertKind::LocalLeaderSummary { .. } => "local-leader".to_owned(),
        AlertKind::IdentityChanged => "identity-change".to_owned(),
    }
}

/// First N chars of the alert body without the prefix the description
/// duplicates (severity / module / count).
fn alert_preview(a: &Alert) -> String {
    const PREVIEW_LEN: usize = 60;
    let body = match &a.kind {
        AlertKind::LogPattern { .. } => {
            // The LogPattern description starts with "LEVEL module (N occurrences): "
            // — strip that prefix so the preview shows just the sample body.
            a.description.find(": ").map_or_else(
                || a.description.clone(),
                |i| a.description[i + 2..].to_owned(),
            )
        }
        _ => a.description.clone(),
    };
    let bytes = body.as_bytes();
    if bytes.len() <= PREVIEW_LEN {
        return body;
    }
    let mut end = PREVIEW_LEN;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &body[..end])
}

// ---------- Detail (right 40%) ----------

fn render_detail(app: &App<'_>, frame: &mut Frame<'_>, area: Rect) {
    let state = app.state;
    let Some(alert) = state.alerts.get(app.alert_scroll) else {
        return;
    };

    let outer = Block::default()
        .borders(Borders::ALL)
        .title(" detail ")
        .title_style(theme::title_style())
        .border_style(theme::label_style());
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    // Three rendering paths depending on what data the alert carries:
    //  - LogPattern: structured group with timestamps -> sparkline
    //  - LocalLeaderSummary: produce_window_timestamps -> sparkline
    //  - everything else: description-only detail
    match &alert.kind {
        AlertKind::LogPattern {
            severity, module, ..
        } => {
            if let Some(group) = state.log_issues_get(*severity, module) {
                render_log_pattern_detail(alert, group, frame, inner);
                return;
            }
        }
        AlertKind::LocalLeaderSummary {
            slot_count,
            window_count,
        } => {
            render_local_leader_detail(alert, *slot_count, *window_count, state, frame, inner);
            return;
        }
        _ => {}
    }
    render_generic_detail(alert, frame, inner);
}

/// Detail pane for the `LocalLeaderSummary` alert — meta-info +
/// sparkline of `ProduceWindow` announcement timestamps so the user
/// can see when their leader windows fell across the log.
fn render_local_leader_detail(
    alert: &Alert,
    slot_count: u64,
    window_count: u64,
    state: &State,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let timestamps = &state.overall.produce_window_timestamps;
    let (tag, tag_style) = severity_tag(alert.severity);
    let (first_at, last_at) = match (timestamps.first(), timestamps.last()) {
        (Some(f), Some(l)) => (*f, *l),
        _ => (alert.at, alert.at),
    };
    let span_secs = (last_at - first_at).whole_seconds().max(0);
    let span_str = humanize_dur(span_secs);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(7),
            Constraint::Length(1), // axis
            Constraint::Length(3), // sparkline
            Constraint::Length(1), // caption
        ])
        .split(area);

    // Annotate the math explicitly: a "leader window" is a 4-slot
    // burst (Solana `NUM_CONSECUTIVE_LEADER_SLOTS = 4`). Without the
    // qualifier the relationship between `windows` and `slots` reads
    // arbitrary.
    let lines = vec![
        Line::from(vec![
            Span::styled(tag, tag_style),
            Span::raw("  "),
            Span::styled("local validator leader schedule", theme::accent_style()),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("windows ", theme::label_style()),
            Span::styled(
                commas(window_count),
                theme::value_style().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "    (4-slot bursts, per NUM_CONSECUTIVE_LEADER_SLOTS)",
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled("slots   ", theme::label_style()),
            Span::styled(
                commas(slot_count),
                theme::value_style().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("    = {} × 4", commas(window_count)),
                theme::label_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled("first   ", theme::label_style()),
            Span::styled(fmt_ts(first_at), theme::value_style()),
        ]),
        Line::from(vec![
            Span::styled("last    ", theme::label_style()),
            Span::styled(fmt_ts(last_at), theme::value_style()),
        ]),
        Line::from(vec![
            Span::styled("span    ", theme::label_style()),
            Span::styled(span_str, theme::accent_style()),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), parts[0]);

    let axis_caption = Line::from(Span::styled(
        format!(
            "  leader windows over time   {}  →  {}",
            short_ts(first_at),
            short_ts(last_at),
        ),
        theme::label_style(),
    ));
    frame.render_widget(Paragraph::new(axis_caption), parts[1]);

    let bucket_count = parts[2].width.max(8) as usize;
    let buckets = bucket_timestamps(timestamps, bucket_count);
    let peak = buckets.iter().copied().max().unwrap_or(1).max(1);
    let spark = Sparkline::default()
        .data(&buckets[..])
        .max(peak)
        .style(Style::default().fg(theme::OK));
    frame.render_widget(spark, parts[2]);

    let caption = Line::from(Span::styled(
        format!(
            "  peak bucket = {}   (one column per ≈ time slice)",
            commas(peak)
        ),
        theme::label_style(),
    ));
    frame.render_widget(Paragraph::new(caption), parts[3]);
}

fn render_log_pattern_detail(
    alert: &Alert,
    group: &LogIssueGroup,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let (tag, tag_style) = severity_tag(alert.severity);
    let span_secs = (group.last_at - group.first_at).whole_seconds().max(0);
    let span_str = humanize_dur(span_secs);
    let rate_per_min = if span_secs > 0 {
        group.count as f64 * 60.0 / span_secs as f64
    } else {
        f64::from(u32::try_from(group.count.min(u64::from(u32::MAX))).unwrap_or(u32::MAX))
    };

    // Split: meta-info + body (top) | spacer | sparkline (bottom).
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),    // meta + body
            Constraint::Length(1), // axis hint
            Constraint::Length(3), // sparkline
            Constraint::Length(1), // sparkline caption
        ])
        .split(area);

    let lines = vec![
        Line::from(vec![
            Span::styled(tag, tag_style),
            Span::raw("  "),
            Span::styled(group.module.clone(), theme::accent_style()),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("count   ", theme::label_style()),
            Span::styled(
                commas(group.count),
                theme::value_style().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("    ({rate_per_min:.1}/min)"), theme::label_style()),
        ]),
        Line::from(vec![
            Span::styled("first   ", theme::label_style()),
            Span::styled(fmt_ts(group.first_at), theme::value_style()),
        ]),
        Line::from(vec![
            Span::styled("last    ", theme::label_style()),
            Span::styled(fmt_ts(group.last_at), theme::value_style()),
        ]),
        Line::from(vec![
            Span::styled("span    ", theme::label_style()),
            Span::styled(span_str, theme::accent_style()),
        ]),
        Line::raw(""),
        Line::from(vec![Span::styled("first sample", theme::label_style())]),
        Line::from(vec![Span::styled(
            group.sample_body.clone(),
            theme::value_style(),
        )]),
        Line::from(vec![Span::styled(
            "  (later bodies may differ — sample is the first one seen)",
            theme::label_style(),
        )]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), parts[0]);

    // Axis caption above the sparkline so the user can read time→.
    let axis_caption = Line::from(Span::styled(
        format!(
            "  events over time   {}  →  {}",
            short_ts(group.first_at),
            short_ts(group.last_at),
        ),
        theme::label_style(),
    ));
    frame.render_widget(Paragraph::new(axis_caption), parts[1]);

    // Bucket timestamps across the available width and plot.
    let bucket_count = parts[2].width.max(8) as usize;
    let buckets = bucket_timestamps(&group.timestamps, bucket_count);
    let peak = buckets.iter().copied().max().unwrap_or(1).max(1);
    let spark_style = match alert.severity {
        Severity::Critical => Style::default().fg(theme::BAD),
        Severity::Warn => Style::default().fg(theme::WARN),
        Severity::Info => Style::default().fg(theme::DIM),
    };
    let spark = Sparkline::default()
        .data(&buckets[..])
        .max(peak)
        .style(spark_style);
    frame.render_widget(spark, parts[2]);

    let caption = Line::from(Span::styled(
        format!(
            "  peak bucket = {}   (one column per ≈ time slice)",
            commas(peak)
        ),
        theme::label_style(),
    ));
    frame.render_widget(Paragraph::new(caption), parts[3]);
}

fn render_generic_detail(alert: &Alert, frame: &mut Frame<'_>, area: Rect) {
    let (tag, tag_style) = severity_tag(alert.severity);
    let kind_label = match &alert.kind {
        AlertKind::ClusterSlotsShutdownObserved => "cluster-slots service shutdown",
        AlertKind::StandstillObserved { .. } => "standstill observation",
        AlertKind::LeaderTimeoutCrashed { .. } => "leader timeout-crashed observation",
        AlertKind::IdentityChanged => "operator identity rotation",
        _ => "single-event marker",
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(tag, tag_style),
            Span::raw("  "),
            Span::styled(kind_label, theme::accent_style()),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("at        ", theme::label_style()),
            Span::styled(fmt_ts(alert.at), theme::value_style()),
        ]),
        Line::raw(""),
        Line::from(vec![Span::styled("description", theme::label_style())]),
        Line::from(vec![Span::styled(
            alert.description.clone(),
            theme::value_style(),
        )]),
        Line::raw(""),
        Line::from(vec![Span::styled(
            "  (this alert is a single-event marker — no per-line timestamps to plot)",
            theme::label_style(),
        )]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Bucket `timestamps` into `n` equal-width bins across [first..=last].
/// Empty when timestamps are empty; falls back to single bin if span=0.
fn bucket_timestamps(timestamps: &[time::OffsetDateTime], n: usize) -> Vec<u64> {
    let n = n.max(1);
    if timestamps.is_empty() {
        return vec![0; n];
    }
    let first = timestamps[0];
    let last = timestamps[timestamps.len() - 1];
    let total_ns = (last - first).whole_nanoseconds().max(1);
    let mut buckets = vec![0u64; n];
    for ts in timestamps {
        let elapsed_ns = (*ts - first).whole_nanoseconds().max(0);
        // Map elapsed/total into [0, n). Saturating arithmetic keeps the
        // hottest end-of-range timestamp in the last bucket.
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let idx = ((elapsed_ns as f64 / total_ns as f64) * n as f64) as usize;
        let idx = idx.min(n - 1);
        buckets[idx] = buckets[idx].saturating_add(1);
    }
    buckets
}

// ---------- Helpers ----------

fn humanize_dur(secs: i64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

fn fmt_ts(t: time::OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
    )
}

fn short_ts(t: time::OffsetDateTime) -> String {
    format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second())
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
