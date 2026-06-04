//! Chain pane — calm spinner + live timing table.
//!
//! Most slots on a healthy validator are fast-finalised canonical
//! slots — there is no useful per-slot visual to draw for them.
//! Instead the pane shows:
//!
//! - A spinner and the **tip slot number** at the top, proving the
//!   stream is live and giving the operator a slot counter.
//! - A four-row **live timing table** (p50 / p95 in ms) for the four
//!   stage-delta families the Windows tab also reports — local slot
//!   cadence (bank-frozen inter-arrival at this node, used as a
//!   proxy for cluster cadence), assembly, consensus, lifecycle.
//!   Definitions match [`crate::model::analysis::LatencyStages`]
//!   exactly so the live numbers are directly comparable to the
//!   Windows-tab snapshot.
//!
//! The underlying graph model tracks every `Block` / `Finalized` /
//! `VotingSkip` / `VotingNotarize` / `SettingRoot` event:
//!
//! - [`EventKind::Block { slot, hash, parent_slot, parent_hash, .. }`]
//!   stores the parent edge `(slot, hash) → (parent_slot, parent_hash)`.
//!   Two distinct hashes for the same slot ⇒ fork.
//! - [`EventKind::Finalized { slot, hash, .. }`] anchors
//!   `(slot, hash)` as canonical, then walks back through parent
//!   edges marking every ancestor canonical too. If the `Finalized`
//!   for this `(slot, hash)` had already been seen before its
//!   `Block` event (so we had no parent edge to walk), the next
//!   `Block` event retroactively replays the walk-back from the
//!   parent.
//! - [`EventKind::VotingSkip { slot }`] records the skip. At render
//!   time the skip is classified (see below).
//!
//! Snapshot row tallies canonical-skip / indeterminate counts.
//!
//! Label vocabulary matches the Slots tab: `CSKIP` is the canonical-skip
//! term across the whole TUI. The Live-tab chain pane uses the same
//! token; see `tui/panel/slots.rs` for the legend that defines it.
//!
//! Note: `ParentReady` and `SettingRoot` events are intentionally not
//! surfaced in this pane — operator deemed them low-signal noise.
//! Earlier iterations carried a scrolling event log; that pane was
//! removed and the timing table + snapshot row replaced it.
//!
//! Classification (mirrors the aggregator):
//!
//! - **Canonical-skip** — the slot has a canonical block (either
//!   `Finalized` fired for it directly, or it is in the parent chain
//!   of some `Finalized` slot reached via walk-back through observed
//!   `Block` parent edges). We voted skip on a real slot.
//! - **Indeterminate** — no canonical evidence for the slot. Most
//!   often this means we don't have enough parent edges yet to walk
//!   back far enough; it can also mean a block for the slot is
//!   coming and the chain went through it (so canonical-skip is
//!   pending). A skip with no walk-back ancestry proof never
//!   upgrades to a positive "safe" verdict — the parent-edge-spans-
//!   slot heuristic is unsound (the canonical chain can skip a slot
//!   *and* later finalize a block for it after a reorg).
//!
//! Eager forward propagation ("if parent is canonical, this block
//! is canonical") is **not** safe: a canonical parent can have
//! multiple children, only one of which is on the canonical chain;
//! the others are fork siblings. We rely solely on `Finalized`
//! anchors walking backwards through observed parent edges.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use time::OffsetDateTime;

use crate::live::animation::{spinner_frame, Pane};
use crate::parser::{Event, EventKind};
use crate::tui::theme;

pub const PANE_HEIGHT: u16 = 6;

const HISTORY_CAPACITY: usize = 512;
const EDGES_CAPACITY: usize = 1024;
/// Keep this many slots visible behind the rolling root before
/// pruning them.
const ROOT_TRAILING_SLOTS: u64 = 64;

#[derive(Debug, Clone)]
struct SlotState {
    slot: u64,
    /// Distinct hashes we have seen for this slot.
    hashes: Vec<String>,
    skipped: bool,
    /// Did we observe a `VotingNotarize` for this slot?
    notarized: bool,
    /// `Finalized.fast` if a `Finalized` event fired directly for
    /// this slot. `None` for slots only marked canonical by walk-back
    /// from a descendant.
    fast_finalized: Option<bool>,
    /// Timestamps of stage events. Drive the rolling timing table
    /// (cluster / assembly / consensus / lifecycle) that replaces the
    /// old "recent activity" log. Definitions match
    /// [`crate::model::analysis::LatencyStages`] exactly so the live
    /// values stay comparable to the Windows-tab snapshot.
    first_shred_at: Option<OffsetDateTime>,
    block_emitted_at: Option<OffsetDateTime>,
    bank_frozen_at: Option<OffsetDateTime>,
    finalized_at: Option<OffsetDateTime>,
}

impl SlotState {
    const fn new(slot: u64) -> Self {
        Self {
            slot,
            hashes: Vec::new(),
            skipped: false,
            notarized: false,
            fast_finalized: None,
            first_shred_at: None,
            block_emitted_at: None,
            bank_frozen_at: None,
            finalized_at: None,
        }
    }

    const fn is_forked(&self) -> bool {
        self.hashes.len() >= 2
    }

    fn record_hash(&mut self, hash: &str) {
        if !self.hashes.iter().any(|h| h == hash) {
            self.hashes.push(hash.to_owned());
        }
    }
}

