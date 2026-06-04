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

/// Cannon glyph painted at the top-centre of the visualisation
/// area. Points down — particles fall vertically into the bucket.
const CANNON_GLYPH: char = '▼';

/// Default bucket width in cells. 25 × 4 = 100 = [`PAGE_CAPACITY`].
const DEFAULT_BUCKET_COLS: usize = 25;
/// Default bucket height in rows.
const DEFAULT_BUCKET_ROWS: usize = 4;
/// Per-cell horizontal stride: `glyph + space`. Packed solid the
/// grid reads as a bar; spaced cells read as discrete slots.
const BUCKET_STRIDE: u16 = 2;

/// Magic-wipe per-column flash window as a fraction of the wipe
/// duration. A cell flashes between `column_progress` and
/// `column_progress + WIPE_FLASH_WINDOW`, then renders blank.
const WIPE_FLASH_WINDOW: f32 = 0.12;

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
            Constraint::Length(1), // top blank
            Constraint::Length(1), // header line 1: spinner + tip + cannon
            Constraint::Length(1), // header line 2: timing percentiles
            Constraint::Length(1), // blank
            Constraint::Min(1),    // visualisation (arena + bucket)
        ])
        .split(inner);

    render_header(pane, frame, chunks[1]);
    render_timing(pane, frame, chunks[2]);
    render_visualisation(pane, frame, chunks[4]);
}

/// Compose the header `Line`. Extracted from `render_header` for
/// direct test coverage.
///
/// Content: spinner + tip slot. Optional ` ⚠ N anom` segment when
/// parser walk-back anomalies have fired (silent by default).
/// CSKIP / indeterminate-skip / fork counts are not shown — the
/// bucket renders those classes visually. The cannon glyph is
/// painted in the visualisation area (not the header) by
/// [`render_cannon`] so particles can spawn directly beneath it.
pub(super) fn header_line(pane: &ChainPane) -> Line<'static> {
    let spinner = spinner_frame(pane.event_count, pane.last_event_at);
    let tip = pane
        .tip_slot()
        .map_or_else(|| "—".to_owned(), |s| s.to_string());

    let mut spans = vec![
        Span::styled(
            spinner.to_owned(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            tip,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" tip", theme::label_style()),
    ];
    if pane.walk_back_anomalies > 0 {
        spans.push(sep());
        spans.push(Span::styled(
            pane.walk_back_anomalies.to_string(),
            theme::bad_style().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" anom", theme::label_style()));
    }
    Line::from(spans)
}

fn render_header(pane: &ChainPane, frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(header_line(pane)).alignment(Alignment::Center),
        area,
    );
}

/// Compose the timing strip `Line`. Extracted from `render_timing`
/// for direct test coverage.
///
/// Format: each stage renders as `<name> <p50>/<p95>ms`. Full words
/// match the Windows-tab labels (`cadence` / `assembly` /
/// `consensus` / `lifecycle`) — opaque 2-3 letter abbreviations
/// (`asm`, `cons`, `lc`) drop legibility for almost no width win at
/// the half-pane widths the chain pane runs at. Trailing `ms` is
/// shown once per value, not once per stage — both percentiles
/// share the unit.
pub(super) fn timing_line(pane: &ChainPane) -> Line<'static> {
    let table = pane.timing_table();
    let stages: [(&str, StagePercentiles); 4] = [
        ("cadence", table.cluster),
        ("assembly", table.assembly),
        ("consensus", table.consensus),
        ("lifecycle", table.lifecycle),
    ];
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut first = true;
    for (label, pct) in stages {
        if !first {
            spans.push(sep());
        }
        spans.push(Span::styled(format!("{label} "), theme::label_style()));
        match pct {
            Some((p50, p95)) => {
                spans.push(Span::styled(format!("{p50}/{p95}"), theme::value_style()));
                spans.push(Span::styled("ms", theme::label_style()));
            }
            None => {
                spans.push(Span::styled("—", Style::default().fg(Color::DarkGray)));
            }
        }
        first = false;
    }
    Line::from(spans)
}

fn render_timing(pane: &ChainPane, frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(timing_line(pane)).alignment(Alignment::Center),
        area,
    );
}

