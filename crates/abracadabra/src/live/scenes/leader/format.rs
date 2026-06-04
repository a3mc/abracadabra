//! Per-slot detail formatting + width constants for the block-
//! production cards.
//!
//! All width constants and the per-field write helpers live here so
//! the rendering module ([`super::render`]) can compose lines without
//! re-deriving column geometry.

use std::fmt::Write as _;

use super::state::OurSlot;

/// Column header positioned over the per-slot row's data columns
/// (see [`slot_detail_compact`]). Each label's first character sits
/// directly above the first digit of its data value column (assuming
/// 3-digit ms / 3-char Nk values, the common steady-state shape).
///
/// **Drift at 4-digit+ values is an accepted design tradeoff.** Wider
/// values extend one column left past the header label; right-aligning
/// the header to value-end would require a parallel set of width
/// constants and run the same drift risk in the opposite direction.
/// Steady-state operator use sees 3-digit ms / 3-char Nk values; the
/// rare 4-digit case is still readable.
///
/// Row icon glyphs (see `super::render::slot_icon`):
///
/// - `[✓]` Produced — bank_frozen + finalized.
/// - `[~]` Banked — bank_frozen, no finalized yet (dim green: bank is
///   frozen, we've done our part).
/// - `[…]` Banking — block emitted, no bank_frozen yet (yellow:
///   still working).
/// - `[✗]` Skipped — we cast Voting skip / skip-fallback.
/// - `[A]` Abandoned — `Unable to produce window` ERROR, no skip vote
///   on the slot. When a slot has both a skip vote AND `Unable to
///   produce window` covering it, the icon is `[✗]` (skip-vote
///   precedence) but the row body shows the verbatim abandon reason.
/// - `[ ]` Pending.
///
/// Column `tx` is `banking_stage_scheduler_slot_counts.num_finished` —
/// transactions banking-stage finished executing. Named `tx` (not
/// `fin`) to avoid clashing with `Finalized` semantics used elsewhere
/// in the codebase.
pub(super) const COLUMN_HEADER: &str = "      slot    bank   sigs   bcast   sh    tx";

/// Width budget for the slot-number field inside a card. Aligned right
/// so multi-digit slot numbers don't shift the columns to their right.
pub(super) const SLOT_FIELD_WIDTH: usize = 7;
/// Width budget for the bank-time field (`NNNms`, right-aligned).
pub(super) const BANK_MS_FIELD_WIDTH: usize = 5;
/// Width budget for the signature-count field, post-compaction
/// (right-aligned). Values ≥1 000 compact to `Nk`.
pub(super) const SIGS_FIELD_WIDTH: usize = 4;
/// Width budget for the shred-count field, post-compaction
/// (right-aligned). Same compaction rule as sigs.
pub(super) const SHREDS_FIELD_WIDTH: usize = 4;

/// Visible cells consumed by the 3-character icon (e.g. "[✓]").
pub(super) const ICON_VISIBLE_COLS: usize = 3;
/// Row prefix width: leading space + 3-cell icon + space + slot field.
/// Mirrors the same constants used by `super::render::card_slot_line`
/// and [`COLUMN_HEADER`].
pub(super) const ROW_PREFIX_WIDTH: usize = 1 + ICON_VISIBLE_COLS + 1 + SLOT_FIELD_WIDTH;
/// Detail body width: `{bank}ms {sigs}  {bcast}ms {shreds}  {tx}`.
pub(super) const DETAIL_WIDTH: usize = BANK_MS_FIELD_WIDTH
    + 3 /* "ms " */
    + SIGS_FIELD_WIDTH
    + 2 /* "  " */
    + BANK_MS_FIELD_WIDTH
    + 3 /* "ms " */
    + SHREDS_FIELD_WIDTH
    + 2 /* "  " */
    + SIGS_FIELD_WIDTH /* `tx` reuses the sigs field width */;
/// Total per-card column width.
pub(super) const CARD_ROW_WIDTH: usize = ROW_PREFIX_WIDTH + DETAIL_WIDTH;

