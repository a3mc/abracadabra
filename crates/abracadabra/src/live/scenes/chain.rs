//! Chain pane — calm spinner + event log.
//!
//! Most slots on a healthy validator are fast-finalised canonical
//! slots that we notarised in time — there is no useful per-slot
//! visual to draw for them. Instead the pane shows:
//!
//! - A spinner and the **tip slot number** at the top, proving the
//!   stream is live and giving the operator a slot counter.
//! - A short **event log** of *only* the notable things that
//!   happened recently: forks, canonical-skips, slow finalisations,
//!   slots we did not notarise, and leader-window announcements.
//!
//! The underlying graph model still tracks every `Block` /
//! `Finalized` / `VotingSkip` / `VotingNotarize` /
//! `SettingRoot` / `ProduceWindow` event:
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

use crate::live::animation::Pane;
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
}

impl SlotState {
    const fn new(slot: u64) -> Self {
        Self {
            slot,
            hashes: Vec::new(),
            skipped: false,
            notarized: false,
            fast_finalized: None,
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
    last_root: Option<u64>,
    /// Recent leader windows observed via `ProduceWindow`. Surfaced
    /// in the event log as `★ leader N..M`.
    recent_leader_windows: VecDeque<(u64, u64)>,
    /// Anchor for the render-time spinner. Not read in `tick`;
    /// driven by `Instant::elapsed` purely in `render`.
    stream_start: Instant,
}

const RECENT_WINDOWS_CAPACITY: usize = 8;
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

impl ChainPane {
    pub fn new() -> Self {
        Self {
            slots: VecDeque::with_capacity(HISTORY_CAPACITY),
            parents: HashMap::with_capacity(EDGES_CAPACITY),
            canonical: HashSet::with_capacity(EDGES_CAPACITY),
            last_root: None,
            recent_leader_windows: VecDeque::with_capacity(RECENT_WINDOWS_CAPACITY),
            stream_start: Instant::now(),
        }
    }

    fn tip_slot(&self) -> Option<u64> {
        self.slots.back().map(|s| s.slot)
    }

    fn upsert_slot(&mut self, slot: u64) -> &mut SlotState {
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
                if let Some(i) = self.slots.iter().position(|s| s.slot == slot) {
                    i
                } else {
                    self.slots.push_back(SlotState::new(slot));
                    let mut v: Vec<_> = self.slots.drain(..).collect();
                    v.sort_by_key(|s| s.slot);
                    self.slots.extend(v);
                    self.slots.iter().position(|s| s.slot == slot).unwrap_or(0)
                }
            }
        };
        &mut self.slots[idx]
    }

    /// Mark `(slot, hash)` canonical and walk back through parent
    /// edges, marking every ancestor canonical. Stops at edges we
    /// don't have (chain root or out-of-window slots).
    fn mark_canonical_and_walk_back(&mut self, slot: u64, hash: String) {
        let mut current = (slot, hash);
        loop {
            if !self.canonical.insert(current.clone()) {
                // Already canonical — chain explored, stop.
                break;
            }
            match self.parents.get(&current) {
                Some(parent) => {
                    if parent.0 >= current.0 {
                        // Sanity: parent should be older.
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
    fn classify_skip(&self, slot: u64) -> SkipClass {
        if self.canonical.iter().any(|(s, _)| *s == slot) {
            SkipClass::OnCanonical
        } else {
            SkipClass::Indeterminate
        }
    }

    fn prune(&mut self) {
        while self.slots.len() > HISTORY_CAPACITY {
            if let Some(s) = self.slots.pop_front() {
                for hash in &s.hashes {
                    let key = (s.slot, hash.clone());
                    self.parents.remove(&key);
                    self.canonical.remove(&key);
                }
            }
        }
        if let Some(root) = self.last_root {
            let cutoff = root.saturating_sub(ROOT_TRAILING_SLOTS);
            while let Some(s) = self.slots.front() {
                if s.slot < cutoff {
                    let dropped = self.slots.pop_front();
                    if let Some(s) = dropped {
                        for hash in &s.hashes {
                            let key = (s.slot, hash.clone());
                            self.parents.remove(&key);
                            self.canonical.remove(&key);
                        }
                    }
                } else {
                    break;
                }
            }
        }
    }

    fn fork_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_forked()).count()
    }

    fn canonical_skip_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.skipped && self.classify_skip(s.slot) == SkipClass::OnCanonical)
            .count()
    }

    fn indeterminate_skip_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.skipped && self.classify_skip(s.slot) == SkipClass::Indeterminate)
            .count()
    }
}

