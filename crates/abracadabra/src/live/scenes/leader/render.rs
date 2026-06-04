//! Ratatui rendering for the block-production pane.
//!
//! Free functions that take immutable references to pane state
//! ([`super::state::LeaderPane`], [`super::state::OurWindow`],
//! [`super::state::OurSlot`]) and a [`Frame`] sink. The split keeps
//! all `ratatui::*` use out of the state module.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::live::animation::spinner_frame;
use crate::tui::theme;

use super::format::{
    format_count_compact, slot_detail_compact, CARD_ROW_WIDTH, COLUMN_HEADER, DETAIL_WIDTH,
    SLOT_FIELD_WIDTH,
};
use super::state::{LeaderPane, OurSlot, OurWindow, SlotOutcome};

/// Card inner content height: 1 blank · 1 column header · 1 blank ·
/// 4 slot rows · 1 alert-or-blank row.
pub(super) const CARD_INNER_HEIGHT: u16 = 8;

/// Minimum widget width to render a single card. Derived from the
/// actual row geometry — no hardcoded magic number.
pub(super) const MIN_ONE_CARD_WIDTH: u16 = CARD_ROW_WIDTH as u16;
/// Minimum widget width to render two cards with the 1-col separator
/// between them. Below this we fall back to the single-card path.
pub(super) const MIN_TWO_CARD_WIDTH: u16 = (CARD_ROW_WIDTH * 2 + 1) as u16;

/// Render the entire pane (border, headline, cards) inside `area`.
pub(super) fn render(pane: &LeaderPane, frame: &mut Frame<'_>, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" block production ")
        .title_style(theme::title_style())
        .border_style(theme::title_style());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 20 || inner.height == 0 {
        return;
    }

    // Top blank · headline · cards area (each card carries its
    // own column header).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // top blank
            Constraint::Length(1), // headline
            Constraint::Min(0),    // cards
        ])
        .split(inner);

    render_headline(pane, frame, chunks[1]);
    render_windows(pane, frame, chunks[2]);
}