/// Lay out the visualisation area: cannon glyph at the top-centre,
/// in-flight particles falling toward the bucket, bucket centred
/// both horizontally and vertically inside `viz`. Particle
/// world-space spans the full `viz`, so a particle launched at
/// `(CANNON_X, CANNON_Y)` ends up at world `(LANDING_X, LANDING_Y)`
/// — the centre of the bucket area — at TTL expiry.
fn render_visualisation(pane: &ChainPane, frame: &mut Frame<'_>, viz: Rect) {
    if viz.width == 0 || viz.height == 0 {
        return;
    }
    render_cannon(frame, viz);
    render_particles(pane, frame, viz);
    let bucket_area = compute_bucket_area(viz);
    render_bucket(pane, frame, bucket_area, Instant::now());
}

/// Paint the cannon `▼` at world `(CANNON_X, 0.0)` — top-centre of
/// the viz area. Static glyph; future steps could add a muzzle-
/// flash flicker on particle spawn.
fn render_cannon(frame: &mut Frame<'_>, viz: Rect) {
    let Some((cx, cy)) = world_to_screen(viz, super::particle::CANNON_X, 0.0) else {
        return;
    };
    let buf = frame.buffer_mut();
    buf[(cx, cy)].set_char(CANNON_GLYPH).set_style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
}

/// Position a centred bucket sub-rect inside `viz`. Aims for the
/// default 25×4 grid; falls back to whatever fits when the viz
/// width is narrower. Centred both horizontally AND vertically so
/// the bucket reads as a contained "inner widget" with breathing
/// margin above (where particles fly) and below.
fn compute_bucket_area(viz: Rect) -> Rect {
    let want_w = u16::try_from(DEFAULT_BUCKET_COLS).unwrap_or(u16::MAX) * BUCKET_STRIDE;
    let want_h = u16::try_from(DEFAULT_BUCKET_ROWS).unwrap_or(u16::MAX);
    let w = want_w.min(viz.width);
    let h = want_h.min(viz.height);
    let x_offset = viz.width.saturating_sub(w) / 2;
    // Vertically centre. When the available height is odd, the
    // extra row goes ABOVE the bucket (giving particles more arena)
    // so the bottom margin stays tight; matches the operator's
    // mockup where the top margin breathes more than the bottom.
    let y_offset = viz.height.saturating_sub(h) / 2 + viz.height.saturating_sub(h) % 2;
    Rect::new(viz.x + x_offset, viz.y + y_offset, w, h)
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

    for (i, slot) in pane.cannon.bucket.iter().enumerate() {
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

        let CellGlyph { ch, style } = classify_slot(pane, *slot);

        let (ch_out, style_out) = wipe_progress.map_or((ch, style), |p| {
            wipe_cell(ch, style, col, cols_minus_one, p)
        });
        buf[(x, y)].set_char(ch_out).set_style(style_out);
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

fn sep() -> Span<'static> {
    Span::styled(
        "  ·  ",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    )
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
        // 25 cols × 2 stride = 50; 4 rows. Plenty of room in 80×8.
        let viz = Rect::new(0, 0, 80, 8);
        let b = compute_bucket_area(viz);
        assert_eq!(b.width, 50);
        assert_eq!(b.height, 4);
        // Centred horizontally.
        assert_eq!(b.x, 15);
        // Vertically centred — viz_height - bucket_height = 4 → 2
        // rows of margin total, biased upward (top gets the extra
        // row when odd) so bucket sits at row 2.
        assert_eq!(b.y, 2);
    }

    #[test]
    fn compute_bucket_area_falls_back_when_narrow() {
        // Viz tighter than the default 50-cell width: bucket
        // shrinks to fit.
        let viz = Rect::new(0, 0, 30, 4);
        let b = compute_bucket_area(viz);
        assert_eq!(b.width, 30);
        assert_eq!(b.height, 4);
    }

    #[test]
    fn compute_bucket_area_biases_top_margin_when_height_is_odd() {
        // viz_height (9) − bucket_height (4) = 5 → split 3 above / 2
        // below so the particles have more arena to fall through.
        let viz = Rect::new(0, 0, 80, 9);
        let b = compute_bucket_area(viz);
        assert_eq!(b.height, 4);
        assert_eq!(b.y, 3, "top margin should take the extra row");
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
