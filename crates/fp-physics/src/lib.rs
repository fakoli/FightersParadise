//! # fp-physics
//!
//! Physics simulation for the Fighters Paradise engine. Handles gravity,
//! velocity integration, and ground plane detection for character movement.
//!
//! # Coordinate Convention
//!
//! - Y = 0 is the ground plane
//! - Y < 0 is airborne (above ground)
//! - Gravity is a positive value that pulls Y toward 0
//! - Horizontal X uses standard left-negative, right-positive
//!
//! # Collision detection
//!
//! AABB collision for MUGEN Clsn boxes lives in the [`collision`] module and is
//! re-exported here: [`Clsn`], [`Facing`], [`rects_overlap`], [`place_clsn`],
//! [`any_overlap`], and [`any_clsn_overlap`].
//!
//! # Player push and stage bounds
//!
//! Horizontal character-interaction geometry (MUGEN `PlayerPush` / `ScreenBound`) lives
//! in the [`push`] module and is re-exported here: [`PushBody`], [`PushResolution`],
//! [`resolve_push`], [`push_bodies`], [`clamp_to_bounds`], and [`clamp_body_to_bounds`].

#![warn(missing_docs)]

use fp_core::Vec2;

pub mod collision;
pub mod push;

pub use collision::{
    any_clsn_overlap, any_overlap, place_clsn, rects_overlap, Clsn, Facing,
};
pub use push::{
    clamp_body_to_bounds, clamp_to_bounds, push_bodies, resolve_push, PushBody, PushResolution,
};

/// Default gravity acceleration matching MUGEN's standard value.
///
/// Applied as positive Y acceleration (pulls airborne characters toward Y=0 ground).
pub const DEFAULT_GRAVITY: f32 = 0.44;

/// A physics body with position, velocity, and acceleration.
///
/// Uses simple Euler integration. The ground plane is at Y=0; the body
/// is clamped so that `pos.y` never exceeds 0 (i.e., it lands when
/// reaching ground level).
///
/// # Examples
///
/// ```
/// use fp_physics::PhysicsBody;
///
/// let mut body = PhysicsBody::new(100.0, 0.0);
/// body.vel.y = -8.0; // jump upward
/// body.apply_gravity(0.44);
/// for _ in 0..60 {
///     body.step();
/// }
/// assert!(body.on_ground());
/// ```
pub struct PhysicsBody {
    /// Current position. Y=0 is ground, Y<0 is airborne.
    pub pos: Vec2<f32>,
    /// Current velocity (pixels per tick).
    pub vel: Vec2<f32>,
    /// Current acceleration (pixels per tick per tick).
    pub accel: Vec2<f32>,
}

impl PhysicsBody {
    /// Creates a new physics body at the given position with zero velocity.
    pub fn new(x: f32, y: f32) -> Self {
        Self {
            pos: Vec2::new(x, y),
            vel: Vec2::<f32>::ZERO,
            accel: Vec2::<f32>::ZERO,
        }
    }

    /// Advance one tick: apply acceleration to velocity, then velocity to position.
    ///
    /// After integration, clamps `pos.y` to be <= 0 (ground plane).
    /// If the body lands (was airborne, now at ground), velocity.y and accel.y are zeroed.
    pub fn step(&mut self) {
        self.vel += self.accel;
        self.pos += self.vel;

        // Ground clamp: Y=0 is ground, Y<0 is airborne
        if self.pos.y > 0.0 {
            self.pos.y = 0.0;
            self.vel.y = 0.0;
            self.accel.y = 0.0;
        }
    }

    /// Returns `true` if the body is on the ground (Y >= 0).
    pub fn on_ground(&self) -> bool {
        self.pos.y >= 0.0
    }

    /// Returns `true` if the body is airborne (Y < 0).
    pub fn in_air(&self) -> bool {
        self.pos.y < 0.0
    }

    /// Sets vertical acceleration to the given gravity value.
    ///
    /// In MUGEN's coordinate system, gravity is positive (pulls toward Y=0 ground).
    pub fn apply_gravity(&mut self, gravity: f32) {
        self.accel.y = gravity;
    }

    /// Immediately land the body: reset Y position, vertical velocity, and vertical acceleration.
    pub fn land(&mut self) {
        self.pos.y = 0.0;
        self.vel.y = 0.0;
        self.accel.y = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_applies_velocity() {
        let mut body = PhysicsBody::new(0.0, 0.0);
        body.vel.x = 2.0;
        body.step();
        assert!((body.pos.x - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn step_applies_acceleration() {
        let mut body = PhysicsBody::new(0.0, -10.0); // airborne
        body.accel.x = 1.0;
        body.step();
        assert!((body.vel.x - 1.0).abs() < f32::EPSILON);
        body.step();
        assert!((body.vel.x - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn ground_clamp() {
        let mut body = PhysicsBody::new(0.0, -1.0); // just above ground
        body.vel.y = 5.0; // moving toward ground fast
        body.step();
        assert!((body.pos.y).abs() < f32::EPSILON); // clamped to 0
        assert!((body.vel.y).abs() < f32::EPSILON); // vel zeroed
    }

    #[test]
    fn ground_predicates() {
        let ground = PhysicsBody::new(0.0, 0.0);
        assert!(ground.on_ground());
        assert!(!ground.in_air());

        let air = PhysicsBody::new(0.0, -5.0);
        assert!(!air.on_ground());
        assert!(air.in_air());
    }

    #[test]
    fn gravity_integration() {
        let mut body = PhysicsBody::new(0.0, -100.0); // high up
        body.apply_gravity(DEFAULT_GRAVITY);
        for _ in 0..30 {
            body.step();
        }
        // After 30 ticks with gravity 0.44, body should have moved significantly
        assert!(body.pos.y > -100.0); // moved toward ground
    }

    #[test]
    fn jump_arc() {
        let mut body = PhysicsBody::new(0.0, 0.0);
        body.vel.y = -8.0; // jump
        body.apply_gravity(DEFAULT_GRAVITY);

        let mut was_airborne = false;
        for _ in 0..100 {
            body.step();
            if body.in_air() {
                was_airborne = true;
            }
            if was_airborne && body.on_ground() {
                break;
            }
        }
        assert!(was_airborne);
        assert!(body.on_ground());
    }

    #[test]
    fn land_resets() {
        let mut body = PhysicsBody::new(0.0, -5.0);
        body.vel.y = 3.0;
        body.accel.y = 0.44;
        body.land();
        assert!((body.pos.y).abs() < f32::EPSILON);
        assert!((body.vel.y).abs() < f32::EPSILON);
        assert!((body.accel.y).abs() < f32::EPSILON);
    }

    #[test]
    fn horizontal_movement() {
        let mut body = PhysicsBody::new(0.0, 0.0);
        body.vel.x = 3.0;
        for _ in 0..10 {
            body.step();
        }
        assert!((body.pos.x - 30.0).abs() < f32::EPSILON);
        assert!(body.on_ground()); // Y unchanged
    }
}
