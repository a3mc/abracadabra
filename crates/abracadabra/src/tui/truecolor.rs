//! Terminal truecolor capability detection + 6×6×6 cube fallback.
//!
//! macOS Terminal.app reports `TERM=xterm-256color` but does NOT parse
//! the SGR 38;2;R;G;B / 48;2;R;G;B sequences for 24-bit RGB — it
//! misparses each `;<digit>` group as an additional palette index, so a
//! `Color::Rgb` style renders as fragmented multi-color text. Modern
//! terminals (iTerm2, Alacritty, Kitty, WezTerm, GNOME Terminal,
//! Konsole, Windows Terminal, recent xterm) parse the sequence
//! correctly.
//!
//! We solve this by routing every truecolor callsite through [`rgb`]:
//! when truecolor is detected, the helper returns `Color::Rgb` as
//! before; when it is not, the helper quantises the input to the
//! nearest of the 216 colours in the ANSI 256-colour cube
//! (`Color::Indexed`). The cube colours are universally supported on
//! any terminal that handles `TERM=xterm-256color`, so the result
//! reads correctly on macOS Terminal.app and any other terminal that
//! lies about its capabilities.
//!
//! ## Detection ladder
//!
//! [`init`] is called once at startup, before the first frame renders.
//! The ladder is:
//!
//! 1. `--no-truecolor` CLI flag → quantise (operator override, off).
//! 2. `--force-truecolor` CLI flag → truecolor (operator override, on).
//! 3. `NO_COLOR` env var set and non-empty → quantise
//!    (no-color.org convention).
//! 4. `COLORTERM=truecolor` or `COLORTERM=24bit` → truecolor.
//! 5. `TERM_PROGRAM=Apple_Terminal` → quantise (macOS Terminal.app).
//! 6. `TERM` contains `kitty` / `alacritty` / `wezterm` / `vscode` /
//!    `ghostty` / `iterm` → truecolor.
//! 7. Otherwise → truecolor (default for terminals released since
//!    approximately 2017). Operators on older terminals can pass
//!    `--no-truecolor` or set `NO_COLOR` to force the fallback.
//!
//! ## SSH caveat
//!
//! SSH strips `COLORTERM` by default, so the env-var ladder is
//! conservative under SSH — `Apple_Terminal` detection still works
//! (the local terminal sets `TERM_PROGRAM` and SSH does forward `TERM`
//! variants on most setups), but a friend SSHing from a modern
//! terminal whose `COLORTERM` is stripped will still get truecolor by
//! default unless they explicitly set `--no-truecolor`. Reasonable
//! trade-off: most modern terminals handle truecolor, Terminal.app is
//! the specific outlier.
//!
//! ## Test discipline
//!
//! Tests use [`Color::Rgb`] inline in assertions to pin gradient
//! interpolation behaviour. The global flag defaults to `true` so
//! every test sees the same colour shape as production on a modern
//! terminal. Tests that need to exercise the quantiser call
//! [`quantize_to_cube`] directly rather than flipping the global flag,
//! so they remain order-independent under parallel execution.

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::style::Color;

/// Global truecolor flag. Defaults to `true` so tests, which do not
/// call [`init_from_env`], see the same colour shape as production on
/// a modern terminal.
static TRUECOLOR_ENABLED: AtomicBool = AtomicBool::new(true);

/// Read the detected mode. Cheap (lock-free atomic load).
#[must_use]
pub fn truecolor_enabled() -> bool {
    TRUECOLOR_ENABLED.load(Ordering::Relaxed)
}

