//! Ratatui rendering for the chain pane — cannon-particle spike.
//!
//! Layout (top → bottom inside the border):
//!
//! 1. **Header** — one line: spinner · tip slot · `CSKIP` · `indet`
//!    · `forks` · (optional) `anom`. Compact so the rest of the
//!    pane is animation space.
//! 2. **Arena** — empty area above the matrix where in-flight
//!    cannon particles travel. Cannon glyph `▶` anchors the top-left.
//! 3. **Matrix** — grid of cells, one per slot that has landed.
//!    Each cell's glyph + colour is derived from the slot's current
//!    [`super::state::SlotState`] so subsequent events (finalize
//!    arriving after landing, fork detected after landing) visibly
//!    update the cell colour.
//!
//! **World-space → screen-space.** Particles carry normalised
//! `(x, y) ∈ [0, 1]²` coordinates. The visualisation area's `Rect`
//! scales them at render time so the system is layout-agnostic —
//! resizing the terminal does not break the trajectories.
//!
//! **No real classifier yet.** The matrix paints every landed slot
//! the same colour (`■` cyan) and the cannon glyph is static. Step 2
//! of the rebuild will swap in the per-slot classifier from the
//! previous chain pane's event-log vocabulary (`■ ◐ ▴ ⊕ ○ ·`).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::live::animation::spinner_frame;
use crate::tui::theme;

use super::format::StagePercentiles;
use super::glyph::{classify_slot, CellGlyph};
use super::particle::{CANNON_X, CANNON_Y};
use super::state::ChainPane;

/// Logical pane height advertised to the scene engine. Real layout
/// uses `Constraint::Min(0)` (see `live/scenes/mod.rs`); this constant
/// is only consulted by the vertical-stack fallback.
pub const PANE_HEIGHT: u16 = 12;

/// Glyph painted at the cannon position. Static — does not animate
/// in the spike. Future: muzzle-flash flicker on spawn.
const CANNON_GLYPH: &str = "▶";

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
            Constraint::Length(1), // header line 1: spinner + tip + counters
            Constraint::Length(1), // header line 2: timing percentiles
            Constraint::Length(1), // blank
            Constraint::Min(1),    // visualisation (arena + matrix)
        ])
        .split(inner);

    render_header(pane, frame, chunks[1]);
    render_timing(pane, frame, chunks[2]);
    render_visualisation(pane, frame, chunks[4]);
}

