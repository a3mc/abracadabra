// Integration tests use `expect()` for setup that cannot fail in a
// well-formed test binary (TestBackend construction, Mutex lock,
// terminal draw against a fixed-size buffer). The unwrap/expect ban
// applies to production code paths; tests are exempt per CLAUDE.md.
#![allow(clippy::expect_used, clippy::needless_collect)]

//! End-to-end verification that the LIVE-67 truecolor wiring works.
//!
//! Math tests in `tui::truecolor::tests` cover the quantiser; pure-
//! function tests cover the detection ladder. This file closes the
//! last gap: that the WIRING from `truecolor::rgb` through the chain-
//! pane chip accessors and the tx-pressure header all the way to the
//! rendered ratatui buffer actually carries the quantised value.
//!
//! Tests in this file mutate the process-global `TRUECOLOR_ENABLED`
//! flag. They serialise through a single [`Mutex`] so the two
//! variants never run in parallel; each test ends with the global
//! restored to truecolor-on (the safe default) so any state leak is
//! harmless.
//!
//! What we prove:
//!
//! - **truecolor OFF**: every cell in the rendered chain pane and
//!   tx-pressure buffer carries either `Color::Indexed` / a named
//!   ANSI colour / `Color::Reset` — NEVER `Color::Rgb`. Any
//!   `Color::Rgb` cell would emit `\e[38;2;R;G;B m`, the escape
//!   sequence macOS Terminal.app misparses.
//!
//! - **truecolor OFF**: at least one cell carries a `Color::Indexed`
//!   in the 6×6×6 cube range (16..=231), proving the quantiser was
//!   actually invoked rather than the wiring silently falling through.
//!
//! - **truecolor ON**: the same render produces ≥1 `Color::Rgb`
//!   cell — anchors the capable-terminal path against regression.

use std::sync::{Mutex, PoisonError};

use abracadabra::live::animation::Pane;
use abracadabra::live::scenes::chain::ChainPane;
use abracadabra::live::scenes::tx_pressure::TxPressurePane;
use abracadabra::parser::{Event, EventKind};
use abracadabra::tui::truecolor;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::Terminal;
use time::OffsetDateTime;

/// Serialises tests that mutate the process-global truecolor flag.
/// Both ON-path and OFF-path tests acquire this before flipping
/// `TRUECOLOR_ENABLED`, so they never interleave. Poison is drained
/// via `PoisonError::into_inner` so a panic inside one critical
/// section does not mask the next test's real failure with a
/// generic "test mutex" panic.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Seed a chain pane with enough event volume that the chip line and
/// slot chip render with real values. We need real values because the
/// chip backgrounds are the surface the LIVE-67 wiring paints.
fn seeded_chain_pane() -> ChainPane {
    let mut pane = ChainPane::new();
    let base_ts = OffsetDateTime::UNIX_EPOCH;
    for i in 0..40u64 {
        let slot = 500_000 + i;
        let ts = base_ts + time::Duration::milliseconds(i64::try_from(i * 400).unwrap_or(0));
        pane.on_event(&Event {
            ts,
            kind: EventKind::Block {
                slot,
                hash: "h".into(),
                parent_slot: slot.saturating_sub(1),
                parent_hash: "p".into(),
            },
        });
        pane.on_event(&Event {
            ts: ts + time::Duration::milliseconds(120),
            kind: EventKind::BankFrozen {
                slot,
                hash: "h".into(),
                signature_count: 1_000,
            },
        });
        pane.on_event(&Event {
            ts: ts + time::Duration::milliseconds(180),
            kind: EventKind::Finalized {
                slot,
                hash: "h".into(),
                fast: true,
            },
        });
    }
    pane
}

fn seeded_tx_pressure_pane() -> TxPressurePane {
    let mut pane = TxPressurePane::new();
    for i in 0..20u64 {
        pane.on_event(&Event {
            ts: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(i64::try_from(i).unwrap_or(0)),
            kind: EventKind::BankFrozen {
                slot: 500_000 + i,
                hash: "h".into(),
                signature_count: 1_000 * (i + 1),
            },
        });
    }
    pane
}

