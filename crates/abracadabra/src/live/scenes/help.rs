//! Live-tab help panel.
//!
//! Toggled into the `tx pressure` slot by pressing `[h]` on the
//! Live tab. Static engineering glossary: one-line descriptions of
//! every label / glyph / event source the other live panes display,
//! sourced directly from the metric event field names so the
//! operator reading this never has to wonder whether the
//! description is accurate.
//!
//! No marketing language, no inferred meanings — each entry names
//! the underlying field or event variant. When a column reads
//! `sigs` we say it is `BankFrozen.signature_count`; when it reads
//! `tx` we say it is `BankingStageCounts.num_finished`. That is the
//! source of truth.
//!
//! The panel scrolls vertically. The scene engine keeps a
//! `help_scroll: usize` offset (top visible line). Scroll wraps
//! `0..=lines.len().saturating_sub(area.height)`.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::theme;

/// Container for the help glossary lines + render entry point.
/// Lines are pre-built once at construction; render slices the
/// visible window using the scroll offset.
#[derive(Debug)]
pub struct HelpPanel {
    lines: Vec<Line<'static>>,
}

impl Default for HelpPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl HelpPanel {
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: build_lines(),
        }
    }

    /// Total line count of the glossary — used by the scene engine
    /// to clamp the scroll offset.
    #[must_use]
    pub const fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Maximum scroll offset given a render area of `area_height`
    /// rows (inside the border).
    #[must_use]
    pub fn max_scroll(&self, area_height: u16) -> usize {
        // The inner viewport excludes the 2-row border and the
        // 1-row top hint, but those are render-side details — for
        // clamping we approximate by subtracting the area height
        // from the line count.
        let visible = usize::from(area_height).saturating_sub(2);
        self.lines.len().saturating_sub(visible)
    }

    /// Render the glossary into `area`, starting from line
    /// `scroll`. Lines below the visible window are hidden; lines
    /// above are skipped. The top row carries a hint line so the
    /// operator never has to wonder how to dismiss the panel.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect, scroll: usize) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" help — press [h] to close ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let take = usize::from(inner.height);
        let visible: Vec<Line<'static>> =
            self.lines.iter().skip(scroll).take(take).cloned().collect();
        frame.render_widget(Paragraph::new(visible), inner);
    }
}