/// Skip classification for a slot the operator voted to skip.
/// Mirrors `aggregator::SkipClassification` minus the variant for
/// "not skipped at all".
///
/// A skip is `OnCanonical` iff the slot has a canonical block
/// (direct `Finalized` or reached via walk-back through observed
/// parent edges). Everything else is `Indeterminate` — we have no
/// positive proof the canonical chain bypassed the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipClass {
    /// Slot has a canonical block (direct `Finalized` or via parent
    /// chain of one). We missed a real slot — bad.
    OnCanonical,
    /// We voted skip and have no positive evidence either way.
    Indeterminate,
}

/// `(slot, hash)` pair — used as both edge endpoints and canonical
/// chain anchors throughout the pane.
type BlockId = (u64, String);

pub struct ChainPane {
    slots: VecDeque<SlotState>,
    /// `(slot, hash) → (parent_slot, parent_hash)` edges from
    /// `Block` events. Drives the canonical walk-back.
    parents: HashMap<BlockId, BlockId>,
    /// Set of `(slot, hash)` pairs proven canonical, anchored by
    /// `Finalized` events and walked back through `parents`.
    canonical: HashSet<BlockId>,
    /// Slot-only projection of `canonical`. Used by `classify_skip`
    /// for O(1) lookup; previously a linear scan of `canonical` per
    /// skipped slot per render frame. Kept synchronised with
    /// `canonical` in [`Self::mark_canonical_and_walk_back`] and
    /// [`Self::prune`].
    canonical_slots: HashSet<u64>,
    /// Count of malformed parent edges seen during walk-back
    /// (`parent.0 >= current.0`). Surfaced on the snapshot row only
    /// when nonzero so the operator notices upstream parser regressions
    /// rather than silently degrading canonical-skip detection.
    walk_back_anomalies: u64,
    last_root: Option<u64>,
    /// Count of events observed since the pane was constructed.
    /// Drives the spinner — every Nth event ticks one frame, so the
    /// spinner pauses when the stream is silent (honest liveness).
    event_count: u64,
    /// Wall-clock instant of the most recent event observation. The
    /// spinner only advances if events arrived within the shared
    /// liveness window; otherwise the cell freezes.
    last_event_at: Option<Instant>,
}

/// BankFrozen inter-arrival deltas spanning more than this many slots
/// are treated as skip runs and excluded from cluster-cadence
/// percentiles. Mirrors the same defence in [`crate::live::scenes::leader`].
const MAX_SLOT_GAP: u64 = 8;

/// Result of [`ChainPane::timing_table`]: p50/p95 (ms) for each
/// stage-delta family. `None` if no samples retained.
#[derive(Debug, Default, Clone, Copy)]
struct TimingTable {
    cluster: StagePercentiles,
    assembly: StagePercentiles,
    consensus: StagePercentiles,
    lifecycle: StagePercentiles,
}

/// Whole-microsecond delta `end - start` when both timestamps are
/// present and the delta is non-negative. Used to harvest stage
/// samples from per-slot timing fields.
fn stage_delta_us(start: Option<OffsetDateTime>, end: Option<OffsetDateTime>) -> Option<i64> {
    let (s, e) = (start?, end?);
    let raw = e - s;
    if raw.is_negative() {
        return None;
    }
    i64::try_from(raw.whole_microseconds()).ok()
}

/// `(p50_ms, p95_ms)` from a stage-sample slice.
type StagePercentiles = Option<(i64, i64)>;

/// Sort `samples` in place and return `(p50_ms, p95_ms)` derived from
/// integer positional percentiles. Inputs are microseconds; output is
/// milliseconds. `None` when the input is empty.
fn percentiles_ms(samples: &mut [i64]) -> StagePercentiles {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let pick = |frac: f64| -> i64 {
        let n = samples.len();
        // Invariant: `n <= HISTORY_CAPACITY` (512), so `n as f64` is
        // exact and the `f64 → usize` cast cannot truncate.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let idx = ((frac * n as f64).ceil() as usize)
            .saturating_sub(1)
            .min(n - 1);
        samples[idx] / 1000
    };
    Some((pick(0.50), pick(0.95)))
}

impl ChainPane {
    pub fn new() -> Self {
        Self {
            slots: VecDeque::with_capacity(HISTORY_CAPACITY),
            parents: HashMap::with_capacity(EDGES_CAPACITY),
            canonical: HashSet::with_capacity(EDGES_CAPACITY),
            canonical_slots: HashSet::with_capacity(EDGES_CAPACITY),
            walk_back_anomalies: 0,
            last_root: None,
            event_count: 0,
            last_event_at: None,
        }
    }

    fn tip_slot(&self) -> Option<u64> {
        self.slots.back().map(|s| s.slot)
    }

    fn upsert_slot(&mut self, slot: u64) -> &mut SlotState {
        // Sorted-by-slot invariant on `self.slots` is preserved by:
        // - tip-extension (`slot > last.slot`) appends at the back,
        // - tip-match returns the existing back entry,
        // - out-of-order arrival inserts at `partition_point`.
        //
        // Out-of-order is genuine: `Block` / `Finalized` can arrive
        // for slots older than the current tip (different threads,
        // and the `Finalized`-before-`Block` retroactive path in
        // `on_event`). `VecDeque::insert` is O(N) shifts but no
        // allocation and no sort — dramatically cheaper than the
        // previous drain+sort+extend on `HISTORY_CAPACITY = 512`.
        let idx = match self.slots.back() {
            None => {
                self.slots.push_back(SlotState::new(slot));
                0
            }
            Some(last) if slot > last.slot => {
                self.slots.push_back(SlotState::new(slot));
                self.slots.len() - 1
            }
            Some(last) if slot == last.slot => self.slots.len() - 1,
            Some(_) => {
                let pos = self.slots.partition_point(|s| s.slot < slot);
                let already_present = pos < self.slots.len() && self.slots[pos].slot == slot;
                if !already_present {
                    self.slots.insert(pos, SlotState::new(slot));
                }
                pos
            }
        };
        debug_assert!(
            self.slots.iter().map(|s| s.slot).is_sorted(),
            "slots must remain sorted by slot after upsert_slot"
        );
        &mut self.slots[idx]
    }