/// Compact 1-line timing strip: `cluster N · asm N · cons N · lc N`
/// (p50 ms each). p95 is intentionally dropped from the strip so it
/// fits a single row — operators who want the full distribution have
/// the Windows tab. Stages with no samples render as `—` placeholders
/// so the strip width stays stable.
fn render_timing(pane: &ChainPane, frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(Paragraph::new(timing_line(pane)), area);
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
    let mut spans: Vec<Span<'static>> = vec![Span::raw("   ")];
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

/// Compact 1-line header: spinner + tip slot + counters. Replaces
/// the previous 4-row timing table. Timing percentiles will return
/// in step 2 as a second header line when the pane is tall enough.
fn render_header(pane: &ChainPane, frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(Paragraph::new(header_line(pane)), area);
}

/// Compose the header `Line`. Extracted from `render_header` for
/// direct test coverage of the label vocabulary (no frame
/// roundtripping required).
pub(super) fn header_line(pane: &ChainPane) -> Line<'static> {
    let spinner = spinner_frame(pane.event_count, pane.last_event_at);
    let tip = pane
        .tip_slot()
        .map_or_else(|| "—".to_owned(), |s| s.to_string());
    let (canonical_skips, indeterminate) = pane.skip_tallies();
    let forks = pane.fork_count();

    let cskip_style = if canonical_skips > 0 {
        theme::bad_style().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let fork_style = if forks > 0 {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let mut spans = vec![
        Span::raw(" "),
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
        sep(),
        Span::styled(canonical_skips.to_string(), cskip_style),
        Span::styled(" CSKIP", theme::label_style()),
        sep(),
        Span::styled(
            indeterminate.to_string(),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(" indet", theme::label_style()),
        sep(),
        Span::styled(forks.to_string(), fork_style),
        Span::styled(" forks", theme::label_style()),
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

/// Paint the cannon + in-flight particles + landing matrix inside
/// `viz`. Splits `viz` vertically into an arena (top) and a matrix
/// area (bottom). Particle world-space spans the FULL `viz` so a
/// particle launched at `(CANNON_X, CANNON_Y)` ends up landing in
/// the matrix half of the rect at TTL expiry.
fn render_visualisation(pane: &ChainPane, frame: &mut Frame<'_>, viz: Rect) {
    if viz.width == 0 || viz.height == 0 {
        return;
    }
    // Matrix gets the bottom 60% (rounded) so a typical 6-row viz
    // area gives ~2 rows of arena and ~4 rows of matrix. Below 4
    // rows the matrix takes priority — the cannon is decorative,
    // the matrix carries the data.
    let matrix_height = (u32::from(viz.height) * 6 / 10) as u16;
    let matrix_height = matrix_height.max(1).min(viz.height);
    let arena_height = viz.height.saturating_sub(matrix_height);
    let arena = Rect::new(viz.x, viz.y, viz.width, arena_height);
    let matrix = Rect::new(viz.x, viz.y + arena_height, viz.width, matrix_height);

    render_cannon(frame, viz);
    render_particles(pane, frame, viz);
    render_matrix(pane, frame, matrix);
    // arena is currently passive — kept as a named binding so step 2
    // can layer arena decorations (tracer trails, edge caret) without
    // re-deriving the rect.
    let _ = arena;
}

/// Paint a static cannon glyph at `(CANNON_X, CANNON_Y)` in the
/// `viz` rect. Future: animate on spawn for muzzle-flash.
fn render_cannon(frame: &mut Frame<'_>, viz: Rect) {
    let Some((cx, cy)) = world_to_screen(viz, CANNON_X, CANNON_Y) else {
        return;
    };
    let buf = frame.buffer_mut();
    buf.set_string(
        cx,
        cy,
        CANNON_GLYPH,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
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

/// Paint landed-slot cells into the matrix area.
///
/// **Layout: bottom-up cup fill.** The oldest landed slot sits at
/// the bottom-left; each subsequent slot fills left-to-right across
/// the bottom row, then rows above. The newest slot is at the
/// top-right (when the matrix is full) or in the top-most partial
/// row. Mirrors the "filling cup" metaphor the operator asked for —
/// new arrivals stack ON TOP of older ones.
///
/// **Stride.** 2-cell wide per slot: glyph + one space. Packed solid
/// reads as a bar; spaced cells read as discrete slots.
///
/// **Age decay.** Cells dim in three tiers by their depth in the
/// landed deque:
///
/// - newest 15%: full classifier style (often BOLD)
/// - middle: classifier style without the BOLD modifier
/// - oldest 20%: DIM modifier and BOLD stripped
///
/// The decay band makes the leading edge visually anchor while old
/// history visibly fades — the user can tell at a glance which cells
/// are fresh signal vs settled history.
fn render_matrix(pane: &ChainPane, frame: &mut Frame<'_>, matrix_area: Rect) {
    if matrix_area.width == 0 || matrix_area.height == 0 {
        return;
    }
    let stride: u16 = 2;
    let cols = matrix_area.width / stride;
    if cols == 0 {
        return;
    }
    let rows = matrix_area.height;
    let capacity = usize::from(cols) * usize::from(rows);
    if capacity == 0 {
        return;
    }

    let visible_count = pane.cannon.matrix.len().min(capacity);
    let start = pane.cannon.matrix.len() - visible_count;
    // Decay band thresholds. Computed once per render so the per-cell
    // loop only does an integer comparison.
    let fresh_cutoff = visible_count.saturating_sub(visible_count * 15 / 100);
    let stale_cutoff = visible_count * 20 / 100;
    let buf = frame.buffer_mut();
    for (i, slot) in pane.cannon.matrix.iter().skip(start).enumerate() {
        // Bottom-up: oldest at row=rows-1 (bottom), newest at row=0
        // (top). Column fills left-to-right within each row.
        #[allow(clippy::cast_possible_truncation)]
        let col = (i % usize::from(cols)) as u16;
        #[allow(clippy::cast_possible_truncation)]
        let row_from_bottom = (i / usize::from(cols)) as u16;
        if row_from_bottom >= rows {
            // Defence-in-depth: capacity guard above should prevent
            // this, but if `visible_count` ever exceeds `capacity`
            // we'd paint outside the area.
            break;
        }
        let row = rows - 1 - row_from_bottom;
        let x = matrix_area.x + col * stride;
        let y = matrix_area.y + row;

        let CellGlyph { ch, mut style } = classify_slot(pane, *slot);
        if i >= fresh_cutoff {
            // Newest band — keep classifier style as-is.
        } else if i < stale_cutoff {
            // Oldest band — dim and strip bold.
            style = style.remove_modifier(Modifier::BOLD);
            style = style.add_modifier(Modifier::DIM);
        } else {
            // Middle band — strip bold so only the fresh tier reads
            // as the brightest. Keep the classifier colour intact.
            style = style.remove_modifier(Modifier::BOLD);
        }
        buf[(x, y)].set_char(ch).set_style(style);
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
    // Clamp to `[0, 1]` so transient float drift past the bounds
    // does not lead to a wraparound cast.
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
        // Past the right/bottom edge — clamped, not wrapped.
        assert_eq!(world_to_screen(area, 1.5, 1.5), Some((19, 9)));
        // Negative — clamped to origin.
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
}
