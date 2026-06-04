//! Animation engine for the Live tab.
//!
//! Provides the small set of primitives all concrete scenes share:
//!
//! - [`Entity`] — a sprite with position, velocity, character, colour,
//!   age, and optional time-to-live. Used by scenes for transient
//!   particles (shreds raining, slot cards drifting along a lane,
//!   etc.). One char per entity; sub-cell motion is tracked in `f32`
//!   coordinates and rounded only at render time.
//! - [`World`] — a bounded collection of entities with `spawn`, `tick`,
//!   and `iter` operations. Tick advances positions, increments ages,
//!   and drops anything past its TTL.
//! - [`SlotStage`] / [`Slot`] — the protocol-level state machine that
//!   the slot-pipeline scene drives. Lives here because future panes
//!   (leader-window view, validator-mind view) may share the same
//!   vocabulary.
//! - [`Pane`] — the rendering contract. Each visual zone implements it.
//!   Composition (vertical / horizontal layout of multiple panes) is
//!   the scene's responsibility; this trait does not assume one.
//!
//! Engine state is intentionally not Send — the TUI runs single-threaded
//! and panes live on the render thread. The tail thread (LIVE-3) is the
//! only producer crossing thread boundaries; events flow into panes by
//! polling the shared [`crate::live::tail::LiveBuffer`] from the render
//! thread, never by sharing Pane state across threads.

use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::Frame;

use crate::parser::{Event, EventKind};

/// Hard cap on entity count per [`World`].
///
/// Keeps render and tick cheap during bursts. Older entities (oldest
/// first) are evicted when a spawn would otherwise push past this;
/// aggregate counters that survive eviction belong on the owning
/// scene, not on the engine.
pub const WORLD_CAPACITY: usize = 512;

/// One on-screen sprite at a sub-cell position.
///
/// Position uses `f32` so velocity-driven motion can be smooth across
/// frames; cells are rendered with `x as u16` / `y as u16` at draw
/// time. A negative velocity moves left / up; the engine does no
/// boundary clipping — out-of-area entities are still ticked and
/// counted toward the world cap until their TTL expires.
#[derive(Debug, Clone)]
pub struct Entity {
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub ch: char,
    pub fg: Color,
    pub age: Duration,
    pub ttl: Option<Duration>,
}

impl Entity {
    /// Stationary sprite at `(x, y)` with the given glyph and colour.
    /// Velocities and age default to zero, no TTL.
    pub const fn at(x: f32, y: f32, ch: char, fg: Color) -> Self {
        Self {
            x,
            y,
            vx: 0.0,
            vy: 0.0,
            ch,
            fg,
            age: Duration::ZERO,
            ttl: None,
        }
    }

    #[must_use]
    pub const fn with_velocity(mut self, vx: f32, vy: f32) -> Self {
        self.vx = vx;
        self.vy = vy;
        self
    }

    #[must_use]
    pub const fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// True iff this entity has aged past its TTL. Entities without a
    /// TTL never expire on their own; the scene must despawn them.
    pub fn is_expired(&self) -> bool {
        self.ttl.is_some_and(|t| self.age >= t)
    }
}