impl Default for ChainPane {
    fn default() -> Self {
        Self::new()
    }
}

impl Pane for ChainPane {
    fn on_event(&mut self, ev: &Event) {
        match &ev.kind {
            EventKind::Block {
                slot,
                hash,
                parent_slot,
                parent_hash,
            } => {
                let s = self.upsert_slot(*slot);
                s.record_hash(hash);
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
                let s = self.upsert_slot(*slot);
                s.record_hash(hash);
                s.fast_finalized = Some(*fast);
                self.mark_canonical_and_walk_back(*slot, hash.clone());
            }
            EventKind::VotingNotarize { slot, .. } => {
                let s = self.upsert_slot(*slot);
                s.notarized = true;
            }
            EventKind::VotingSkip { slot } => {
                let s = self.upsert_slot(*slot);
                s.skipped = true;
            }
            EventKind::SettingRoot { slot } | EventKind::NewRoot { slot } => {
                self.last_root = Some(*slot);
            }
            EventKind::ProduceWindow { start, end, .. } => {
                if *end >= *start && end.saturating_sub(*start) <= 32 {
                    self.recent_leader_windows.push_back((*start, *end));
                    while self.recent_leader_windows.len() > RECENT_WINDOWS_CAPACITY {
                        self.recent_leader_windows.pop_front();
                    }
                }
            }
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
                Constraint::Length(1), // "recent activity" label
                Constraint::Min(1),    // event log
                Constraint::Length(1), // snapshot
            ])
            .split(inner);

        self.render_tip(frame, chunks[1]);
        Self::render_section_label(frame, chunks[3], "recent activity");
        self.render_event_log(frame, chunks[4]);
        self.render_snapshot(frame, chunks[5]);
    }
}

/// One row in the "recent activity" log: a slot number, a glyph,
/// and a short description. Boring slots (fast-finalised canonical
/// that we notarised in time) never produce a log entry.
#[derive(Debug, Clone)]
struct EventEntry {
    slot: u64,
    glyph: &'static str,
    colour: Color,
    label: String,
}

