//! Horizontal player-push and stage-bound geometry for character interaction.
//!
//! This module provides the *geometric core* that Phase 6/7 character interaction
//! builds on (MUGEN's `PlayerPush` and `ScreenBound` state controllers). It deals
//! purely with 1-D horizontal geometry on the ground plane — there is no game state,
//! [`HitDef`], round, or velocity logic here (that lives in `fp-combat` / `fp-engine`).
//!
//! # The character body model
//!
//! MUGEN gives each character a *width* on the ground via `size.ground.front` and
//! `size.ground.back` (in the `.cns` constants). These are **facing-relative**
//! distances from the character's axis (its center X):
//!
//! - `front` extends in the direction the character faces,
//! - `back` extends behind the character.
//!
//! A [`PushBody`] captures this: a center X plus a `front`/`back` half-width and a
//! [`Facing`]. The body's world-space extent on the X axis is `[left, right]` where the
//! `front` half-width is placed on the facing side and `back` on the other (see
//! [`PushBody::left`] / [`PushBody::right`]).
//!
//! # Coordinate convention
//!
//! Consistent with the rest of `fp-physics`: X uses standard left-negative,
//! right-positive. Player push and bound clamping are purely horizontal, so the Y
//! (Y-down) convention used elsewhere in the crate does not come into play here.
//!
//! # What the two operations do
//!
//! - [`resolve_push`] / [`push_bodies`]: given two bodies that overlap horizontally,
//!   push them apart along X until they *just touch*, splitting the correction evenly
//!   (each body moves half the overlap). Non-overlapping bodies are left unchanged.
//! - [`clamp_to_bounds`] / [`clamp_body_to_bounds`]: clamp a center X so the body's
//!   extent stays within a stage's `[left, right]` bounds (MUGEN `ScreenBound`).
//!
//! All functions are pure, deterministic, and never panic.

use crate::collision::Facing;

/// A character's horizontal body on the ground, used for player-push and bound clamping.
///
/// The body is described by its center X (the character's axis) plus two
/// **facing-relative** half-widths matching MUGEN's `size.ground.front` and
/// `size.ground.back`:
///
/// - `front` is the distance from the axis to the body edge on the side the character
///   *faces*,
/// - `back` is the distance from the axis to the body edge *behind* the character.
///
/// When [`Facing::Right`], `front` lies on the `+X` side and `back` on the `-X` side;
/// when [`Facing::Left`] the two are mirrored. Use [`PushBody::left`] / [`PushBody::right`]
/// to get the resolved world-space extent.
///
/// Half-widths are normally non-negative; negative values are not rejected (the type is
/// pure geometry), but they simply shrink the corresponding side. See [`PushBody::left`].
///
/// # Examples
///
/// ```
/// use fp_physics::{Facing, PushBody};
///
/// // Axis at x=100, reaches 15px in front and 12px behind, facing right.
/// let body = PushBody::new(100.0, 15.0, 12.0, Facing::Right);
/// assert_eq!(body.left(), 88.0);  // back side (-X) when facing right
/// assert_eq!(body.right(), 115.0); // front side (+X) when facing right
///
/// // Same widths, facing left: front/back swap sides about the axis.
/// let mirrored = PushBody::new(100.0, 15.0, 12.0, Facing::Left);
/// assert_eq!(mirrored.left(), 85.0);
/// assert_eq!(mirrored.right(), 112.0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PushBody {
    /// Center X of the body (the character's axis).
    pub center: f32,
    /// Facing-relative half-width on the side the character faces (`size.ground.front`).
    pub front: f32,
    /// Facing-relative half-width behind the character (`size.ground.back`).
    pub back: f32,
    /// Which way the character faces, deciding which side `front`/`back` apply to.
    pub facing: Facing,
}

impl PushBody {
    /// Creates a body from a center X, facing-relative `front`/`back` half-widths, and facing.
    pub const fn new(center: f32, front: f32, back: f32, facing: Facing) -> Self {
        Self {
            center,
            front,
            back,
            facing,
        }
    }

    /// Returns the half-width on the `+X` (right) side of the axis for this facing.
    ///
    /// `front` when [`Facing::Right`], `back` when [`Facing::Left`].
    fn right_half_width(self) -> f32 {
        match self.facing {
            Facing::Right => self.front,
            Facing::Left => self.back,
        }
    }