    /// Mark `(slot, hash)` canonical and walk back through parent
    /// edges, marking every ancestor canonical. Stops at edges we
    /// don't have (chain root or out-of-window slots).
    ///
    /// Increments [`Self::walk_back_anomalies`] when a parent edge
    /// violates the `parent.0 < current.0` invariant. That state
    /// indicates either a parser regression upstream or a corrupt
    /// `(slot, hash) → (parent_slot, parent_hash)` mapping; surfacing
    /// it as a counter avoids silently degrading the canonical-skip
    /// detection rate.
    fn mark_canonical_and_walk_back(&mut self, slot: u64, hash: String) {
        let mut current = (slot, hash);
        loop {
            if !self.canonical.insert(current.clone()) {
                // Already canonical — chain explored, stop.
                break;
            }
            self.canonical_slots.insert(current.0);
            match self.parents.get(&current) {
                Some(parent) => {
                    if parent.0 >= current.0 {
                        // Malformed parent edge — parent must be older.
                        // Stop walk-back and surface as an anomaly.
                        self.walk_back_anomalies = self.walk_back_anomalies.saturating_add(1);
                        break;
                    }
                    current = parent.clone();
                }
                None => break,
            }
        }
    }

    /// Classify the skip on `slot` against the canonical chain.
    ///
    /// `OnCanonical` iff any hash for the slot is in the canonical
    /// set (direct `Finalized` or via walk-back through observed
    /// parent edges). Everything else is `Indeterminate`.
    ///
    /// Uses the [`Self::canonical_slots`] projection so the lookup is
    /// O(1) per call rather than O(|canonical|).
    fn classify_skip(&self, slot: u64) -> SkipClass {
        if self.canonical_slots.contains(&slot) {
            SkipClass::OnCanonical
        } else {
            SkipClass::Indeterminate
        }
    }

    /// NIT-02: a skip-vote upsert for a slot below the cutoff will be
    /// evicted on the same call. Currently mitigated by the
    /// `skip_to_present` cursor (`live/scenes/mod.rs:96`) on
    /// resume-after-pause; protocol-side voting is recent-slot only.
    fn prune(&mut self) {
        while self.slots.len() > HISTORY_CAPACITY {
            if let Some(s) = self.slots.pop_front() {
                self.evict_slot_indexes(&s);
            }
        }
        if let Some(root) = self.last_root {
            let cutoff = root.saturating_sub(ROOT_TRAILING_SLOTS);
            while let Some(s) = self.slots.front() {
                if s.slot < cutoff {
                    if let Some(s) = self.slots.pop_front() {
                        self.evict_slot_indexes(&s);
                    }
                } else {
                    break;
                }
            }
        }
    }

    /// Drop secondary indexes (`parents`, `canonical`, `canonical_slots`)
    /// for an evicted `SlotState`. Keeps the secondary indexes
    /// consistent with the primary `slots` deque.
    fn evict_slot_indexes(&mut self, s: &SlotState) {
        for hash in &s.hashes {
            let key = (s.slot, hash.clone());
            self.parents.remove(&key);
            self.canonical.remove(&key);
        }
        // Drop the slot-only projection entry too; no remaining
        // `SlotState` carries this slot, so the projection cannot be
        // out of sync after this point.
        self.canonical_slots.remove(&s.slot);
    }

    fn fork_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_forked()).count()
    }

    #[cfg(test)]
    fn canonical_skip_count(&self) -> usize {
        self.skip_tallies().0
    }

    /// Single-pass tally `(canonical_skips, indeterminate_skips)` over
    /// the retained slot deque. The snapshot row needs both counters;
    /// folding them into one walk halves the per-frame cost.
    fn skip_tallies(&self) -> (usize, usize) {
        let mut canon = 0usize;
        let mut indet = 0usize;
        for s in &self.slots {
            if !s.skipped {
                continue;
            }
            match self.classify_skip(s.slot) {
                SkipClass::OnCanonical => canon += 1,
                SkipClass::Indeterminate => indet += 1,
            }
        }
        (canon, indet)
    }

    /// Sample `(p50, p95)` (ms) for every stage-delta family the chain
    /// pane surfaces. Definitions:
    ///
    /// - `slot cadence` (internal field name: `cluster`) —
    ///   `bank_frozen_at[N] → bank_frozen_at[N+gap]` divided by `gap`,
    ///   treating gaps larger than [`MAX_SLOT_GAP`] as skip runs
    ///   (excluded). Measured at OUR node: `bank_frozen_at` fires
    ///   when our replay finishes freezing the bank, so the value is
    ///   influenced by network transit time of shreds to us and by
    ///   our own replay throughput. In steady state this approximates
    ///   the true cluster cadence; a node behind on replay would
    ///   inflate the value without that being visible in this row.
    /// - `assembly` — `first_shred_at → block_emitted_at` per slot.
    /// - `consensus` — `block_emitted_at → finalized_at` per slot.
    /// - `lifecycle` — `first_shred_at → finalized_at` per slot.
    ///
    /// Exact definitions of assembly/consensus/lifecycle come from
    /// [`crate::model::analysis::LatencyStages`] so values are directly
    /// comparable to the Windows-tab snapshot.
    fn timing_table(&self) -> TimingTable {
        let mut cluster: Vec<i64> = Vec::new();
        let mut assembly: Vec<i64> = Vec::new();
        let mut consensus: Vec<i64> = Vec::new();
        let mut lifecycle: Vec<i64> = Vec::new();
        let mut prev: Option<(u64, OffsetDateTime)> = None;
        for s in &self.slots {
            if let Some(us) = stage_delta_us(s.first_shred_at, s.block_emitted_at) {
                assembly.push(us);
            }
            if let Some(us) = stage_delta_us(s.block_emitted_at, s.finalized_at) {
                consensus.push(us);
            }
            if let Some(us) = stage_delta_us(s.first_shred_at, s.finalized_at) {
                lifecycle.push(us);
            }
            if let Some(bf) = s.bank_frozen_at {
                if let Some((prev_slot, prev_bf)) = prev {
                    if s.slot > prev_slot {
                        let gap = s.slot - prev_slot;
                        if gap <= MAX_SLOT_GAP {
                            let raw = bf - prev_bf;
                            if !raw.is_negative() {
                                let total_us =
                                    i64::try_from(raw.whole_microseconds()).unwrap_or(i64::MAX);
                                cluster
                                    .push(total_us / i64::try_from(gap).unwrap_or(i64::MAX).max(1));
                            }
                        }
                    }
                }
                prev = Some((s.slot, bf));
            }
        }
        TimingTable {
            cluster: percentiles_ms(&mut cluster),
            assembly: percentiles_ms(&mut assembly),
            consensus: percentiles_ms(&mut consensus),
            lifecycle: percentiles_ms(&mut lifecycle),
        }
    }

    #[cfg(test)]
    fn indeterminate_skip_count(&self) -> usize {
        self.skip_tallies().1
    }
}

