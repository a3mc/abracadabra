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
//! 2. `Fork` — `len(hashes) >= 2`.
//! 3. `CanonicalSkip` — skipped AND in `canonical_slots`.
//! 4. `VoteSkip` — skipped, no canonical evidence.
//! 5. `FastFinal` — canonical AND `fast_finalized == Some(true)`.
//! 6. `SlowFinal` — canonical AND `fast_finalized == Some(false)`.
//! 7. `Notarised` — canonical AND we observed VotingNotarize.
//! 8. `CanonicalSilent` — canonical via walk-back, no direct event.
//! 9. `Pending` — slot seen, no canonical evidence yet.

use ratatui::style::{Color, Modifier, Style};

use super::state::ChainPane;

/// Cell payload returned by [`classify_slot`]. Caller writes
/// `(ch, style)` into the buffer at the cell position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CellGlyph {
    pub(super) ch: char,
    pub(super) style: Style,
}

/// Classify `slot` against the pane's current state. See module
/// docs for the precedence ladder. Inputs are immutable; the
/// classifier may be called many times per frame for the same
/// slot (matrix render walks the landed deque).
pub(super) fn classify_slot(pane: &ChainPane, slot: u64) -> CellGlyph {
    let Some(s) = pane.slot_state(slot) else {
        return CellGlyph {
            ch: '·',
            style: Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        };
    };

    if s.hashes.len() >= 2 {
        return CellGlyph {
            ch: '⊕',
            style: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        };
    }

    if s.skipped {
        if pane.canonical_slots.contains(&s.slot) {
            // Canonical-skip: we voted skip but the chain kept the
            // slot — operationally bad signal, paint bold red.
            return CellGlyph {
                ch: '▴',
                style: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            };
        }
        // Indeterminate vote-skip: no canonical evidence on either
        // fork yet. Plain red, no bold — softer signal until the
        // classification firms up.
        return CellGlyph {
            ch: '▾',
            style: Style::default().fg(Color::Red),
        };
    }

    if pane.canonical_slots.contains(&s.slot) {
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
                    // Canonical by walk-back, no direct event — dim
                    // green so the eye reads it as "good but stale".
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
