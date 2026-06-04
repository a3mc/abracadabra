//! Block production — per-slot detail for our own leader windows.
//!
//! The pane joins the event streams keyed on our `ProduceWindow`
//! ranges to produce honest per-slot status:
//!
//! - `Block (N, hash) parent (…)` — our block emitted
//! - `First shred N`              — first shred for the slot retransmitted
//! - `bank frozen N hash` … sig=K — block banked, K transactions
//! - `Finalized (N, hash) fast`   — finalization arrived (with fast/slow tag)
//! - `Voting skip for N`          — local validator cast a skip vote
//! - `Voting skip-fallback for N` — fallback-round skip vote
//! - `Unable to produce window N-M, skipping window: <reason>` —
//!   validator left its own leader window; the trailing `<reason>`
//!   string is preserved verbatim and displayed as the slot's reason
//!   (e.g. `PohRecorder`).
//!
//! **No inferred reasons.** `Timeout`, `TimeoutCrashedLeader`, and
//! `SafeToSkip` events that fire around our skipped slots are NOT
//! correlated with our skip votes here — the log does not state any
//! of them as the reason for a `Voting skip`, and inferring causality
//! would be unsafe to publish (the operator runs a public validator).
//! The only "reason" surfaced is the verbatim string Solana's own
//! code prints on the `Unable to produce window` line.
//!
//! Per-slot status is derived on render from the captured fields; no
//! status field is stored, so the rule is in one place and adding a
//! new event only requires extending the capture (not a state machine).
//!
//! Layout (top → bottom):
//! - 1 row: spinner + headline showing windows count, produced count,
//!   average bank time, and (when nonzero) skip count + reason breakdown
//! - N rows of cards: each card = one window's 4 slots, no header.
//!   The slot numbers themselves identify the window; status icons +
//!   colours convey produced/skipped without a textual count.
//!
//! Per-slot row format (fixed widths so multi-digit values do not
//! shift the columns to their right):
//!
//! ```text
//!  [✓] 1234567    45ms    19k    ← produced: bank time + sig count
//!  [✗] 1234568  poh-late         ← skipped:  short reason mnemonic
//! ```
//!
//! Bank time per slot is read directly from the validator's
//! `leader-slot-start-to-cleared-elapsed-ms` metric datapoint, which
//! reports the authoritative leader-slot duration. We do NOT derive
//! it from `First shred N` event timestamps because that event only
//! fires when we *receive* a first shred for slot N (i.e. somebody
//! else produced it); when we are the leader, no such event exists.
//!
//! Spinner advances on event arrival (not wall-clock elapsed), so a
//! stalled stream visibly stops the spin — making it a real liveness
//! signal rather than a screensaver.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use time::OffsetDateTime;

use crate::live::animation::Pane;
use crate::parser::{Event, EventKind, MetricEvent};
use crate::tui::theme;

/// Braille spinner frames; same cell pattern Cargo uses.
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Leader windows kept in memory (oldest dropped on overflow).
const RECENT_WINDOWS_CAPACITY: usize = 8;

/// Maximum tolerated `end - start` span on a `ProduceWindow` event.
/// Mirrors the aggregator's `MAX_LEADER_WINDOW_SPAN` defence against
/// truncated log lines that would otherwise materialise a huge range.
const MAX_WINDOW_SPAN: u64 = 32;