    /// Returns the half-width on the `-X` (left) side of the axis for this facing.
    ///
    /// `back` when [`Facing::Right`], `front` when [`Facing::Left`].
    fn left_half_width(self) -> f32 {
        match self.facing {
            Facing::Right => self.back,
            Facing::Left => self.front,
        }
    }

    /// Returns the world-space left edge of the body (`center - left half-width`).
    ///
    /// The left half-width is `back` when facing right and `front` when facing left.
    /// If the relevant half-width is negative the "edge" can lie on the other side of
    /// the axis; this is pure geometry and is not treated as an error.
    pub fn left(self) -> f32 {
        self.center - self.left_half_width()
    }

    /// Returns the world-space right edge of the body (`center + right half-width`).
    ///
    /// The right half-width is `front` when facing right and `back` when facing left.
    pub fn right(self) -> f32 {
        self.center + self.right_half_width()
    }
}

/// Adjusted center positions returned by [`resolve_push`].
///
/// `a` / `b` are the new center X values for the two input bodies, in the same order
/// they were passed. When the bodies did not overlap, these equal the inputs' centers
/// and [`PushResolution::pushed`] is `false`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PushResolution {
    /// New center X for the first body.
    pub a: f32,
    /// New center X for the second body.
    pub b: f32,
    /// `true` if the bodies overlapped and were separated; `false` if left unchanged.
    pub pushed: bool,
}

/// Resolves a horizontal overlap between two character bodies by pushing them apart.
///
/// Models MUGEN's mutual `PlayerPush`: if the two bodies overlap along X, they are
/// separated until their facing edges *just touch*, with the correction **split evenly**
/// — each center moves by half the overlap, in opposite directions. The leftmost body
/// (smaller center) moves further left and the rightmost moves further right; ties
/// (equal centers) push `a` left and `b` right deterministically.
///
/// # Overlap definition
///
/// Overlap is the signed gap `min(right edges) - max(left edges)` between the bodies'
/// world extents (see [`PushBody::left`] / [`PushBody::right`]). A **strictly positive**
/// overlap triggers a push; an overlap of `<= 0` (disjoint or merely edge-touching) is
/// left unchanged, consistent with the strict edge-touch convention used by
/// [`crate::rects_overlap`].
///
/// # Result
///
/// Returns a [`PushResolution`] with the adjusted center X of each body (same order as
/// the arguments) and a `pushed` flag. Only the centers move; widths and facing are
/// unchanged, so the caller can rebuild bodies if needed. After a push the bodies are
/// just-touching: the overlap becomes zero (within floating-point rounding).
///
/// Pure, deterministic, never panics. Non-finite inputs propagate as non-finite outputs
/// rather than panicking.
///
/// # Examples
///
/// ```
/// use fp_physics::{resolve_push, Facing, PushBody};
///
/// // Two bodies, half-width 10 each, centers 5 apart -> extents 0..20 and 5..25.
/// // Overlap is 20 - 5 = 15... here a simpler symmetric case:
/// let a = PushBody::new(0.0, 10.0, 10.0, Facing::Right);  // extent -10..10
/// let b = PushBody::new(6.0, 10.0, 10.0, Facing::Left);   // extent -4..16
/// // Overlap = min(10,16) - max(-10,-4) = 10 - (-4) = 14.
/// let r = resolve_push(a, b);
/// assert!(r.pushed);
/// // Each moves half the overlap (7) outward: a -> -7, b -> 13.
/// assert!((r.a - -7.0).abs() < 1e-6);
/// assert!((r.b - 13.0).abs() < 1e-6);
///
/// // Far apart: unchanged.
/// let far = resolve_push(
///     PushBody::new(0.0, 5.0, 5.0, Facing::Right),
///     PushBody::new(100.0, 5.0, 5.0, Facing::Left),
/// );
/// assert!(!far.pushed);
/// assert_eq!((far.a, far.b), (0.0, 100.0));
/// ```
pub fn resolve_push(a: PushBody, b: PushBody) -> PushResolution {
    // Horizontal overlap of the two world extents. Positive => they interpenetrate.
    let overlap = a.right().min(b.right()) - a.left().max(b.left());

    // Only a strictly positive overlap triggers a push. Using the positive condition
    // (rather than negating `<= 0.0`) keeps the NaN case in the `else` branch — a
    // non-finite overlap leaves both bodies unchanged instead of panicking.
    if overlap > 0.0 {
        let half = overlap * 0.5;

        // Decide push direction from center ordering so the pair separates rather than
        // swapping sides. Equal centers deterministically push `a` left and `b` right.
        let (new_a, new_b) = if a.center <= b.center {
            (a.center - half, b.center + half)
        } else {
            (a.center + half, b.center - half)
        };

        PushResolution {
            a: new_a,
            b: new_b,
            pushed: true,
        }
    } else {
        // Disjoint, edge-touching, or non-finite: leave both bodies exactly where they are.
        PushResolution {
            a: a.center,
            b: b.center,
            pushed: false,
        }
    }
}