impl Default for ChainPane {
    fn default() -> Self {
        Self::new()
    }
}

impl Pane for ChainPane {
    fn on_event(&mut self, ev: &Event) {
        self.event_count = self.event_count.saturating_add(1);
        self.last_event_at = Some(Instant::now());
        // `EventKind::local_skip_vote_slot` matches both round-1 and
        // fallback-round skip votes — a future third skip variant
        // would slot in without touching the per-pane match arms.
        if let Some(slot) = ev.kind.local_skip_vote_slot() {
            let s = self.upsert_slot(slot);
            s.skipped = true;
            self.prune();
            return;
        }
        match &ev.kind {
            EventKind::Block {
                slot,
                hash,
                parent_slot,
                parent_hash,
            } => {
                let ts = ev.ts;
                let s = self.upsert_slot(*slot);
                s.record_hash(hash);
                s.block_emitted_at.get_or_insert(ts);
                let edge_key: BlockId = (*slot, hash.clone());
                let already_canonical = self.canonical.contains(&edge_key);
                self.parents
                    .insert(edge_key, (*parent_slot, parent_hash.clone()));
                // Eager forward propagation ("parent canonical → this
                // block canonical") would be wrong: a canonical
                // parent can have multiple children, only one of
                // which is on the canonical chain. We only mark
                // canonical via walk-back from a `Finalized` anchor.
                //
                // BUT: if `Finalized` for this (slot, hash) arrived
                // *before* its `Block` event, we already inserted
                // (slot, hash) into `canonical` without being able
                // to walk back (no parent edge yet). Now that we
                // have the parent edge, walk back retroactively.
                if already_canonical {
                    self.mark_canonical_and_walk_back(*parent_slot, parent_hash.clone());
                }
            }
            EventKind::Finalized { slot, hash, fast } => {
                let ts = ev.ts;
                let s = self.upsert_slot(*slot);
                s.record_hash(hash);
                s.fast_finalized = Some(*fast);
                s.finalized_at.get_or_insert(ts);
                self.mark_canonical_and_walk_back(*slot, hash.clone());
            }
            EventKind::FirstShred { slot } => {
                let ts = ev.ts;
                let s = self.upsert_slot(*slot);
                s.first_shred_at.get_or_insert(ts);
            }
            EventKind::BankFrozen { slot, .. } => {
                let ts = ev.ts;
                let s = self.upsert_slot(*slot);
                s.bank_frozen_at.get_or_insert(ts);
            }
            EventKind::VotingNotarize { slot, .. } => {
                let s = self.upsert_slot(*slot);
                s.notarized = true;
            }
            EventKind::SettingRoot { slot } | EventKind::NewRoot { slot } => {
                self.last_root = Some(*slot);
            }
            // ProduceWindow is consumed by the block-production pane;
            // leader-window events do not belong in the chain log.
            // Skip-vote variants are handled by the
            // `local_skip_vote_slot` fast-path above.
            _ => return,
        }
        self.prune();
    }

    fn tick(&mut self, _now: Instant) {}

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" chain ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < 20 || inner.height < 4 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // top spacer
                Constraint::Length(1), // spinner + tip slot
                Constraint::Length(1), // blank
                Constraint::Length(1), // "live timing (p50 / p95)" label
                Constraint::Min(1),    // timing table
                Constraint::Length(1), // snapshot
            ])
            .split(inner);

        self.render_tip(frame, chunks[1]);
        Self::render_section_label(frame, chunks[3], "live timing  (p50 / p95)");
        self.render_timing_table(frame, chunks[4]);
        self.render_snapshot(frame, chunks[5]);
    }
}

