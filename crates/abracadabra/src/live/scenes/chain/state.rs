//! ChainPane state: per-slot deque, parent edges, canonical set,
//! event observation, and derived queries used by [`super::render`].
//!
//! Keeps all `ratatui::*` use out of this module — state mutation and
//! classification only.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use time::OffsetDateTime;

use crate::parser::{Event, EventKind};

use super::format::{percentiles_ms, stage_delta_us, TimingTable};
use super::particle::CannonSystem;

pub(super) const HISTORY_CAPACITY: usize = 512;
pub(super) const EDGES_CAPACITY: usize = 1024;
/// Keep this many slots visible behind the rolling root before
/// pruning them.
pub(super) const ROOT_TRAILING_SLOTS: u64 = 64;

/// BankFrozen inter-arrival deltas spanning more than this many slots
/// are treated as skip runs and excluded from cluster-cadence
/// percentiles. Mirrors the same defence in [`crate::live::scenes::leader`].
pub(super) const MAX_SLOT_GAP: u64 = 8;

#[derive(Debug, Clone)]
pub(super) struct SlotState {
    pub(super) slot: u64,
    /// Distinct hashes we have seen for this slot.
    pub(super) hashes: Vec<String>,
    pub(super) skipped: bool,
    /// Did we observe a `VotingNotarize` for this slot?
    pub(super) notarized: bool,
    /// `Finalized.fast` if a `Finalized` event fired directly for
    /// this slot. `None` for slots only marked canonical by walk-back
    /// from a descendant.
    pub(super) fast_finalized: Option<bool>,
    /// Timestamps of stage events. Drive the rolling timing table
    /// (cluster / assembly / consensus / lifecycle) that replaces the
    /// old "recent activity" log. Definitions match
    /// [`crate::model::analysis::LatencyStages`] exactly so the live
    /// values stay comparable to the Windows-tab snapshot.
    pub(super) first_shred_at: Option<OffsetDateTime>,
    pub(super) block_emitted_at: Option<OffsetDateTime>,
    pub(super) bank_frozen_at: Option<OffsetDateTime>,
    pub(super) finalized_at: Option<OffsetDateTime>,
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