fn render_headline(pane: &LeaderPane, frame: &mut Frame<'_>, area: Rect) {
    let spinner = spinner_frame(pane.event_count, pane.last_event_at);

    let s = pane.summary();
    let spinner_span = Span::styled(
        format!(" {spinner} "),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    if !s.has_windows {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                spinner_span,
                Span::styled(
                    "waiting for first leader window",
                    Style::default().fg(Color::DarkGray),
                ),
            ])),
            area,
        );
        return;
    }

    let mut spans = vec![spinner_span];
    if let Some(ms) = s.bank_ms_avg {
        spans.push(Span::styled("bank avg ", theme::label_style()));
        spans.push(Span::styled(format!("{ms}ms"), theme::value_style()));
    }
    if let Some(max) = s.sig_max {
        spans.push(Span::styled("   sig max ", theme::label_style()));
        spans.push(Span::styled(
            format_count_compact(max),
            theme::value_style().add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(max) = s.sh_max {
        spans.push(Span::styled("   sh max ", theme::label_style()));
        spans.push(Span::styled(
            format_count_compact(max),
            theme::value_style().add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Render the recent leader-window cards.
///
/// **Layout convention:** newest window on the LEFT (and only card
/// when only one fits). Operators read top-of-screen as "freshest
/// data"; this matches that scan order rather than the more common
/// left-to-right time-series convention. The decision is intentional
/// and called out here so future contributors don't "fix" it.
///
/// **Width fallback ladder:**
///
/// - `width >= MIN_TWO_CARD_WIDTH` — render two cards (newest left,
///   one prior right) with a 1-col separator between them.
/// - `width >= MIN_ONE_CARD_WIDTH` — render the newest window only,
///   full width.
/// - otherwise — render a one-line "widget too narrow" message so
///   the pane is never silently blank.
fn render_windows(pane: &LeaderPane, frame: &mut Frame<'_>, area: Rect) {
    if pane.windows.is_empty() || area.height < CARD_INNER_HEIGHT {
        return;
    }
    if area.width >= MIN_TWO_CARD_WIDTH {
        // Two cards side by side with a 1-col vertical separator
        // between them. Newest window on the left. Each card's
        // CARD_ROW_WIDTH-wide content is horizontally centered inside
        // its half-area so the right card does not hug the separator.
        let cells = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(area);
        let mut iter = pane.windows.iter().rev();
        for cell in &[cells[0], cells[2]] {
            let Some(w) = iter.next() else {
                break;
            };
            render_card(frame, centered_card_area(*cell), w);
        }
        render_separator(frame, cells[1]);
    } else if area.width >= MIN_ONE_CARD_WIDTH {
        // Single-card fallback at narrow widths — newest window only.
        // Centered horizontally inside the full area for the same
        // reason as the two-card path.
        if let Some(w) = pane.windows.back() {
            render_card(frame, centered_card_area(area), w);
        }
    } else {
        // Widget too narrow even for one card — surface a one-line
        // message so the pane is never silently blank.
        let line = Line::from(Span::styled(
            " widget too narrow — resize terminal ",
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(Paragraph::new(line), area);
    }
}

/// Pull a CARD_ROW_WIDTH-wide sub-rect from the centre of `cell`,
/// keeping the original top alignment. When `cell.width <= CARD_ROW_WIDTH`
/// the input area is returned unchanged so narrow paths still render.
const fn centered_card_area(cell: Rect) -> Rect {
    let card_w = CARD_ROW_WIDTH as u16;
    if cell.width <= card_w {
        return cell;
    }
    let offset = (cell.width - card_w) / 2;
    Rect::new(cell.x + offset, cell.y, card_w, cell.height)
}

/// Render a dashed gray separator between the two cards. Glyph and
/// styling mirror the `shred_streams` pane's `CARD_DIVIDER` so all
/// Live-tab dividers read alike. Height is capped at the card's
/// content height (`CARD_INNER_HEIGHT`) so the separator does not
/// extend through the empty space below the cards.
///
/// Writes directly into the frame buffer rather than building a
/// `Vec<Line>` and a `Paragraph` — the previous form allocated
/// `area.height` `Line`s + `Span`s per frame for a constant glyph.
fn render_separator(frame: &mut Frame<'_>, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let span = area.height.min(CARD_INNER_HEIGHT);
    let buf = frame.buffer_mut();
    for dy in 0..span {
        buf.set_string(area.x, area.y + dy, "┊", style);
    }
}

fn render_card(frame: &mut Frame<'_>, area: Rect, w: &OurWindow) {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(CARD_INNER_HEIGHT as usize);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        COLUMN_HEADER,
        theme::label_style(),
    )));
    lines.push(Line::from(""));
    for s in &w.slots {
        lines.push(card_slot_line(s));
    }
    // Alert footer or blank — keeps card height constant regardless
    // of whether any alerts fired this window.
    lines.push(card_alert_line(w).unwrap_or_else(|| Line::from("")));
    frame.render_widget(Paragraph::new(lines), area);
}

/// Per-card footer that surfaces nonzero counts from three
/// `solana_metrics::metrics` datapoint fields, summed across the
/// window's slots. Returns `None` when every counter across every
/// slot in the window is zero — silence is the correct default for
/// a healthy window.
///
/// **Threshold = 1.** Any nonzero count surfaces the footer. The
/// underlying fields are all operationally interesting at any nonzero
/// value (a single dropped transaction or a single replay lag is a
/// real signal on a leader slot), so the noise/signal trade favors
/// surfacing low values. If empirical operation later shows this is
/// too noisy, raise per-field thresholds rather than gating on the
/// aggregate.
///
/// Mnemonics map to verbatim datapoint field names:
///
/// - `drops` — `banking_stage_scheduler_slot_counts.num_dropped_on_capacity`.
///   Banking-stage scheduler dropped transactions because the per-slot
///   capacity was exhausted. Summed across the window's slots.
/// - `bad-handover` — `slot-metrics.leader_handover_sad`. Validator's
///   own 1/0 flag on a bad handover from the prior leader. Counted as
///   `1` per slot it fired (so the value is "slots in this window
///   with a bad handover"). Display label is descriptive; the source
///   field name stays `leader_handover_sad` for grep parity.
/// - `behind` — `slot-metrics.replay_is_behind_count`. Number of
///   times replay lagged during the slot. Summed across the window's
///   slots.
pub(super) fn card_alert_line(w: &OurWindow) -> Option<Line<'static>> {
    let mut drops = 0u64;
    let mut bad_handover = 0u64;
    let mut behind = 0u64;
    for s in &w.slots {
        drops = drops.saturating_add(s.num_dropped_on_capacity.unwrap_or(0));
        if s.leader_handover_sad == Some(true) {
            bad_handover = bad_handover.saturating_add(1);
        }
        behind = behind.saturating_add(s.replay_is_behind_count.unwrap_or(0));
    }
    if drops == 0 && bad_handover == 0 && behind == 0 {
        return None;
    }
    let warn = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    let label = Style::default().fg(Color::Red);
    let mut spans = vec![Span::styled("    ⚠ ", warn)];
    let mut first = true;
    for (n, name) in [
        (drops, "drops"),
        (bad_handover, "bad-handover"),
        (behind, "behind"),
    ] {
        if n == 0 {
            continue;
        }
        if !first {
            spans.push(Span::styled("  ·  ", label));
        }
        spans.push(Span::styled(format!("{name} {n}"), warn));
        first = false;
    }
    Some(Line::from(spans))
}

/// Per-slot row. Dual-channel design:
///
/// - The **icon** comes from [`OurSlot::status`] — skip-vote
///   precedence wins over abandoned (a locally banked fork is still
///   protocol-skipped, and the network-side skip vote is the right
///   single-glyph signal). So a slot with both a `Voting skip` AND
///   an `Unable to produce window` covering it renders `[✗]`.
/// - The **detail body** independently checks `abandoned_at` — if
///   set and a verbatim `abandoned_reason` is recorded, it replaces
///   the `bank/sigs/bcast/sh/fin` column row with that reason text.
///   This way both pieces of evidence stay visible: the protocol
///   category from the icon, and the validator's own stated reason
///   from the row body. Neither suppresses the other.
pub(super) fn card_slot_line(s: &OurSlot) -> Line<'static> {
    let (icon, icon_style) = slot_icon(s.status());
    let slot_field = format!("{:>w$}", s.slot, w = SLOT_FIELD_WIDTH);
    // No explicit gap between slot field and detail — the bank value's
    // own right-align padding (5-char field for a 3-digit value gives
    // "  ") supplies a 2-col visual gap. Row total = 44 cols so two
    // cards + 1-col separator fit cleanly in an 89-col widget.
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(icon, icon_style),
        Span::raw(" "),
        Span::styled(slot_field, theme::value_style()),
    ];
    if let (Some(_), Some(reason)) = (s.abandoned_at, s.abandoned_reason.as_deref()) {
        // Two-space gap mirrors the leading padding of `slot_detail_compact`
        // (a 5-col `BANK_MS_FIELD_WIDTH` field holding a 3-digit value
        // starts with two spaces). Keeps the column-header / icon /
        // slot-field prefix alignment stable across both row variants.
        // NIT-01: cap the reason body to `DETAIL_WIDTH` so mainnet-scale
        // slot numbers in the reason text (e.g.
        // `PohRecorder(WindowMovedOn(300123456))` at 33 chars + 2-space
        // prefix) cannot spill past the card column into the right
        // card. Truncated reasons end with `…`; the slot column already
        // shows the slot number directly.
        let body = format!("  {reason}");
        let body = if body.chars().count() > DETAIL_WIDTH {
            let truncated: String = body.chars().take(DETAIL_WIDTH.saturating_sub(1)).collect();
            format!("{truncated}…")
        } else {
            body
        };
        spans.push(Span::styled(body, theme::bad_style()));
    } else {
        spans.push(Span::styled(slot_detail_compact(s), theme::label_style()));
    }
    Line::from(spans)
}

/// Per-status icon glyph and style. Two distinct red-bold glyphs
/// distinguish the underlying signal:
///
/// - `[✗]` for `Skipped` — we cast a `Voting skip` or `Voting
///   skip-fallback` for the slot. Network-side ground truth.
/// - `[A]` for `Abandoned` — our `block_creation_loop` emitted
///   `Unable to produce window … skipping window: <reason>`. Local
///   block-creation ground truth.
///
/// Both use `theme::bad_style()` (red, BOLD); the glyph carries the
/// semantic difference. A slot with both signals shows `[✗]` (the
/// skip-vote precedence in [`OurSlot::status`]) but the row body
/// still surfaces the verbatim `abandoned_reason` — see
/// [`card_slot_line`] for that dual-channel render.
pub(super) fn slot_icon(status: SlotOutcome) -> (&'static str, Style) {
    let bad = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    match status {
        SlotOutcome::Produced { fast: true } => (
            "[✓]",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        SlotOutcome::Produced { fast: false } => ("[✓]", Style::default().fg(Color::Green)),
        // Banked: bank frozen — we've done OUR part — rendered in dim
        // green to read closer-to-done than `[…]` Banking (yellow,
        // still in flight). Distinct from `[✓]` Produced (bright/bold
        // green) which adds network-side finalization.
        SlotOutcome::Banked => (
            "[~]",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::DIM),
        ),
        SlotOutcome::Banking => ("[…]", Style::default().fg(Color::Yellow)),
        SlotOutcome::Abandoned => ("[A]", bad),
        SlotOutcome::Skipped { .. } => ("[✗]", bad),
        SlotOutcome::Pending => ("[ ]", Style::default().fg(Color::DarkGray)),
    }
}