/// Bounded entity collection plus the tick clock.
///
/// `last_tick` anchors delta-time computation: every `tick` advances
/// `(x, y)` by `(vx, vy) * dt`, increments `age` by `dt`, then drops
/// expired entities. Spawn obeys [`WORLD_CAPACITY`] by evicting the
/// oldest entity (front of the vector) when full.
#[derive(Debug)]
pub struct World {
    entities: Vec<Entity>,
    last_tick: Instant,
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl World {
    /// Empty world anchored at `Instant::now()`. First `tick` will use
    /// a small or zero delta because no real time has passed yet.
    pub fn new() -> Self {
        Self {
            entities: Vec::new(),
            last_tick: Instant::now(),
        }
    }

    /// Add `e` to the world. When the world is at [`WORLD_CAPACITY`],
    /// the oldest entry (front of the vector) is evicted first.
    pub fn spawn(&mut self, e: Entity) {
        if self.entities.len() >= WORLD_CAPACITY {
            self.entities.remove(0);
        }
        self.entities.push(e);
    }

    /// Advance every entity by `(now - last_tick)`. Entities past
    /// their TTL are dropped after the position update so a callable
    /// pane sees their final position before they disappear.
    pub fn tick(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;
        let dt_secs = dt.as_secs_f32();
        for e in &mut self.entities {
            e.x = e.vx.mul_add(dt_secs, e.x);
            e.y = e.vy.mul_add(dt_secs, e.y);
            e.age += dt;
        }
        self.entities.retain(|e| !e.is_expired());
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Entity> {
        self.entities.iter()
    }

    pub const fn len(&self) -> usize {
        self.entities.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }
}

impl<'a> IntoIterator for &'a World {
    type Item = &'a Entity;
    type IntoIter = std::slice::Iter<'a, Entity>;

    fn into_iter(self) -> Self::IntoIter {
        self.entities.iter()
    }
}

// ---- Slot state machine ----------------------------------------------------

/// Protocol-level stage of a slot's life. Ordered so that any forward
/// transition increases the discriminant; the [`Slot::advance`] method
/// uses this to refuse out-of-order updates from the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SlotStage {
    /// `FirstShred` observed; bank assembly underway.
    Shred,
    /// `bank frozen` observed for this slot; signatures known.
    Bank,
    /// Our `Voting notarize` cast.
    Voted,
    /// `Block Notarized` cert observed (60% threshold reached).
    Notarized,
    /// `Finalized` cert observed (fast or slow path).
    Finalized,
    /// `new root` observed; slot is in the rooted chain.
    Rooted,
    /// We voted skip for this slot. Terminal in the local view; the
    /// cluster outcome (canonical / right-skip) is a separate
    /// classification handled by `aggregator::classify_skips`.
    Skipped,
}

/// Path by which a finalize cert was reached. Populated when the slot
/// advances into `SlotStage::Finalized`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizePath {
    /// Single-round 80% NotarizeFast.
    Fast,
    /// Two-round 60% Notarize + 60% Finalize.
    Slow,
}

/// Per-slot animation record.
///
/// Carries the protocol stage plus enough metadata to render the slot
/// distinctively (leader flag, signature count, finalize path). `x` is
/// the animated lane-x position used by the slot-pipeline pane; other
/// scenes (e.g. a vertical "validator mind" view) may ignore it.
#[derive(Debug, Clone)]
pub struct Slot {
    pub number: u64,
    pub stage: SlotStage,
    pub entered_stage_at: Instant,
    pub signature_count: Option<u64>,
    pub finalize_path: Option<FinalizePath>,
    pub our_leader: bool,
    pub x: f32,
}

impl Slot {
    /// Construct a freshly-shredded slot.
    pub const fn new(number: u64, our_leader: bool, now: Instant) -> Self {
        Self {
            number,
            stage: SlotStage::Shred,
            entered_stage_at: now,
            signature_count: None,
            finalize_path: None,
            our_leader,
            x: 0.0,
        }
    }