impl ChainPane {
    fn render_tip(&self, frame: &mut Frame<'_>, area: Rect) {
        let spinner = spinner_frame(self.event_count, self.last_event_at);
        let tip = self
            .tip_slot()
            .map_or_else(|| "—".to_owned(), |s| s.to_string());
        let line = Line::from(vec![
            Span::raw("  "),
            Span::styled(
                spinner.to_owned(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                tip,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  tip slot", theme::label_style()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_section_label(frame: &mut Frame<'_>, area: Rect, label: &str) {
        let line = Line::from(Span::styled(format!("  {label}"), theme::label_style()));
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_timing_table(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let table = self.timing_table();
        // Order: cluster (network cadence), then assembly → consensus
        // → lifecycle (stage breakdown matching Windows-tab semantics).
        let rows: [(&str, StagePercentiles); 4] = [
            ("slot cadence", table.cluster),
            ("assembly", table.assembly),
            ("consensus", table.consensus),
            ("lifecycle", table.lifecycle),
        ];
        let max = area.height as usize;
        for (i, (label, pct)) in rows.iter().enumerate().take(max) {
            let y = area.y + i as u16;
            let row = Rect::new(area.x, y, area.width, 1);
            let line = match pct {
                Some((p50, p95)) => Line::from(vec![
                    Span::styled(format!("    {label:<14}"), theme::label_style()),
                    Span::styled(format!("p50 {p50}ms"), theme::value_style()),
                    Span::styled("   ", theme::label_style()),
                    Span::styled(format!("p95 {p95}ms"), theme::value_style()),
                ]),
                None => Line::from(vec![
                    Span::styled(format!("    {label:<14}"), theme::label_style()),
                    Span::styled("—", Style::default().fg(Color::DarkGray)),
                ]),
            };
            frame.render_widget(Paragraph::new(line), row);
        }
    }

    fn render_snapshot(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(Paragraph::new(self.snapshot_line()), area);
    }

    /// Compose the snapshot `Line`. Extracted from `render_snapshot`
    /// for direct test coverage of the label vocabulary (no frame
    /// roundtripping required).
    fn snapshot_line(&self) -> Line<'static> {
        let (canonical_skips, indeterminate) = self.skip_tallies();
        let forks = self.fork_count();

        let range = match (self.slots.front(), self.slots.back()) {
            (Some(f), Some(l)) if f.slot != l.slot => format!("{}..{}", f.slot, l.slot),
            (Some(f), _) => format!("{}", f.slot),
            _ => "—".to_owned(),
        };

        let cskip_style = if canonical_skips > 0 {
            theme::bad_style().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let fork_style = if forks > 0 {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        // `CSKIP` matches the Slots tab vocabulary (see slots.rs:303,384
        // and tui/view.rs status_str — single token across the TUI).
        let mut spans = vec![
            Span::styled(" slots ", theme::label_style()),
            Span::styled(
                range,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            sep(),
            Span::styled(canonical_skips.to_string(), cskip_style),
            Span::styled(" CSKIP", theme::label_style()),
            sep(),
            Span::styled(
                indeterminate.to_string(),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(" indet", theme::label_style()),
            sep(),
            Span::styled(forks.to_string(), fork_style),
            Span::styled(" forks", theme::label_style()),
        ];
        // Walk-back anomalies are only surfaced when nonzero — silence
        // is the correct default for a healthy stream (no-explanatory-UX
        // policy). When nonzero, surface as a red-bold counter so the
        // operator notices upstream parser regressions.
        if self.walk_back_anomalies > 0 {
            spans.push(sep());
            spans.push(Span::styled(
                self.walk_back_anomalies.to_string(),
                theme::bad_style().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(" anom", theme::label_style()));
        }
        Line::from(spans)
    }
}

fn sep() -> Span<'static> {
    Span::styled(
        "  ·  ",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    )
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

    fn block_ev(slot: u64, hash: &str, parent_slot: u64, parent_hash: &str) -> Event {
        mk(EventKind::Block {
            slot,
            hash: hash.into(),
            parent_slot,
            parent_hash: parent_hash.into(),
        })
    }

    #[test]
    fn block_event_records_slot_and_hash() {
        let mut p = ChainPane::new();
        p.on_event(&block_ev(100, "a", 99, "p"));
        assert_eq!(p.slots.len(), 1);
        assert_eq!(p.slots[0].slot, 100);
        assert_eq!(p.slots[0].hashes, vec!["a".to_owned()]);
    }

    #[test]
    fn second_block_same_slot_marks_forked() {
        let mut p = ChainPane::new();
        p.on_event(&block_ev(100, "a", 99, "p"));
        p.on_event(&block_ev(100, "b", 99, "p"));
        assert!(p.slots[0].is_forked());
        assert_eq!(p.fork_count(), 1);
    }

    #[test]
    fn finalized_walks_back_marking_canonical_chain() {
        let mut p = ChainPane::new();
        // Chain: 100 → 101 → 102
        p.on_event(&block_ev(100, "a", 99, "root"));
        p.on_event(&block_ev(101, "b", 100, "a"));
        p.on_event(&block_ev(102, "c", 101, "b"));
        // Finalize 102 → walk back marks 102, 101, 100 canonical.
        p.on_event(&mk(EventKind::Finalized {
            slot: 102,
            hash: "c".into(),
            fast: true,
        }));
        assert!(p.canonical.contains(&(102, "c".to_owned())));
        assert!(p.canonical.contains(&(101, "b".to_owned())));
        assert!(p.canonical.contains(&(100, "a".to_owned())));
    }

    #[test]
    fn canonical_parent_does_not_make_sibling_canonical() {
        // Regression for the eager-forward-propagation bug: a
        // canonical parent can have multiple children, only one of
        // which is on the canonical chain. Marking every child
        // canonical would over-detect canonical-skips.
        let mut p = ChainPane::new();
        p.on_event(&block_ev(100, "a", 99, "root"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "a".into(),
            fast: true,
        }));
        // Two children of canonical 100. Neither is finalised — we
        // don't know which (if either) is canonical yet.
        p.on_event(&block_ev(101, "b", 100, "a"));
        p.on_event(&block_ev(101, "c", 100, "a"));
        assert!(
            !p.canonical.contains(&(101, "b".to_owned())),
            "no forward propagation"
        );
        assert!(
            !p.canonical.contains(&(101, "c".to_owned())),
            "no forward propagation"
        );
    }

    #[test]
    fn finalized_before_block_walks_back_when_block_arrives() {
        // Regression for the missing-retroactive-walk-back bug.
        // Finalized for slot 102 arrives before its Block event;
        // initial walk-back can't find a parent edge and stops at
        // slot 102 alone. When Block for 102 arrives later, the
        // walk-back must replay from 102's parent.
        let mut p = ChainPane::new();
        p.on_event(&block_ev(100, "a", 99, "root"));
        p.on_event(&block_ev(101, "b", 100, "a"));
        // Finalized arrives BEFORE Block for slot 102.
        p.on_event(&mk(EventKind::Finalized {
            slot: 102,
            hash: "c".into(),
            fast: true,
        }));
        // Only 102 is canonical so far — no parent edge yet.
        assert!(p.canonical.contains(&(102, "c".to_owned())));
        assert!(!p.canonical.contains(&(101, "b".to_owned())));
        // Now the Block for 102 arrives with its parent edge.
        p.on_event(&block_ev(102, "c", 101, "b"));
        // Retroactive walk-back should mark 101 and 100 canonical.
        assert!(p.canonical.contains(&(101, "b".to_owned())));
        assert!(p.canonical.contains(&(100, "a".to_owned())));
    }

    #[test]
    fn skip_on_canonical_slot_classified_canonical_skip() {
        let mut p = ChainPane::new();
        p.on_event(&block_ev(100, "a", 99, "root"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "a".into(),
            fast: true,
        }));
        p.on_event(&mk(EventKind::VotingSkip { slot: 100 }));
        assert_eq!(p.classify_skip(100), SkipClass::OnCanonical);
        assert_eq!(p.canonical_skip_count(), 1);
    }

    #[test]
    fn skip_on_non_canonical_slot_stays_indeterminate_without_ancestry_proof() {
        // Slot 200 has a non-canonical block (forked off); the
        // canonical chain goes 199 → 205 → 206, with walk-back from
        // Finalized(206) only reaching 205 and 199 (parent of 205).
        // 200 is *not* in the canonical set, and no parent edge of
        // an observed canonical block lands on 200. With the unsound
        // parent-edge-spans-slot bypass rule removed, 200 must stay
        // Indeterminate — the chain could still reorg and finalize
        // a different block at 200.
        let mut p = ChainPane::new();
        p.on_event(&block_ev(200, "b", 199, "root"));
        p.on_event(&block_ev(205, "x", 199, "root"));
        p.on_event(&block_ev(206, "y", 205, "x"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 206,
            hash: "y".into(),
            fast: true,
        }));
        p.on_event(&mk(EventKind::VotingSkip { slot: 200 }));
        assert_eq!(p.classify_skip(200), SkipClass::Indeterminate);
        assert_eq!(p.canonical_skip_count(), 0);
        assert_eq!(p.indeterminate_skip_count(), 1);
    }

    #[test]
    fn timing_table_consensus_uses_block_emitted_to_finalized() {
        let mut p = ChainPane::new();
        let t0 = time::OffsetDateTime::UNIX_EPOCH;
        let t_be = t0 + time::Duration::milliseconds(50);
        let t_fin = t0 + time::Duration::milliseconds(130);
        // first_shred → block_emitted = 50 ms (assembly)
        // block_emitted → finalized = 80 ms (consensus)
        // first_shred → finalized = 130 ms (lifecycle)
        p.on_event(&Event {
            ts: t0,
            kind: EventKind::FirstShred { slot: 100 },
        });
        p.on_event(&Event {
            ts: t_be,
            kind: EventKind::Block {
                slot: 100,
                hash: "a".into(),
                parent_slot: 99,
                parent_hash: "root".into(),
            },
        });
        p.on_event(&Event {
            ts: t_fin,
            kind: EventKind::Finalized {
                slot: 100,
                hash: "a".into(),
                fast: true,
            },
        });
        let table = p.timing_table();
        let (a50, _) = table.assembly.expect("assembly sample");
        let (c50, _) = table.consensus.expect("consensus sample");
        let (l50, _) = table.lifecycle.expect("lifecycle sample");
        assert_eq!(a50, 50);
        assert_eq!(c50, 80);
        assert_eq!(l50, 130);
    }

    #[test]
    fn timing_table_cluster_uses_bank_frozen_inter_arrival() {
        let mut p = ChainPane::new();
        let t0 = time::OffsetDateTime::UNIX_EPOCH;
        for (i, ms) in [0i64, 400, 800, 1200].iter().enumerate() {
            let slot = 100 + i as u64;
            p.on_event(&Event {
                ts: t0 + time::Duration::milliseconds(*ms),
                kind: EventKind::BankFrozen {
                    slot,
                    hash: "h".into(),
                    signature_count: 1,
                },
            });
        }
        let table = p.timing_table();
        let (cluster_p50, _) = table.cluster.expect("cluster samples");
        // 3 samples of 400 ms each → p50 = 400 ms.
        assert_eq!(cluster_p50, 400);
    }

    #[test]
    fn timing_table_empty_when_no_timing_observed() {
        let p = ChainPane::new();
        let t = p.timing_table();
        assert!(t.cluster.is_none());
        assert!(t.assembly.is_none());
        assert!(t.consensus.is_none());
        assert!(t.lifecycle.is_none());
    }

    #[test]
    fn produce_window_event_is_ignored_by_chain_pane() {
        // Leader-window events belong to the block-production pane.
        // The chain pane must NOT surface them — duplicating data
        // across panes is the bug LIVE-37 fixed.
        let mut p = ChainPane::new();
        p.on_event(&mk(EventKind::ProduceWindow {
            start: 200,
            end: 203,
            parent_slot: 199,
            parent_hash: "x".into(),
        }));
        assert_eq!(p.fork_count(), 0);
        assert_eq!(p.canonical_skip_count(), 0);
    }

    #[test]
    fn skip_indeterminate_when_no_canonical_chain_yet() {
        let mut p = ChainPane::new();
        p.on_event(&mk(EventKind::VotingSkip { slot: 100 }));
        assert_eq!(p.classify_skip(100), SkipClass::Indeterminate);
    }

    #[test]
    fn parent_edge_jump_alone_does_not_prove_safe_skip() {
        // The canonical block at 205 has parent_slot = 199. The old
        // bypass rule would have classified votes on 200 and 202 as
        // OnNonCanonical (safe-skip). That heuristic is unsound:
        // a later reorg could still finalize a block at 200 or 202.
        // Without explicit ancestry proof reaching the slot, the
        // verdict must stay Indeterminate.
        let mut p = ChainPane::new();
        p.on_event(&block_ev(205, "x", 199, "root"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 205,
            hash: "x".into(),
            fast: true,
        }));
        p.on_event(&mk(EventKind::VotingSkip { slot: 200 }));
        p.on_event(&mk(EventKind::VotingSkip { slot: 202 }));
        assert_eq!(p.classify_skip(200), SkipClass::Indeterminate);
        assert_eq!(p.classify_skip(202), SkipClass::Indeterminate);
        assert_eq!(p.indeterminate_skip_count(), 2);
    }

    #[test]
    fn skip_indeterminate_when_no_canonical_edge_proves_bypass() {
        // No canonical entries with a parent edge yet — we voted
        // skip but have nothing to argue with.
        let mut p = ChainPane::new();
        // Finalized arrived with no Block, so no parent edge.
        p.on_event(&mk(EventKind::Finalized {
            slot: 205,
            hash: "x".into(),
            fast: true,
        }));
        p.on_event(&mk(EventKind::VotingSkip { slot: 200 }));
        assert_eq!(p.classify_skip(200), SkipClass::Indeterminate);
    }

    #[test]
    fn chain_safe_skip_flips_to_canonical_when_later_finalize_lands() {
        // Set up the same parent-edge-spans-slot scenario as the
        // old safe-skip test: canonical 205 has parent 199, slot
        // 200 voted-skipped. With the unsound rule removed, this
        // is Indeterminate, not OnNonCanonical.
        let mut p = ChainPane::new();
        p.on_event(&block_ev(205, "x", 199, "root"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 205,
            hash: "x".into(),
            fast: true,
        }));
        p.on_event(&mk(EventKind::VotingSkip { slot: 200 }));
        assert_eq!(p.classify_skip(200), SkipClass::Indeterminate);

        // Now a descendant block whose walk-back reaches 200 lands
        // and gets finalized. Chain: 200 → 201 → ... → 210.
        p.on_event(&block_ev(200, "b200", 199, "root"));
        p.on_event(&block_ev(210, "z", 200, "b200"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 210,
            hash: "z".into(),
            fast: true,
        }));
        // Walk-back from 210 → 200 marks slot 200 canonical, so the
        // earlier skip vote retroactively classifies as OnCanonical.
        assert_eq!(p.classify_skip(200), SkipClass::OnCanonical);
        assert_eq!(p.canonical_skip_count(), 1);
    }

    #[test]
    fn setting_root_updates_last_root() {
        let mut p = ChainPane::new();
        p.on_event(&mk(EventKind::SettingRoot { slot: 95 }));
        assert_eq!(p.last_root, Some(95));
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn snapshot_line_uses_cskip_label_not_canon() {
        // UX-04: chain pane previously labelled the canonical-skip
        // count as ` canon`. Unified across the TUI to `CSKIP` to
        // match the Slots tab and status bar vocabulary.
        let p = ChainPane::new();
        let text = line_text(&p.snapshot_line());
        assert!(text.contains(" CSKIP"), "snapshot missing CSKIP: {text:?}");
        assert!(
            !text.contains(" canon"),
            "snapshot still uses ` canon` label: {text:?}"
        );
    }

    #[test]
    fn voting_skip_fallback_classifies_canonical_when_finalized() {
        // Regression for SKIP-01. The fallback-round skip vote must
        // drive the same `skipped` flag as round-1 `VotingSkip`. A
        // slot we skipped via fallback alone that subsequently
        // finalises is a canonical-skip — matching only `VotingSkip`
        // would let it evade `canonical_skip_count()`.
        let mut p = ChainPane::new();
        p.on_event(&mk(EventKind::VotingSkipFallback { slot: 100 }));
        p.on_event(&block_ev(100, "a", 99, "root"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "a".into(),
            fast: true,
        }));
        assert_eq!(p.classify_skip(100), SkipClass::OnCanonical);
        assert_eq!(p.canonical_skip_count(), 1);
    }

    #[test]
    fn upsert_slot_preserves_sorted_invariant_on_out_of_order_insert() {
        // PERF-03: the previous drain+sort+extend path is gone; the
        // new code inserts at `partition_point`. Verify the resulting
        // deque stays sorted across mixed-order arrivals.
        let mut p = ChainPane::new();
        // Arrival order: 105, 100, 103, 102, 110 — only 105 and 110
        // are tip-extensions; 100/103/102 hit the out-of-order branch.
        for slot in [105, 100, 103, 102, 110] {
            p.upsert_slot(slot);
        }
        let observed: Vec<u64> = p.slots.iter().map(|s| s.slot).collect();
        assert_eq!(observed, vec![100, 102, 103, 105, 110]);
    }

    #[test]
    fn upsert_slot_returning_index_points_at_inserted_or_existing_slot() {
        // The `&mut SlotState` returned by `upsert_slot` is used to
        // mutate the just-inserted (or pre-existing) slot. The
        // partition_point return path must point at the correct entry,
        // not at slot 0 like the previous `.unwrap_or(0)` fallback did.
        let mut p = ChainPane::new();
        // Tip extension at 100; then out-of-order at 50.
        p.upsert_slot(100).hashes.push("a".into());
        p.upsert_slot(50).hashes.push("b".into());
        // Out-of-order re-touch of slot 100 — must return the slot-100
        // entry, not slot-50 or slot-0.
        p.upsert_slot(100).hashes.push("c".into());
        let slot_100 = p.slots.iter().find(|s| s.slot == 100).unwrap();
        let slot_50 = p.slots.iter().find(|s| s.slot == 50).unwrap();
        assert_eq!(slot_100.hashes, vec!["a".to_owned(), "c".to_owned()]);
        assert_eq!(slot_50.hashes, vec!["b".to_owned()]);
    }

    #[test]
    fn timing_table_excludes_inter_arrival_when_gap_exceeds_max() {
        // TEST-02: gap > MAX_SLOT_GAP (= 8) is treated as a skip run
        // and the sample MUST be excluded from cluster percentiles.
        // Two samples spaced by gap = 9 — no cluster percentile emitted.
        let mut p = ChainPane::new();
        let t0 = time::OffsetDateTime::UNIX_EPOCH;
        p.on_event(&Event {
            ts: t0,
            kind: EventKind::BankFrozen {
                slot: 100,
                hash: "h".into(),
                signature_count: 1,
            },
        });
        p.on_event(&Event {
            ts: t0 + time::Duration::milliseconds(400),
            kind: EventKind::BankFrozen {
                slot: 109, // gap = 9 > MAX_SLOT_GAP
                hash: "h".into(),
                signature_count: 1,
            },
        });
        assert!(
            p.timing_table().cluster.is_none(),
            "gap > MAX_SLOT_GAP must be excluded from cluster percentiles"
        );
    }

    #[test]
    fn timing_table_includes_inter_arrival_when_gap_equals_max() {
        // TEST-02: gap exactly equal to MAX_SLOT_GAP (= 8) is the
        // boundary case — INCLUDED in cluster percentiles. The check
        // is `gap <= MAX_SLOT_GAP`, so 8 must pass.
        let mut p = ChainPane::new();
        let t0 = time::OffsetDateTime::UNIX_EPOCH;
        p.on_event(&Event {
            ts: t0,
            kind: EventKind::BankFrozen {
                slot: 100,
                hash: "h".into(),
                signature_count: 1,
            },
        });
        p.on_event(&Event {
            ts: t0 + time::Duration::milliseconds(800),
            kind: EventKind::BankFrozen {
                slot: 108, // gap = 8 == MAX_SLOT_GAP
                hash: "h".into(),
                signature_count: 1,
            },
        });
        let (p50, _) = p
            .timing_table()
            .cluster
            .expect("gap = MAX_SLOT_GAP must be included");
        // 800 ms across 8 slots = 100 ms per slot.
        assert_eq!(p50, 100);
    }

    #[test]
    fn walk_back_anomaly_counter_increments_on_invalid_parent_edge() {
        // CORRECT-01: a parent edge with `parent.0 >= current.0` must
        // increment the anomaly counter. Construct a corrupt graph
        // directly so the walk-back hits the invalid edge.
        let mut p = ChainPane::new();
        // Block(100, "a") with a parent at the same slot (corrupt).
        // Then Finalize(100, "a") triggers walk-back into the bad edge.
        p.on_event(&block_ev(100, "a", 100, "self-loop"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "a".into(),
            fast: true,
        }));
        assert_eq!(p.walk_back_anomalies, 1);
    }

    #[test]
    fn walk_back_anomaly_stays_zero_on_well_formed_chain() {
        // Negative case for CORRECT-01: a well-formed chain must not
        // increment the anomaly counter.
        let mut p = ChainPane::new();
        p.on_event(&block_ev(100, "a", 99, "root"));
        p.on_event(&block_ev(101, "b", 100, "a"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 101,
            hash: "b".into(),
            fast: true,
        }));
        assert_eq!(p.walk_back_anomalies, 0);
    }

    #[test]
    fn snapshot_line_surfaces_anomaly_only_when_nonzero() {
        // CORRECT-01 display policy: silence on a healthy stream,
        // surface a red ` anom` segment only when anomalies were seen.
        let mut p = ChainPane::new();
        let text = line_text(&p.snapshot_line());
        assert!(
            !text.contains(" anom"),
            "snapshot must stay silent on healthy stream: {text:?}"
        );
        // Inject a corrupt parent edge and finalise to trip the
        // anomaly path.
        p.on_event(&block_ev(100, "a", 100, "self-loop"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "a".into(),
            fast: true,
        }));
        let text = line_text(&p.snapshot_line());
        assert!(
            text.contains(" anom"),
            "snapshot must surface anomaly counter when nonzero: {text:?}"
        );
        assert!(
            text.contains("1 anom"),
            "anomaly count missing in snapshot: {text:?}"
        );
    }
}