/// Returns the signed center-X delta to apply to each body to separate them.
///
/// Convenience wrapper over [`resolve_push`] returning *deltas* instead of absolute
/// positions: `(delta_a, delta_b)` such that `a.center + delta_a` and `b.center + delta_b`
/// are the just-touching positions. When the bodies do not overlap both deltas are `0.0`.
///
/// The deltas are equal in magnitude and opposite in sign (each is half the overlap),
/// reflecting the even split. Pure, deterministic, never panics.
///
/// # Examples
///
/// ```
/// use fp_physics::{push_bodies, Facing, PushBody};
///
/// let a = PushBody::new(0.0, 10.0, 10.0, Facing::Right); // -10..10
/// let b = PushBody::new(6.0, 10.0, 10.0, Facing::Left);  // -4..16, overlap 14
/// let (da, db) = push_bodies(a, b);
/// assert!((da - -7.0).abs() < 1e-6);
/// assert!((db -  7.0).abs() < 1e-6);
/// ```
pub fn push_bodies(a: PushBody, b: PushBody) -> (f32, f32) {
    let r = resolve_push(a, b);
    (r.a - a.center, r.b - b.center)
}

/// Clamps a body's center X so its world extent stays within `[left, right]`.
///
/// Implements MUGEN `ScreenBound`-style clamping for a body described by a center X and
/// `half_left` / `half_right` distances from that center to the body's left and right
/// edges (already resolved for facing — see [`clamp_body_to_bounds`] for the
/// facing-aware entry point).
///
/// # Edge behavior
///
/// - **Fully inside:** if `[center - half_left, center + half_right]` lies within
///   `[left, right]`, the center is returned **unchanged**.
/// - **Over the left edge:** the center is moved right so the left edge sits exactly on
///   `left` (`center = left + half_left`).
/// - **Over the right edge:** the center is moved left so the right edge sits exactly on
///   `right` (`center = right - half_right`).
/// - **Wider than the bounds:** if the body cannot fit (its width exceeds
///   `right - left`), the left-edge clamp takes priority, pinning the left edge to
///   `left`. This is a deterministic safe default rather than an error.
///
/// `bound_left` / `bound_right` are assumed ordered (`left <= right`); if they are
/// reversed the function still returns a finite, deterministic result (it clamps toward
/// `left`) without panicking.
///
/// Pure, deterministic, never panics.
///
/// # Examples
///
/// ```
/// use fp_physics::clamp_to_bounds;
///
/// // Body half-widths 10 each, stage bounds [0, 200].
/// assert_eq!(clamp_to_bounds(100.0, 10.0, 10.0, 0.0, 200.0), 100.0); // inside, unchanged
/// assert_eq!(clamp_to_bounds(-50.0, 10.0, 10.0, 0.0, 200.0), 10.0);  // left edge -> 0
/// assert_eq!(clamp_to_bounds(500.0, 10.0, 10.0, 0.0, 200.0), 190.0); // right edge -> 200
/// ```
pub fn clamp_to_bounds(
    center: f32,
    half_left: f32,
    half_right: f32,
    bound_left: f32,
    bound_right: f32,
) -> f32 {
    let min_center = bound_left + half_left; // center when left edge sits on bound_left
    let max_center = bound_right - half_right; // center when right edge sits on bound_right

    // Over the right edge: pull left so the right edge meets bound_right.
    let clamped = center.min(max_center);
    // Over the left edge: push right so the left edge meets bound_left. Applied last so
    // that when the body is wider than the bounds (min_center > max_center) the left
    // edge wins deterministically.
    clamped.max(min_center)
}

