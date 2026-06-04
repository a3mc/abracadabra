//! Cannon-particle system for the chain pane visualization.
//!
//! Each new slot the pane observes spawns one particle at the cannon
//! (top-left of the pane). Particles fly across the canvas to a
//! landing zone, then the slot's identity is appended to the matrix
//! ring buffer. The matrix renderer looks up each slot's current
//! `SlotState` and chooses glyph + colour from the classifier in
//! [`super::glyph`] (added in step 2 of the rebuild).
//!
//! **Coordinates.** World-space is normalised `[0.0, 1.0]` on both
//! axes so the system is layout-agnostic: the render path scales
//! `(x, y)` to the inner area's cell rect. Cannon sits near the
//! top-left (`(CANNON_X, CANNON_Y)`); particles travel toward
//! `(MATRIX_CENTRE_X, MATRIX_CENTRE_Y)` — a single canonical landing
//! point for the spike. Step 2 will vary per-particle target columns
//! so the eye can follow each slot to its specific cell.
//!
//! **Spawn dedupe.** A `HashSet<u64>` tracks slots that have already
//! fired so duplicate events (e.g. `Block` then `Finalized` for the
//! same slot, or `FirstShred` then `Block`) do not double-spawn.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

/// World-space cannon position (normalised, top-left origin).
pub(super) const CANNON_X: f32 = 0.04;
pub(super) const CANNON_Y: f32 = 0.15;

/// World-space landing point — middle of the matrix area for the
/// spike. Step 2 will replace this with a per-particle target column
/// so the trajectories fan out.
const MATRIX_CENTRE_X: f32 = 0.55;
const MATRIX_CENTRE_Y: f32 = 0.55;

/// How long a particle spends in flight before landing. ~700 ms keeps
/// the motion legible at 30 fps (≈21 frames) without feeling sluggish
/// against Solana's 400 ms slot cadence.
const FLIGHT_DURATION: Duration = Duration::from_millis(700);

/// Maximum number of slot identities the landing matrix retains.
/// Sized for a typical 60-col × 5-row matrix; the render path clips
/// to whatever area the layout grants and the rest is held in reserve
/// so a window resize does not erase visible history.
pub(super) const MATRIX_CAPACITY: usize = 320;

/// Soft cap on in-flight particles. If a burst exceeds this we drop
/// the oldest — particles in flight are visual only, so losing one
/// is preferable to unbounded memory growth.
const MAX_IN_FLIGHT: usize = 128;

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
    /// Slot identities that have already landed in the matrix, oldest
    /// first. Render looks each slot up in [`super::state::ChainPane`]
    /// to choose its glyph and style.
    pub(super) matrix: VecDeque<u64>,
    fired_slots: HashSet<u64>,
    last_tick: Instant,
}

impl CannonSystem {
    pub(super) fn new() -> Self {
        Self {
            particles: Vec::with_capacity(32),
            matrix: VecDeque::with_capacity(MATRIX_CAPACITY),
            fired_slots: HashSet::with_capacity(MATRIX_CAPACITY),
            last_tick: Instant::now(),
        }
    }

    /// Spawn one particle for `slot` if it has not been fired yet.
    /// Returns `true` if a new particle was launched.
    pub(super) fn fire(&mut self, slot: u64) -> bool {
        if !self.fired_slots.insert(slot) {
            return false;
        }
        if self.particles.len() >= MAX_IN_FLIGHT {
            // Drop the oldest in-flight particle — its slot still
            // gets a matrix cell on the next tick that lands.
            self.particles.remove(0);
        }
        let ttl_secs = FLIGHT_DURATION.as_secs_f32();
        let vx = (MATRIX_CENTRE_X - CANNON_X) / ttl_secs;
        let vy = (MATRIX_CENTRE_Y - CANNON_Y) / ttl_secs;
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

    /// Advance every particle by `(now - last_tick)`. Particles whose
    /// age has reached `ttl` are appended to the landing matrix and
    /// dropped from the in-flight set.
    pub(super) fn tick(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;
        for p in &mut self.particles {
            p.x = p.vx.mul_add(dt, p.x);
            p.y = p.vy.mul_add(dt, p.y);
        }
        // Two-step partition: capture landed slots in order, then
        // retain only the still-in-flight ones. Preserves insertion
        // order in the matrix without an allocator-thrash sort.
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
            if self.matrix.len() >= MATRIX_CAPACITY {
                let evicted = self.matrix.pop_front();
                // Allow the evicted slot to be re-fired if the chain
                // re-encounters it (unlikely with the upstream prune,
                // but the semantic is "matrix and fired-set evict
                // together").
                if let Some(s) = evicted {
                    self.fired_slots.remove(&s);
                }
            }
            self.matrix.push_back(slot);
        }
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
    fn ttl_expiry_moves_slot_to_matrix() {
        let mut sys = CannonSystem::new();
        sys.fire(100);
        // Force the particle past its TTL by advancing `now`.
        let past_ttl = Instant::now() + FLIGHT_DURATION + Duration::from_millis(10);
        sys.tick(past_ttl);
        assert!(sys.particles.is_empty(), "expired particle must despawn");
        assert_eq!(sys.matrix.back().copied(), Some(100));
    }

    #[test]
    fn matrix_capacity_evicts_oldest() {
        let mut sys = CannonSystem::new();
        for slot in 0..MATRIX_CAPACITY as u64 + 5 {
            sys.fire(slot);
            sys.tick(Instant::now() + FLIGHT_DURATION + Duration::from_millis(1));
        }
        assert_eq!(sys.matrix.len(), MATRIX_CAPACITY);
        assert_eq!(sys.matrix.front().copied(), Some(5));
        assert_eq!(sys.matrix.back().copied(), Some(MATRIX_CAPACITY as u64 + 4));
    }
}
