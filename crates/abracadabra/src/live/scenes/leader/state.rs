//! Block-production pane state: per-slot observations, per-window
//! containers, the derived [`SlotOutcome`] vocabulary, and the
//! [`LeaderPane`] event-observation surface.
//!
//! Rendering and formatting live in sibling modules
//! ([`super::render`], [`super::format`]). This module exposes only
//! the pure-state shape and the `on_event` mutation rules so an event
//! stream can drive the pane without touching any ratatui APIs.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use time::OffsetDateTime;

use crate::aggregator::MAX_LEADER_WINDOW_SPAN;
use crate::parser::{Event, EventKind, MetricEvent};

/// Leader windows kept in memory (oldest dropped on overflow).
pub(super) const RECENT_WINDOWS_CAPACITY: usize = 8;

/// Per-slot observation state inside one of our `ProduceWindow`s.
///
/// All fields are pure observations; the derived status is computed
/// on render from these timestamps. Times are parsed log timestamps
/// so playback speed does not affect derivations.
#[derive(Debug, Default, Clone)]
pub(super) struct OurSlot {
    pub(super) slot: u64,
    pub(super) block_at: Option<OffsetDateTime>,
    pub(super) bank_frozen_at: Option<OffsetDateTime>,
    pub(super) finalized_at: Option<OffsetDateTime>,
    pub(super) fast_finalize: Option<bool>,
    pub(super) sig_count: Option<u64>,
    /// Authoritative leader-slot duration in milliseconds reported by
    /// the validator itself via the `leader-slot-start-to-cleared-elapsed-ms`
    /// metric datapoint. The `First shred N` event does **not** fire
    /// for slots we produce as leader (it only fires when we *receive*
    /// a first shred from elsewhere), so subtracting log timestamps
    /// would never yield a value for our own slots. This metric is
    /// what the validator emits for every slot it produced; using it
    /// directly is the only honest source.
    pub(super) leader_elapsed_ms: Option<u64>,
    /// `broadcast-process-shreds-stats.slot_broadcast_time` (µs).
    /// `None` when the slot was abandoned mid-broadcast (the validator
    /// emits `-1` on the `-interrupted-stats` variant).
    pub(super) broadcast_us: Option<u64>,
    /// `broadcast-process-shreds-stats.num_data_shreds`. Set on both
    /// the normal and `-interrupted-stats` variants — for an
    /// abandoned slot this is how many data shreds we shipped before
    /// clearing.
    pub(super) num_data_shreds: Option<u64>,
    /// `banking_stage_scheduler_slot_counts.num_finished` — txns the
    /// banking-stage scheduler finished executing for this slot.
    pub(super) num_finished: Option<u64>,
    /// `banking_stage_scheduler_slot_counts.num_dropped_on_capacity`.
    /// Normally 0; nonzero is the **only** kind we surface in the
    /// card alert footer because it's actionable (banking buffer
    /// pressure).
    pub(super) num_dropped_on_capacity: Option<u64>,
    /// `slot-metrics.leader_handover_sad` — validator's 1/0 flag for
    /// a bad handover from the prior leader.
    pub(super) leader_handover_sad: Option<bool>,
    /// `slot-metrics.replay_is_behind_count` — count of times replay
    /// lagged during this slot. Normally 0.
    pub(super) replay_is_behind_count: Option<u64>,
    /// Did we cast `Voting skip` for this slot. Direct observation.
    pub(super) voted_skip_at: Option<OffsetDateTime>,
    /// Did we cast `Voting skip-fallback` for this slot. Direct
    /// observation. Distinct from `voted_skip_at` because the two
    /// vote types are different protocol rounds.
    pub(super) voted_skip_fallback_at: Option<OffsetDateTime>,
    /// `Unable to produce window … skipping window: <reason>` fired.
    /// `abandoned_reason` carries the verbatim trailing text from that
    /// line — that string is the validator's own stated reason, so it
    /// is safe to display as a reason. Stored as `Arc<str>` so the
    /// reason is shared across the (up to `MAX_LEADER_WINDOW_SPAN+1`)
    /// slots of one window without a full `String` clone per slot.
    ///
    /// Rendered by `card_slot_line` in the per-slot row's detail
    /// column whenever both `abandoned_at` and `abandoned_reason` are
    /// set, regardless of whether the icon glyph resolves to `[A]`
    /// (no skip vote) or `[✗]` (skip vote also cast on the slot).
    pub(super) abandoned_at: Option<OffsetDateTime>,
    pub(super) abandoned_reason: Option<Arc<str>>,
    /// Pre-summarised form of `abandoned_reason`, produced at write
    /// time so the render path does not re-run
    /// `summarize_abandon_reason` for every frame the footer shows.
    /// Shared across the slots of one window via [`Arc::clone`] (same
    /// pattern as `abandoned_reason`).
    pub(super) abandoned_reason_summary: Option<Arc<str>>,
}