/// Clamps a [`PushBody`]'s center so its facing-resolved extent stays within `[left, right]`.
///
/// Facing-aware wrapper over [`clamp_to_bounds`]: it resolves the body's left/right
/// half-widths from its `front`/`back` and [`Facing`] (matching [`PushBody::left`] /
/// [`PushBody::right`]) and returns the clamped center X. Same edge behavior as
/// [`clamp_to_bounds`]: inside is unchanged, over-edge is clamped, and an over-wide body
/// pins its left edge to `bound_left`.
///
/// Pure, deterministic, never panics.
///
/// # Examples
///
/// ```
/// use fp_physics::{clamp_body_to_bounds, Facing, PushBody};
///
/// // Facing right: left edge uses `back` (12), right edge uses `front` (15).
/// let body = PushBody::new(5.0, 15.0, 12.0, Facing::Right);
/// // Left edge would be 5-12 = -7, below bound 0 -> push right to center = 0 + 12 = 12.
/// assert_eq!(clamp_body_to_bounds(body, 0.0, 200.0), 12.0);
/// ```
pub fn clamp_body_to_bounds(body: PushBody, bound_left: f32, bound_right: f32) -> f32 {
    clamp_to_bounds(
        body.center,
        body.left_half_width(),
        body.right_half_width(),
        bound_left,
        bound_right,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-5;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < EPS
    }

    #[test]
    fn body_edges_facing_right() {
        let b = PushBody::new(100.0, 15.0, 12.0, Facing::Right);
        assert!(approx(b.left(), 88.0)); // center - back
        assert!(approx(b.right(), 115.0)); // center + front
    }

    #[test]
    fn body_edges_facing_left_mirrors_front_back() {
        let b = PushBody::new(100.0, 15.0, 12.0, Facing::Left);
        assert!(approx(b.left(), 85.0)); // center - front
        assert!(approx(b.right(), 112.0)); // center + back
    }

    #[test]
    fn push_overlap_splits_evenly_and_just_touches() {
        // a: -10..10, b: -4..16. overlap = 10 - (-4) = 14, half = 7.
        let a = PushBody::new(0.0, 10.0, 10.0, Facing::Right);
        let b = PushBody::new(6.0, 10.0, 10.0, Facing::Left);
        let r = resolve_push(a, b);
        assert!(r.pushed);
        // Even split: a moves left by 7, b moves right by 7.
        assert!(approx(r.a, -7.0));
        assert!(approx(r.b, 13.0));

        // After moving, the bodies are just-touching (overlap == 0).
        let a2 = PushBody { center: r.a, ..a };
        let b2 = PushBody { center: r.b, ..b };
        let new_overlap = a2.right().min(b2.right()) - a2.left().max(b2.left());
        assert!(approx(new_overlap, 0.0));
    }

    #[test]
    fn push_is_symmetric_in_magnitude() {
        // Each body moves the same distance (half the overlap), opposite directions.
        let a = PushBody::new(10.0, 8.0, 8.0, Facing::Right); // 2..18
        let b = PushBody::new(20.0, 8.0, 8.0, Facing::Left); // 12..28, overlap = 18-12 = 6
        let (da, db) = push_bodies(a, b);
        assert!(approx(da, -3.0));
        assert!(approx(db, 3.0));
        assert!(approx(da, -db)); // equal and opposite
    }

    #[test]
    fn push_respects_argument_order() {
        // Swapping arguments swaps the returned positions but keeps the same geometry.
        let a = PushBody::new(0.0, 10.0, 10.0, Facing::Right);
        let b = PushBody::new(6.0, 10.0, 10.0, Facing::Left);
        let r1 = resolve_push(a, b);
        let r2 = resolve_push(b, a);
        assert!(approx(r1.a, r2.b));
        assert!(approx(r1.b, r2.a));
    }

    #[test]
    fn push_reversed_center_order_separates_correctly() {
        // a is to the RIGHT of b: a must move right, b must move left.
        let a = PushBody::new(6.0, 10.0, 10.0, Facing::Left); // -4..16
        let b = PushBody::new(0.0, 10.0, 10.0, Facing::Right); // -10..10
        let r = resolve_push(a, b);
        assert!(r.pushed);
        assert!(r.a > 6.0); // a (rightmost) pushed further right
        assert!(r.b < 0.0); // b (leftmost) pushed further left
    }

    #[test]
    fn no_overlap_leaves_bodies_unchanged() {
        let a = PushBody::new(0.0, 5.0, 5.0, Facing::Right); // -5..5
        let b = PushBody::new(100.0, 5.0, 5.0, Facing::Left); // 95..105
        let r = resolve_push(a, b);
        assert!(!r.pushed);
        assert!(approx(r.a, 0.0));
        assert!(approx(r.b, 100.0));
        let (da, db) = push_bodies(a, b);
        assert!(approx(da, 0.0) && approx(db, 0.0));
    }

    #[test]
    fn edge_touch_is_not_a_push() {
        // a: -5..5, b: 5..15 — share exactly the x=5 edge (overlap == 0, not > 0).
        let a = PushBody::new(0.0, 5.0, 5.0, Facing::Right);
        let b = PushBody::new(10.0, 5.0, 5.0, Facing::Left);
        let r = resolve_push(a, b);
        assert!(!r.pushed);
        assert!(approx(r.a, 0.0) && approx(r.b, 10.0));
    }

    #[test]
    fn equal_centers_push_deterministically() {
        // Fully coincident bodies: a goes left, b goes right by half the overlap.
        let a = PushBody::new(50.0, 10.0, 10.0, Facing::Right); // 40..60
        let b = PushBody::new(50.0, 10.0, 10.0, Facing::Left); // 40..60, overlap = 20
        let r = resolve_push(a, b);
        assert!(r.pushed);
        assert!(approx(r.a, 40.0)); // 50 - 10
        assert!(approx(r.b, 60.0)); // 50 + 10
    }

    #[test]
    fn clamp_inside_is_unchanged() {
        assert!(approx(clamp_to_bounds(100.0, 10.0, 10.0, 0.0, 200.0), 100.0));
    }

    #[test]
    fn clamp_left_over_edge() {
        // Center -50, half_left 10 -> left edge -60, below 0. Push to center = 0 + 10.
        assert!(approx(clamp_to_bounds(-50.0, 10.0, 10.0, 0.0, 200.0), 10.0));
    }

    #[test]
    fn clamp_right_over_edge() {
        // Center 500, half_right 10 -> right edge 510, above 200. Pull to 200 - 10.
        assert!(approx(clamp_to_bounds(500.0, 10.0, 10.0, 0.0, 200.0), 190.0));
    }

    #[test]
    fn clamp_exactly_on_edge_is_unchanged() {
        // Left edge exactly on bound_left: already valid, unchanged.
        assert!(approx(clamp_to_bounds(10.0, 10.0, 10.0, 0.0, 200.0), 10.0));
        // Right edge exactly on bound_right: unchanged.
        assert!(approx(clamp_to_bounds(190.0, 10.0, 10.0, 0.0, 200.0), 190.0));
    }

    #[test]
    fn clamp_body_facing_aware() {
        // Facing right: left edge uses back (12), right edge uses front (15).
        let body = PushBody::new(5.0, 15.0, 12.0, Facing::Right);
        // Left edge 5-12 = -7 < 0 -> center = 0 + 12 = 12.
        assert!(approx(clamp_body_to_bounds(body, 0.0, 200.0), 12.0));

        // Same body facing left: left edge uses front (15), right edge uses back (12).
        let body_l = PushBody::new(5.0, 15.0, 12.0, Facing::Left);
        // Left edge 5-15 = -10 < 0 -> center = 0 + 15 = 15.
        assert!(approx(clamp_body_to_bounds(body_l, 0.0, 200.0), 15.0));
    }

    #[test]
    fn clamp_body_wider_than_bounds_pins_left_edge() {
        // Body 100 wide, bounds only 50 wide: cannot fit, left edge wins -> center = left + half_left.
        let body = PushBody::new(0.0, 50.0, 50.0, Facing::Right);
        let c = clamp_body_to_bounds(body, 0.0, 50.0);
        // half_left (back) = 50 -> center = 0 + 50 = 50; left edge sits on 0.
        assert!(approx(c, 50.0));
        let clamped = PushBody { center: c, ..body };
        assert!(approx(clamped.left(), 0.0));
    }

    #[test]
    fn pure_functions_never_panic_on_non_finite() {
        // Acceptance criterion 3: never panics, even on NaN / infinities.
        let nan = f32::NAN;
        let inf = f32::INFINITY;
        let weird = PushBody::new(nan, inf, -inf, Facing::Right);
        let other = PushBody::new(inf, nan, 0.0, Facing::Left);
        let _ = resolve_push(weird, other);
        let _ = push_bodies(weird, other);
        let _ = clamp_to_bounds(nan, inf, -inf, nan, inf);
        let _ = clamp_body_to_bounds(weird, nan, inf);
    }

    #[test]
    fn push_is_deterministic_across_calls() {
        let a = PushBody::new(0.0, 10.0, 10.0, Facing::Right);
        let b = PushBody::new(6.0, 10.0, 10.0, Facing::Left);
        let first = resolve_push(a, b);
        for _ in 0..16 {
            assert_eq!(resolve_push(a, b), first);
        }
    }

    // ---------------------------------------------------------------------
    // Proctor-added tests: additional edge cases, error paths, and
    // MUGEN-semantics coverage layered on top of Forge's originals.
    //
    // Acceptance-criteria mapping:
    //   AC1 (push): asymmetric widths, idempotency, delta round-trip,
    //               argument-order symmetry for deltas, partial overlap.
    //   AC2 (clamp): facing-aware right-edge clamp, raw wider-than-bounds,
    //                reversed bounds, zero-width body, exact-fit body.
    //   AC3 (pure/deterministic/never-panic): clamp determinism, more
    //               non-finite paths.
    // ---------------------------------------------------------------------

    /// Recompute the world-extent overlap for a pair of bodies (test helper).
    fn overlap_of(a: PushBody, b: PushBody) -> f32 {
        a.right().min(b.right()) - a.left().max(b.left())
    }

    // ---- AC1: player-push edge cases ----

    #[test]
    fn push_with_asymmetric_front_back_widths_just_touches() {
        // MUGEN size.ground.front != size.ground.back. The push must use the
        // resolved world extents (not the raw front/back), split evenly, and end
        // just-touching regardless of which side the wider half-width is on.
        // a faces right: front=20 on +X, back=5 on -X -> extent -5..20.
        // b faces left:  front=20 on -X, back=5 on +X -> extent  5..30.
        let a = PushBody::new(0.0, 20.0, 5.0, Facing::Right);
        let b = PushBody::new(25.0, 20.0, 5.0, Facing::Left);
        assert!(approx(a.left(), -5.0) && approx(a.right(), 20.0));
        assert!(approx(b.left(), 5.0) && approx(b.right(), 30.0));

        // overlap = min(20,30) - max(-5,5) = 20 - 5 = 15, half = 7.5.
        let r = resolve_push(a, b);
        assert!(r.pushed);
        assert!(approx(r.a, -7.5)); // 0 - 7.5
        assert!(approx(r.b, 32.5)); // 25 + 7.5

        // Each moved exactly half the overlap, and the result is just-touching.
        let a2 = PushBody { center: r.a, ..a };
        let b2 = PushBody { center: r.b, ..b };
        assert!(approx(overlap_of(a2, b2), 0.0));
    }

    #[test]
    fn push_is_idempotent_on_just_touching_bodies() {
        // Re-running the push on the already-separated positions must be a no-op:
        // overlap is now exactly 0 (not strictly > 0), so `pushed` is false and the
        // centers are returned unchanged. Guards against oscillation if a caller
        // applies the resolver every frame.
        let a = PushBody::new(0.0, 10.0, 10.0, Facing::Right);
        let b = PushBody::new(6.0, 10.0, 10.0, Facing::Left);
        let r1 = resolve_push(a, b);
        assert!(r1.pushed);

        let a2 = PushBody { center: r1.a, ..a };
        let b2 = PushBody { center: r1.b, ..b };
        let r2 = resolve_push(a2, b2);
        assert!(!r2.pushed, "second pass on just-touching bodies must not push");
        assert!(approx(r2.a, r1.a) && approx(r2.b, r1.b));
    }

    #[test]
    fn push_deltas_round_trip_to_resolved_centers() {
        // push_bodies must agree with resolve_push: applying the deltas to the
        // original centers reproduces resolve_push's absolute positions.
        let a = PushBody::new(10.0, 8.0, 8.0, Facing::Right);
        let b = PushBody::new(20.0, 8.0, 8.0, Facing::Left);
        let r = resolve_push(a, b);
        let (da, db) = push_bodies(a, b);
        assert!(approx(a.center + da, r.a));
        assert!(approx(b.center + db, r.b));
        // Even split => the two deltas cancel exactly.
        assert!(approx(da + db, 0.0));
    }

    #[test]
    fn push_bodies_argument_order_swaps_deltas() {
        // Swapping the arguments swaps which delta belongs to which body but keeps
        // the magnitudes (the geometry is unchanged).
        let a = PushBody::new(0.0, 10.0, 10.0, Facing::Right);
        let b = PushBody::new(6.0, 10.0, 10.0, Facing::Left);
        let (da, db) = push_bodies(a, b);
        let (db2, da2) = push_bodies(b, a);
        assert!(approx(da, da2));
        assert!(approx(db, db2));
    }

    #[test]
    fn push_partial_overlap_only_corrects_the_overlap() {
        // A small interpenetration must produce a small, exact correction (not a
        // full separation by the sum of half-widths). Bodies 10 wide each, centers
        // 18 apart -> extents -5..5 and 13..23, overlap = 5 - 13 = -8 (disjoint).
        let disjoint_a = PushBody::new(0.0, 5.0, 5.0, Facing::Right);
        let disjoint_b = PushBody::new(18.0, 5.0, 5.0, Facing::Left);
        assert!(!resolve_push(disjoint_a, disjoint_b).pushed);

        // Now centers 8 apart -> extents -5..5 and 3..13, overlap = 5 - 3 = 2.
        let a = PushBody::new(0.0, 5.0, 5.0, Facing::Right);
        let b = PushBody::new(8.0, 5.0, 5.0, Facing::Left);
        let r = resolve_push(a, b);
        assert!(r.pushed);
        // half overlap = 1: a -> -1, b -> 9.
        assert!(approx(r.a, -1.0));
        assert!(approx(r.b, 9.0));
    }

    #[test]
    fn push_negative_half_width_bodies_do_not_overlap() {
        // Degenerate/negative half-widths (front < 0) shrink the side so the world
        // extents end up disjoint; this is pure geometry, must not push or panic.
        let a = PushBody::new(0.0, -5.0, 10.0, Facing::Right); // extent -10..-5
        let b = PushBody::new(0.0, -5.0, 10.0, Facing::Left); // extent   5..10
        assert!(overlap_of(a, b) < 0.0);
        let r = resolve_push(a, b);
        assert!(!r.pushed);
        assert!(approx(r.a, 0.0) && approx(r.b, 0.0));
    }

    #[test]
    fn push_one_fully_inside_the_other() {
        // A narrow body whose extent lies entirely within a wide body. Overlap is
        // the narrow body's full width; both still split evenly and separate.
        let wide = PushBody::new(0.0, 50.0, 50.0, Facing::Right); // -50..50
        let narrow = PushBody::new(0.0, 5.0, 5.0, Facing::Left); // -5..5
        // overlap = min(50,5) - max(-50,-5) = 5 - (-5) = 10, half = 5.
        let r = resolve_push(wide, narrow);
        assert!(r.pushed);
        // Equal centers tie-break: a (wide) left, b (narrow) right.
        assert!(approx(r.a, -5.0));
        assert!(approx(r.b, 5.0));
    }

    // ---- AC2: stage / screen bound clamping edge cases ----

    #[test]
    fn clamp_body_over_right_edge_facing_aware() {
        // Only the left edge was exercised before. Facing right: right edge uses
        // `front` (15). center 195 -> right edge 210 > 200 -> center = 200 - 15 = 185.
        let body = PushBody::new(195.0, 15.0, 12.0, Facing::Right);
        assert!(approx(clamp_body_to_bounds(body, 0.0, 200.0), 185.0));

        // Facing left: right edge uses `back` (12). center 195 -> right edge 207 ->
        // center = 200 - 12 = 188.
        let body_l = PushBody::new(195.0, 15.0, 12.0, Facing::Left);
        assert!(approx(clamp_body_to_bounds(body_l, 0.0, 200.0), 188.0));
    }

    #[test]
    fn clamp_raw_wider_than_bounds_pins_left_edge() {
        // Raw clamp_to_bounds (not the body wrapper): body 60 wide, bounds 40 wide.
        // min_center (30) > max_center (10): left edge must win deterministically.
        let c = clamp_to_bounds(5.0, 30.0, 30.0, 0.0, 40.0);
        assert!(approx(c, 30.0)); // left edge sits on bound_left = 0.
        // And it holds no matter where the (impossible-to-fit) center starts.
        assert!(approx(clamp_to_bounds(1000.0, 30.0, 30.0, 0.0, 40.0), 30.0));
        assert!(approx(clamp_to_bounds(-1000.0, 30.0, 30.0, 0.0, 40.0), 30.0));
    }

    #[test]
    fn clamp_reversed_bounds_is_finite_and_does_not_panic() {
        // Documented: reversed bounds (left > right) still return a finite,
        // deterministic result clamped toward `left`, without panicking.
        // min_center = 200 + 10 = 210, max_center = 0 - 10 = -10; left wins -> 210.
        let c = clamp_to_bounds(50.0, 10.0, 10.0, 200.0, 0.0);
        assert!(c.is_finite());
        assert!(approx(c, 210.0));
    }

    #[test]
    fn clamp_zero_width_body_pins_to_a_point() {
        // A point body (both half-widths 0) clamps its center directly into
        // [left, right]; inside is unchanged, outside snaps to the nearer bound.
        assert!(approx(clamp_to_bounds(50.0, 0.0, 0.0, 0.0, 100.0), 50.0)); // inside
        assert!(approx(clamp_to_bounds(-10.0, 0.0, 0.0, 0.0, 100.0), 0.0)); // left bound
        assert!(approx(clamp_to_bounds(150.0, 0.0, 0.0, 0.0, 100.0), 100.0)); // right bound
    }

    #[test]
    fn clamp_body_exactly_filling_bounds_is_unchanged() {
        // A body whose extent exactly equals the bounds: min_center == max_center;
        // the unique valid center is returned and an in-bounds center is unchanged.
        // front=back=20, bounds [0,40] -> only valid center is 20.
        let body = PushBody::new(20.0, 20.0, 20.0, Facing::Right);
        assert!(approx(clamp_body_to_bounds(body, 0.0, 40.0), 20.0));
        // Nudge it left and right: both snap back to the single feasible center.
        let left = PushBody::new(5.0, 20.0, 20.0, Facing::Right);
        let right = PushBody::new(35.0, 20.0, 20.0, Facing::Right);
        assert!(approx(clamp_body_to_bounds(left, 0.0, 40.0), 20.0));
        assert!(approx(clamp_body_to_bounds(right, 0.0, 40.0), 20.0));
    }

    #[test]
    fn clamp_body_inside_is_unchanged_for_both_facings() {
        // Sanity: an in-bounds body is untouched irrespective of facing (the
        // facing only chooses which half-width maps to which side).
        for facing in [Facing::Right, Facing::Left] {
            let body = PushBody::new(100.0, 15.0, 12.0, facing);
            assert!(approx(clamp_body_to_bounds(body, 0.0, 200.0), 100.0));
        }
    }

    #[test]
    fn clamp_is_deterministic_across_calls() {
        // AC3: pure & deterministic for the clamp path too.
        let first = clamp_to_bounds(500.0, 10.0, 10.0, 0.0, 200.0);
        for _ in 0..16 {
            assert_eq!(clamp_to_bounds(500.0, 10.0, 10.0, 0.0, 200.0), first);
        }
        assert!(approx(first, 190.0));
    }

    // ---- AC3: more non-finite / never-panic coverage ----

    #[test]
    fn clamp_non_finite_bounds_never_panic() {
        // Infinite/NaN bounds and half-widths must not panic (results unspecified).
        let inf = f32::INFINITY;
        let nan = f32::NAN;
        let _ = clamp_to_bounds(0.0, 10.0, 10.0, -inf, inf); // unbounded stage
        let _ = clamp_to_bounds(nan, 10.0, 10.0, 0.0, 200.0);
        let _ = clamp_to_bounds(50.0, nan, nan, 0.0, 200.0);
        let _ = clamp_to_bounds(50.0, 10.0, 10.0, nan, nan);
    }

    #[test]
    fn clamp_unbounded_stage_leaves_center_unchanged() {
        // With [-inf, +inf] bounds, min_center = -inf, max_center = +inf, so any
        // finite center passes through unchanged.
        let inf = f32::INFINITY;
        assert!(approx(clamp_to_bounds(123.0, 10.0, 10.0, -inf, inf), 123.0));
    }
}