/// Collect every distinct `Color` that appears on either fg or bg of
/// any cell in `buf`. `Color::Reset` cells are filler from the
/// test-backend default state and carry no signal about our wiring,
/// so they are skipped.
fn distinct_colors(buf: &Buffer) -> std::collections::HashSet<Color> {
    let mut out = std::collections::HashSet::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            if cell.fg != Color::Reset {
                out.insert(cell.fg);
            }
            if cell.bg != Color::Reset {
                out.insert(cell.bg);
            }
        }
    }
    out
}

fn render_chain_pane(pane: &ChainPane) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(80, 16)).expect("test backend");
    terminal
        .draw(|f| pane.render(f, Rect::new(0, 0, 80, 16)))
        .expect("chain pane render");
    terminal.backend().buffer().clone()
}

fn render_tx_pressure_pane(pane: &TxPressurePane) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(80, 16)).expect("test backend");
    terminal
        .draw(|f| pane.render(f, Rect::new(0, 0, 80, 16)))
        .expect("tx pressure render");
    terminal.backend().buffer().clone()
}

// ============================================================
// Truecolor OFF — the Apple_Terminal fallback path
// ============================================================

#[test]
fn truecolor_off_chain_pane_emits_zero_rgb_cells() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    truecolor::init_from_env(true);

    let pane = seeded_chain_pane();
    let buf = render_chain_pane(&pane);
    let colors = distinct_colors(&buf);

    let leaked_rgb: Vec<Color> = colors
        .iter()
        .filter(|c| matches!(c, Color::Rgb(..)))
        .copied()
        .collect();
    assert!(
        leaked_rgb.is_empty(),
        "truecolor-off chain render leaked Color::Rgb cells: {leaked_rgb:?}. \
         These would render as fragmented multi-colour text on macOS \
         Terminal.app. Every Color::Rgb construction in the chain pane \
         must route through truecolor::rgb()."
    );

    truecolor::init_from_env(false);
}

#[test]
fn truecolor_off_chain_pane_produces_indexed_cube_colours() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    truecolor::init_from_env(true);

    let pane = seeded_chain_pane();
    let buf = render_chain_pane(&pane);
    let colors = distinct_colors(&buf);

    let indexed_cube: Vec<u8> = colors
        .iter()
        .filter_map(|c| match c {
            Color::Indexed(n) if (16..=231).contains(n) => Some(*n),
            _ => None,
        })
        .collect();
    assert!(
        !indexed_cube.is_empty(),
        "truecolor-off chain render must produce ≥1 Color::Indexed cell \
         in the 6×6×6 cube range (16..=231); got distinct colours \
         {colors:?}. If this fails, truecolor::rgb() is not actually \
         being invoked for the chip backgrounds."
    );

    truecolor::init_from_env(false);
}

#[test]
fn truecolor_off_tx_pressure_emits_zero_rgb_cells() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    truecolor::init_from_env(true);

    let pane = seeded_tx_pressure_pane();
    let buf = render_tx_pressure_pane(&pane);
    let colors = distinct_colors(&buf);

    let leaked_rgb: Vec<Color> = colors
        .iter()
        .filter(|c| matches!(c, Color::Rgb(..)))
        .copied()
        .collect();
    assert!(
        leaked_rgb.is_empty(),
        "truecolor-off tx-pressure render leaked Color::Rgb cells: \
         {leaked_rgb:?}. Header text colours, gradient curve, and \
         dimmed area-fill must all route through truecolor::rgb()."
    );

    truecolor::init_from_env(false);
}

// ============================================================
// Truecolor ON (default) — capable-terminal sanity check
// ============================================================

#[test]
fn truecolor_on_chain_pane_renders_rgb_cells() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    truecolor::init_from_env(false);

    let pane = seeded_chain_pane();
    let buf = render_chain_pane(&pane);
    let colors = distinct_colors(&buf);

    let rgb_count = colors
        .iter()
        .filter(|c| matches!(c, Color::Rgb(..)))
        .count();
    assert!(
        rgb_count > 0,
        "truecolor-on chain render must produce ≥1 Color::Rgb cell; \
         got {colors:?}. If this fails, the modern-terminal path \
         regressed while wiring the fallback. The chain pane's chip \
         backgrounds should appear as truecolor RGB when capable."
    );

    truecolor::init_from_env(false);
}