impl ChainPane {
    fn render_tip(&self, frame: &mut Frame<'_>, area: Rect) {
        let elapsed_ms = self.stream_start.elapsed().as_millis() as u64;
        let spinner_idx = (elapsed_ms / 100) as usize % SPINNER.len();
        let spinner = SPINNER[spinner_idx];
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

    fn render_event_log(&self, frame: &mut Frame<'_>, area: Rect) {
        let max = area.height as usize;
        if max == 0 {
            return;
        }
        let events = self.recent_events(max);
        if events.is_empty() {
            let line = Line::from(vec![
                Span::raw("     "),
                Span::styled(
                    "(no notable events yet)",
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            let row = Rect::new(area.x, area.y, area.width, 1);
            frame.render_widget(Paragraph::new(line), row);
            return;
        }
        for (i, e) in events.iter().enumerate().take(max) {
            let y = area.y + i as u16;
            let row = Rect::new(area.x, y, area.width, 1);
            let line = Line::from(vec![
                Span::styled(
                    format!("    {}", e.slot),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
                Span::raw("  "),
                Span::styled(
                    e.glyph.to_owned(),
                    Style::default().fg(e.colour).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(e.label.clone(), Style::default().fg(Color::White)),
            ]);
            frame.render_widget(Paragraph::new(line), row);
        }
    }

    /// Derive the event log from current slot state + leader window
    /// deque. Newest first. Boring fast-finalised-canonical slots that
    /// we notarised in time produce no entry.
    fn recent_events(&self, max: usize) -> Vec<EventEntry> {
        let mut out: Vec<EventEntry> = Vec::new();
        for s in self.slots.iter().rev() {
            if out.len() >= max * 2 {
                break;
            }
            if s.is_forked() {
                out.push(EventEntry {
                    slot: s.slot,
                    glyph: "⊕",
                    colour: Color::Yellow,
                    label: format!("fork ({} blocks)", s.hashes.len()),
                });
                continue;
            }
            let canonical_here = s
                .hashes
                .iter()
                .any(|h| self.canonical.contains(&(s.slot, h.clone())));
            if canonical_here && s.skipped {
                out.push(EventEntry {
                    slot: s.slot,
                    glyph: "▴",
                    colour: Color::Red,
                    label: "canonical-skip".to_owned(),
                });
                continue;
            }
            if canonical_here && !s.notarized {
                out.push(EventEntry {
                    slot: s.slot,
                    glyph: "○",
                    colour: Color::Yellow,
                    label: "canonical, we did not notarise".to_owned(),
                });
                continue;
            }
            if canonical_here && s.fast_finalized == Some(false) {
                out.push(EventEntry {
                    slot: s.slot,
                    glyph: "◐",
                    colour: Color::Yellow,
                    label: "slow finalise".to_owned(),
                });
            }
        }
        for &(start, end) in self.recent_leader_windows.iter().rev().take(2) {
            out.push(EventEntry {
                slot: start,
                glyph: "★",
                colour: Color::Green,
                label: format!("leader {start}..{end}"),
            });
        }
        out.sort_by_key(|e| std::cmp::Reverse(e.slot));
        out.truncate(max);
        out
    }

    fn render_snapshot(&self, frame: &mut Frame<'_>, area: Rect) {
        let canonical_skips = self.canonical_skip_count();
        let indeterminate = self.indeterminate_skip_count();
        let forks = self.fork_count();

        let range = match (self.slots.front(), self.slots.back()) {
            (Some(f), Some(l)) if f.slot != l.slot => format!("{}..{}", f.slot, l.slot),
            (Some(f), _) => format!("{}", f.slot),
            _ => "—".to_owned(),
        };

        let canon_style = if canonical_skips > 0 {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
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

        let line = Line::from(vec![
            Span::styled(" slots ", theme::label_style()),
            Span::styled(
                range,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            sep(),
            Span::styled(canonical_skips.to_string(), canon_style),
            Span::styled(" canon", theme::label_style()),
            sep(),
            Span::styled(
                indeterminate.to_string(),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(" indet", theme::label_style()),
            sep(),
            Span::styled(forks.to_string(), fork_style),
            Span::styled(" forks", theme::label_style()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
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
    fn canonical_slot_we_did_not_notarise_appears_in_event_log() {
        let mut p = ChainPane::new();
        p.on_event(&block_ev(100, "a", 99, "root"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "a".into(),
            fast: true,
        }));
        // No VotingNotarize for slot 100 — should surface as a notable event.
        let events = p.recent_events(10);
        let e = events
            .iter()
            .find(|e| e.slot == 100)
            .expect("event for 100");
        assert_eq!(e.glyph, "○");
        assert!(e.label.contains("notarise"), "got {}", e.label);
    }

    #[test]
    fn slow_finalised_with_notarise_appears_in_event_log() {
        let mut p = ChainPane::new();
        p.on_event(&block_ev(100, "a", 99, "root"));
        p.on_event(&mk(EventKind::VotingNotarize {
            slot: 100,
            hash: "a".into(),
        }));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "a".into(),
            fast: false,
        }));
        let events = p.recent_events(10);
        let e = events
            .iter()
            .find(|e| e.slot == 100)
            .expect("event for 100");
        assert_eq!(e.glyph, "◐");
        assert!(e.label.contains("slow"), "got {}", e.label);
    }

    #[test]
    fn fast_finalised_with_notarise_is_a_boring_slot_no_event() {
        let mut p = ChainPane::new();
        p.on_event(&block_ev(100, "a", 99, "root"));
        p.on_event(&mk(EventKind::VotingNotarize {
            slot: 100,
            hash: "a".into(),
        }));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "a".into(),
            fast: true,
        }));
        // Fast + notarised = ideal slot. Nothing to log.
        let events = p.recent_events(10);
        assert!(
            events.iter().all(|e| e.slot != 100),
            "fast notarised slot should not appear in event log"
        );
    }

    #[test]
    fn produce_window_event_appears_in_event_log() {
        let mut p = ChainPane::new();
        p.on_event(&mk(EventKind::ProduceWindow {
            start: 200,
            end: 203,
            parent_slot: 199,
            parent_hash: "x".into(),
        }));
        let events = p.recent_events(10);
        let e = events.iter().find(|e| e.slot == 200).expect("leader event");
        assert_eq!(e.glyph, "★");
        assert!(e.label.contains("leader"), "got {}", e.label);
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
}
