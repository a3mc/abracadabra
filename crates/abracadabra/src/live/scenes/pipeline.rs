//! Slot pipeline pane.
//!
//! Visual layout (one row per active slot, multiple rows concurrent):
//!
//! ```text
//!  shreds        bank        vote        cert        final       root
//!  ·  *           ⌬           ^           ◇           ◆            ▓▓▓▓
//!  *              ⌬                                                ▓▓▓
//!  ·                          ^           ◇                        ▓
//!                                                                  ░░░░  (grave)
//!  <C-                                                                   (pacman)
//! ```
//!
//! Each slot is one [`SlotVisual`]: a `Slot` from the engine plus an
//! assigned row (`y`) and a fractional lane position (`current_lane`)
//! that interpolates toward the lane index of the current stage. When
//! the slot reaches the root lane it is removed and the rooted counter
//! ticks up. When it transitions to `Skipped`, it falls vertically
//! into the grave pile.
//!
//! Shred glyphs (`*` / `·`) are transient particles in the engine
//! [`World`] spawned on `FirstShred`; they fall through the shred
//! lane and despawn on TTL. Pacman is one always-on entity that
//! walks back and forth at the bottom of the shred lane — fun
//! mascot, no game mechanics.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::live::animation::{Entity, Pane, Slot, SlotStage, World, WORLD_CAPACITY};
use crate::parser::{Event, EventKind};
use crate::tui::theme;

/// Lane count (Shred / Bank / Voted / Notarized / Finalized + visual
/// root sink). The root sink is rendered as a pile rather than a lane
/// proper, but lane indices 0..=4 use it as their right boundary.
const LANE_COUNT: usize = 6;

/// Pacman moves this many cells per second.
const PACMAN_SPEED: f32 = 6.0;

/// Slots accelerate between lanes at this many lane-widths per second.
const SLOT_LANE_SPEED: f32 = 1.5;

/// Vertical fall speed for slots transitioning to `Skipped`. Faster
/// than horizontal so the failure path reads as "dropped".
const SKIP_FALL_SPEED: f32 = 12.0;

/// Maximum slot rows rendered concurrently. Round-robin reused; older
/// rooted / skipped slots vacate the row.
const MAX_SLOT_ROWS: u16 = 10;

/// TTL for transient shred particles (the falling `*` / `·` glyphs).
const SHRED_PARTICLE_TTL: Duration = Duration::from_millis(1500);

/// Visual state for one slot.
#[derive(Debug)]
struct SlotVisual {
    inner: Slot,
    /// Assigned terminal row (relative to the pane's inner area).
    y: u16,
    /// Fractional lane position. Lane integers 0..=5 are the targets;
    /// each tick moves this toward `target_lane()` by `SLOT_LANE_SPEED * dt`.
    current_lane: f32,
    /// True once the slot has moved past the rightmost lane and should
    /// be retired into the rooted pile.
    reached_root: bool,
    /// Negative until the slot transitions to `Skipped`; once skipped,
    /// y drifts downward by this value × dt each tick (falling into
    /// the grave pile).
    fall_vy: f32,
    /// True once a skipped slot has descended past the visible area.
    fell_into_grave: bool,
}

impl SlotVisual {
    const fn new(slot: Slot, y: u16) -> Self {
        Self {
            inner: slot,
            y,
            current_lane: 0.0,
            reached_root: false,
            fall_vy: 0.0,
            fell_into_grave: false,
        }
    }

    const fn target_lane(&self) -> f32 {
        match self.inner.stage {
            SlotStage::Shred => 0.0,
            SlotStage::Bank => 1.0,
            SlotStage::Voted => 2.0,
            SlotStage::Notarized => 3.0,
            SlotStage::Finalized => 4.0,
            SlotStage::Rooted => 5.0,
            SlotStage::Skipped => self.current_lane,
        }
    }

    fn stage_glyph(&self) -> (char, Style) {
        match self.inner.stage {
            SlotStage::Shred => ('·', Style::default().fg(Color::White)),
            SlotStage::Bank => ('o', Style::default().fg(Color::Cyan)),
            SlotStage::Voted => ('^', Style::default().fg(Color::Blue)),
            SlotStage::Notarized => ('◇', Style::default().fg(Color::Yellow)),
            SlotStage::Finalized => ('◆', Style::default().fg(Color::Green)),
            SlotStage::Rooted => ('▓', Style::default().fg(Color::Green)),
            SlotStage::Skipped => ('x', Style::default().fg(Color::Red)),
        }
    }
}

