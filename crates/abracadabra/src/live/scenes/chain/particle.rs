//! Cannon-particle system for the chain pane visualization.
//!
//! Each new slot the pane observes spawns one particle at the cannon
//! position (anchored to the header's `▶` glyph). Particles fly to a
//! landing point in the bucket area, then the slot identity is
//! appended to the **paged bucket** ring.
//!
//! **Paged bucket.** The bucket holds exactly [`PAGE_CAPACITY`] = 100
//! slots arranged in a fixed grid. Once a slot lands at a cell that
//! cell **never moves** until the page completes. When the 100th slot
//! lands the system starts a **magic wipe** animation (left-to-right
//! sweep, ~500 ms) that flashes each column white then clears it.
//! After the wipe completes the bucket is empty and the next page
//! begins from cell 0.
//!
//! The fixed-position design was deliberate: the previous sliding
//! window with eviction caused every visible cell to shift one slot
//! per arrival — the eye reads it as the whole grid moving, even
//! though only one cell really changed. Paged static positions let
//! the eye lock onto the grid and absorb the per-slot signal one
//! cell at a time.
//!
//! **Coordinates.** World-space is normalised `[0.0, 1.0]` on both
//! axes so the system is layout-agnostic. The render path scales
//! `(x, y)` to the inner area's cell rect.
//!
//! **Spawn dedupe.** A `HashSet<u64>` tracks slots fired during the
//! current page so duplicate events for the same slot don't double-
//! spawn. The set clears on each wipe.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

/// World-space cannon position (normalised, top-left origin).
/// Particles spawn at the **top of the viz area** (the cannon `▼`
/// glyph itself is rendered in a layout row directly above viz, so
/// the spawn point sits flush under the cannon visually).
pub(super) const CANNON_X: f32 = 0.50;
pub(super) const CANNON_Y: f32 = 0.0;

/// World-space landing-cluster centre. Bucket is bottom-aligned in
/// viz, so the cluster centre sits near the top of the bucket area
/// (~75% down the viz rect for the default 25×8 bucket inside a
/// ~10-row viz). Per-particle horizontal jitter spreads trajectories
/// so a burst doesn't collapse into one column.
const LANDING_X: f32 = 0.50;
const LANDING_Y: f32 = 0.70;

/// Maximum normalised horizontal offset added to a particle's
/// landing target. Slot-deterministic so the same slot always takes
/// the same path, but distinct slots fan out across the bucket
/// width. ±0.18 keeps the fan well inside the bucket area.
const LANDING_X_JITTER: f32 = 0.18;

/// How long a particle spends in flight before landing. ~700 ms keeps
/// the motion legible at 30 fps (~21 frames) without feeling sluggish
/// against Solana's 400 ms slot cadence.
const FLIGHT_DURATION: Duration = Duration::from_millis(700);

/// Slots per bucket page. Bumped to 200 in LIVE-54 to fill the
/// vertical space the half-pane layout grants — 100 cells in a
/// 25×4 grid left a large empty bottom margin. The 25×8 = 200 grid
/// fits comfortably while keeping the wipe cadence visible
/// (200 × ~0.4 s slot rate ≈ 80 s per page).
pub(super) const PAGE_CAPACITY: usize = 200;

/// Magic-wipe sweep duration. Long enough for the column-by-column
/// flash to read as a wave; short enough that the next page can
/// start before the next slot event arrives.
const WIPE_DURATION: Duration = Duration::from_millis(500);

/// Soft cap on in-flight particles. Excess drops the oldest in
/// flight — particles are visual only, losing one is preferable to
/// unbounded memory growth. Sized at twice [`PAGE_CAPACITY`] so a
/// burst large enough to fill a whole page in one tick can still
/// fly every particle without trimming the head.
const MAX_IN_FLIGHT: usize = PAGE_CAPACITY * 2;

/// One in-flight slot marker.
#[derive(Debug, Clone, Copy)]
pub(super) struct ChainParticle {
    pub(super) slot: u64,
    /// Normalised world-space position, both axes in `[0.0, 1.0]`.
    pub(super) x: f32,
    pub(super) y: f32,
    /// Normalised velocity (units per second).
    vx: f32,
    vy: f32,
    born: Instant,
    ttl: Duration,
}