/// Per-slot observation state inside one of our `ProduceWindow`s.
///
/// All fields are pure observations; the derived status is computed
/// on render from these timestamps. Times are parsed log timestamps
/// so playback speed does not affect derivations.
#[derive(Debug, Default, Clone)]
struct OurSlot {
    slot: u64,
    block_at: Option<OffsetDateTime>,
    bank_frozen_at: Option<OffsetDateTime>,
    finalized_at: Option<OffsetDateTime>,
    fast_finalize: Option<bool>,
    sig_count: Option<u64>,
    /// Authoritative leader-slot duration in milliseconds reported by
    /// the validator itself via the `leader-slot-start-to-cleared-elapsed-ms`
    /// metric datapoint. The `First shred N` event does **not** fire
    /// for slots we produce as leader (it only fires when we *receive*
    /// a first shred from elsewhere), so subtracting log timestamps
    /// would never yield a value for our own slots. This metric is
    /// what the validator emits for every slot it produced; using it
    /// directly is the only honest source.
    leader_elapsed_ms: Option<u64>,
    /// `broadcast-process-shreds-stats.slot_broadcast_time` (µs).
    /// `None` when the slot was abandoned mid-broadcast (the validator
    /// emits `-1` on the `-interrupted-stats` variant).
    broadcast_us: Option<u64>,
    /// `broadcast-process-shreds-stats.num_data_shreds`. Set on both
    /// the normal and `-interrupted-stats` variants — for an
    /// abandoned slot this is how many data shreds we shipped before
    /// clearing.
    num_data_shreds: Option<u64>,
    /// `banking_stage_scheduler_slot_counts.num_finished` — txns the
    /// banking-stage scheduler finished executing for this slot.
    num_finished: Option<u64>,
    /// `banking_stage_scheduler_slot_counts.num_dropped_on_capacity`.
    /// Normally 0; nonzero is the **only** kind we surface in the
    /// card alert footer because it's actionable (banking buffer
    /// pressure).
    num_dropped_on_capacity: Option<u64>,
    /// `slot-metrics.leader_handover_sad` — validator's 1/0 flag for
    /// a bad handover from the prior leader.
    leader_handover_sad: Option<bool>,
    /// `slot-metrics.replay_is_behind_count` — count of times replay
    /// lagged during this slot. Normally 0.
    replay_is_behind_count: Option<u64>,
    /// Did we cast `Voting skip` for this slot. Direct observation.
    voted_skip_at: Option<OffsetDateTime>,
    /// Did we cast `Voting skip-fallback` for this slot. Direct
    /// observation. Distinct from `voted_skip_at` because the two
    /// vote types are different protocol rounds.
    voted_skip_fallback_at: Option<OffsetDateTime>,
    /// `Unable to produce window … skipping window: <reason>` fired.
    /// `abandoned_reason` carries the verbatim trailing text from that
    /// line — that string is the validator's own stated reason, so it
    /// is safe to display as a reason.
    abandoned_at: Option<OffsetDateTime>,
    abandoned_reason: Option<String>,
}

/// One of our `ProduceWindow` ranges. Slots are inclusive `start..=end`.
#[derive(Debug, Clone)]
struct OurWindow {
    start: u64,
    end: u64,
    slots: Vec<OurSlot>,
}

/// Derived per-slot status. Computed in [`OurSlot::status`].
///
/// **No inferred reasons.** Variants reflect only what the log
/// literally states for this slot:
///
/// - `Abandoned` corresponds to a `Unable to produce window N-M,
///   skipping window: <reason>` ERROR line. The trailing `<reason>`
///   is the validator's own stated reason and is preserved verbatim
///   on [`OurSlot::abandoned_reason`].
/// - `Skipped` corresponds to a `Voting skip` and/or `Voting
///   skip-fallback` line. The log does NOT state a reason for these
///   votes; correlating preceding `Timeout` / `TimeoutCrashedLeader`
///   / `SafeToSkip` events with the skip vote would be inference, so
///   no such labels are claimed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotOutcome {
    /// No producing/skipping signal yet; the slot is in flight or
    /// strictly newer than the latest observation cursor.
    Pending,
    /// `bank_frozen` + `finalized` observed; the slot landed on chain.
    Produced { fast: bool },
    /// `bank_frozen` observed but no `finalized` yet.
    Banked,
    /// `block` observed but no `bank_frozen` yet — banking is in flight.
    Banking,
    /// `Unable to produce window … skipping window: <reason>` fired
    /// for this slot. Display reads `OurSlot::abandoned_reason` for
    /// the verbatim reason text.
    Abandoned,
    /// `Voting skip` and/or `Voting skip-fallback` cast for this slot.
    /// `fallback` is `true` iff the fallback vote was cast; both votes
    /// can be cast for the same slot, in which case `fallback` is true.
    Skipped { fallback: bool },
}

impl OurSlot {
    fn status(&self) -> SlotOutcome {
        // Casting a skip vote — whether `Voting skip` (round 1) or
        // `Voting skip-fallback` (round 2) — is the strongest "we
        // did not produce this slot canonically" signal in the log.
        // It overrides `bank_frozen` because we can locally bank a
        // fork block that never becomes canonical: the banking
        // pipeline still emits BankFrozen / num_finished / shred
        // counts for that work, but the network skipped the slot.
        // The skip vote is the network-side ground truth.
        if self.voted_skip_at.is_some() || self.voted_skip_fallback_at.is_some() {
            return SlotOutcome::Skipped {
                fallback: self.voted_skip_fallback_at.is_some(),
            };
        }
        if self.abandoned_at.is_some() {
            return SlotOutcome::Abandoned;
        }
        if self.bank_frozen_at.is_some() {
            if self.finalized_at.is_some() {
                return SlotOutcome::Produced {
                    fast: self.fast_finalize.unwrap_or(false),
                };
            }
            return SlotOutcome::Banked;
        }
        if self.block_at.is_some() {
            return SlotOutcome::Banking;
        }
        SlotOutcome::Pending
    }
}