/// The pipeline pane. Owns all active slots, the particle world, the
/// pacman position, and the cumulative outcome counters.
pub struct PipelinePane {
    slots: BTreeMap<u64, SlotVisual>,
    /// Row assignment cursor — increments on each new FirstShred,
    /// wraps at `MAX_SLOT_ROWS`. Crude but predictable.
    next_row: u16,
    world: World,
    pacman_x: f32,
    pacman_dir: f32,
    rooted_count: u64,
    grave_count: u64,
}

impl PipelinePane {
    pub fn new() -> Self {
        Self {
            slots: BTreeMap::new(),
            next_row: 0,
            world: World::new(),
            pacman_x: 0.0,
            pacman_dir: 1.0,
            rooted_count: 0,
            grave_count: 0,
        }
    }

    fn spawn_shred_particles(&mut self, count: u8) {
        // Particles fall from y=0 toward the bottom of the shred lane.
        // Random-ish x by hashing the count + a wrapping cursor; no
        // RNG dependency.
        for i in 0..count {
            #[allow(clippy::cast_precision_loss)]
            let x = ((self.world.len() + i as usize) % 12) as f32;
            self.world.spawn(
                Entity::at(x, 0.0, if i % 2 == 0 { '*' } else { '·' }, Color::White)
                    .with_velocity(0.0, 2.5)
                    .with_ttl(SHRED_PARTICLE_TTL),
            );
        }
    }

    const fn assign_row(&mut self) -> u16 {
        let row = self.next_row;
        self.next_row = (self.next_row + 1) % MAX_SLOT_ROWS;
        row
    }

    fn ensure_room_for_new_slot(&mut self) {
        // Hard cap to keep render cheap. Drop the oldest slot (lowest
        // slot number) if we have too many — its counters survive on
        // the rooted / grave tallies if it reached one of those.
        if self.slots.len() >= WORLD_CAPACITY {
            if let Some((&first, _)) = self.slots.iter().next() {
                self.slots.remove(&first);
            }
        }
    }
}

impl Default for PipelinePane {
    fn default() -> Self {
        Self::new()
    }
}

impl Pane for PipelinePane {
    fn on_event(&mut self, ev: &Event) {
        let now = Instant::now();
        // Birth: FirstShred starts a new slot in this pane's view.
        if let EventKind::FirstShred { slot } = ev.kind {
            self.ensure_room_for_new_slot();
            let row = self.assign_row();
            self.slots
                .entry(slot)
                .or_insert_with(|| SlotVisual::new(Slot::new(slot, false, now), row));
            self.spawn_shred_particles(3);
            return;
        }
        // All other events feed into the existing slot state machine.
        // Iterate by slot number — the events that target a specific
        // slot will only match that slot's record.
        for sv in self.slots.values_mut() {
            sv.inner.advance(ev, now);
        }
    }

