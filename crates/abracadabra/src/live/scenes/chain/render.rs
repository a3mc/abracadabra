//! Ratatui rendering for the chain pane.
//!
//! Layout (top → bottom inside the border):
//!
//! 1. **Header line 1** — `<spinner> <tip> tip ▶` (plus `⚠ N anom`
//!    when parser anomalies have fired). The `▶` cannon glyph anchors
//!    the operator's eye to the particle origin; counters previously
//!    on this line (`CSKIP`, `indet`, `forks`) were dropped because
//!    the matrix itself surfaces those classes visually.
//! 2. **Header line 2** — timing strip `cadence p50/p95ms · …`.
//! 3. **Arena** — empty area above the bucket where in-flight
//!    cannon particles travel.
//! 4. **Bucket** — fixed 100-cell grid laid out in a centred 25×4
//!    block (or the largest grid that fits when the area is
//!    narrower). Cells are placed at **static positions** and never
//!    move until the page wipes; when the 100th slot lands, a left-
//!    to-right **magic-wipe** sweep clears the bucket over ~500 ms
//!    and the next page begins from cell 0.
//!
//! **World-space → screen-space.** Particles carry normalised
//! `(x, y) ∈ [0, 1]²` coordinates. The visualisation area's `Rect`
//! scales them at render time so the system is layout-agnostic —
//! resizing the terminal does not break the trajectories.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use std::time::Instant;

use crate::live::animation::spinner_frame;
use crate::tui::theme;

use super::format::StagePercentiles;
use super::glyph::{classify_slot, CellGlyph};
use super::state::ChainPane;

/// Logical pane height advertised to the scene engine. Real layout
/// uses `Constraint::Min(0)` (see `live/scenes/mod.rs`); this constant
/// is only consulted by the vertical-stack fallback.
pub const PANE_HEIGHT: u16 = 12;

/// Cannon glyph painted just below the slot chip — points down,
/// particles fall vertically into the bucket.
const CANNON_GLYPH: char = '▼';

/// Default bucket width in cells. 25 × 5 = 125 = `PAGE_CAPACITY`.
const DEFAULT_BUCKET_COLS: usize = 25;
/// Default bucket height in rows.
const DEFAULT_BUCKET_ROWS: usize = 5;
/// Per-cell horizontal stride: `glyph + space`. Packed solid the
/// grid reads as a bar; spaced cells read as discrete slots.
const BUCKET_STRIDE: u16 = 2;

/// Magic-wipe per-column flash window as a fraction of the wipe
/// duration. A cell flashes between `column_progress` and
/// `column_progress + WIPE_FLASH_WINDOW`, then renders blank.
const WIPE_FLASH_WINDOW: f32 = 0.12;

/// Chip background colours. Pre-built once and cloned into Style
/// each call so the render loop allocates only the `format!`
/// strings — `Style` is `Copy` so the colour values themselves
/// don't allocate.
const CHIP_LABEL_BG: Color = Color::Rgb(46, 54, 68);
const CHIP_LABEL_FG: Color = Color::Rgb(168, 180, 198);
const CHIP_VALUE_BG: Color = Color::Rgb(76, 110, 148);
const CHIP_VALUE_FG: Color = Color::Rgb(244, 248, 255);
const SLOT_CHIP_BG: Color = Color::Rgb(36, 50, 76);
const SLOT_CHIP_FG: Color = Color::Rgb(252, 252, 255);

/// Render the entire pane (border + composition) inside `area`.
pub(super) fn render(pane: &ChainPane, frame: &mut Frame<'_>, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" chain ")
        .title_style(theme::title_style())
        .border_style(theme::title_style());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 24 || inner.height < 4 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status chips (timing)
            Constraint::Length(1), // blank
            Constraint::Length(1), // spinner + slot chip
            Constraint::Length(1), // cannon ▼
            Constraint::Min(1),    // visualisation (arena + bucket)
        ])
        .split(inner);

    render_status(pane, frame, chunks[0]);
    render_slot_chip(pane, frame, chunks[2]);
    render_cannon_row(frame, chunks[3]);
    render_visualisation(pane, frame, chunks[4]);
}