#[derive(Debug)]
pub(super) struct CannonSystem {
    pub(super) particles: Vec<ChainParticle>,
    /// Landed slots for the **current page**, oldest first. Capped
    /// at [`PAGE_CAPACITY`]; reaching the cap triggers a wipe.
    pub(super) bucket: VecDeque<u64>,
    /// Slots that landed while a wipe was in progress. Applied to
    /// the next page once the wipe completes — keeps the new-page
    /// alignment honest even if particle TTL races the wipe.
    pending_landed: Vec<u64>,
    /// `Some(start_instant)` while the magic-wipe animation is in
    /// progress. `None` when the bucket is in normal fill mode.
    pub(super) wipe_started_at: Option<Instant>,
    /// Slots fired in the current page — dedupe within a page only.
    /// Cleared on every wipe so the same slot can re-fire if it
    /// reappears in a later page (very rare; the upstream prune
    /// usually drops re-occurring slots before that).
    fired_slots: HashSet<u64>,
    last_tick: Instant,
}

impl CannonSystem {
    pub(super) fn new() -> Self {
        Self {
            particles: Vec::with_capacity(32),
            bucket: VecDeque::with_capacity(PAGE_CAPACITY),
            pending_landed: Vec::with_capacity(8),
            wipe_started_at: None,
            fired_slots: HashSet::with_capacity(PAGE_CAPACITY * 2),
            last_tick: Instant::now(),
        }
    }

    /// Spawn one particle for `slot` if it has not been fired in the
    /// current page yet. Returns `true` if a new particle was launched.
    pub(super) fn fire(&mut self, slot: u64) -> bool {
        if !self.fired_slots.insert(slot) {
            return false;
        }
        if self.particles.len() >= MAX_IN_FLIGHT {
            // Drop the oldest in-flight particle — its slot still
            // gets a bucket cell on the next tick that lands.
            self.particles.remove(0);
        }
        let ttl_secs = FLIGHT_DURATION.as_secs_f32();
        // Slot-deterministic horizontal jitter so different slots
        // fan out across the bucket width instead of stacking on
        // the same column. A prime modulus gives reasonable spread
        // across consecutive slots without needing a real RNG
        // (which is unavailable from the Pane trait anyway).
        #[allow(clippy::cast_precision_loss)]
        let jitter_unit = (slot % 17) as f32 / 17.0 - 0.5;
        let target_x = (jitter_unit * LANDING_X_JITTER)
            .mul_add(2.0, LANDING_X)
            .clamp(0.0, 1.0);
        let vx = (target_x - CANNON_X) / ttl_secs;
        let vy = (LANDING_Y - CANNON_Y) / ttl_secs;
        self.particles.push(ChainParticle {
            slot,
            x: CANNON_X,
            y: CANNON_Y,
            vx,
            vy,
            born: Instant::now(),
            ttl: FLIGHT_DURATION,
        });
        true
    }

    /// Advance every particle by `(now - last_tick)`, drive the
    /// page lifecycle (fill → wipe → reset), and append expired
    /// particles to the bucket (or queue them if a wipe is active).
    pub(super) fn tick(&mut self, now: Instant) {
        // 1. Complete any in-progress wipe whose duration has
        //    elapsed. After the clear, drain any slots that landed
        //    during the wipe into the new page.
        if let Some(start) = self.wipe_started_at {
            if now.saturating_duration_since(start) >= WIPE_DURATION {
                self.bucket.clear();
                self.fired_slots.clear();
                self.wipe_started_at = None;
                // Move slots that arrived during the wipe into the
                // fresh page. `pending_landed` is drained — re-firing
                // would be wrong because the particles already flew.
                for slot in self.pending_landed.drain(..) {
                    self.fired_slots.insert(slot);
                    self.bucket.push_back(slot);
                }
            }
        }

        // 2. Advance particle positions.
        let dt = now.saturating_duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;
        for p in &mut self.particles {
            p.x = p.vx.mul_add(dt, p.x);
            p.y = p.vy.mul_add(dt, p.y);
        }

        // 3. Land expired particles. During an active wipe, queue
        //    them in `pending_landed` so they enter the FRESH page
        //    after the wipe completes (rather than crowding the
        //    page that just finished).
        let mut landed: Vec<u64> = Vec::new();
        self.particles.retain(|p| {
            if now.saturating_duration_since(p.born) >= p.ttl {
                landed.push(p.slot);
                false
            } else {
                true
            }
        });
        for slot in landed {
            if self.wipe_started_at.is_some() {
                self.pending_landed.push(slot);
            } else {
                self.bucket.push_back(slot);
            }
        }

        // 4. Start a wipe if the bucket just filled.
        if self.bucket.len() >= PAGE_CAPACITY && self.wipe_started_at.is_none() {
            self.wipe_started_at = Some(now);
        }
    }

