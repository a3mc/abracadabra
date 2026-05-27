//! Reusable TUI widgets above what ratatui ships with.

pub mod hbar;
pub mod kpi;

/// Format `n` with comma group separators (`1,234,567`). Single
/// canonical implementation; all panels import this rather than
/// carrying their own copy.
#[must_use]
pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Strip ASCII control characters from a string for safe rendering as
/// `Span` content. Replaces every byte in `[0x00..=0x1F]` or `0x7F`
/// with U+FFFD, except `\t` which is preserved. Returns `Cow::Borrowed`
/// on the common path (input clean) to avoid allocation.
///
/// Rationale: ratatui forwards cell symbols to crossterm `Print(...)`
/// verbatim. A literal ESC (`0x1B`) byte in a log line is interpreted
/// by xterm-class terminals as the start of a CSI sequence — cursor
/// moves, persistent style changes, screen wipes. Log-derived strings
/// (`sample_body`, alert descriptions, module names) flow into Spans;
/// they must be filtered first.
#[must_use]
pub fn sanitize_for_tui(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7F) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let c = ch as u32;
        if (c < 0x20 && ch != '\t') || c == 0x7F {
            out.push('\u{FFFD}');
        } else {
            out.push(ch);
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Resample `data` so it has exactly `width` points. Used to stretch
/// sparklines across the full width of their panel — ratatui's `Sparkline`
/// draws one column per input point, so a 100-bucket series in a 200-wide
/// panel only fills half the area without this.
///
/// Uses nearest-neighbour sampling: simple, no smoothing, preserves peaks.
#[must_use]
pub fn fit_to_width(data: &[u64], width: usize) -> Vec<u64> {
    if data.is_empty() || width == 0 {
        return Vec::new();
    }
    (0..width).map(|i| data[i * data.len() / width]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_empty_data() {
        assert!(fit_to_width(&[], 10).is_empty());
    }

    #[test]
    fn fit_zero_width() {
        assert!(fit_to_width(&[1, 2, 3], 0).is_empty());
    }

    #[test]
    fn fit_upsample_repeats_buckets() {
        // 3 input points -> 9 output points, each input repeated 3 times.
        let out = fit_to_width(&[10, 20, 30], 9);
        assert_eq!(out, vec![10, 10, 10, 20, 20, 20, 30, 30, 30]);
    }

    #[test]
    fn fit_downsample_picks_evenly() {
        let out = fit_to_width(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], 5);
        assert_eq!(out, vec![0, 2, 4, 6, 8]);
    }

    #[test]
    fn fit_identity_when_equal() {
        let out = fit_to_width(&[1, 2, 3, 4], 4);
        assert_eq!(out, vec![1, 2, 3, 4]);
    }

    #[test]
    fn commas_under_1000_unchanged() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(42), "42");
        assert_eq!(commas(999), "999");
    }

    #[test]
    fn commas_inserts_separators() {
        assert_eq!(commas(1_000), "1,000");
        assert_eq!(commas(1_234_567), "1,234,567");
        assert_eq!(commas(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn sanitize_passes_clean_ascii_unchanged() {
        let s = "validator slot 12345 ok";
        let out = sanitize_for_tui(s);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out, s);
    }

    #[test]
    fn sanitize_strips_ansi_escape() {
        // ESC [31m red foreground attempt.
        let s = "alert \x1b[31mhostile\x1b[0m";
        let out = sanitize_for_tui(s);
        assert!(!out.contains('\x1b'));
        assert!(out.contains('\u{FFFD}'));
    }

    #[test]
    fn sanitize_preserves_tab() {
        let s = "col1\tcol2";
        let out = sanitize_for_tui(s);
        assert_eq!(out, "col1\tcol2");
    }

    #[test]
    fn sanitize_replaces_del_byte() {
        let s = "x\x7Fy";
        let out = sanitize_for_tui(s);
        assert_eq!(out, "x\u{FFFD}y");
    }

    #[test]
    fn sanitize_preserves_multibyte_utf8() {
        // U+2192 RIGHTWARDS ARROW + U+2713 CHECK MARK.
        let s = "fast → ✓";
        let out = sanitize_for_tui(s);
        assert_eq!(out, "fast → ✓");
    }
}