pub struct LeaderPane {
    /// FIFO of our recent ProduceWindow ranges, with per-slot state.
    windows: VecDeque<OurWindow>,
    /// Count of events observed since the pane was constructed.
    /// Drives the spinner — every Nth event ticks one frame, so the
    /// spinner pauses when the stream is silent (honest liveness).
    event_count: u64,
    /// Wall-clock instant of the most recent event observation. The
    /// spinner only advances if events arrived within the last
    /// [`SPINNER_LIVE_WINDOW`]; otherwise the cell freezes. Together
    /// with `event_count` this gives both a paused-when-quiet visual
    /// and a tickless idle state.
    last_event_at: Option<Instant>,
}

/// Wall-clock window over which event arrivals still count as
/// "stream live". Past this, the spinner freezes on its last frame.
const SPINNER_LIVE_WINDOW: Duration = Duration::from_millis(750);

/// Events per spinner frame. Each event nudges the spinner by one
/// step; 1 → maximum responsiveness but fast in steady-state, 4 → calm.
const SPINNER_EVENTS_PER_FRAME: u64 = 4;

/// Stats that the operator cannot read by glancing at the cards.
/// Slot counts are deliberately omitted — those are visible.
#[derive(Debug, Default, Clone, Copy)]
struct WindowSummary {
    /// Any retained windows at all.
    has_windows: bool,
    /// Mean of `leader-slot-start-to-cleared-elapsed-ms` across produced slots.
    bank_ms_avg: Option<i64>,
    /// Max `signature_count` across retained produced slots.
    sig_max: Option<u64>,
    /// Max `num_data_shreds` across retained produced slots.
    sh_max: Option<u64>,
}

impl LeaderPane {
    pub fn new() -> Self {
        Self {
            windows: VecDeque::with_capacity(RECENT_WINDOWS_CAPACITY),
            event_count: 0,
            last_event_at: None,
        }
    }

    fn summary(&self) -> WindowSummary {
        let mut out = WindowSummary {
            has_windows: !self.windows.is_empty(),
            ..WindowSummary::default()
        };
        let mut bank_total_us: i128 = 0;
        let mut bank_n: i128 = 0;
        for w in &self.windows {
            for s in &w.slots {
                if matches!(
                    s.status(),
                    SlotOutcome::Produced { .. } | SlotOutcome::Banked
                ) {
                    if let Some(ms) = bank_ms(s) {
                        bank_total_us = bank_total_us.saturating_add(i128::from(ms) * 1000);
                        bank_n = bank_n.saturating_add(1);
                    }
                    if let Some(c) = s.sig_count {
                        out.sig_max = Some(out.sig_max.map_or(c, |m| m.max(c)));
                    }
                    if let Some(sh) = s.num_data_shreds {
                        out.sh_max = Some(out.sh_max.map_or(sh, |m| m.max(sh)));
                    }
                }
            }
        }
        if bank_n > 0 {
            out.bank_ms_avg = i64::try_from(bank_total_us / bank_n / 1000).ok();
        }
        out
    }

    fn window_for_slot_mut(&mut self, slot: u64) -> Option<&mut OurWindow> {
        self.windows
            .iter_mut()
            .rev()
            .find(|w| slot >= w.start && slot <= w.end)
    }

    fn slot_mut(&mut self, slot: u64) -> Option<&mut OurSlot> {
        self.window_for_slot_mut(slot)
            .and_then(|w| w.slots.iter_mut().find(|s| s.slot == slot))
    }

    fn observe_bank_frozen(&mut self, slot: u64, ts: OffsetDateTime, sig_count: u64) {
        // Cluster slot cadence lives in the chain pane (which sees all
        // slots, not just ours). Here we only need to record the
        // per-slot bank-frozen timestamp on our own slots.
        if let Some(s) = self.slot_mut(slot) {
            s.bank_frozen_at.get_or_insert(ts);
            s.sig_count.get_or_insert(sig_count);
        }
    }
}

impl Default for LeaderPane {
    fn default() -> Self {
        Self::new()
    }
}