    #[cfg(test)]
    pub(super) const fn is_forked(&self) -> bool {
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
///
/// **Test-only API** since LIVE-52 — the header dropped the CSKIP
/// counter (visible from the bucket glyphs instead) so prod code
/// no longer materialises the classification. Kept compiled under
/// `#[cfg(test)]` because the walk-back invariant tests pin behaviour
/// here, not in the renderer.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SkipClass {
    /// Slot has a canonical block (direct `Finalized` or via parent
    /// chain of one). We missed a real slot — bad.
    OnCanonical,
    /// We voted skip and have no positive evidence either way.
    Indeterminate,
}

/// `(slot, hash)` pair — used as both edge endpoints and canonical
/// chain anchors throughout the pane.
pub(super) type BlockId = (u64, String);

pub struct ChainPane {
    pub(super) slots: VecDeque<SlotState>,
    /// `(slot, hash) → (parent_slot, parent_hash)` edges from
    /// `Block` events. Drives the canonical walk-back.
    pub(super) parents: HashMap<BlockId, BlockId>,
    /// Set of `(slot, hash)` pairs proven canonical, anchored by
    /// `Finalized` events and walked back through `parents`.
    pub(super) canonical: HashSet<BlockId>,
    /// Slot-only projection of `canonical`. Used by `classify_skip`
    /// for O(1) lookup; previously a linear scan of `canonical` per
    /// skipped slot per render frame. Kept synchronised with
    /// `canonical` in [`Self::mark_canonical_and_walk_back`] and
    /// [`Self::prune`].
    pub(super) canonical_slots: HashSet<u64>,
    /// Count of malformed parent edges seen during walk-back
    /// (`parent.0 >= current.0`). Surfaced on the snapshot row only
    /// when nonzero so the operator notices upstream parser regressions
    /// rather than silently degrading canonical-skip detection.
    pub(super) walk_back_anomalies: u64,
    pub(super) last_root: Option<u64>,
    /// Count of events observed since the pane was constructed.
    /// Drives the spinner — every Nth event ticks one frame, so the
    /// spinner pauses when the stream is silent (honest liveness).
    pub(super) event_count: u64,
    /// Wall-clock instant of the most recent event observation. The
    /// spinner only advances if events arrived within the shared
    /// liveness window; otherwise the cell freezes.
    pub(super) last_event_at: Option<Instant>,
    /// Cannon-particle visualisation state. Each new slot the pane
    /// observes fires one particle from the cannon (top-left of the
    /// pane); particles land in the matrix after a fixed flight
    /// duration. Render reads `particles` (in flight) and `matrix`
    /// (landed) to paint the visualisation. See [`super::particle`].
    pub(super) cannon: CannonSystem,
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
            cannon: CannonSystem::new(),
        }
    }

    pub(super) fn tip_slot(&self) -> Option<u64> {
        self.slots.back().map(|s| s.slot)
    }

    /// Look up a slot's state in the retained deque. Returns `None`
    /// when the slot has been pruned or was never observed. The
    /// classifier ([`super::glyph::classify_slot`]) calls this once
    /// per visible matrix cell per frame, so the implementation is
    /// O(log N) via [`VecDeque::binary_search_by_key`] against the
    /// sorted-by-slot invariant `upsert_slot` maintains.
    pub(super) fn slot_state(&self, slot: u64) -> Option<&SlotState> {
        slot_state_in_deque(&self.slots, slot)
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
        // `observe_event`). `VecDeque::insert` is O(N) shifts but no
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
    ///
    /// **Test-only API** since LIVE-52 — see [`SkipClass`] note.
    #[cfg(test)]
    pub(super) fn classify_skip(&self, slot: u64) -> SkipClass {
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

    /// **Test-only API** since LIVE-52.
    #[cfg(test)]
    pub(super) fn fork_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_forked()).count()
    }

    #[cfg(test)]
    pub(super) fn canonical_skip_count(&self) -> usize {
        self.skip_tallies().0
    }

    /// Single-pass tally `(canonical_skips, indeterminate_skips)` over
    /// the retained slot deque.
    ///
    /// **Test-only API** since LIVE-52 — the header dropped the
    /// CSKIP/indet counters in favour of the bucket glyphs that
    /// surface those classes visually.
    #[cfg(test)]
    pub(super) fn skip_tallies(&self) -> (usize, usize) {
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
    pub(super) fn timing_table(&self) -> TimingTable {
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
    pub(super) fn indeterminate_skip_count(&self) -> usize {
        self.skip_tallies().1
    }

    /// Apply an event to the pane state. Called from the `Pane`
    /// `on_event` shim in [`super`].
    pub(super) fn observe_event(&mut self, ev: &Event) {
        self.event_count = self.event_count.saturating_add(1);
        self.last_event_at = Some(Instant::now());
        // `EventKind::local_skip_vote_slot` matches both round-1 and
        // fallback-round skip votes — a future third skip variant
        // would slot in without touching the per-pane match arms.
        if let Some(slot) = ev.kind.local_skip_vote_slot() {
            let s = self.upsert_slot(slot);
            s.skipped = true;
            self.cannon.fire(slot);
            self.prune();
            return;
        }
        // Slot the event addresses, captured for the cannon spawn at
        // the bottom of the function so each arm doesn't repeat the
        // `cannon.fire()` call. `None` for non-slot events (roots).
        let particle_slot: Option<u64> = match &ev.kind {
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
                Some(*slot)
            }
            EventKind::Finalized { slot, hash, fast } => {
                let ts = ev.ts;
                let s = self.upsert_slot(*slot);
                s.record_hash(hash);
                s.fast_finalized = Some(*fast);
                s.finalized_at.get_or_insert(ts);
                self.mark_canonical_and_walk_back(*slot, hash.clone());
                Some(*slot)
            }
            EventKind::FirstShred { slot } => {
                let ts = ev.ts;
                let s = self.upsert_slot(*slot);
                s.first_shred_at.get_or_insert(ts);
                Some(*slot)
            }
            EventKind::BankFrozen { slot, .. } => {
                let ts = ev.ts;
                let s = self.upsert_slot(*slot);
                s.bank_frozen_at.get_or_insert(ts);
                Some(*slot)
            }
            EventKind::VotingNotarize { slot, .. } => {
                let s = self.upsert_slot(*slot);
                s.notarized = true;
                Some(*slot)
            }
            EventKind::SettingRoot { slot } | EventKind::NewRoot { slot } => {
                self.last_root = Some(*slot);
                None
            }
            // ProduceWindow is consumed by the block-production pane;
            // leader-window events do not belong in the chain log.
            // Skip-vote variants are handled by the
            // `local_skip_vote_slot` fast-path above.
            _ => return,
        };
        if let Some(slot) = particle_slot {
            self.cannon.fire(slot);
        }
        self.prune();
    }
}

impl Default for ChainPane {
    fn default() -> Self {
        Self::new()
    }
}

/// Free-function variant of [`ChainPane::slot_state`]. Used by the
/// `Pane::tick` refresh in [`super`] which needs to read the slots
/// deque while also holding a `&mut` borrow on a different
/// `ChainPane` field — the method form's `&self` receiver would
/// conflict with that disjoint mutable borrow.
///
/// O(log N) via [`VecDeque::binary_search_by_key`] against the
/// sorted-by-slot invariant.
pub(super) fn slot_state_in_deque(slots: &VecDeque<SlotState>, slot: u64) -> Option<&SlotState> {
    let (front, back) = slots.as_slices();
    // VecDeque does not expose binary_search directly across the
    // wrap boundary, so search the two contiguous slices and fall
    // through to the second if the first does not contain the slot.
    // Both slices stay sorted by `upsert_slot`.
    if let Ok(idx) = front.binary_search_by_key(&slot, |s| s.slot) {
        return Some(&front[idx]);
    }
    if let Ok(idx) = back.binary_search_by_key(&slot, |s| s.slot) {
        return Some(&back[idx]);
    }
    None
}
