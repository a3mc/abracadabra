//! Per-slot classifier — turns a `SlotState` (plus the pane's
//! canonical-slot projection) into a glyph + colour pair used by
//! both the matrix renderer (for landed cells) and the particle
//! renderer (for in-flight slots).
//!
//! Vocabulary mirrors the event log of the pre-LIVE-39 chain pane so
//! glyphs already familiar to operators carry the same meaning here:
//!
//! | Glyph | Colour | Meaning |
//! |-------|--------|---------|
//! | `★`   | magenta BOLD | OUR leader slot — canonical + fast-finalised (success) |
//! | `★`   | yellow BOLD  | OUR leader slot — canonical + slow-finalised |
//! | `★`   | green DIM    | OUR leader slot — canonical by walk-back only (banked) |
//! | `★`   | red BOLD     | OUR leader slot — we voted skip (LSKIP) |
//! | `★`   | cyan BOLD    | OUR leader slot — pending (no canonical / no skip yet) |
//! | `■`   | green BOLD | canonical + fast-finalised |
//! | `■`   | green DIM  | canonical + no fast/slow yet (notarised silent) |
//! | `◐`   | yellow     | canonical + slow-finalised (Finalized.fast == false) |
//! | `○`   | yellow     | canonical-by-walkback + we observed VotingNotarize |
//! | `▴`   | red BOLD   | canonical-skip (we voted skip on a canonical slot) |
//! | `▾`   | red        | vote-skip with no canonical evidence (indeterminate) |
//! | `⊕`   | yellow BOLD | fork (≥2 distinct hashes on this slot) |
//! | `·`   | dark-gray  | pending — slot seen, no terminal classification yet |
//! | `·`   | dark-gray DIM | unknown — slot is not in our retained deque (pruned) |
//!
//! Precedence (top → bottom, first match wins):
//!
//! 1. `Unknown` — slot pruned out of the deque.
//! 2. `Fork` — `len(hashes) >= 2` (kept above `we_are_leader` so a
//!    fork on our own slot still surfaces with the ⊕ shape).
//! 3. `OwnLeader{Skip,Fast,Slow,Banked,Pending}` — substate of our
//!    own leader slot, painted as `★` in a colour that encodes the
//!    outcome.
//! 4. `CanonicalSkip` — skipped AND in `canonical_slots`.
//! 5. `VoteSkip` — skipped, no canonical evidence.
//! 6. `FastFinal` — canonical AND `fast_finalized == Some(true)`.
//! 7. `SlowFinal` — canonical AND `fast_finalized == Some(false)`.
//! 8. `Notarised` — canonical AND we observed VotingNotarize.
//! 9. `CanonicalSilent` — canonical via walk-back, no direct event.
//! 10. `Pending` — slot seen, no canonical evidence yet.

use ratatui::style::{Color, Modifier, Style};

use super::state::{ChainPane, SlotState};

/// Cell payload returned by [`classify_slot`]. Caller writes
/// `(ch, style)` into the buffer at the cell position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CellGlyph {
    pub(super) ch: char,
    pub(super) style: Style,
}

/// Classify `slot` against the pane's current state. Returns the
/// "unknown" dim gray dot when the slot is not in the retained
/// deque (e.g. pruned via the root cutoff). Used by the particle
/// renderer for in-flight slots — pruning during flight is rare
/// but possible.
pub(super) fn classify_slot(pane: &ChainPane, slot: u64) -> CellGlyph {
    classify_known(pane, slot).unwrap_or(UNKNOWN_GLYPH)
}

/// Classify `slot` against the pane's current state, returning
/// `None` when the slot is not in the retained deque. Used by the
/// bucket-cell glyph refresh: a `None` result means "do not change
/// the cached glyph". When the slot is later pruned the bucket cell
/// keeps the last known classification rather than degrading to the
/// unknown dot.
pub(super) fn classify_known(pane: &ChainPane, slot: u64) -> Option<CellGlyph> {
    let s = pane.slot_state(slot)?;
    Some(classify_slot_state(s, pane.canonical_slots.contains(&slot)))
}

/// Classify a slot from its [`SlotState`] + canonical-membership
/// flag. Used by the bucket-glyph refresh in
/// [`crate::live::scenes::chain`]'s `Pane::tick` handler, which has
/// disjoint-field access to the pane's `slots` deque and the
/// `canonical_slots` projection and so cannot call the
/// [`classify_slot`] helper that takes the whole `ChainPane`.
pub(super) fn classify_slot_state(s: &SlotState, is_canonical: bool) -> CellGlyph {
    // Fork is the highest-priority shape — even on our own slot a
    // ⊕ surfaces because two distinct hashes on the same slot is
    // operationally significant regardless of whose slot it is.
    if s.hashes.len() >= 2 {
        return CellGlyph {
            ch: '⊕',
            style: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        };
    }
    // OUR leader slot — paint as ★ with a colour that encodes the
    // substate. Sits above the generic skip / canonical branches
    // so own slots ALWAYS stand out in the bucket.
    if s.we_are_leader {
        if s.skipped {
            // LSKIP — we voted skip on our own leader window. The
            // worst-case signal in the chain pane.
            return CellGlyph {
                ch: '★',
                style: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            };
        }
        if is_canonical {
            return match s.fast_finalized {
                Some(true) => CellGlyph {
                    ch: '★',
                    style: Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                },
                Some(false) => CellGlyph {
                    ch: '★',
                    style: Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                },
                None => CellGlyph {
                    // Canonical via walk-back, no direct Finalized
                    // event yet. Dim green ★ reads as "banked but
                    // outcome not yet sealed".
                    ch: '★',
                    style: Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::DIM),
                },
            };
        }
        // We are leader, no canonical evidence and no skip vote yet
        // — the window just opened. Cyan BOLD ★ — outcome pending.
        return CellGlyph {
            ch: '★',
            style: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        };
    }
    if s.skipped {
        if is_canonical {
            return CellGlyph {
                ch: '▴',
                style: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            };
        }
        return CellGlyph {
            ch: '▾',
            style: Style::default().fg(Color::Red),
        };
    }
    if is_canonical {
        return match s.fast_finalized {
            Some(true) => CellGlyph {
                ch: '■',
                style: Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            },
            Some(false) => CellGlyph {
                ch: '◐',
                style: Style::default().fg(Color::Yellow),
            },
            None => {
                if s.notarized {
                    CellGlyph {
                        ch: '○',
                        style: Style::default().fg(Color::Yellow),
                    }
                } else {
                    CellGlyph {
                        ch: '■',
                        style: Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::DIM),
                    }
                }
            }
        };
    }
    CellGlyph {
        ch: '·',
        style: Style::default().fg(Color::DarkGray),
    }
}

/// The "unknown" dim gray dot returned by [`classify_slot`] when the
/// slot is not in the retained deque. Held in a `const` because it
/// has no runtime data — `Style::new()` is a const constructor.
const UNKNOWN_GLYPH: CellGlyph = CellGlyph {
    ch: '·',
    style: Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
};