/// One of our `ProduceWindow` ranges. Slots are inclusive `start..=end`.
#[derive(Debug, Clone)]
pub(super) struct OurWindow {
    pub(super) start: u64,
    pub(super) end: u64,
    pub(super) slots: Vec<OurSlot>,
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
pub(super) enum SlotOutcome {
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
    /// for this slot AND no skip vote was cast on it. The row
    /// renders with the `[A]` icon and the verbatim
    /// [`OurSlot::abandoned_reason`] text in the detail column.
    ///
    /// **Dual-channel display when both signals are present.** If a
    /// skip vote AND `abandoned_at` are both set on the same slot,
    /// [`OurSlot::status`] returns [`SlotOutcome::Skipped`] (skip-vote
    /// precedence — the protocol-side ground truth wins for the icon
    /// glyph). But `card_slot_line` still reads `abandoned_reason`
    /// independently and renders it in the row body, so neither
    /// signal suppresses the other in the UI. The icon shows the
    /// protocol category and the body shows the validator's own
    /// stated reason.
    Abandoned,
    /// `Voting skip` and/or `Voting skip-fallback` cast for this slot.
    /// `fallback` is `true` iff the fallback vote was cast; both votes
    /// can be cast for the same slot, in which case `fallback` is true.
    Skipped { fallback: bool },
}

impl OurSlot {
    pub(super) fn status(&self) -> SlotOutcome {
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

/// Block-production pane state.
///
/// Public type re-exported by the parent `live::scenes::leader`
/// module; the `impl Pane` lives in the parent module so the
/// on-event / render dispatch sits next to the public surface.
pub struct LeaderPane {
    /// FIFO of our recent ProduceWindow ranges, with per-slot state.
    pub(super) windows: VecDeque<OurWindow>,
    /// Count of events observed since the pane was constructed.
    /// Drives the spinner — every Nth event ticks one frame, so the
    /// spinner pauses when the stream is silent (honest liveness).
    pub(super) event_count: u64,
    /// Wall-clock instant of the most recent event observation. The
    /// spinner only advances if events arrived within the shared
    /// liveness window (see [`crate::live::animation::spinner_frame`]);
    /// otherwise the cell freezes. Together with `event_count` this
    /// gives both a paused-when-quiet visual and a tickless idle state.
    pub(super) last_event_at: Option<Instant>,
}

/// Stats that the operator cannot read by glancing at the cards.
/// Slot counts are deliberately omitted — those are visible.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct WindowSummary {
    /// Any retained windows at all.
    pub(super) has_windows: bool,
    /// Mean of `leader-slot-start-to-cleared-elapsed-ms` across produced slots.
    pub(super) bank_ms_avg: Option<i64>,
    /// Max `signature_count` across retained produced slots.
    pub(super) sig_max: Option<u64>,
    /// Max `num_data_shreds` across retained produced slots.
    pub(super) sh_max: Option<u64>,
    /// Latest `bank_frozen_at` across produced/banked slots. Drives
    /// the "since last block" timer in the headline. `None` when no
    /// slot has been banked yet — the headline omits the segment
    /// rather than printing a meaningless zero. The empty-pane case
    /// is already covered by the `waiting for first leader window`
    /// line so there is no duplication risk between them.
    pub(super) last_produced_at: Option<OffsetDateTime>,
}

impl LeaderPane {
    /// Construct an empty pane. Equivalent to the type's `Default`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            windows: VecDeque::with_capacity(RECENT_WINDOWS_CAPACITY),
            event_count: 0,
            last_event_at: None,
        }
    }