impl Pane for LeaderPane {
    fn on_event(&mut self, ev: &Event) {
        self.event_count = self.event_count.saturating_add(1);
        self.last_event_at = Some(Instant::now());
        match &ev.kind {
            EventKind::ProduceWindow { start, end, .. } => {
                if *end < *start || end.saturating_sub(*start) > MAX_WINDOW_SPAN {
                    return;
                }
                let slots = (*start..=*end)
                    .map(|s| OurSlot {
                        slot: s,
                        ..OurSlot::default()
                    })
                    .collect();
                self.windows.push_back(OurWindow {
                    start: *start,
                    end: *end,
                    slots,
                });
                while self.windows.len() > RECENT_WINDOWS_CAPACITY {
                    self.windows.pop_front();
                }
            }
            EventKind::Block { slot, .. } => {
                if let Some(s) = self.slot_mut(*slot) {
                    s.block_at.get_or_insert(ev.ts);
                }
            }
            EventKind::Metric(MetricEvent::LeaderSlotElapsed { slot, elapsed_ms }) => {
                if let Some(s) = self.slot_mut(*slot) {
                    s.leader_elapsed_ms.get_or_insert(*elapsed_ms);
                }
            }
            EventKind::Metric(MetricEvent::BroadcastShreds {
                slot,
                broadcast_us,
                num_data_shreds,
                ..
            }) => {
                if let Some(s) = self.slot_mut(*slot) {
                    if let Some(us) = *broadcast_us {
                        s.broadcast_us.get_or_insert(us);
                    }
                    s.num_data_shreds.get_or_insert(*num_data_shreds);
                }
            }
            EventKind::Metric(MetricEvent::BankingStageCounts {
                slot,
                num_finished,
                num_dropped_on_capacity,
            }) => {
                if let Some(s) = self.slot_mut(*slot) {
                    s.num_finished.get_or_insert(*num_finished);
                    s.num_dropped_on_capacity
                        .get_or_insert(*num_dropped_on_capacity);
                }
            }
            EventKind::Metric(MetricEvent::SlotMetrics {
                slot,
                leader_handover_sad,
                replay_is_behind_count,
            }) => {
                if let Some(s) = self.slot_mut(*slot) {
                    s.leader_handover_sad.get_or_insert(*leader_handover_sad);
                    s.replay_is_behind_count
                        .get_or_insert(*replay_is_behind_count);
                }
            }
            EventKind::BankFrozen {
                slot,
                signature_count,
                ..
            } => {
                self.observe_bank_frozen(*slot, ev.ts, *signature_count);
            }
            EventKind::Finalized { slot, fast, .. } => {
                if let Some(s) = self.slot_mut(*slot) {
                    s.finalized_at.get_or_insert(ev.ts);
                    s.fast_finalize.get_or_insert(*fast);
                }
            }
            EventKind::VotingSkip { slot } => {
                if let Some(s) = self.slot_mut(*slot) {
                    s.voted_skip_at.get_or_insert(ev.ts);
                }
            }
            EventKind::VotingSkipFallback { slot } => {
                if let Some(s) = self.slot_mut(*slot) {
                    s.voted_skip_fallback_at.get_or_insert(ev.ts);
                }
            }
            EventKind::UnableToProduceWindow { start, end, reason } => {
                let ts = ev.ts;
                // The error may cover a window we never saw a ProduceWindow
                // for (log replay started mid-stream). In that case we have
                // no slots to mark — silently no-op.
                for slot in *start..=*end {
                    if let Some(s) = self.slot_mut(slot) {
                        s.abandoned_at.get_or_insert(ts);
                        s.abandoned_reason.get_or_insert_with(|| reason.clone());
                    }
                }
            }
            _ => {}
        }
    }

    fn tick(&mut self, _now: Instant) {
        // No wall-clock state to advance; spinner derives from
        // `event_count` / `last_event_at`, both updated in `on_event`.
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
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

        self.render_headline(frame, chunks[1]);
        self.render_windows(frame, chunks[2]);
    }
}