/// Compose the top status `Line` — four chip pairs, one per
/// percentile stage. Each chip is a `[label][value]` pair with
/// distinct background colours so the eye reads them as discrete
/// pills. Stages with no samples render their value as `—`.
///
/// The trailing `ms` unit is dropped from each value to keep the
/// line under ~78 cols (half-pane width) — operators familiar
/// with the timing-percentile vocabulary read the values as
/// milliseconds. The exact units are documented at
/// [`crate::live::scenes::chain::state::ChainPane::timing_table`].
pub(super) fn status_line(pane: &ChainPane) -> Line<'static> {
    let table = pane.timing_table();
    let stages: [(&str, StagePercentiles); 4] = [
        ("cadence", table.cluster),
        ("assembly", table.assembly),
        ("consensus", table.consensus),
        ("lifecycle", table.lifecycle),
    ];
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(stages.len() * 3);
    let mut first = true;
    for (label, pct) in stages {
        if !first {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!(" {label} "),
            Style::default().fg(CHIP_LABEL_FG).bg(CHIP_LABEL_BG),
        ));
        let value_text = match pct {
            Some((p50, p95)) => format!(" {p50}/{p95} "),
            None => "  —  ".to_owned(),
        };
        spans.push(Span::styled(
            value_text,
            Style::default()
                .fg(CHIP_VALUE_FG)
                .bg(CHIP_VALUE_BG)
                .add_modifier(Modifier::BOLD),
        ));
        first = false;
    }
    Line::from(spans)
}

fn render_status(pane: &ChainPane, frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(status_line(pane)).alignment(Alignment::Center),
        area,
    );
}

/// Compose the slot chip `Line` — spinner + tip slot number on a
/// dark slate background, plus an optional `⚠ N anom` segment when
/// parser walk-back anomalies have fired. The `tip` label was
/// dropped (the slot number is self-explanatory) and the chip
/// background visually binds spinner + slot into one element above
/// the cannon `▼` on the next row.
pub(super) fn slot_chip_line(pane: &ChainPane) -> Line<'static> {
    let spinner = spinner_frame(pane.event_count, pane.last_event_at);
    let tip = pane
        .tip_slot()
        .map_or_else(|| "—".to_owned(), |s| s.to_string());
    let mut spans = vec![Span::styled(
        format!("  {spinner} {tip}  "),
        Style::default()
            .fg(SLOT_CHIP_FG)
            .bg(SLOT_CHIP_BG)
            .add_modifier(Modifier::BOLD),
    )];
    if pane.walk_back_anomalies > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!(" ⚠ {} anom ", pane.walk_back_anomalies),
            theme::bad_style().add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn render_slot_chip(pane: &ChainPane, frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(slot_chip_line(pane)).alignment(Alignment::Center),
        area,
    );
}

