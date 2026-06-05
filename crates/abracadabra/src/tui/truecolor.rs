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
//! [`init_from_env`] is called once at startup, before the first frame
//! renders. The ladder is:
//!
//! 1. `--no-truecolor` CLI flag → quantise (operator override).
//! 2. `COLORTERM=truecolor` or `COLORTERM=24bit` → truecolor.
//! 3. `TERM_PROGRAM=Apple_Terminal` → quantise (macOS Terminal.app).
//! 4. `TERM` contains `kitty` / `alacritty` / `wezterm` / `vscode` /
//!    `ghostty` / `iterm` → truecolor.
//! 5. Otherwise → truecolor (modern default). Operators on legacy
//!    terminals can pass `--no-truecolor` to force fallback.
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
/// `--no-truecolor` flag. Call once from `main`, before the first
/// frame renders.
pub fn init_from_env(force_disable: bool) {
    let enabled = if force_disable {
        false
    } else {
        detect_truecolor_support()
    };
    TRUECOLOR_ENABLED.store(enabled, Ordering::Relaxed);
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
    )
}

/// Pure decision function for the detection ladder.
///
/// Takes the relevant env-var values explicitly so unit tests can
/// exercise every rung without mutating process env (parallel tests
/// race catastrophically on shared env). See module docs for the
/// ladder. Rung numbers refer to the ladder in the module docstring.
pub fn detect_truecolor_from(
    colorterm: Option<&str>,
    term_program: Option<&str>,
    term: Option<&str>,
) -> bool {
    // Rung 1.
    if let Some(ct) = colorterm {
        let ct = ct.to_ascii_lowercase();
        if ct == "truecolor" || ct == "24bit" {
            return true;
        }
    }
    // Rung 2.
    if let Some(prog) = term_program {
        if prog == "Apple_Terminal" {
            return false;
        }
    }
    // Rung 3.
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
    // Rung 4.
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // The four chain chip colours (label bg, label fg, value bg,
        // value fg) must not collapse to the same cube index under
        // quantisation — otherwise the chips would render as a flat
        // single-colour rectangle on a 256-colour terminal.
        let label_bg = quantize_to_cube(46, 54, 68);
        let label_fg = quantize_to_cube(168, 180, 198);
        let value_bg = quantize_to_cube(76, 110, 148);
        let value_fg = quantize_to_cube(244, 248, 255);
        let palette = [label_bg, label_fg, value_bg, value_fg];
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
        assert!(detect_truecolor_from(Some("truecolor"), None, None));
        assert!(detect_truecolor_from(Some("24bit"), None, None));
        // Case-insensitive: real terminals advertise mixed-case.
        assert!(detect_truecolor_from(Some("TrueColor"), None, None));
        assert!(detect_truecolor_from(Some("24BIT"), None, None));
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
                detect_truecolor_from(None, None, Some(term)),
                "TERM={term:?} must be detected as truecolor-capable"
            );
        }
    }

    #[test]
    fn rung_3_case_insensitive() {
        assert!(detect_truecolor_from(None, None, Some("XTERM-KITTY")));
        assert!(detect_truecolor_from(None, None, Some("Alacritty")));
    }

    #[test]
    fn rung_4_default_is_truecolor_when_nothing_matches() {
        // Modern terminals overwhelmingly support truecolor; default
        // to it so the operator does not have to opt in. Apple_Terminal
        // is the only widely-deployed terminal that lies about it,
        // and rung 2 catches that case.
        assert!(detect_truecolor_from(None, None, Some("xterm-256color")));
        assert!(detect_truecolor_from(None, None, None));
        assert!(detect_truecolor_from(None, None, Some("dumb")));
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
        ));
        assert!(detect_truecolor_from(
            Some("rxvt"),
            None,
            Some("xterm-kitty")
        ));
    }

    #[test]
    fn rung_2_other_term_programs_do_not_force_false() {
        // iTerm2 sets TERM_PROGRAM=iTerm.app and DOES support
        // truecolor. Rung 2 must not match it. Falls through to
        // rung 4 (default true) when TERM doesn't match rung 3.
        assert!(detect_truecolor_from(None, Some("iTerm.app"), None));
    }
}