    /// Apply `ev` to this slot. Returns `true` if the slot advanced
    /// to a new stage or had its metadata updated; `false` if the
    /// event was unrelated or would have been a backward transition.
    ///
    /// Backward transitions (e.g. observing a `BankFrozen` after the
    /// slot has reached `Notarized`) are ignored: the log can deliver
    /// these out of order in narrow windows around the threshold
    /// (vote events emit before / after the cert log line depending
    /// on observer ordering), and the cluster always wins.
    pub fn advance(&mut self, ev: &Event, now: Instant) -> bool {
        match (self.stage, &ev.kind) {
            (
                _,
                EventKind::BankFrozen {
                    slot,
                    signature_count,
                    ..
                },
            ) if *slot == self.number => {
                self.signature_count = Some(*signature_count);
                self.transition(SlotStage::Bank, now)
            }
            (_, EventKind::VotingNotarize { slot, .. }) if *slot == self.number => {
                self.transition(SlotStage::Voted, now)
            }
            (_, EventKind::BlockNotarized { slot, .. }) if *slot == self.number => {
                self.transition(SlotStage::Notarized, now)
            }
            (_, EventKind::Finalized { slot, fast, .. }) if *slot == self.number => {
                self.finalize_path = Some(if *fast {
                    FinalizePath::Fast
                } else {
                    FinalizePath::Slow
                });
                self.transition(SlotStage::Finalized, now)
            }
            (_, EventKind::NewRoot { slot, .. }) if *slot == self.number => {
                self.transition(SlotStage::Rooted, now)
            }
            // Skip is terminal but idempotent: re-firing on a slot
            // already in Skipped is a no-op. Direct assignment is used
            // instead of `transition()` because Skipped does not
            // compare as "forward" against the other stages in a way
            // that carries operator meaning.
            (SlotStage::Skipped, EventKind::VotingSkip { slot }) if *slot == self.number => false,
            (_, EventKind::VotingSkip { slot }) if *slot == self.number => {
                self.stage = SlotStage::Skipped;
                self.entered_stage_at = now;
                true
            }
            _ => false,
        }
    }

    fn transition(&mut self, to: SlotStage, now: Instant) -> bool {
        if to > self.stage {
            self.stage = to;
            self.entered_stage_at = now;
            true
        } else {
            false
        }
    }

    /// Wall-clock age in the current stage. Used by scenes to fade
    /// out long-stationary slots or accelerate them past stale stages.
    pub fn stage_age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.entered_stage_at)
    }
}

// ---- Spinner ---------------------------------------------------------------

/// Braille spinner frames; same cell pattern Cargo uses.
const SPINNER_FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Wall-clock window over which event arrivals still count as
/// "stream live". Past this, the spinner freezes on its last frame.
pub const SPINNER_LIVE_WINDOW: Duration = Duration::from_millis(750);

/// Events per spinner frame. Each event nudges the spinner by one
/// step; 4 → calm cadence under steady streams.
pub const SPINNER_EVENTS_PER_FRAME: u64 = 4;

/// Frame count in the shared spinner table. Exposed so callers can
/// compute the same index a future frame would land on (used by tests
/// that exercise the frame-from-event-count math).
pub const SPINNER_FRAME_COUNT: usize = SPINNER_FRAMES.len();

/// Pick the current spinner glyph for the calling pane.
///
/// Honest-liveness: the spinner is event-driven, not wall-clock-driven.
/// `event_count` advances the frame index; `last_event_at` gates whether
/// any frame other than the first renders, so a stalled stream freezes
/// the cell on frame 0 rather than spinning over silent input.
pub fn spinner_frame(event_count: u64, last_event_at: Option<Instant>) -> &'static str {
    let alive =
        last_event_at.is_some_and(|t| Instant::now().duration_since(t) < SPINNER_LIVE_WINDOW);
    let idx = if alive {
        usize::try_from(event_count / SPINNER_EVENTS_PER_FRAME).unwrap_or(0) % SPINNER_FRAME_COUNT
    } else {
        0
    };
    SPINNER_FRAMES[idx]
}

// ---- Pane trait ------------------------------------------------------------

/// One visual zone of the Live tab.
///
/// A pane owns its own state (entity worlds, counters, layout) and
/// reacts to two streams: log events arriving from the tail buffer
/// (`on_event`) and frame ticks (`tick`). Rendering reads owned state
/// only; the trait makes no assumptions about composition.
///
/// Object-safe: methods take `&mut self` / `&self` and use only
/// concrete arg types. Scenes can hold `Box<dyn Pane>` directly.
pub trait Pane {
    /// Apply one log event. Called once per new event drained from the
    /// shared tail buffer. The scene driving multiple panes is
    /// responsible for the cursor / dedupe; panes themselves see each
    /// event exactly once per call.
    fn on_event(&mut self, ev: &Event);

    /// Advance the pane's simulation to `now`. Called at the engine's
    /// frame rate (~10 Hz). Panes should be idempotent under
    /// repeated `tick(now)` calls with no `on_event` in between.
    fn tick(&mut self, now: Instant);