/// Paint the cannon `▼` centred on its row. Sits one row below the
/// slot chip with no blank between — the vertical adjacency binds
/// the two into a single "cannon-loaded-with-slot" visual unit.
fn render_cannon_row(frame: &mut Frame<'_>, area: Rect) {
    let line = Line::from(Span::styled(
        CANNON_GLYPH.to_string(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);
}

/// Lay out the visualisation area: in-flight particles drawn
/// across the full `viz`, bucket centred horizontally and
/// bottom-aligned vertically inside `viz`. The cannon glyph
/// itself is one row above `viz` (its own layout chunk above
/// `viz` in [`render`]), so particles spawn at viz top and visually
/// drop from beneath the cannon row into the bucket.
fn render_visualisation(pane: &ChainPane, frame: &mut Frame<'_>, viz: Rect) {
    if viz.width == 0 || viz.height == 0 {
        return;
    }
    render_particles(pane, frame, viz);
    let bucket_area = compute_bucket_area(viz);
    render_bucket(pane, frame, bucket_area, Instant::now());
    let stream_area = compute_tx_stream_area(viz, bucket_area);
    render_tx_stream(pane, frame, stream_area);
}

/// Width budget for the left-side tx stream — `slot bar count`
/// fits in this many cells. Below the configured minimum the
/// stream is skipped entirely (returns zero-area rect).
const TX_STREAM_MAX_WIDTH: u16 = 14;
const TX_STREAM_MIN_WIDTH: u16 = 10;

/// 9-level horizontal-bar glyphs for the tx-stream column. Cell 0
/// is blank (renders only when the slot's count is missing or
/// genuinely zero); cells 1..8 are filled at increasing block
/// heights so the eye reads them as a tiny histogram.
const STREAM_BARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Position a bucket sub-rect inside `viz`. Aims for the default
/// 25×8 grid; falls back to whatever fits when the viz width is
/// narrower. Centred horizontally, **bottom-aligned vertically** —
/// the bucket sits flush at the bottom of `viz` so the rows above
/// become the arena where particles fall. Matches the operator's
/// mockup where the bottom has no empty margin.
fn compute_bucket_area(viz: Rect) -> Rect {
    let want_w = u16::try_from(DEFAULT_BUCKET_COLS).unwrap_or(u16::MAX) * BUCKET_STRIDE;
    let want_h = u16::try_from(DEFAULT_BUCKET_ROWS).unwrap_or(u16::MAX);
    let w = want_w.min(viz.width);
    let h = want_h.min(viz.height);
    let x_offset = viz.width.saturating_sub(w) / 2;
    let y_offset = viz.height.saturating_sub(h);
    Rect::new(viz.x + x_offset, viz.y + y_offset, w, h)
}

/// Position the left-side tx-stream rect: the strip between the
/// left edge of `viz` and the left edge of `bucket`, with one
/// column of breathing space before the bucket. Returns a zero-area
/// rect when the available width is below [`TX_STREAM_MIN_WIDTH`]
/// (chain pane too narrow — drop the stream rather than crowd the
/// bucket).
fn compute_tx_stream_area(viz: Rect, bucket: Rect) -> Rect {
    if bucket.x <= viz.x {
        return Rect::new(viz.x, viz.y, 0, 0);
    }
    let raw_w = bucket.x.saturating_sub(viz.x).saturating_sub(1);
    let w = raw_w.min(TX_STREAM_MAX_WIDTH);
    if w < TX_STREAM_MIN_WIDTH {
        return Rect::new(viz.x, viz.y, 0, 0);
    }
    Rect::new(viz.x, viz.y, w, viz.height)
}

/// Paint every in-flight particle as the classifier's chosen glyph
/// at its current world position. The classifier looks at the
/// particle's slot in the pane's current state on EVERY frame, so a
/// particle launched as `·` (pending) can mutate mid-flight to `■`
/// (canonical fast-finalised) the moment its `Finalized` event
/// lands — the user sees the trajectory's colour and glyph change
/// in real time.
fn render_particles(pane: &ChainPane, frame: &mut Frame<'_>, viz: Rect) {
    let buf = frame.buffer_mut();
    for p in &pane.cannon.particles {
        let Some((px, py)) = world_to_screen(viz, p.x, p.y) else {
            continue;
        };
        let CellGlyph { ch, style } = classify_slot(pane, p.slot);
        buf[(px, py)].set_char(ch).set_style(style);
    }
}

/// Paint the bucket inside `bucket_area`.
///
/// **Static positions.** A slot landing at bucket index `i` always
/// renders at `(col = i % cols, row = i / cols)` until the page
/// wipes. The grid never shifts — the eye locks onto specific cells
/// and absorbs per-slot signal one cell at a time.
///
/// **Bottom-up fill within the grid.** Slot 0 of a page lands at
/// the bottom-left, slot `cols-1` at the bottom-right, slot `cols`
/// at the second-from-bottom row's left column, and so on. The
/// 100th slot lands at the top-right. Mirrors the "cup filling"
/// metaphor.
///
/// **Magic wipe.** When the bucket reaches [`PAGE_CAPACITY`] the
/// cannon system starts a wipe. Per-cell wipe state machine:
///
/// - column progress `cp = col / (cols - 1)` ∈ `[0, 1]`
/// - elapsed wipe progress `p` ∈ `[0, 1]` from the cannon system
/// - if `p < cp`: render normally (sweep front hasn't reached this column)
/// - if `cp ≤ p < cp + WIPE_FLASH_WINDOW`: render WHITE BOLD inverse
///   — the bright sweep front passing this column
/// - if `p ≥ cp + WIPE_FLASH_WINDOW`: render blank — column has
///   been swept
fn render_bucket(pane: &ChainPane, frame: &mut Frame<'_>, bucket_area: Rect, now: Instant) {
    if bucket_area.width == 0 || bucket_area.height == 0 {
        return;
    }
    let cols = bucket_area.width / BUCKET_STRIDE;
    if cols == 0 {
        return;
    }
    let rows = bucket_area.height;
    let capacity = usize::from(cols) * usize::from(rows);
    if capacity == 0 {
        return;
    }
    let wipe_progress = pane.cannon.wipe_progress(now);
    let buf = frame.buffer_mut();
    let cols_minus_one = u16::max(cols, 1) - 1;

    for (i, cell) in pane.cannon.bucket.iter().enumerate() {
        if i >= capacity {
            break;
        }
        #[allow(clippy::cast_possible_truncation)]
        let col = (i % usize::from(cols)) as u16;
        #[allow(clippy::cast_possible_truncation)]
        let row_from_bottom = (i / usize::from(cols)) as u16;
        if row_from_bottom >= rows {
            break;
        }
        let row = rows - 1 - row_from_bottom;
        let x = bucket_area.x + col * BUCKET_STRIDE;
        let y = bucket_area.y + row;

        // Cached glyph wins. Only fall back to a live classify when
        // the cache is still `None` (the slot landed THIS tick — the
        // tick refresh will populate the cache before the next
        // render).
        let CellGlyph { ch, style } = cell.glyph.unwrap_or_else(|| classify_slot(pane, cell.slot));

        let (ch_out, style_out) = wipe_progress.map_or((ch, style), |p| {
            wipe_cell(ch, style, col, cols_minus_one, p)
        });
        buf[(x, y)].set_char(ch_out).set_style(style_out);
    }
}

/// Paint the left-side tx stream inside `area`. Each row shows one
/// recent slot whose `BankFrozen` carried a nonzero
/// `signature_count`, newest at top:
///
/// ```text
///  535928 ▆ 67k
///  535921 ▂  8k
///  535914 ▁  2k
/// ```
///
/// Three styled fields per row:
///
/// - **slot** — dim gray, fixed 6-cell right-aligned column.
/// - **bar** — cyan single-cell glyph from [`STREAM_BARS`]; height
///   scales against the max count in the visible window so a
///   pressure spike pops without flattening the rest.
/// - **count** — bold white compact integer (`67k`-style). Aligned
///   right inside whatever cells remain after slot + bar + spacing.
///
/// **Zero-sig filter.** Empirically (24h capture against this
/// validator's log) only ~9 % of bank-frozen slots carry a nonzero
/// `signature_count` — the other 89 % are `signature_count: 0` and
/// look identical to "no data yet" in a per-row stream. Surfacing
/// them as `0` rows leaves the stream noisy with empty blocks and
/// hides the operationally interesting (nonzero) blocks below them.
/// The time-series tx-pressure card aggregates across 10-minute
/// buckets so the few nonzero spikes dominate the rate; here, with
/// row-per-slot granularity, the filter is the equivalent operator
/// signal. Slot-numbers in the stream may therefore be sparse — that
/// gap is the data, not a render bug.
fn render_tx_stream(pane: &ChainPane, frame: &mut Frame<'_>, area: Rect) {
    if area.width < TX_STREAM_MIN_WIDTH || area.height == 0 {
        return;
    }
    let rows = usize::from(area.height);
    // Walk the deque newest-first, collecting (slot, sigs) for the
    // first `rows` slots with a nonzero captured count — see the
    // zero-sig filter note in the doc-comment above.
    let recent: Vec<(u64, u64)> = pane
        .slots
        .iter()
        .rev()
        .filter_map(|s| s.signature_count.filter(|c| *c > 0).map(|c| (s.slot, c)))
        .take(rows)
        .collect();
    if recent.is_empty() {
        return;
    }
    // Bar scale: max sig count in the visible window, clamped to ≥1
    // to avoid div-by-zero when every visible slot carries zero.
    let max_count = recent.iter().map(|(_, c)| *c).max().unwrap_or(1).max(1);
    let buf = frame.buffer_mut();
    let dim_gray = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let bar_style = Style::default().fg(Color::Cyan);
    let count_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    for (row_idx, (slot, count)) in recent.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let y = area.y + row_idx as u16;
        let slot_field = format!("{slot:>6}");
        // Bar level: 0..8 from the relative magnitude. `count == 0`
        // explicitly maps to the blank glyph so the eye distinguishes
        // "real-zero" from missing.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let bar_idx = if *count == 0 {
            0
        } else {
            (((*count as f64 / max_count as f64) * 8.0).round() as usize).clamp(1, 8)
        };
        let bar_glyph = STREAM_BARS[bar_idx];
        let count_text = compact_count(*count);
        // Cell layout: " 535928 ▆  67k" — 1 pad + 6 slot + 1 sp + 1
        // bar + 1 sp + count (right-aligned in remaining cells).
        let mut x = area.x;
        // Leading space — keeps stream off the very left edge so it
        // doesn't look glued to the pane border.
        if area.width >= 1 {
            buf[(x, y)].set_char(' ');
            x = x.saturating_add(1);
        }
        // Slot number (right-aligned in 6 cells).
        for ch in slot_field.chars().take(6) {
            buf[(x, y)].set_char(ch).set_style(dim_gray);
            x = x.saturating_add(1);
        }
        // Spacer.
        buf[(x, y)].set_char(' ');
        x = x.saturating_add(1);
        // Bar.
        buf[(x, y)].set_char(bar_glyph).set_style(bar_style);
        x = x.saturating_add(1);
        // Spacer + count right-aligned in remaining cells.
        let end = area.x + area.width;
        if x < end {
            buf[(x, y)].set_char(' ');
            x = x.saturating_add(1);
        }
        // Right-align: pad the count into the remaining width.
        #[allow(clippy::cast_possible_truncation)]
        let remaining = end.saturating_sub(x) as usize;
        if remaining == 0 {
            continue;
        }
        let count_chars: Vec<char> = count_text.chars().collect();
        let pad = remaining.saturating_sub(count_chars.len());
        for _ in 0..pad {
            buf[(x, y)].set_char(' ');
            x = x.saturating_add(1);
        }
        for ch in count_chars.into_iter().take(remaining) {
            buf[(x, y)].set_char(ch).set_style(count_style);
            x = x.saturating_add(1);
        }
    }
}