#[allow(clippy::vec_init_then_push)]
fn build_lines() -> Vec<Line<'static>> {
    // Building the glossary as a sequence of `push` calls keeps each
    // section visually contiguous and easy to extend. A single `vec![]`
    // would be one massive literal — harder to scan, harder to edit
    // when a single entry needs a tweak.
    let mut out: Vec<Line<'static>> = Vec::new();

    out.push(blank());
    out.push(hint(
        "  Engineering glossary. Each entry names the underlying log",
    ));
    out.push(hint(
        "  field so what you see on screen maps to a real source.",
    ));
    out.push(blank());

    out.push(section("SHRED STREAMS"));
    out.push(kv(
        "turbine",
        "ShredFetch.shred_count — shreds received via turbine",
    ));
    out.push(kv(
        "repair",
        "ShredFetchRepair.shred_count — shreds we requested",
    ));
    out.push(kv(
        "drop",
        "ShredSigverify.num_discards — sigverify rejects",
    ));
    out.push(kv("err", "RecvWindowInsert.num_errors — insert failures"));
    out.push(blank());

    out.push(section("SLOT OUTCOMES"));
    out.push(kv(
        "fast",
        "Finalized.fast == true — finalised in one round",
    ));
    out.push(kv(
        "slow",
        "Finalized.fast == false — finalised in two rounds",
    ));
    out.push(kv("skip", "Voting skip / skip-fallback — we voted skip"));
    out.push(kv(
        "fec",
        "ShredInsertIsFull.num_recovered — shreds reconstructed",
    ));
    out.push(hint("       via Forward Error Correction (coding shreds)"));
    out.push(kv(
        "CSKIP",
        "we voted skip on a slot the network kept canonical",
    ));
    out.push(blank());

    out.push(section("CHAIN — STATUS CHIPS"));
    out.push(kv(
        "cadence",
        "bank_frozen inter-arrival at this node (cluster cadence proxy)",
    ));
    out.push(kv("assembly", "first_shred → block_emitted"));
    out.push(kv("consensus", "block_emitted → finalized"));
    out.push(kv("lifecycle", "first_shred → finalized (end-to-end)"));
    out.push(hint("  values are p50/p95 in milliseconds"));
    out.push(blank());

    out.push(section("CHAIN — BUCKET GLYPHS"));
    out.push(kv("■  green BOLD", "canonical + fast-finalised (success)"));
    out.push(kv(
        "■  green DIM",
        "canonical via walk-back, no Finalized event",
    ));
    out.push(kv("◐  yellow", "canonical + slow-finalised (2-round)"));
    out.push(kv("○  yellow", "canonical + we observed VotingNotarize"));
    out.push(kv(
        "▴  red BOLD",
        "canonical-skip — we voted skip, chain kept it",
    ));
    out.push(kv("▾  red", "vote-skip — no canonical evidence yet"));
    out.push(kv(
        "⊕  yellow BOLD",
        "fork — ≥2 distinct hashes for the slot",
    ));
    out.push(kv("·  dim gray", "pending / not yet classified"));
    out.push(blank());

    out.push(section("CHAIN — OUR LEADER SLOT (★)"));
    out.push(kv(
        "★  magenta BOLD",
        "our slot, canonical + fast-finalised",
    ));
    out.push(kv("★  yellow BOLD", "our slot, canonical + slow-finalised"));
    out.push(kv("★  green DIM", "our slot, canonical via walk-back only"));
    out.push(kv("★  red BOLD", "our slot, we voted skip (LSKIP)"));
    out.push(kv("★  cyan BOLD", "our slot, outcome pending"));
    out.push(hint("  fork ⊕ still wins precedence on our own slots"));
    out.push(blank());

    out.push(section("CHAIN — TX STREAM (left of bucket)"));
    out.push(kv("slot", "tip-side slot number with nonzero sigs"));
    out.push(kv("bar", "magnitude relative to visible-window max"));
    out.push(kv(
        "count",
        "BankFrozen.signature_count — sigs in the block",
    ));
    out.push(hint(
        "  hidden filters: signature_count == 0 (most slots), and",
    ));
    out.push(hint(
        "  slots we voted skip on (orphaned local-bank events)",
    ));
    out.push(blank());

    out.push(section("BLOCK PRODUCTION — HEADLINE"));
    out.push(kv(
        "bank avg",
        "mean leader-slot-start-to-cleared-elapsed-ms",
    ));
    out.push(kv(
        "sig max",
        "max BankFrozen.signature_count across retained slots",
    ));
    out.push(kv(
        "sh max",
        "max broadcast-process-shreds-stats.num_data_shreds",
    ));
    out.push(kv(
        "since last block",
        "wall-clock since latest bank_frozen",
    ));
    out.push(blank());

    out.push(section("BLOCK PRODUCTION — CARD COLUMNS"));
    out.push(kv("slot", "leader slot number"));
    out.push(kv(
        "bank",
        "leader-slot-start-to-cleared-elapsed-ms.elapsed",
    ));
    out.push(kv(
        "sigs",
        "BankFrozen.signature_count — TXS IN THE FINAL BLOCK",
    ));
    out.push(kv(
        "bcast",
        "broadcast-process-shreds-stats.slot_broadcast_time (µs→ms)",
    ));
    out.push(kv("sh", "broadcast-process-shreds-stats.num_data_shreds"));
    out.push(kv("tx", "banking_stage_scheduler_slot_counts.num_finished"));
    out.push(hint(
        "       — TXS BANKING FINISHED EXECUTING (may differ from",
    ));
    out.push(hint(
        "       sigs: banking can finish more txs than fit in the",
    ));
    out.push(hint("       block; failed txs still count as finished)"));
    out.push(blank());

    out.push(section("BLOCK PRODUCTION — ROW ICONS"));
    out.push(kv(
        "[✓]  green BOLD",
        "Produced — bank_frozen + Finalized fast",
    ));
    out.push(kv("[✓]  green", "Produced — bank_frozen + Finalized slow"));
    out.push(kv(
        "[~]  green DIM",
        "Banked — bank_frozen, no Finalized yet",
    ));
    out.push(kv("[…]  yellow", "Banking — block emitted, no bank_frozen"));
    out.push(kv(
        "[✗]  red BOLD",
        "Skipped — we voted skip / skip-fallback",
    ));
    out.push(kv(
        "[A]  red BOLD",
        "Abandoned — `Unable to produce window` ERROR fired",
    ));
    out.push(kv("[ ]  dim gray", "Pending"));
    out.push(blank());

    out.push(section("BLOCK PRODUCTION — ALERT FOOTER"));
    out.push(kv(
        "skipped — PoH moved on",
        "PohRecorder(WindowMovedOn(N))",
    ));
    out.push(hint("       — leader window dropped: PoH advanced past us"));
    out.push(kv(
        "N tx dropped (scheduler full)",
        "banking_stage_...num_dropped_on_capacity",
    ));
    out.push(kv(
        "N bad handover",
        "slot-metrics.leader_handover_sad (per slot)",
    ));
    out.push(kv(
        "N replay lag",
        "slot-metrics.replay_is_behind_count (per slot)",
    ));
    out.push(blank());

    out.push(section("TX PRESSURE"));
    out.push(hint(
        "  one sample per BankFrozen event, signature_count on Y",
    ));
    out.push(hint(
        "  axis. Chart scrolls left as time advances. Press [h]",
    ));
    out.push(hint("  again to bring this widget back."));
    out.push(blank());

    out.push(section("KEYS (LIVE TAB)"));
    out.push(kv("SPACE", "start / stop the tail"));
    out.push(kv("p", "pause / resume the animation"));
    out.push(kv("h", "toggle this help panel"));
    out.push(kv("j / k / ↓ / ↑", "scroll help by one line"));
    out.push(kv("PgDn / PgUp", "scroll help by twenty lines"));
    out.push(blank());

    out
}