    /// Draw the pane into `area`. Called immediately after `tick` on
    /// every frame the pane is visible. Should not mutate state.
    fn render(&self, frame: &mut Frame<'_>, area: Rect);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elapsed_world(secs: f32) -> Instant {
        Instant::now() + Duration::from_secs_f32(secs)
    }

    // ---- World mechanics ----

    #[test]
    fn world_starts_empty() {
        let w = World::new();
        assert!(w.is_empty());
        assert_eq!(w.len(), 0);
    }

    #[test]
    fn spawn_appends_until_capacity_then_evicts_oldest() {
        let mut w = World::new();
        for i in 0..(WORLD_CAPACITY + 5) {
            // Tag entities by their starting x so we can check eviction order.
            w.spawn(Entity::at(i as f32, 0.0, '*', Color::White));
        }
        assert_eq!(w.len(), WORLD_CAPACITY);
        // The first 5 should have been evicted from the front.
        let first_x = w.iter().next().unwrap().x;
        assert_eq!(first_x, 5.0);
    }

    #[test]
    fn tick_advances_position_by_velocity_times_dt() {
        let mut w = World::new();
        w.spawn(Entity::at(0.0, 0.0, '*', Color::White).with_velocity(2.0, -1.0));
        // Stamp last_tick to a known past value so dt is precise.
        w.last_tick = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        w.tick(Instant::now());
        let e = w.iter().next().unwrap();
        // Allow small floating-point slack.
        assert!((e.x - 2.0).abs() < 0.05, "x = {}", e.x);
        assert!((e.y - -1.0).abs() < 0.05, "y = {}", e.y);
    }

    #[test]
    fn tick_drops_entities_past_ttl() {
        let mut w = World::new();
        w.spawn(Entity::at(0.0, 0.0, '*', Color::White).with_ttl(Duration::from_millis(50)));
        w.spawn(Entity::at(1.0, 1.0, '#', Color::Red));
        w.last_tick = Instant::now()
            .checked_sub(Duration::from_millis(100))
            .unwrap();
        w.tick(Instant::now());
        assert_eq!(w.len(), 1);
        assert_eq!(w.iter().next().unwrap().ch, '#');
    }

    // ---- Slot transitions ----

    fn make_event(kind: EventKind) -> Event {
        Event {
            ts: time::OffsetDateTime::UNIX_EPOCH,
            kind,
        }
    }

    #[test]
    fn slot_advances_through_full_pipeline() {
        let now = Instant::now();
        let mut s = Slot::new(42, false, now);
        assert_eq!(s.stage, SlotStage::Shred);

        let advance_with = |s: &mut Slot, kind: EventKind| -> bool {
            s.advance(&make_event(kind), Instant::now())
        };

        assert!(advance_with(
            &mut s,
            EventKind::BankFrozen {
                slot: 42,
                hash: "h".into(),
                signature_count: 100,
            },
        ));
        assert_eq!(s.stage, SlotStage::Bank);
        assert_eq!(s.signature_count, Some(100));

        assert!(advance_with(
            &mut s,
            EventKind::VotingNotarize {
                slot: 42,
                hash: "h".into(),
            },
        ));
        assert_eq!(s.stage, SlotStage::Voted);

        assert!(advance_with(
            &mut s,
            EventKind::BlockNotarized {
                slot: 42,
                hash: "h".into(),
            },
        ));
        assert_eq!(s.stage, SlotStage::Notarized);

        assert!(advance_with(
            &mut s,
            EventKind::Finalized {
                slot: 42,
                hash: "h".into(),
                fast: true,
            },
        ));
        assert_eq!(s.stage, SlotStage::Finalized);
        assert_eq!(s.finalize_path, Some(FinalizePath::Fast));

        assert!(advance_with(&mut s, EventKind::NewRoot { slot: 42 }));
        assert_eq!(s.stage, SlotStage::Rooted);
    }