    /// Wipe progress in `[0.0, 1.0]` when active. `None` when no
    /// wipe is in progress. Used by the matrix renderer to drive the
    /// left-to-right sweep flash.
    pub(super) fn wipe_progress(&self, now: Instant) -> Option<f32> {
        let start = self.wipe_started_at?;
        let elapsed = now.saturating_duration_since(start).as_secs_f32();
        let total = WIPE_DURATION.as_secs_f32();
        Some((elapsed / total).clamp(0.0, 1.0))
    }
}

impl Default for CannonSystem {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn fire_inserts_one_particle_per_slot() {
        let mut sys = CannonSystem::new();
        assert!(sys.fire(100));
        assert!(
            !sys.fire(100),
            "duplicate fire on same slot must be a no-op"
        );
        assert!(sys.fire(101));
        assert_eq!(sys.particles.len(), 2);
    }

    #[test]
    fn tick_advances_particle_position() {
        let mut sys = CannonSystem::new();
        sys.fire(100);
        let start = sys.particles[0].x;
        sleep(Duration::from_millis(20));
        sys.tick(Instant::now());
        assert!(
            sys.particles[0].x > start,
            "tick should advance x toward landing"
        );
    }

    #[test]
    fn ttl_expiry_moves_slot_into_bucket() {
        let mut sys = CannonSystem::new();
        sys.fire(100);
        let past_ttl = Instant::now() + FLIGHT_DURATION + Duration::from_millis(10);
        sys.tick(past_ttl);
        assert!(sys.particles.is_empty(), "expired particle must despawn");
        assert_eq!(sys.bucket.back().copied(), Some(100));
        assert!(
            sys.wipe_started_at.is_none(),
            "single landing must not trigger a wipe"
        );
    }

    #[test]
    fn filling_page_capacity_triggers_wipe() {
        let mut sys = CannonSystem::new();
        let past_ttl = Instant::now() + FLIGHT_DURATION + Duration::from_millis(10);
        for slot in 0..PAGE_CAPACITY as u64 {
            sys.fire(slot);
        }
        sys.tick(past_ttl);
        assert_eq!(sys.bucket.len(), PAGE_CAPACITY);
        assert!(
            sys.wipe_started_at.is_some(),
            "100th landing must start a wipe"
        );
    }

    #[test]
    fn wipe_completion_clears_bucket_and_applies_pending() {
        let mut sys = CannonSystem::new();
        // Land 100 slots → wipe triggers.
        let landing = Instant::now() + FLIGHT_DURATION + Duration::from_millis(10);
        for slot in 0..PAGE_CAPACITY as u64 {
            sys.fire(slot);
        }
        sys.tick(landing);
        assert!(sys.wipe_started_at.is_some());

        // Fire one more slot and land it during the wipe.
        sys.fire(999);
        let mid_wipe = landing + Duration::from_millis(50);
        sys.tick(mid_wipe);
        // Particle from slot 999 is still in flight; only the 100
        // pre-existing landed slots have been touched.
        let after_landing_999 = landing + FLIGHT_DURATION + Duration::from_millis(60);
        sys.tick(after_landing_999);
        // Wipe should have completed and slot 999 entered the fresh
        // page (the wipe duration is 500 ms < landing-to-landing
        // time so by the time slot 999 lands the wipe is done).
        assert!(sys.wipe_started_at.is_none(), "wipe should have completed");
        assert_eq!(
            sys.bucket.back().copied(),
            Some(999),
            "slot landing post-wipe enters fresh page"
        );
    }

    #[test]
    fn wipe_progress_advances_linearly() {
        let mut sys = CannonSystem::new();
        let landing = Instant::now() + FLIGHT_DURATION + Duration::from_millis(10);
        for slot in 0..PAGE_CAPACITY as u64 {
            sys.fire(slot);
        }
        sys.tick(landing);
        let start = sys.wipe_started_at.expect("wipe should be active");
        // Halfway through.
        let half = start + WIPE_DURATION / 2;
        let p = sys.wipe_progress(half).expect("progress while active");
        assert!(
            (p - 0.5).abs() < 0.05,
            "halfway progress should be ~0.5: {p}"
        );
    }
}