/// Compact `N` → `Nk` once it crosses 1 000, mirrors the leader
/// pane's `format_count_compact` style. Inline here to avoid a
/// cross-module dependency on `leader::format`.
fn compact_count(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else {
        format!("{}k", n / 1_000)
    }
}

/// Apply the magic-wipe sweep transform to a single cell. Returns
/// the glyph + style to paint given the wipe progress `p` and the
/// cell's column index. Pure function — easy to unit-test.
fn wipe_cell(ch: char, style: Style, col: u16, cols_minus_one: u16, p: f32) -> (char, Style) {
    // Column progress: 0.0 at leftmost column, 1.0 at rightmost.
    let cp = if cols_minus_one == 0 {
        0.0
    } else {
        f32::from(col) / f32::from(cols_minus_one)
    };
    if p < cp {
        // Sweep hasn't reached this column yet.
        (ch, style)
    } else if p < cp + WIPE_FLASH_WINDOW {
        // Sweep front passing — bright flash.
        (
            ch,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        // Column has been swept — blank cell.
        (' ', Style::default())
    }
}

/// Translate normalised world `(x, y) ∈ [0, 1]²` to a buffer cell
/// inside `area`. Returns `None` if the resulting cell falls outside
/// `area` (catches particles that overshoot before TTL expiry).
fn world_to_screen(area: Rect, x: f32, y: f32) -> Option<(u16, u16)> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let xc = x.clamp(0.0, 1.0);
    let yc = y.clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let col = (xc * f32::from(area.width - 1)).round() as u16;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let row = (yc * f32::from(area.height - 1)).round() as u16;
    Some((area.x + col, area.y + row))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_to_screen_maps_corners() {
        let area = Rect::new(10, 5, 40, 10);
        assert_eq!(world_to_screen(area, 0.0, 0.0), Some((10, 5)));
        assert_eq!(world_to_screen(area, 1.0, 1.0), Some((49, 14)));
    }

    #[test]
    fn world_to_screen_clamps_out_of_bounds_values() {
        let area = Rect::new(0, 0, 20, 10);
        assert_eq!(world_to_screen(area, 1.5, 1.5), Some((19, 9)));
        assert_eq!(world_to_screen(area, -0.5, -0.5), Some((0, 0)));
    }

    #[test]
    fn world_to_screen_handles_zero_size_area() {
        let area = Rect::new(0, 0, 0, 0);
        assert!(world_to_screen(area, 0.5, 0.5).is_none());
    }

    #[test]
    fn world_to_screen_rejects_non_finite_inputs() {
        let area = Rect::new(0, 0, 10, 10);
        assert!(world_to_screen(area, f32::NAN, 0.5).is_none());
        assert!(world_to_screen(area, 0.5, f32::INFINITY).is_none());
    }

    #[test]
    fn compute_bucket_area_targets_default_size_when_room() {
        // 25 cols × 2 stride = 50; 5 rows. Plenty of room in 80×9.
        let viz = Rect::new(0, 0, 80, 9);
        let b = compute_bucket_area(viz);
        assert_eq!(b.width, 50);
        assert_eq!(b.height, 5);
        // Centred horizontally.
        assert_eq!(b.x, 15);
        // Bottom-aligned vertically — bucket flush with viz bottom
        // so the arena above gets the full slack.
        assert_eq!(b.y, 4, "bucket should sit at viz_height - bucket_height");
    }

    #[test]
    fn compute_bucket_area_falls_back_when_narrow() {
        // Viz tighter than the default 50-cell width: bucket
        // shrinks to fit.
        let viz = Rect::new(0, 0, 30, 5);
        let b = compute_bucket_area(viz);
        assert_eq!(b.width, 30);
        assert_eq!(b.height, 5);
    }

    #[test]
    fn compute_tx_stream_area_fills_left_margin_with_breathing_gap() {
        // Bucket centred in an 80-wide viz: 50 cols centred ⇒ left
        // edge at x=15. Stream should take cols 0..(15-1=14), capped
        // at TX_STREAM_MAX_WIDTH = 14. The 1-cell gap between
        // stream and bucket keeps them from touching.
        let viz = Rect::new(0, 0, 80, 9);
        let bucket = compute_bucket_area(viz);
        assert_eq!(bucket.x, 15);
        let stream = compute_tx_stream_area(viz, bucket);
        assert_eq!(stream.x, 0);
        assert_eq!(stream.width, 14);
        assert_eq!(stream.height, viz.height);
    }

    #[test]
    fn compute_tx_stream_area_returns_zero_when_margin_too_narrow() {
        // Narrow viz: bucket would take the whole width, leaving no
        // left margin. Stream must drop out (zero-area) rather than
        // squeeze into 2-3 cells where the slot column alone can't
        // fit.
        let viz = Rect::new(0, 0, 30, 9);
        let bucket = compute_bucket_area(viz);
        let stream = compute_tx_stream_area(viz, bucket);
        assert_eq!(stream.width, 0, "no room ⇒ no stream: {stream:?}");
    }

    #[test]
    fn compute_bucket_area_clamps_height_when_viz_shorter_than_default() {
        // Viz shorter than the default 5-row bucket: bucket
        // shrinks to fit and is flush with the bottom.
        let viz = Rect::new(0, 0, 80, 4);
        let b = compute_bucket_area(viz);
        assert_eq!(b.height, 4, "bucket clamps to viz height");
        assert_eq!(b.y, 0, "no top margin when bucket already fills viz");
    }

    #[test]
    fn wipe_cell_passes_through_when_sweep_not_reached() {
        // Sweep at 10%, column at 50% → cell renders normally.
        let style = Style::default().fg(Color::Green);
        let (ch, _) = wipe_cell('■', style, 12, 24, 0.1);
        assert_eq!(ch, '■');
    }

    #[test]
    fn wipe_cell_flashes_white_when_sweep_front_passes() {
        // Column at 50%, sweep at 52% → within flash window.
        let style = Style::default().fg(Color::Green);
        let (_, out_style) = wipe_cell('■', style, 12, 24, 0.52);
        assert_eq!(out_style.fg, Some(Color::White));
        assert!(out_style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn wipe_cell_blanks_after_sweep_passes() {
        // Column at 0%, sweep at 80% → far past, cell is blank.
        let style = Style::default().fg(Color::Green);
        let (ch, _) = wipe_cell('■', style, 0, 24, 0.8);
        assert_eq!(ch, ' ');
    }
}