/// Card-form per-slot detail. Same fixed-width column layout for
/// every status — produced and abandoned slots both render
/// `bank | sigs | bcast | sh`. Missing values become `—` so the
/// columns stay aligned. The status icon already conveys produced /
/// banking / abandoned / skipped — no extra label text is needed.
///
/// All values come verbatim from `solana_metrics::metrics` datapoints:
///
/// - `bank` — `leader-slot-start-to-cleared-elapsed-ms.elapsed`.
/// - `sigs` — `bank frozen` line's `signature_count`.
/// - `bcast` — `broadcast-process-shreds-stats.slot_broadcast_time`
///   (ms). For slots the validator abandoned mid-broadcast this is
///   `—` (the `-interrupted-stats` variant emits `-1`).
/// - `sh` — `broadcast-process-shreds-stats.num_data_shreds` (same
///   field on the `-interrupted-stats` variant for partial slots).
///
/// All five sub-fields are written into a single pre-sized `String`
/// buffer via [`std::fmt::Write`]. The previous form built five
/// throwaway `String`s per call; on a busy 2-card layout that was
/// ~10 short allocations per card render per frame.
pub(super) fn slot_detail_compact(s: &OurSlot) -> String {
    let mut out = String::with_capacity(DETAIL_WIDTH);
    write_bank_field(&mut out, bank_ms(s));
    out.push_str("ms ");
    write_sigs_field(&mut out, s.sig_count);
    out.push_str("  ");
    write_bank_field(&mut out, broadcast_ms(s));
    out.push_str("ms ");
    write_shreds_field(&mut out, s.num_data_shreds);
    out.push_str("  ");
    write_sigs_field(&mut out, s.num_finished);
    out
}

/// Whole-millisecond broadcast time. Sourced from
/// `broadcast-process-shreds-stats.slot_broadcast_time` (µs).
pub(super) fn broadcast_ms(s: &OurSlot) -> Option<i64> {
    s.broadcast_us.and_then(|us| i64::try_from(us / 1000).ok())
}

/// Leader-slot elapsed time in whole milliseconds.
///
/// Sourced directly from the validator's `leader-slot-start-to-cleared-elapsed-ms`
/// metric datapoint. Not derivable from event timestamps for our own
/// leader slots because `First shred N` only fires when we *receive*
/// a first shred for slot N, never when we *produce* N ourselves.
pub(super) fn bank_ms(s: &OurSlot) -> Option<i64> {
    s.leader_elapsed_ms.and_then(|v| i64::try_from(v).ok())
}

/// Right-align the bank-time number into `out`, padded to
/// [`BANK_MS_FIELD_WIDTH`] columns. Renders `—` when no sample is
/// available so the column stays the same visual width.
fn write_bank_field(out: &mut String, ms: Option<i64>) {
    let w = BANK_MS_FIELD_WIDTH;
    // `write!` to a `String` is infallible; the `Result` is discarded
    // intentionally. Output width tracked by the `{:>w$}` formatter.
    match ms {
        Some(v) => {
            let _ = write!(out, "{v:>w$}");
        }
        None => {
            let _ = write!(out, "{:>w$}", "—");
        }
    }
}

/// Right-align the signature count into `out`, padded to
/// [`SIGS_FIELD_WIDTH`] columns. Values ≥ 1 000 compact to `Nk`.
fn write_sigs_field(out: &mut String, n: Option<u64>) {
    write_compacted_count(out, n, SIGS_FIELD_WIDTH);
}

/// Right-align the shred count into `out`, padded to
/// [`SHREDS_FIELD_WIDTH`] columns, with the same `Nk` compaction
/// rule as signatures.
fn write_shreds_field(out: &mut String, n: Option<u64>) {
    write_compacted_count(out, n, SHREDS_FIELD_WIDTH);
}

/// Shared compaction-and-pad routine for `Nk`-style fields. Used by
/// [`write_sigs_field`] and [`write_shreds_field`].
fn write_compacted_count(out: &mut String, n: Option<u64>, width: usize) {
    // Token is at most "18446744073709k" (15 chars) for `u64::MAX / 1000`.
    let mut token = String::with_capacity(16);
    match n {
        None => token.push('—'),
        Some(v) if v >= 1_000 => {
            let _ = write!(token, "{}k", v / 1_000);
        }
        Some(v) => {
            let _ = write!(token, "{v}");
        }
    }
    let _ = write!(out, "{token:>width$}");
}

/// Render a compact count for the headline (`sig max`, `sh max`).
/// `<1 000` literal; anything else compacted to `Nk` (integer thousand).
///
/// No `m` (million) bucket: shreds-per-window is bounded above by
/// `RECENT_WINDOWS_CAPACITY * (MAX_LEADER_WINDOW_SPAN + 1)` × shreds/slot
/// (observed ceiling ~30k/slot), well under 1 m; and a `sig max` over
/// 1 m on a single retained window indicates a Solana TPS regime where
/// `1234k` is just as informative as `1.2m` and keeps a single formatter.
pub(super) fn format_count_compact(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else {
        format!("{}k", n / 1_000)
    }
}