/// Initialise the detector from the process environment + the CLI
/// override flags. Call once from `main`, before the first frame
/// renders.
///
/// Resolution order (defensive: off-wins if both are set):
/// 1. `force_disable` (`--no-truecolor`) → false.
/// 2. `force_enable` (`--force-truecolor`) → true.
/// 3. Otherwise run [`detect_truecolor_support`] (NO_COLOR + env-var
///    ladder).
pub fn init(force_disable: bool, force_enable: bool) {
    let enabled = if force_disable {
        false
    } else if force_enable {
        true
    } else {
        detect_truecolor_support()
    };
    TRUECOLOR_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Initialise the detector with only the `--no-truecolor` flag.
/// Thin wrapper around [`init`]; retained as a convenience for
/// downstream callers that only need the off-switch knob.
pub fn init_from_env(force_disable: bool) {
    init(force_disable, false);
}

/// The user-facing colour helper. Returns `Color::Rgb` on truecolor
/// terminals; on 256-colour terminals, returns the nearest of the 216
/// colours in the ANSI 6×6×6 cube.
#[must_use]
pub fn rgb(r: u8, g: u8, b: u8) -> Color {
    if truecolor_enabled() {
        Color::Rgb(r, g, b)
    } else {
        Color::Indexed(quantize_to_cube(r, g, b))
    }
}

/// Snap an 8-bit channel value to the 6 canonical 6×6×6 cube levels.
/// Levels are the standard xterm 256-colour cube anchors:
/// `[0, 95, 135, 175, 215, 255]`. The midpoint thresholds below are
/// the inflection points where the next anchor becomes closer.
const fn quantize_channel(c: u8) -> u8 {
    // Inflection points (midpoints between consecutive anchor levels):
    //   ( 0 +  95)/2 ≈ 48
    //   (95 + 135)/2 = 115
    //   (135 + 175)/2 = 155
    //   (175 + 215)/2 = 195
    //   (215 + 255)/2 = 235
    if c < 48 {
        0
    } else if c < 115 {
        1
    } else if c < 155 {
        2
    } else if c < 195 {
        3
    } else if c < 235 {
        4
    } else {
        5
    }
}

/// Convert an `(r, g, b)` triple to the 256-colour cube index. Cube
/// indices live in `16..=231` per the xterm 256-colour standard:
/// `16 + 36·R + 6·G + B` for `R, G, B ∈ {0..=5}`.
#[must_use]
pub const fn quantize_to_cube(r: u8, g: u8, b: u8) -> u8 {
    let qr = quantize_channel(r);
    let qg = quantize_channel(g);
    let qb = quantize_channel(b);
    16 + 36 * qr + 6 * qg + qb
}

/// Inspect the process environment and decide whether the terminal
/// supports 24-bit RGB. Thin wrapper around the pure
/// [`detect_truecolor_from`] for testability — reads env once and
/// hands the values to the pure decision function.
fn detect_truecolor_support() -> bool {
    detect_truecolor_from(
        env::var("COLORTERM").ok().as_deref(),
        env::var("TERM_PROGRAM").ok().as_deref(),
        env::var("TERM").ok().as_deref(),
        env::var("NO_COLOR").ok().as_deref(),
    )
}

/// Pure decision function for the detection ladder.
///
/// Takes the relevant env-var values explicitly so unit tests can
/// exercise every rung without mutating process env (parallel tests
/// race catastrophically on shared env). See module docs for the
/// ladder. Rung numbers below align with the ladder rungs that are
/// reachable once CLI flags have been applied upstream by [`init`]
/// (rungs 1-2 of the module-level ladder are handled there).
pub fn detect_truecolor_from(
    colorterm: Option<&str>,
    term_program: Option<&str>,
    term: Option<&str>,
    no_color: Option<&str>,
) -> bool {
    // Rung 3. no-color.org convention: any non-empty `NO_COLOR`
    // disables colour. An empty string is treated as "not set" so
    // a stray `NO_COLOR=` from a shell rc does not silently force
    // the fallback.
    if let Some(nc) = no_color {
        if !nc.is_empty() {
            return false;
        }
    }
    // Rung 4.
    if let Some(ct) = colorterm {
        let ct = ct.to_ascii_lowercase();
        if ct == "truecolor" || ct == "24bit" {
            return true;
        }
    }
    // Rung 5.
    if let Some(prog) = term_program {
        if prog == "Apple_Terminal" {
            return false;
        }
    }
    // Rung 6.
    if let Some(t) = term {
        let t = t.to_ascii_lowercase();
        for needle in [
            "kitty",
            "alacritty",
            "wezterm",
            "vscode",
            "ghostty",
            "iterm",
        ] {
            if t.contains(needle) {
                return true;
            }
        }
    }
    // Rung 7. Default for terminals released since approximately 2017
    // (iTerm2 >= 3.0, Alacritty, Kitty, WezTerm, GNOME Terminal >= 3.12,
    // Konsole >= 18.04, Windows Terminal, xterm >= 331) which parse
    // SGR 38;2;R;G;B per ITU T.416.
    true
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, PoisonError};

    use super::*;

    /// Serialises tests that mutate `TRUECOLOR_ENABLED`. Lib tests in
    /// the same module run in parallel by default; without a lock, the
    /// `rgb_returns_truecolor_in_default_test_mode` test (which reads
    /// the flag) and the override-flag tests (which write it) race.
    /// Poison is drained so the first-failure root cause survives.
    static GLOBAL_FLAG_LOCK: Mutex<()> = Mutex::new(());

    // ---- Channel quantiser ----------------------------------------

    #[test]
    fn quantize_channel_snaps_to_anchor_levels() {
        // Anchor values themselves round to their own band.
        assert_eq!(quantize_channel(0), 0);
        assert_eq!(quantize_channel(95), 1);
        assert_eq!(quantize_channel(135), 2);
        assert_eq!(quantize_channel(175), 3);
        assert_eq!(quantize_channel(215), 4);
        assert_eq!(quantize_channel(255), 5);
    }

    #[test]
    fn quantize_channel_picks_nearest_at_midpoints() {
        // Each midpoint is the first value that flips to the next
        // band. One below stays, the midpoint itself rolls up.
        assert_eq!(quantize_channel(47), 0, "47 stays in band 0");
        assert_eq!(quantize_channel(48), 1, "48 flips to band 1");
        assert_eq!(quantize_channel(114), 1);
        assert_eq!(quantize_channel(115), 2);
        assert_eq!(quantize_channel(154), 2);
        assert_eq!(quantize_channel(155), 3);
        assert_eq!(quantize_channel(194), 3);
        assert_eq!(quantize_channel(195), 4);
        assert_eq!(quantize_channel(234), 4);
        assert_eq!(quantize_channel(235), 5);
    }

    // ---- Cube quantiser -------------------------------------------

    #[test]
    fn quantize_to_cube_returns_valid_cube_indices() {
        // The 6×6×6 cube lives in 16..=231. Sweep the eight corners
        // and verify each lands at a documented xterm cube index.
        assert_eq!(quantize_to_cube(0, 0, 0), 16, "black corner");
        assert_eq!(quantize_to_cube(255, 0, 0), 16 + 36 * 5, "red corner");
        assert_eq!(quantize_to_cube(0, 255, 0), 16 + 6 * 5, "green corner");
        assert_eq!(quantize_to_cube(0, 0, 255), 16 + 5, "blue corner");
        assert_eq!(
            quantize_to_cube(255, 255, 255),
            16 + 36 * 5 + 6 * 5 + 5,
            "white corner"
        );
        // Every cube index must be in the canonical range.
        for r in 0..=5u8 {
            for g in 0..=5u8 {
                for b in 0..=5u8 {
                    let anchors = [0u8, 95, 135, 175, 215, 255];
                    let idx = quantize_to_cube(
                        anchors[r as usize],
                        anchors[g as usize],
                        anchors[b as usize],
                    );
                    assert!((16..=231).contains(&idx), "cube idx {idx} out of range");
                }
            }
        }
    }

    #[test]
    fn quantize_to_cube_maps_chain_chip_palette_to_distinct_indices() {
        // All six chain chip colours (status chip label bg/fg, status
        // chip value bg/fg, slot chip bg/fg) must not collapse to the
        // same cube index under quantisation — otherwise distinct
        // chips would render as visually identical rectangles on a
        // 256-colour terminal. Mirrors the RGB anchors in
        // `live::scenes::chain::render`.
        let label_bg = quantize_to_cube(46, 54, 68);
        let label_fg = quantize_to_cube(168, 180, 198);
        let value_bg = quantize_to_cube(76, 110, 148);
        let value_fg = quantize_to_cube(244, 248, 255);
        let slot_bg = quantize_to_cube(36, 50, 135);
        let slot_fg = quantize_to_cube(252, 252, 234);
        let palette = [label_bg, label_fg, value_bg, value_fg, slot_bg, slot_fg];
        let distinct: std::collections::HashSet<u8> = palette.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            palette.len(),
            "quantised chip palette collapses to {distinct:?}; \
             retune RGB anchors so each chip stays distinguishable"
        );
    }

    // ---- rgb() dispatch -------------------------------------------

    #[test]
    fn rgb_returns_truecolor_in_default_test_mode() {
        // Tests don't call init_from_env(); the global flag defaults
        // to `true` so production-shape RGB assertions still work.
        // Hold the serialisation lock so a concurrently-running
        // override-flag test cannot flip the flag mid-read.
        let _guard = GLOBAL_FLAG_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let c = rgb(76, 110, 148);
        assert!(matches!(c, Color::Rgb(76, 110, 148)));
    }

    // ---- detection ladder -----------------------------------------
    //
    // The pure-function variant `detect_truecolor_from` takes all
    // three env values explicitly so the ladder is testable without
    // mutating shared process env. Every rung gets a direct test.

    #[test]
    fn rung_1_colorterm_truecolor_returns_true() {
        assert!(detect_truecolor_from(Some("truecolor"), None, None, None));
        assert!(detect_truecolor_from(Some("24bit"), None, None, None));
        // Case-insensitive: real terminals advertise mixed-case.
        assert!(detect_truecolor_from(Some("TrueColor"), None, None, None));
        assert!(detect_truecolor_from(Some("24BIT"), None, None, None));
    }

    #[test]
    fn rung_1_takes_precedence_over_apple_terminal() {
        // If someone wrapped Terminal.app in a shim that DOES set
        // COLORTERM, trust the advertisement — there's no good
        // reason to refuse working colour for an opt-in operator.
        assert!(detect_truecolor_from(
            Some("truecolor"),
            Some("Apple_Terminal"),
            Some("xterm-256color"),
            None,
        ));
    }

    #[test]
    fn rung_2_apple_terminal_without_colorterm_returns_false() {
        // The headline scenario: stock macOS Terminal.app. No
        // COLORTERM advertisement, TERM_PROGRAM identifies the
        // terminal, TERM is the misleading `xterm-256color`. The
        // detector must return false so the 6×6×6 cube takes over.
        assert!(!detect_truecolor_from(
            None,
            Some("Apple_Terminal"),
            Some("xterm-256color"),
            None,
        ));
    }

    #[test]
    fn rung_3_known_truecolor_terminals_match() {
        for term in [
            "xterm-kitty",
            "alacritty",
            "wezterm",
            "xterm-ghostty",
            "iterm.app",
            "vscode",
        ] {
            assert!(
                detect_truecolor_from(None, None, Some(term), None),
                "TERM={term:?} must be detected as truecolor-capable"
            );
        }
    }

    #[test]
    fn rung_3_case_insensitive() {
        assert!(detect_truecolor_from(None, None, Some("XTERM-KITTY"), None));
        assert!(detect_truecolor_from(None, None, Some("Alacritty"), None));
    }

    #[test]
    fn rung_4_default_is_truecolor_when_nothing_matches() {
        // Modern terminals overwhelmingly support truecolor; default
        // to it so the operator does not have to opt in. Apple_Terminal
        // is the only widely-deployed terminal that lies about it,
        // and rung 2 catches that case.
        assert!(detect_truecolor_from(
            None,
            None,
            Some("xterm-256color"),
            None
        ));
        assert!(detect_truecolor_from(None, None, None, None));
        assert!(detect_truecolor_from(None, None, Some("dumb"), None));
    }

    #[test]
    fn rung_1_ignores_unrelated_colorterm_values() {
        // Some legacy terminals set COLORTERM to their own name as a
        // breadcrumb; we should not interpret that as a 24-bit
        // capability claim. Fall through to subsequent rungs.
        assert!(!detect_truecolor_from(
            Some("rxvt"),
            Some("Apple_Terminal"),
            None,
            None,
        ));
        assert!(detect_truecolor_from(
            Some("rxvt"),
            None,
            Some("xterm-kitty"),
            None,
        ));
    }

    #[test]
    fn rung_2_other_term_programs_do_not_force_false() {
        // iTerm2 sets TERM_PROGRAM=iTerm.app and DOES support
        // truecolor. Rung 2 must not match it. Falls through to
        // rung 4 (default true) when TERM doesn't match rung 3.
        assert!(detect_truecolor_from(None, Some("iTerm.app"), None, None));
    }

    // ---- NO_COLOR + force-truecolor (DET-01) ----------------------

    #[test]
    fn no_color_overrides_other_rungs() {
        // NO_COLOR set and non-empty disables colour regardless of
        // any other env var that suggests truecolor capability.
        assert!(!detect_truecolor_from(
            Some("truecolor"),
            None,
            Some("xterm-kitty"),
            Some("1"),
        ));
        // Any non-empty value qualifies per no-color.org.
        assert!(!detect_truecolor_from(
            Some("24bit"),
            None,
            Some("alacritty"),
            Some("anything"),
        ));
    }

    #[test]
    fn no_color_empty_string_falls_through() {
        // `NO_COLOR=` (empty) is the no-color.org convention for "not
        // set" — a stray export in a shell rc must not silently force
        // the fallback. The ladder runs normally.
        assert!(detect_truecolor_from(
            Some("truecolor"),
            None,
            None,
            Some(""),
        ));
        // With empty NO_COLOR, Apple_Terminal still wins.
        assert!(!detect_truecolor_from(
            None,
            Some("Apple_Terminal"),
            Some("xterm-256color"),
            Some(""),
        ));
    }

    // ---- init() override flag matrix ------------------------------
    //
    // These tests mutate the process-global TRUECOLOR_ENABLED so they
    // are run sequentially. Each test restores the default at exit.

    #[test]
    fn force_truecolor_via_init_overrides_apple_terminal() {
        // `--force-truecolor` short-circuits the env-var ladder, so
        // even if the environment looks like Apple_Terminal we still
        // enable truecolor. Verified at the init() entry point since
        // that's the operator-facing API.
        let _guard = GLOBAL_FLAG_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        init(false, true);
        assert!(truecolor_enabled());
        // Restore the default for any subsequent test.
        TRUECOLOR_ENABLED.store(true, Ordering::Relaxed);
    }

    #[test]
    fn force_disable_takes_precedence_over_force_enable() {
        // Defensive: if both flags are set (clap would normally reject
        // via conflicts_with, but library callers can still pass both)
        // the off-switch wins. Quieter failure mode than truecolor on
        // a terminal the operator just explicitly disabled.
        let _guard = GLOBAL_FLAG_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        init(true, true);
        assert!(!truecolor_enabled());
        TRUECOLOR_ENABLED.store(true, Ordering::Relaxed);
    }
}