    #[test]
    fn slot_ignores_backward_transition() {
        let mut s = Slot::new(42, false, Instant::now());
        let now = Instant::now();
        let nev = make_event(EventKind::BlockNotarized {
            slot: 42,
            hash: "h".into(),
        });
        assert!(s.advance(&nev, now));
        assert_eq!(s.stage, SlotStage::Notarized);
        // Now an out-of-order BankFrozen arrives.
        let bev = make_event(EventKind::BankFrozen {
            slot: 42,
            hash: "h".into(),
            signature_count: 7,
        });
        // Returns true because we updated signature_count, but stage is preserved.
        s.advance(&bev, now);
        assert_eq!(s.stage, SlotStage::Notarized);
        assert_eq!(s.signature_count, Some(7));
    }

    #[test]
    fn slot_skip_is_terminal_from_any_stage() {
        let mut s = Slot::new(42, false, Instant::now());
        let now = Instant::now();
        assert!(s.advance(
            &make_event(EventKind::VotingNotarize {
                slot: 42,
                hash: "h".into(),
            }),
            now,
        ));
        assert_eq!(s.stage, SlotStage::Voted);
        assert!(s.advance(&make_event(EventKind::VotingSkip { slot: 42 }), now,));
        assert_eq!(s.stage, SlotStage::Skipped);
    }

    #[test]
    fn slot_ignores_events_for_other_slots() {
        let mut s = Slot::new(42, false, Instant::now());
        let now = Instant::now();
        let ev = make_event(EventKind::BankFrozen {
            slot: 99,
            hash: "h".into(),
            signature_count: 1,
        });
        assert!(!s.advance(&ev, now));
        assert_eq!(s.stage, SlotStage::Shred);
        assert!(s.signature_count.is_none());
    }

    #[test]
    fn slow_finalize_path_recorded() {
        let mut s = Slot::new(42, false, Instant::now());
        let now = Instant::now();
        assert!(s.advance(
            &make_event(EventKind::Finalized {
                slot: 42,
                hash: "h".into(),
                fast: false,
            }),
            now,
        ));
        assert_eq!(s.finalize_path, Some(FinalizePath::Slow));
    }

    // Reference `elapsed_world` so it doesn't fall out as dead code.
    #[allow(dead_code)]
    fn _ref_elapsed() -> Instant {
        elapsed_world(1.0)
    }

    // ---- Spinner ----

    #[test]
    fn spinner_freezes_when_no_event_ever_seen() {
        // No `last_event_at` → spinner pins to frame 0 regardless of
        // event_count. Renders a stable cell on the cold-start path.
        let f0 = spinner_frame(0, None);
        let f1 = spinner_frame(999, None);
        assert_eq!(f0, f1);
    }

    #[test]
    fn spinner_freezes_when_stale_past_live_window() {
        // Stale `last_event_at` → spinner pins to frame 0 even if
        // events were observed in the past. Idle-state guarantee.
        let stale = Instant::now()
            .checked_sub(SPINNER_LIVE_WINDOW + Duration::from_millis(50))
            .unwrap();
        let stale_frame = spinner_frame(SPINNER_EVENTS_PER_FRAME * 3, Some(stale));
        let fresh_frame_0 = spinner_frame(0, Some(Instant::now()));
        assert_eq!(stale_frame, fresh_frame_0);
    }

    #[test]
    fn spinner_advances_one_step_per_events_per_frame() {
        // Within the live window, the frame index advances by one
        // every `SPINNER_EVENTS_PER_FRAME` events. Verify two adjacent
        // bands map to different frames.
        let now = Some(Instant::now());
        let f0 = spinner_frame(0, now);
        let f1 = spinner_frame(SPINNER_EVENTS_PER_FRAME, now);
        let f2 = spinner_frame(SPINNER_EVENTS_PER_FRAME * 2, now);
        assert_ne!(f0, f1);
        assert_ne!(f1, f2);
        // Same band → same frame.
        assert_eq!(f0, spinner_frame(SPINNER_EVENTS_PER_FRAME - 1, now));
    }
}