impl LeaderPane {
    fn render_headline(&self, frame: &mut Frame<'_>, area: Rect) {
        let spinner_idx = if self
            .last_event_at
            .is_some_and(|t| Instant::now().duration_since(t) < SPINNER_LIVE_WINDOW)
        {
            usize::try_from(self.event_count / SPINNER_EVENTS_PER_FRAME).unwrap_or(0)
                % SPINNER.len()
        } else {
            0
        };
        let spinner = SPINNER[spinner_idx];

        let s = self.summary();
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
                format_sig_max(max),
                theme::value_style().add_modifier(Modifier::BOLD),
            ));
        }
        if let Some(max) = s.sh_max {
            spans.push(Span::styled("   sh max ", theme::label_style()));
            spans.push(Span::styled(
                format_sig_max(max),
                theme::value_style().add_modifier(Modifier::BOLD),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_windows(&self, frame: &mut Frame<'_>, area: Rect) {
        if self.windows.is_empty() || area.height < CARD_INNER_HEIGHT || area.width < MIN_CARD_WIDTH
        {
            return;
        }
        // Two cards side by side with a 1-col vertical separator
        // between them. Newest window on the left.
        let cells = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(area);
        let mut iter = self.windows.iter().rev();
        for (idx, cell) in [cells[0], cells[2]].iter().enumerate() {
            if idx >= MAX_VISIBLE_CARDS {
                break;
            }
            let Some(w) = iter.next() else {
                break;
            };
            render_card(frame, *cell, w);
        }
        render_separator(frame, cells[1]);
    }
}

/// Render a vertical `│` line down the column between the two cards.
/// Uses the title style so the separator reads as a deliberate divider
/// rather than ghosted text — visible at normal contrast.
fn render_separator(frame: &mut Frame<'_>, area: Rect) {
    let style = theme::title_style();
    let lines: Vec<Line<'static>> = (0..area.height)
        .map(|_| Line::from(Span::styled("│", style)))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// Card inner content height: 1 blank · 1 column header · 1 blank ·
/// 4 slot rows · 1 alert-or-blank row.
const CARD_INNER_HEIGHT: u16 = 8;
/// Total cards visible at once. Newest on the left.
const MAX_VISIBLE_CARDS: usize = 2;
/// Minimum widget width below which the cards do not render. Two
/// cards × ~40 col content (rightmost columns may clip on narrower
/// terminals) + 1 col separator + margin. Permissive so the
/// horizontal layout still attempts at common pane widths.
const MIN_CARD_WIDTH: u16 = 80;
/// Column header positioned over the per-slot row's data columns
/// (see [`slot_detail_compact`]). Labels right-align with their
/// respective value columns.
const COLUMN_HEADER: &str = "        slot   bank   sigs  bcast     sh   fin";

/// Width budget for the slot-number field inside a card. Aligned right
/// so multi-digit slot numbers don't shift the columns to their right.
const SLOT_FIELD_WIDTH: usize = 7;
/// Width budget for the bank-time field (`NNNms`, right-aligned).
const BANK_MS_FIELD_WIDTH: usize = 5;
/// Width budget for the signature-count field, post-compaction
/// (right-aligned). Values ≥1 000 compact to `Nk`.
const SIGS_FIELD_WIDTH: usize = 4;
/// Width budget for the shred-count field, post-compaction
/// (right-aligned). Same compaction rule as sigs.
const SHREDS_FIELD_WIDTH: usize = 4;

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

/// Per-card footer that surfaces nonzero counts from the slot
/// `dropped_on_capacity`, `leader_handover_sad`, and `replay_is_behind`
/// values. Returns `None` when every counter across every slot in
/// the window is zero — silence is the correct default for a healthy
/// window.
fn card_alert_line(w: &OurWindow) -> Option<Line<'static>> {
    let mut drops = 0u64;
    let mut sad = 0u64;
    let mut behind = 0u64;
    for s in &w.slots {
        drops = drops.saturating_add(s.num_dropped_on_capacity.unwrap_or(0));
        if s.leader_handover_sad == Some(true) {
            sad = sad.saturating_add(1);
        }
        behind = behind.saturating_add(s.replay_is_behind_count.unwrap_or(0));
    }
    if drops == 0 && sad == 0 && behind == 0 {
        return None;
    }
    let warn = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    let label = Style::default().fg(Color::Red);
    let mut spans = vec![Span::styled("    ⚠ ", warn)];
    let mut first = true;
    for (n, name) in [(drops, "drops"), (sad, "sad"), (behind, "behind")] {
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

fn card_slot_line(s: &OurSlot) -> Line<'static> {
    let (icon, icon_style) = slot_icon(s.status());
    let slot_field = format!("{:>w$}", s.slot, w = SLOT_FIELD_WIDTH);
    let detail = slot_detail_compact(s);
    Line::from(vec![
        Span::raw(" "),
        Span::styled(icon, icon_style),
        Span::raw(" "),
        Span::styled(slot_field, theme::value_style()),
        Span::raw("  "),
        Span::styled(detail, theme::label_style()),
    ])
}

fn slot_icon(status: SlotOutcome) -> (&'static str, Style) {
    match status {
        SlotOutcome::Produced { fast: true } => (
            "[✓]",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        SlotOutcome::Produced { fast: false } => ("[✓]", Style::default().fg(Color::Green)),
        SlotOutcome::Banked => ("[~]", Style::default().fg(Color::Yellow)),
        SlotOutcome::Banking => ("[…]", Style::default().fg(Color::Yellow)),
        SlotOutcome::Abandoned | SlotOutcome::Skipped { .. } => (
            "[✗]",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        SlotOutcome::Pending => ("[ ]", Style::default().fg(Color::DarkGray)),
    }
}

/// Card-form per-slot detail. Same fixed-width column layout for
/// every status — produced and abandoned slots both render
/// `bank | sigs | bcast | sh`. Missing values become `—` so the
/// columns stay aligned. The status icon already conveys produced /
/// banking / abandoned / skipped — no extra label text is needed.
///
/// All values come verbatim from `solana_metrics::metrics` datapoints:
///
/// - `bank` — `leader-slot-start-to-cleared-elapsed-ms.elapsed`.
/// - `sigs` — `bank frozen` line's `signature_count`.
/// - `bcast` — `broadcast-process-shreds-stats.slot_broadcast_time`
///   (ms). For slots the validator abandoned mid-broadcast this is
///   `—` (the `-interrupted-stats` variant emits `-1`).
/// - `sh` — `broadcast-process-shreds-stats.num_data_shreds` (same
///   field on the `-interrupted-stats` variant for partial slots).
fn slot_detail_compact(s: &OurSlot) -> String {
    let bank = format_bank_field(bank_ms(s));
    let sigs = format_sigs_field(s.sig_count);
    let bcast = format_bank_field(broadcast_ms(s));
    let shreds = format_shreds_field(s.num_data_shreds);
    let fin = format_sigs_field(s.num_finished);
    format!("{bank}ms {sigs}  {bcast}ms {shreds}  {fin}")
}

/// Whole-millisecond broadcast time. Sourced from
/// `broadcast-process-shreds-stats.slot_broadcast_time` (µs).
fn broadcast_ms(s: &OurSlot) -> Option<i64> {
    s.broadcast_us.and_then(|us| i64::try_from(us / 1000).ok())
}

/// Right-align the shred count to [`SHREDS_FIELD_WIDTH`] cols, with
/// the same `Nk` compaction rule as signatures.
fn format_shreds_field(n: Option<u64>) -> String {
    let w = SHREDS_FIELD_WIDTH;
    let s = n.map_or_else(
        || "—".to_owned(),
        |v| {
            if v >= 1000 {
                format!("{}k", v / 1000)
            } else {
                v.to_string()
            }
        },
    );
    format!("{s:>w$}")
}

/// Leader-slot elapsed time in whole milliseconds.
///
/// Sourced directly from the validator's `leader-slot-start-to-cleared-elapsed-ms`
/// metric datapoint. Not derivable from event timestamps for our own
/// leader slots because `First shred N` only fires when we *receive*
/// a first shred for slot N, never when we *produce* N ourselves.
fn bank_ms(s: &OurSlot) -> Option<i64> {
    s.leader_elapsed_ms.and_then(|v| i64::try_from(v).ok())
}

/// Right-align the bank-time number to [`BANK_MS_FIELD_WIDTH`] cols.
/// Renders `—` (centered) when no sample is available so the column
/// stays the same visual width.
fn format_bank_field(ms: Option<i64>) -> String {
    let w = BANK_MS_FIELD_WIDTH;
    ms.map_or_else(|| format!("{:>w$}", "—"), |v| format!("{v:>w$}"))
}

/// Right-align the signature count to [`SIGS_FIELD_WIDTH`] cols.
/// Values ≥ 1 000 compact to `Nk` so the field stays narrow.
fn format_sigs_field(n: Option<u64>) -> String {
    let w = SIGS_FIELD_WIDTH;
    let s = n.map_or_else(
        || "—".to_owned(),
        |v| {
            if v >= 1000 {
                format!("{}k", v / 1000)
            } else {
                v.to_string()
            }
        },
    );
    format!("{s:>w$}")
}

/// Render the peak signature count. `<1 000` literal; `1 000`-`999 999`
/// compacted to `Nk` (integer thousand); `≥1 000 000` to `N.Nm`.
fn format_sig_max(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{}k", n / 1_000)
    } else {
        let m = n as f64 / 1_000_000.0;
        format!("{m:.1}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(kind: EventKind) -> Event {
        Event {
            ts: time::OffsetDateTime::UNIX_EPOCH,
            kind,
        }
    }

    fn pw(start: u64, end: u64) -> EventKind {
        EventKind::ProduceWindow {
            start,
            end,
            parent_slot: start.saturating_sub(1),
            parent_hash: "x".into(),
        }
    }

    #[test]
    fn produce_window_creates_pending_slots() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        assert_eq!(p.windows.len(), 1);
        let w = &p.windows[0];
        assert_eq!(w.slots.len(), 4);
        for (i, s) in w.slots.iter().enumerate() {
            assert_eq!(s.slot, 100 + i as u64);
            assert!(matches!(s.status(), SlotOutcome::Pending));
        }
    }

    #[test]
    fn malformed_window_rejected() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(200, 100)));
        p.on_event(&mk(pw(0, u64::MAX)));
        assert_eq!(p.windows.len(), 0);
    }

    #[test]
    fn full_produced_path_sets_status_to_produced_fast() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::BankFrozen {
            slot: 100,
            hash: "y".into(),
            signature_count: 42,
        }));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "y".into(),
            fast: true,
        }));
        let s = &p.windows[0].slots[0];
        assert!(matches!(s.status(), SlotOutcome::Produced { fast: true }));
        assert_eq!(s.sig_count, Some(42));
    }

    #[test]
    fn skip_vote_wins_over_bank_frozen_for_status() {
        // We can locally bank a fork block whose slot the network
        // ultimately skipped — the banking pipeline still emits
        // BankFrozen with a sig_count. The skip vote we cast is the
        // ground truth: "this slot did not produce canonically".
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        // Locally bank slot 100 (could be our fork, not canonical).
        p.on_event(&mk(EventKind::BankFrozen {
            slot: 100,
            hash: "fork".into(),
            signature_count: 67_000,
        }));
        // Cast skip-fallback for the same slot — the canonical chain
        // skipped this slot.
        p.on_event(&mk(EventKind::VotingSkipFallback { slot: 100 }));
        assert!(matches!(
            p.windows[0].slots[0].status(),
            SlotOutcome::Skipped { fallback: true }
        ));
    }

    #[test]
    fn unable_to_produce_window_marks_all_slots_abandoned_with_verbatim_reason() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::UnableToProduceWindow {
            start: 100,
            end: 103,
            reason: "PohRecorder(WindowMovedOn(103))".into(),
        }));
        for s in &p.windows[0].slots {
            assert!(matches!(s.status(), SlotOutcome::Abandoned));
            // Reason text is preserved verbatim from the log line.
            assert_eq!(
                s.abandoned_reason.as_deref(),
                Some("PohRecorder(WindowMovedOn(103))")
            );
        }
    }

    #[test]
    fn skipped_status_does_not_infer_reason_from_surrounding_events() {
        // Even with Timeout / SafeToSkip / TimeoutCrashedLeader events
        // observed in the surrounding stream, the status is `Skipped`
        // without any reason label. The log does NOT state these as
        // the reason for our vote — claiming them would be inference.
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::Timeout { slot: 101 }));
        p.on_event(&mk(EventKind::VotingSkip { slot: 101 }));
        p.on_event(&mk(EventKind::SafeToSkip { slot: 102 }));
        p.on_event(&mk(EventKind::VotingSkip { slot: 102 }));
        p.on_event(&mk(EventKind::TimeoutCrashedLeader { slot: 103 }));
        p.on_event(&mk(EventKind::VotingSkip { slot: 103 }));
        assert!(matches!(
            p.windows[0].slots[1].status(),
            SlotOutcome::Skipped { fallback: false }
        ));
        assert!(matches!(
            p.windows[0].slots[2].status(),
            SlotOutcome::Skipped { fallback: false }
        ));
        assert!(matches!(
            p.windows[0].slots[3].status(),
            SlotOutcome::Skipped { fallback: false }
        ));
    }

    #[test]
    fn bank_ms_comes_from_leader_slot_elapsed_metric() {
        // The validator emits the authoritative leader-slot duration as
        // a metric datapoint; the per-slot bank time must come from
        // there, not from event-timestamp subtraction (which is
        // impossible for our slots — see [`OurSlot::leader_elapsed_ms`]).
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::Metric(MetricEvent::LeaderSlotElapsed {
            slot: 100,
            elapsed_ms: 400,
        })));
        assert_eq!(bank_ms(&p.windows[0].slots[0]), Some(400));
    }

    #[test]
    fn summary_tracks_bank_avg_and_sig_max_over_produced_slots() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        // Two produced slots with bank ms + sig counts; mean = 350,
        // max sig = 19000.
        for (slot, elapsed, sigs) in [(100u64, 400u64, 12_000u64), (101, 300, 19_000)] {
            p.on_event(&mk(EventKind::Metric(MetricEvent::LeaderSlotElapsed {
                slot,
                elapsed_ms: elapsed,
            })));
            p.on_event(&mk(EventKind::BankFrozen {
                slot,
                hash: "h".into(),
                signature_count: sigs,
            }));
            p.on_event(&mk(EventKind::Finalized {
                slot,
                hash: "h".into(),
                fast: true,
            }));
        }
        let s = p.summary();
        assert!(s.has_windows);
        assert_eq!(s.bank_ms_avg, Some(350));
        assert_eq!(s.sig_max, Some(19_000));
    }

    #[test]
    fn format_sig_max_compacts_thousands_and_millions() {
        assert_eq!(format_sig_max(42), "42");
        assert_eq!(format_sig_max(999), "999");
        assert_eq!(format_sig_max(1_000), "1k");
        assert_eq!(format_sig_max(43_000), "43k");
        assert_eq!(format_sig_max(999_999), "999k");
        assert_eq!(format_sig_max(1_500_000), "1.5m");
    }

    #[test]
    fn voting_skip_fallback_sets_fallback_flag_in_status() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::VotingSkipFallback { slot: 100 }));
        assert!(matches!(
            p.windows[0].slots[0].status(),
            SlotOutcome::Skipped { fallback: true }
        ));
    }

    #[test]
    fn broadcast_shreds_populates_bcast_and_shred_fields() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::Metric(MetricEvent::BroadcastShreds {
            slot: 100,
            broadcast_us: Some(393_182),
            num_data_shreds: 3200,
            interrupted: false,
        })));
        let s = &p.windows[0].slots[0];
        assert_eq!(broadcast_ms(s), Some(393));
        assert_eq!(s.num_data_shreds, Some(3200));
    }

    #[test]
    fn broadcast_interrupted_records_partial_shreds_but_no_broadcast_time() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(pw(100, 103)));
        p.on_event(&mk(EventKind::Metric(MetricEvent::BroadcastShreds {
            slot: 103,
            broadcast_us: None,
            num_data_shreds: 2240,
            interrupted: true,
        })));
        let s = &p.windows[0].slots[3];
        assert_eq!(broadcast_ms(s), None);
        assert_eq!(s.num_data_shreds, Some(2240));
    }

    #[test]
    fn events_outside_any_window_are_ignored_silently() {
        let mut p = LeaderPane::new();
        p.on_event(&mk(EventKind::Block {
            slot: 9999,
            hash: "h".into(),
            parent_slot: 9998,
            parent_hash: "p".into(),
        }));
        assert!(p.windows.is_empty());
    }

    #[test]
    fn unable_to_produce_outside_window_is_silent_no_op() {
        let mut p = LeaderPane::new();
        // No matching ProduceWindow seen — this can happen if the log
        // tail started mid-stream after the leader window event.
        p.on_event(&mk(EventKind::UnableToProduceWindow {
            start: 100,
            end: 103,
            reason: "x".into(),
        }));
        assert!(p.windows.is_empty());
    }

    #[test]
    fn windows_overflow_drops_oldest() {
        let mut p = LeaderPane::new();
        for i in 0..(RECENT_WINDOWS_CAPACITY as u64 + 2) {
            let start = 100 + i * 4;
            p.on_event(&mk(pw(start, start + 3)));
        }
        assert_eq!(p.windows.len(), RECENT_WINDOWS_CAPACITY);
        // First retained window's start advanced by 2*4.
        assert_eq!(p.windows.front().unwrap().start, 100 + 2 * 4);
    }

    #[test]
    fn spinner_index_advances_with_events_and_freezes_when_quiet() {
        let mut p = LeaderPane::new();
        for _ in 0..(SPINNER_EVENTS_PER_FRAME * SPINNER.len() as u64) {
            p.on_event(&mk(EventKind::FirstShred { slot: 1 }));
        }
        assert_eq!(
            p.event_count,
            SPINNER_EVENTS_PER_FRAME * SPINNER.len() as u64
        );
        // Back-date last_event_at past the live window — spinner index
        // should pin to 0.
        p.last_event_at = Some(
            Instant::now()
                .checked_sub(SPINNER_LIVE_WINDOW + Duration::from_millis(50))
                .unwrap(),
        );
        // Re-derive what render would use.
        let idx = if p
            .last_event_at
            .is_some_and(|t| Instant::now().duration_since(t) < SPINNER_LIVE_WINDOW)
        {
            usize::try_from(p.event_count / SPINNER_EVENTS_PER_FRAME).unwrap_or(0) % SPINNER.len()
        } else {
            0
        };
        assert_eq!(idx, 0);
    }
}