fn blank() -> Line<'static> {
    Line::from("")
}

fn hint(text: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        text,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    ))
}

fn section(title: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {title}"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    ))
}

fn kv(label: &'static str, body: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {label:<16}"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(body, Style::default().fg(Color::Gray)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_panel_lines_include_each_section_header() {
        let panel = HelpPanel::new();
        let text: String = panel
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        for section in [
            "SHRED STREAMS",
            "SLOT OUTCOMES",
            "CHAIN — STATUS CHIPS",
            "CHAIN — BUCKET GLYPHS",
            "CHAIN — OUR LEADER SLOT",
            "CHAIN — TX STREAM",
            "BLOCK PRODUCTION — HEADLINE",
            "BLOCK PRODUCTION — CARD COLUMNS",
            "BLOCK PRODUCTION — ROW ICONS",
            "BLOCK PRODUCTION — ALERT FOOTER",
            "TX PRESSURE",
            "KEYS (LIVE TAB)",
        ] {
            assert!(
                text.contains(section),
                "help missing section {section:?}; full text: {text:?}"
            );
        }
    }

    #[test]
    fn help_panel_explains_the_specific_user_confusions() {
        // LIVE-61: anchor regression — the two terms the operator
        // explicitly called out as confusing (`fec` and `sigs` vs
        // `tx`) must each appear in the glossary with their actual
        // metric-event field names.
        let panel = HelpPanel::new();
        let text: String = panel
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            text.contains("ShredInsertIsFull.num_recovered"),
            "fec entry must name the source field"
        );
        assert!(
            text.contains("Forward Error Correction"),
            "fec entry must expand the acronym"
        );
        assert!(
            text.contains("BankFrozen.signature_count"),
            "sigs entry must name the source field"
        );
        assert!(
            text.contains("num_finished"),
            "tx entry must name the source field"
        );
        // The sigs-vs-tx distinction is exactly the operator's
        // confusion — the glossary must spell out both sides.
        assert!(
            text.contains("TXS IN THE FINAL BLOCK"),
            "sigs side of the distinction missing"
        );
        assert!(
            text.contains("TXS BANKING FINISHED EXECUTING"),
            "tx side of the distinction missing"
        );
    }

    #[test]
    fn max_scroll_is_zero_when_area_taller_than_content() {
        let panel = HelpPanel::new();
        // Way more rows than content — should clamp to 0.
        assert_eq!(panel.max_scroll(u16::MAX), 0);
    }

    #[test]
    fn max_scroll_grows_when_area_shorter_than_content() {
        let panel = HelpPanel::new();
        let n = panel.line_count();
        // 10-row visible window leaves (n - 8) scrollable lines
        // (the 2-row border deduction matches `max_scroll`).
        assert_eq!(panel.max_scroll(10), n.saturating_sub(8));
    }
}