    pub(super) fn summary(&self) -> WindowSummary {
        let mut out = WindowSummary {
            has_windows: !self.windows.is_empty(),
            ..WindowSummary::default()
        };
        // Bank time is already a whole-millisecond value coming out of
        // `bank_ms`. Sum and divide directly in ms — the previous
        // `* 1000 / bank_n / 1000` collapsed algebraically to the same
        // result with extra noise.
        let mut bank_total_ms: i128 = 0;
        let mut bank_n: i128 = 0;
        for w in &self.windows {
            for s in &w.slots {
                // Hoist `status()` once per slot so the guard chain
                // doesn't recompute it (LINT-01).
                let status = s.status();
                if matches!(status, SlotOutcome::Produced { .. } | SlotOutcome::Banked) {
                    if let Some(ms) = super::format::bank_ms(s) {
                        bank_total_ms = bank_total_ms.saturating_add(i128::from(ms));
                        bank_n = bank_n.saturating_add(1);
                    }
                    if let Some(c) = s.sig_count {
                        out.sig_max = Some(out.sig_max.map_or(c, |m| m.max(c)));
                    }
                    if let Some(sh) = s.num_data_shreds {
                        out.sh_max = Some(out.sh_max.map_or(sh, |m| m.max(sh)));
                    }
                    if let Some(at) = s.bank_frozen_at {
                        out.last_produced_at =
                            Some(out.last_produced_at.map_or(at, |prev| prev.max(at)));
                    }
                }
            }
        }
        if bank_n > 0 {
            out.bank_ms_avg = i64::try_from(bank_total_ms / bank_n).ok();
        }
        out
    }

    fn window_for_slot_mut(&mut self, slot: u64) -> Option<&mut OurWindow> {
        self.windows
            .iter_mut()
            .rev()
            .find(|w| slot >= w.start && slot <= w.end)
    }

    pub(super) fn slot_mut(&mut self, slot: u64) -> Option<&mut OurSlot> {
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

    /// Dispatch one parsed event into the pane's state. Called from
    /// the `impl Pane for LeaderPane` block in the parent module.
    pub(super) fn observe_event(&mut self, ev: &Event) {
        self.event_count = self.event_count.saturating_add(1);
        self.last_event_at = Some(Instant::now());
        match &ev.kind {
            EventKind::ProduceWindow { start, end, .. } => {
                if *end < *start || end.saturating_sub(*start) > MAX_LEADER_WINDOW_SPAN {
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
                // Defence-in-depth against an out-of-band event with a
                // corrupted span. The parser (`block_creation_loop`)
                // already rejects these; the cap here keeps the live
                // pane safe if a future ingest path delivers a raw
                // event built without going through the parser.
                if *end < *start || end.saturating_sub(*start) > MAX_LEADER_WINDOW_SPAN {
                    return;
                }
                let ts = ev.ts;
                // Materialise the shared `Arc<str>` exactly once per
                // event so the (up to `MAX_LEADER_WINDOW_SPAN+1`) per-
                // slot stores below reuse the same allocation.
                let shared_reason: Arc<str> = Arc::from(reason.as_str());
                // PERF-02: summarise once per event, share the
                // summary Arc across all slots in the window so the
                // render path's `card_alert_line` does not re-run
                // `summarize_abandon_reason` per frame.
                let shared_summary: Arc<str> =
                    Arc::from(super::format::summarize_abandon_reason(reason).as_str());
                // The error may cover a window we never saw a ProduceWindow
                // for (log replay started mid-stream). In that case we have
                // no slots to mark — silently no-op.
                for slot in *start..=*end {
                    if let Some(s) = self.slot_mut(slot) {
                        s.abandoned_at.get_or_insert(ts);
                        s.abandoned_reason
                            .get_or_insert_with(|| Arc::clone(&shared_reason));
                        s.abandoned_reason_summary
                            .get_or_insert_with(|| Arc::clone(&shared_summary));
                    }
                }
            }
            _ => {}
        }
    }
}

impl Default for LeaderPane {
    fn default() -> Self {
        Self::new()
    }
}