    fn tick(&mut self, now: Instant) {
        // Pacman: bounce in the bottom row of the shred lane. Bounds
        // are computed at render time against actual area width.
        // Approximate here with a generous walk range; render clamps.
        self.pacman_x = (self.pacman_dir * PACMAN_SPEED).mul_add(1.0 / 10.0, self.pacman_x);
        if self.pacman_x < 0.0 {
            self.pacman_x = 0.0;
            self.pacman_dir = 1.0;
        } else if self.pacman_x > 14.0 {
            self.pacman_x = 14.0;
            self.pacman_dir = -1.0;
        }

        // Particles tick on their own world.
        self.world.tick(now);

        // Advance each slot toward its lane target.
        let dt = 1.0 / 10.0; // fixed 10 Hz model; engine drives real cadence
        for sv in self.slots.values_mut() {
            let target = sv.target_lane();
            let delta = target - sv.current_lane;
            let step = SLOT_LANE_SPEED * dt;
            sv.current_lane += delta.clamp(-step, step);

            // Skipped: start falling once we register the stage.
            if sv.inner.stage == SlotStage::Skipped && sv.fall_vy == 0.0 {
                sv.fall_vy = SKIP_FALL_SPEED;
            }
            if sv.fall_vy > 0.0 && !sv.fell_into_grave {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let new_y = sv.fall_vy.mul_add(dt, f32::from(sv.y)) as u16;
                sv.y = new_y;
                if sv.y >= MAX_SLOT_ROWS {
                    sv.fell_into_grave = true;
                }
            }

            // Rooted: mark for retirement once lane 5 is reached.
            if sv.inner.stage == SlotStage::Rooted && sv.current_lane >= 4.9 {
                sv.reached_root = true;
            }
        }

        // Retire slots that completed their visual journey. Counters
        // tick up here, not on the protocol transition, so the
        // operator's pile reflects what they actually saw animated.
        let to_remove: Vec<u64> = self
            .slots
            .iter()
            .filter_map(|(&n, sv)| {
                if sv.reached_root || sv.fell_into_grave {
                    Some(n)
                } else {
                    None
                }
            })
            .collect();
        for n in to_remove {
            if let Some(sv) = self.slots.remove(&n) {
                if sv.reached_root {
                    self.rooted_count = self.rooted_count.saturating_add(1);
                } else if sv.fell_into_grave {
                    self.grave_count = self.grave_count.saturating_add(1);
                }
            }
        }
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" pipeline ")
            .title_style(theme::title_style())
            .border_style(theme::title_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < 20 || inner.height < 3 {
            return; // too small to draw meaningful animation
        }

        // Header line: lane labels evenly spaced.
        let lane_width = inner.width as f32 / LANE_COUNT as f32;
        let labels = ["shreds", "bank", "vote", "cert", "final", "root"];
        for (i, label) in labels.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let x = inner.x + (i as f32 * lane_width) as u16;
            if x + label.len() as u16 > inner.x + inner.width {
                continue;
            }
            let area = Rect::new(x, inner.y, label.len() as u16, 1);
            frame.render_widget(
                Paragraph::new(Span::styled(*label, theme::label_style())),
                area,
            );
        }

