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
//!
//! ## Module layout
//!
//! - [`state`] — [`ChainPane`], `SlotState`, `SkipClass`, walk-back,
//!   skip classification, event observation, derived queries.
//! - [`render`] — ratatui rendering (border, spinner, timing table,
//!   snapshot row).
//! - [`format`] — `TimingTable` + stage-percentile helpers shared
//!   between state and render.

mod format;
mod glyph;
mod particle;
mod render;
mod state;

use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::live::animation::Pane;
use crate::parser::Event;

pub use render::PANE_HEIGHT;
pub use state::ChainPane;

impl Pane for ChainPane {
    fn on_event(&mut self, ev: &Event) {
        self.observe_event(ev);
    }

    fn tick(&mut self, now: Instant) {
        // Spinner derives from `event_count` / `last_event_at`,
        // updated in `observe_event` — it does not need a per-tick
        // advance. The cannon-particle world DOES: each frame
        // advances in-flight particles by `(now - last_tick)` and
        // lands any whose TTL has elapsed.
        self.cannon.tick(now);
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        render::render(self, frame, area);
    }
}

#[cfg(test)]
mod tests {
    use super::glyph::classify_slot;
    use super::render::header_line;
    use super::state::{ChainPane, SkipClass};
    use crate::live::animation::Pane;
    use crate::parser::{Event, EventKind};
    use ratatui::style::{Color, Modifier};
    use ratatui::text::Line;

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
    fn header_line_drops_skip_and_fork_counters() {
        // Counters (CSKIP / indet / forks) were moved off the
        // header in step 3 because the bucket itself paints those
        // classes with `▴ ▾ ⊕` glyphs — a numeric count duplicates
        // signal the eye already absorbs from the matrix. The
        // header now carries spinner + tip + cannon glyph only
        // (plus the silent-default ` anom` segment).
        let p = ChainPane::new();
        let text = line_text(&header_line(&p));
        assert!(
            !text.contains("CSKIP"),
            "header must not surface CSKIP counter: {text:?}"
        );
        assert!(
            !text.contains("indet"),
            "header must not surface indet counter: {text:?}"
        );
        assert!(
            !text.contains("forks"),
            "header must not surface forks counter: {text:?}"
        );
        assert!(
            text.contains('▶'),
            "header must show the cannon glyph: {text:?}"
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
        // Drive via `on_event(FirstShred)` so the deque insertion
        // path is exercised through the public surface — the
        // `upsert_slot` helper is module-private.
        for slot in [105, 100, 103, 102, 110] {
            p.on_event(&mk(EventKind::FirstShred { slot }));
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
        //
        // Drive via `on_event(Block)` so the mutation flows through
        // the public surface — every `Block` event records its hash
        // on the matching slot, which would land on the wrong slot
        // if the upsert returned the wrong index.
        let mut p = ChainPane::new();
        // Tip extension at 100; then out-of-order at 50.
        p.on_event(&block_ev(100, "a", 99, "p"));
        p.on_event(&block_ev(50, "b", 49, "p"));
        // Out-of-order re-touch of slot 100 — must land on the
        // slot-100 entry, not slot-50 or slot-0.
        p.on_event(&block_ev(100, "c", 99, "p"));
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
    fn header_line_surfaces_anomaly_only_when_nonzero() {
        // CORRECT-01 display policy: silence on a healthy stream,
        // surface a red ` anom` segment only when anomalies were seen.
        let mut p = ChainPane::new();
        let text = line_text(&header_line(&p));
        assert!(
            !text.contains(" anom"),
            "header must stay silent on healthy stream: {text:?}"
        );
        // Inject a corrupt parent edge and finalise to trip the
        // anomaly path.
        p.on_event(&block_ev(100, "a", 100, "self-loop"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "a".into(),
            fast: true,
        }));
        let text = line_text(&header_line(&p));
        assert!(
            text.contains(" anom"),
            "header must surface anomaly counter when nonzero: {text:?}"
        );
        assert!(
            text.contains("1 anom"),
            "anomaly count missing in header: {text:?}"
        );
    }

    // ---- Cell classifier tests --------------------------------------

    #[test]
    fn classifier_returns_dim_dot_for_unknown_slot() {
        // Slot never observed → not in the deque → classifier must
        // return the "unknown" dim dot rather than panicking or
        // returning a misleading state.
        let p = ChainPane::new();
        let cell = classify_slot(&p, 999);
        assert_eq!(cell.ch, '·');
        assert_eq!(cell.style.fg, Some(Color::DarkGray));
        assert!(cell.style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn classifier_returns_pending_dot_for_observed_unfinalized_slot() {
        let mut p = ChainPane::new();
        p.on_event(&mk(EventKind::FirstShred { slot: 100 }));
        let cell = classify_slot(&p, 100);
        assert_eq!(cell.ch, '·');
        assert_eq!(cell.style.fg, Some(Color::DarkGray));
        assert!(
            !cell.style.add_modifier.contains(Modifier::DIM),
            "pending (observed) must be brighter than unknown (unobserved): \
             {:?}",
            cell.style
        );
    }

    #[test]
    fn classifier_returns_bold_green_square_for_canonical_fast_finalized() {
        let mut p = ChainPane::new();
        p.on_event(&block_ev(100, "a", 99, "root"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "a".into(),
            fast: true,
        }));
        let cell = classify_slot(&p, 100);
        assert_eq!(cell.ch, '■');
        assert_eq!(cell.style.fg, Some(Color::Green));
        assert!(cell.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn classifier_returns_half_circle_for_slow_finalized() {
        let mut p = ChainPane::new();
        p.on_event(&block_ev(100, "a", 99, "root"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "a".into(),
            fast: false,
        }));
        let cell = classify_slot(&p, 100);
        assert_eq!(cell.ch, '◐');
        assert_eq!(cell.style.fg, Some(Color::Yellow));
    }

    #[test]
    fn classifier_returns_bold_red_up_triangle_for_canonical_skip() {
        // Canonical-skip: chain kept the slot, we voted skip — the
        // bad signal that anchors the operator's eye. Up triangle
        // mirrors the b8ec4ed event log vocabulary.
        let mut p = ChainPane::new();
        p.on_event(&block_ev(100, "a", 99, "root"));
        p.on_event(&mk(EventKind::Finalized {
            slot: 100,
            hash: "a".into(),
            fast: true,
        }));
        p.on_event(&mk(EventKind::VotingSkip { slot: 100 }));
        let cell = classify_slot(&p, 100);
        assert_eq!(cell.ch, '▴');
        assert_eq!(cell.style.fg, Some(Color::Red));
        assert!(cell.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn classifier_returns_plain_red_down_triangle_for_indeterminate_skip() {
        // We voted skip but no canonical evidence on either fork —
        // softer signal than canonical-skip, no BOLD.
        let mut p = ChainPane::new();
        p.on_event(&mk(EventKind::VotingSkip { slot: 100 }));
        let cell = classify_slot(&p, 100);
        assert_eq!(cell.ch, '▾');
        assert_eq!(cell.style.fg, Some(Color::Red));
        assert!(
            !cell.style.add_modifier.contains(Modifier::BOLD),
            "indeterminate skip must not be BOLD: {:?}",
            cell.style
        );
    }

    #[test]
    fn classifier_returns_bold_yellow_circled_plus_for_fork() {
        // Two distinct hashes on the same slot ⇒ fork. The fork
        // glyph beats canonical-skip precedence so a forked slot
        // that also has a skip vote still renders as ⊕.
        let mut p = ChainPane::new();
        p.on_event(&block_ev(100, "a", 99, "root"));
        p.on_event(&block_ev(100, "b", 99, "root"));
        let cell = classify_slot(&p, 100);
        assert_eq!(cell.ch, '⊕');
        assert_eq!(cell.style.fg, Some(Color::Yellow));
        assert!(cell.style.add_modifier.contains(Modifier::BOLD));
    }
}