        // Slot rows. Each slot's screen x = current_lane * lane_width +
        // lane_width / 2 (centered in lane). Row y is its assigned row
        // offset from the area's first non-header row.
        for sv in self.slots.values() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let x = inner.x
                + lane_width
                    .mul_add(0.5, sv.current_lane * lane_width)
                    .min(f32::from(inner.width - 1)) as u16;
            let y = inner.y + 1 + sv.y;
            if y >= inner.y + inner.height {
                continue;
            }
            let (ch, style) = sv.stage_glyph();
            let cell = Rect::new(x, y, 1, 1);
            frame.render_widget(Paragraph::new(Span::styled(ch.to_string(), style)), cell);
        }

        // Shred particles from the world. Each entity rendered at its
        // (x, y) clamped to the shred lane.
        let shred_lane_width = lane_width;
        for e in &self.world {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let x = inner.x + (e.x.clamp(0.0, shred_lane_width - 1.0)) as u16;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let y = inner.y + 1 + (e.y as u16).min(inner.height.saturating_sub(2));
            if x >= inner.x + inner.width || y >= inner.y + inner.height {
                continue;
            }
            frame.render_widget(
                Paragraph::new(Span::styled(e.ch.to_string(), Style::default().fg(e.fg))),
                Rect::new(x, y, 1, 1),
            );
        }

        // Pacman: bottom of the shred lane. `<C-` walking right,
        // `-Cv` walking left.
        if inner.height > 2 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pac_x =
                inner.x + (self.pacman_x.clamp(0.0, (shred_lane_width - 3.0).max(0.0))) as u16;
            let pac_y = inner.y + inner.height - 2;
            let glyph = if self.pacman_dir >= 0.0 { "<C-" } else { "-Cv" };
            let cell = Rect::new(
                pac_x,
                pac_y,
                glyph.len().min((inner.width - (pac_x - inner.x)) as usize) as u16,
                1,
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    glyph,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                cell,
            );
        }

        // Bottom-right: rooted and grave pile counters + visual.
        let pile_y = inner.y + inner.height - 1;
        if pile_y < inner.y + inner.height {
            let txt = format!("rooted {} · grave {}", self.rooted_count, self.grave_count);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pile_x = inner.x + inner.width.saturating_sub(txt.len() as u16);
            frame.render_widget(
                Paragraph::new(Span::styled(txt.clone(), Style::default().fg(Color::Gray))),
                Rect::new(pile_x, pile_y, txt.len() as u16, 1),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_event(kind: EventKind) -> Event {
        Event {
            ts: time::OffsetDateTime::UNIX_EPOCH,
            kind,
        }
    }

    #[test]
    fn first_shred_spawns_a_slot_and_particles() {
        let mut p = PipelinePane::new();
        p.on_event(&mk_event(EventKind::FirstShred { slot: 100 }));
        assert_eq!(p.slots.len(), 1);
        assert!(p.world.len() >= 3);
        assert!(p.slots.contains_key(&100));
    }

    #[test]
    fn duplicate_first_shred_is_idempotent_for_slot_set() {
        let mut p = PipelinePane::new();
        p.on_event(&mk_event(EventKind::FirstShred { slot: 100 }));
        p.on_event(&mk_event(EventKind::FirstShred { slot: 100 }));
        // Slot count is 1; particles may accumulate.
        assert_eq!(p.slots.len(), 1);
    }

    #[test]
    fn bank_frozen_advances_existing_slot_to_bank_stage() {
        let mut p = PipelinePane::new();
        p.on_event(&mk_event(EventKind::FirstShred { slot: 7 }));
        p.on_event(&mk_event(EventKind::BankFrozen {
            slot: 7,
            hash: "h".into(),
            signature_count: 42,
        }));
        let sv = p.slots.get(&7).unwrap();
        assert_eq!(sv.inner.stage, SlotStage::Bank);
        assert_eq!(sv.inner.signature_count, Some(42));
    }

    #[test]
    fn skipped_slot_starts_falling_on_tick() {
        let mut p = PipelinePane::new();
        p.on_event(&mk_event(EventKind::FirstShred { slot: 7 }));
        p.on_event(&mk_event(EventKind::VotingSkip { slot: 7 }));
        // First tick should set fall_vy > 0.
        p.tick(Instant::now());
        let sv = p.slots.get(&7).unwrap();
        assert!(sv.fall_vy > 0.0);
    }

    #[test]
    fn rooted_slot_retires_into_pile_after_animation_catches_up() {
        let mut p = PipelinePane::new();
        p.on_event(&mk_event(EventKind::FirstShred { slot: 7 }));
        p.on_event(&mk_event(EventKind::NewRoot { slot: 7 }));
        // Tick enough times that lane animation catches up to root.
        for _ in 0..100 {
            p.tick(Instant::now());
        }
        // Slot vanished from active map, rooted counter incremented.
        assert!(!p.slots.contains_key(&7));
        assert!(p.rooted_count >= 1);
    }

    #[test]
    fn pacman_walks_and_bounces() {
        let mut p = PipelinePane::new();
        let start = p.pacman_x;
        for _ in 0..10 {
            p.tick(Instant::now());
        }
        assert!(p.pacman_x > start, "pacman did not move");
        // Tick enough cycles to cross the lane multiple times. Observe
        // direction flips by sampling `pacman_dir` on each tick and
        // counting changes from positive to negative or back.
        let mut prev_dir = p.pacman_dir;
        let mut flips = 0u32;
        for _ in 0..400 {
            p.tick(Instant::now());
            if p.pacman_dir != prev_dir {
                flips += 1;
                prev_dir = p.pacman_dir;
            }
        }
        assert!(
            flips >= 2,
            "pacman direction never flipped (flips = {flips})"
        );
    }

    #[test]
    fn row_assignment_round_robins() {
        let mut p = PipelinePane::new();
        for i in 0..(MAX_SLOT_ROWS as u64 + 3) {
            p.on_event(&mk_event(EventKind::FirstShred { slot: i }));
        }
        // Rows for the first MAX_SLOT_ROWS slots are 0..MAX_SLOT_ROWS-1;
        // wraps after that.
        let row_for = |n: u64| p.slots.get(&n).map(|s| s.y);
        assert_eq!(row_for(0), Some(0));
        assert_eq!(row_for((MAX_SLOT_ROWS as u64) - 1), Some(MAX_SLOT_ROWS - 1));
        assert_eq!(row_for(MAX_SLOT_ROWS as u64), Some(0));
    }
}
