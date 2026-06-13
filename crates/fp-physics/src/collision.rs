//! Axis-aligned bounding-box (AABB) collision detection for MUGEN-style Clsn boxes.
//!
//! This module provides the *geometric core* that Phase 6 combat builds on. It deals
//! purely with rectangle geometry — there is no [`HitDef`], damage, priority, or guard
//! logic here (that lives in `fp-combat`).
//!
//! # MUGEN Clsn boxes
//!
//! In MUGEN, collision boxes are declared per animation frame in `.air` files as
//! corner pairs `x1,y1,x2,y2`, expressed in the character's *local* coordinate space
//! (relative to the character axis). Two flavours exist:
//!
//! - **Clsn1** = attack boxes
//! - **Clsn2** = hurt boxes
//!
//! A hit occurs when **any** of the attacker's Clsn1 boxes overlaps **any** of the
//! defender's Clsn2 boxes — see [`any_overlap`], the hit-detection primitive.
//!
//! # Coordinate convention
//!
//! Consistent with the rest of `fp-physics` and [`fp_core::Rect`]: Y increases
//! *downward*. A box is placed into world space relative to a character position and
//! facing; a left-facing character mirrors its local X about the axis (see
//! [`place_clsn`]).
//!
//! # Edge-touch behavior
//!
//! Overlap tests use *strict* inequalities, matching [`fp_core::Rect::overlaps`]:
//! rectangles that merely touch along an edge or corner (zero shared area) are **not**
//! considered overlapping. This is intentional and consistent across [`rects_overlap`],
//! [`Clsn::overlaps`], and [`any_overlap`].

use fp_core::{Rect, Vec2};

/// Which way a character faces, used when placing a local Clsn box into world space.
///
/// MUGEN characters mirror their collision boxes horizontally when facing left:
/// the local X axis is reflected about the character's axis. Vertical (Y) geometry
/// is unaffected by facing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Facing {
    /// Facing right: local X maps to world X without mirroring.
    Right,
    /// Facing left: local X is mirrored about the character axis.
    Left,
}

impl Facing {
    /// Returns the horizontal sign multiplier for this facing: `+1.0` for
    /// [`Facing::Right`], `-1.0` for [`Facing::Left`].
    ///
    /// Multiplying a character-local X offset by this value yields the offset in
    /// world space (before adding the character's axis X).
    pub const fn sign(self) -> f32 {
        match self {
            Facing::Right => 1.0,
            Facing::Left => -1.0,
        }
    }

    /// Returns the opposite facing.
    pub const fn flipped(self) -> Self {
        match self {
            Facing::Right => Facing::Left,
            Facing::Left => Facing::Right,
        }
    }
}

impl Default for Facing {
    /// Defaults to [`Facing::Right`], matching MUGEN's P1-facing-right convention.
    fn default() -> Self {
        Facing::Right
    }
}

/// A MUGEN collision box stored as a corner pair `(x1, y1, x2, y2)`.
///
/// This mirrors the raw `.air` representation (`Clsn1[i] = x1,y1,x2,y2`). Either
/// corner ordering is accepted — geometry helpers normalize internally so that a box
/// written as `(x2,y2,x1,y1)` behaves identically to `(x1,y1,x2,y2)`.
///
/// Coordinates are in the character's *local* space until placed into world space via
/// [`Clsn::place`] / [`place_clsn`]. As elsewhere in `fp-physics`, Y increases downward.
///
/// # Examples
///
/// ```
/// use fp_physics::Clsn;
///
/// // Corner ordering does not matter: these are the same box.
/// let a = Clsn::new(-10.0, -20.0, 10.0, 0.0);
/// let b = Clsn::new(10.0, 0.0, -10.0, -20.0);
/// assert_eq!(a.to_rect(), b.to_rect());
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clsn {
    /// First corner X (as authored; not necessarily the minimum).
    pub x1: f32,
    /// First corner Y (as authored; not necessarily the minimum).
    pub y1: f32,
    /// Second corner X (as authored; not necessarily the maximum).
    pub x2: f32,
    /// Second corner Y (as authored; not necessarily the maximum).
    pub y2: f32,
}

impl Clsn {
    /// Creates a Clsn box from a corner pair. Either ordering is accepted.
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    /// Converts this box into a normalized [`fp_core::Rect`] (top-left + size, Y down).
    ///
    /// Corner ordering is normalized: the resulting rect has non-negative width and
    /// height regardless of how the corners were supplied. A degenerate box (a point
    /// or line) yields a zero-area rect; note that under [`rects_overlap`] (which uses
    /// strict inequalities) such a rect can still overlap a box that strictly contains
    /// its point or line.
    pub fn to_rect(self) -> Rect {
        let min_x = self.x1.min(self.x2);
        let min_y = self.y1.min(self.y2);
        let w = (self.x1 - self.x2).abs();
        let h = (self.y1 - self.y2).abs();
        Rect::new(min_x, min_y, w, h)
    }

    /// Places this character-local box into world space given the character's axis
    /// position and facing. See [`place_clsn`] for the full description.
    pub fn place(self, pos: Vec2<f32>, facing: Facing) -> Rect {
        place_clsn(self, pos, facing)
    }

    /// Tests whether this box (normalized) overlaps another box (normalized).
    ///
    /// Both boxes are treated in the same coordinate space. Edge-touching boxes do
    /// **not** overlap (strict inequality), consistent with [`rects_overlap`].
    pub fn overlaps(self, other: Clsn) -> bool {
        rects_overlap(&self.to_rect(), &other.to_rect())
    }
}

/// Tests whether two rectangles overlap (share positive area).
///
/// This is a thin, explicitly-documented wrapper over [`fp_core::Rect::overlaps`].
/// Rectangles that merely touch along an edge or corner are **not** considered
/// overlapping (strict inequality). The test assumes normalized rects (non-negative
/// `w`/`h`); use [`Clsn::to_rect`] to normalize corner-pair boxes first.
///
/// Pure and deterministic; never panics.
///
/// # Examples
///
/// ```
/// use fp_core::Rect;
/// use fp_physics::rects_overlap;
///
/// let a = Rect::new(0.0, 0.0, 10.0, 10.0);
/// let b = Rect::new(5.0, 5.0, 10.0, 10.0);
/// let touching = Rect::new(10.0, 0.0, 10.0, 10.0);
/// assert!(rects_overlap(&a, &b));
/// assert!(!rects_overlap(&a, &touching)); // edge-touch is not overlap
/// ```
pub fn rects_overlap(a: &Rect, b: &Rect) -> bool {
    a.overlaps(b)
}

/// Places a character-local Clsn box into world space.
///
/// The character sits at world position `pos` (its axis). The box's local coordinates
/// are interpreted relative to that axis:
///
/// - **X:** mirrored about the axis when `facing` is [`Facing::Left`] (`world_x =
///   pos.x + facing.sign() * local_x`), unchanged when [`Facing::Right`].
/// - **Y:** always `world_y = pos.y + local_y` (Y down; facing never affects Y).
///
/// Mirroring can swap the relative order of the two X corners, so the result is
/// normalized into a [`fp_core::Rect`] with non-negative width/height. Either input
/// corner ordering is therefore handled correctly.
///
/// Pure and deterministic; never panics.
///
/// # Examples
///
/// ```
/// use fp_core::Vec2;
/// use fp_physics::{place_clsn, Clsn, Facing};
///
/// // Local box extends from x=10 to x=30, right of the axis.
/// let box_ = Clsn::new(10.0, -40.0, 30.0, 0.0);
/// let pos = Vec2::new(100.0, 0.0);
///
/// // Facing right: stays to the right of the axis.
/// let r = place_clsn(box_, pos, Facing::Right);
/// assert_eq!((r.x, r.right()), (110.0, 130.0));
///
/// // Facing left: mirrored to the left of the axis.
/// let l = place_clsn(box_, pos, Facing::Left);
/// assert_eq!((l.x, l.right()), (70.0, 90.0));
/// ```
pub fn place_clsn(clsn: Clsn, pos: Vec2<f32>, facing: Facing) -> Rect {
    let sign = facing.sign();
    // Transform both X corners (mirror about axis when facing left), then translate.
    let wx1 = pos.x + sign * clsn.x1;
    let wx2 = pos.x + sign * clsn.x2;
    // Y is never affected by facing.
    let wy1 = pos.y + clsn.y1;
    let wy2 = pos.y + clsn.y2;
    // Normalize: mirroring/corner-order may leave x1 > x2 or y1 > y2.
    Clsn::new(wx1, wy1, wx2, wy2).to_rect()
}

/// Returns `true` if **any** box in `a` overlaps **any** box in `b`.
///
/// This is the MUGEN hit-detection primitive: pass the attacker's Clsn1 boxes as `a`
/// and the defender's Clsn2 boxes as `b` (both already placed into world space via
/// [`place_clsn`]). A hit occurs iff this returns `true`.
///
/// Empty slices never overlap (returns `false`). Edge-touching boxes do not count as
/// overlapping, consistent with [`rects_overlap`]. Pure, deterministic, never panics.
///
/// # Examples
///
/// ```
/// use fp_core::Rect;
/// use fp_physics::any_overlap;
///
/// let attack = [Rect::new(0.0, 0.0, 10.0, 10.0)];
/// let hurt_hit = [Rect::new(100.0, 0.0, 5.0, 5.0), Rect::new(5.0, 5.0, 5.0, 5.0)];
/// let hurt_miss = [Rect::new(100.0, 0.0, 5.0, 5.0)];
///
/// assert!(any_overlap(&attack, &hurt_hit));   // second hurt box overlaps
/// assert!(!any_overlap(&attack, &hurt_miss)); // none overlap
/// assert!(!any_overlap(&attack, &[]));        // empty set never hits
/// ```
pub fn any_overlap(a: &[Rect], b: &[Rect]) -> bool {
    a.iter()
        .any(|ra| b.iter().any(|rb| rects_overlap(ra, rb)))
}

/// Returns `true` if any Clsn box in `a` overlaps any Clsn box in `b`, placing both
/// sets into world space first.
///
/// Convenience wrapper over [`place_clsn`] + [`any_overlap`] for the common case where
/// you have the attacker's Clsn1 boxes (`a`) at `pos_a`/`facing_a` and the defender's
/// Clsn2 boxes (`b`) at `pos_b`/`facing_b`, all still in character-local space.
///
/// Pure, deterministic, never panics. Empty slices never overlap.
///
/// # Examples
///
/// ```
/// use fp_core::Vec2;
/// use fp_physics::{any_clsn_overlap, Clsn, Facing};
///
/// // Attacker at x=0 facing right; a forward punch box reaching out to x=55.
/// let attack = [Clsn::new(10.0, -60.0, 55.0, -40.0)];
/// // Defender at x=60 facing left; hurt box -18..18 about its axis -> world 42..78.
/// let hurt = [Clsn::new(-18.0, -70.0, 18.0, 0.0)];
///
/// // Attack world x 10..55 overlaps hurt world x 42..78 (and the y ranges overlap).
/// assert!(any_clsn_overlap(
///     &attack, Vec2::new(0.0, 0.0), Facing::Right,
///     &hurt, Vec2::new(60.0, 0.0), Facing::Left,
/// ));
/// ```
pub fn any_clsn_overlap(
    a: &[Clsn],
    pos_a: Vec2<f32>,
    facing_a: Facing,
    b: &[Clsn],
    pos_b: Vec2<f32>,
    facing_b: Facing,
) -> bool {
    a.iter().any(|ca| {
        let ra = place_clsn(*ca, pos_a, facing_a);
        b.iter()
            .any(|cb| rects_overlap(&ra, &place_clsn(*cb, pos_b, facing_b)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-6;

    fn rect_eq(a: Rect, b: Rect) -> bool {
        (a.x - b.x).abs() < EPS
            && (a.y - b.y).abs() < EPS
            && (a.w - b.w).abs() < EPS
            && (a.h - b.h).abs() < EPS
    }

    #[test]
    fn rects_overlap_hit() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        assert!(rects_overlap(&a, &b));
        assert!(rects_overlap(&b, &a)); // symmetric
    }

    #[test]
    fn rects_overlap_miss() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let far = Rect::new(100.0, 100.0, 5.0, 5.0);
        assert!(!rects_overlap(&a, &far));
    }

    #[test]
    fn rects_overlap_edge_touch_is_miss() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let right_touch = Rect::new(10.0, 0.0, 10.0, 10.0); // shares the x=10 edge
        let corner_touch = Rect::new(10.0, 10.0, 10.0, 10.0); // shares only a corner
        assert!(!rects_overlap(&a, &right_touch));
        assert!(!rects_overlap(&a, &corner_touch));
    }

    #[test]
    fn clsn_normalizes_corner_order() {
        // Same box, opposite corner ordering -> identical rect.
        let a = Clsn::new(-10.0, -20.0, 10.0, 0.0);
        let b = Clsn::new(10.0, 0.0, -10.0, -20.0);
        let ra = a.to_rect();
        assert!(rect_eq(ra, b.to_rect()));
        assert!(rect_eq(ra, Rect::new(-10.0, -20.0, 20.0, 20.0)));
    }

    #[test]
    fn degenerate_box_has_zero_area() {
        let point = Clsn::new(5.0, 5.0, 5.0, 5.0);
        let r = point.to_rect();
        assert!((r.w).abs() < EPS && (r.h).abs() < EPS);
        // A zero-area box does not overlap a box it lies entirely outside of.
        let elsewhere = Clsn::new(100.0, 100.0, 200.0, 200.0);
        assert!(!point.overlaps(elsewhere));
    }

    #[test]
    fn place_facing_right_translates() {
        let box_ = Clsn::new(10.0, -40.0, 30.0, 0.0);
        let r = place_clsn(box_, Vec2::new(100.0, 0.0), Facing::Right);
        // X: 100+10..100+30 ; Y: 0+(-40)..0+0
        assert!(rect_eq(r, Rect::new(110.0, -40.0, 20.0, 40.0)));
    }

    #[test]
    fn place_facing_left_mirrors_x_about_axis() {
        let box_ = Clsn::new(10.0, -40.0, 30.0, 0.0);
        let r = place_clsn(box_, Vec2::new(100.0, 0.0), Facing::Left);
        // X mirrored: 100-30..100-10 ; Y unchanged by facing.
        assert!(rect_eq(r, Rect::new(70.0, -40.0, 20.0, 40.0)));
    }

    #[test]
    fn place_left_then_right_is_symmetric_about_axis() {
        // A box placed left-facing should be the mirror image (about pos.x) of the
        // same box placed right-facing.
        let box_ = Clsn::new(5.0, -10.0, 25.0, 10.0);
        let pos = Vec2::new(50.0, 0.0);
        let right = place_clsn(box_, pos, Facing::Right);
        let left = place_clsn(box_, pos, Facing::Left);
        // Reflecting a world X about pos.x is `2*pos.x - X`; right's right edge maps to
        // left's left edge and vice versa.
        let mirrored_left_x = 2.0 * pos.x - right.right();
        let mirrored_right_x = 2.0 * pos.x - right.x;
        assert!((left.x - mirrored_left_x).abs() < EPS);
        assert!((left.right() - mirrored_right_x).abs() < EPS);
        // Y identical regardless of facing.
        assert!((left.y - right.y).abs() < EPS && (left.h - right.h).abs() < EPS);
    }

    #[test]
    fn place_handles_reversed_input_corner_order() {
        let normal = Clsn::new(10.0, -40.0, 30.0, 0.0);
        let reversed = Clsn::new(30.0, 0.0, 10.0, -40.0);
        let pos = Vec2::new(100.0, 0.0);
        for facing in [Facing::Right, Facing::Left] {
            assert!(rect_eq(
                place_clsn(normal, pos, facing),
                place_clsn(reversed, pos, facing)
            ));
        }
    }

    #[test]
    fn any_overlap_true() {
        let attack = [Rect::new(0.0, 0.0, 10.0, 10.0)];
        let hurt = [
            Rect::new(100.0, 0.0, 5.0, 5.0), // miss
            Rect::new(5.0, 5.0, 5.0, 5.0),   // hit
        ];
        assert!(any_overlap(&attack, &hurt));
    }

    #[test]
    fn any_overlap_false() {
        let attack = [Rect::new(0.0, 0.0, 10.0, 10.0)];
        let hurt = [
            Rect::new(100.0, 0.0, 5.0, 5.0),
            Rect::new(200.0, 0.0, 5.0, 5.0),
        ];
        assert!(!any_overlap(&attack, &hurt));
    }

    #[test]
    fn any_overlap_empty_sets_never_hit() {
        let attack = [Rect::new(0.0, 0.0, 10.0, 10.0)];
        let empty: [Rect; 0] = [];
        assert!(!any_overlap(&attack, &empty));
        assert!(!any_overlap(&empty, &attack));
        assert!(!any_overlap(&empty, &empty));
    }

    #[test]
    fn any_overlap_edge_touch_is_miss() {
        let attack = [Rect::new(0.0, 0.0, 10.0, 10.0)];
        let touching = [Rect::new(10.0, 0.0, 10.0, 10.0)];
        assert!(!any_overlap(&attack, &touching));
    }

    #[test]
    fn any_clsn_overlap_two_facing_characters_hit() {
        // P1 at x=0 facing right throws a punch reaching x=10..55 in front.
        let attack = [Clsn::new(10.0, -60.0, 55.0, -40.0)];
        // P2 at x=60 facing left; hurt box -18..18 around its axis -> world 42..78.
        let hurt = [Clsn::new(-18.0, -70.0, 18.0, 0.0)];
        // attack 10..55 overlaps hurt 42..78 in x; y -60..-40 overlaps -70..0.
        assert!(any_clsn_overlap(
            &attack,
            Vec2::new(0.0, 0.0),
            Facing::Right,
            &hurt,
            Vec2::new(60.0, 0.0),
            Facing::Left,
        ));
    }

    #[test]
    fn any_clsn_overlap_out_of_range_miss() {
        let attack = [Clsn::new(10.0, -60.0, 55.0, -40.0)];
        let hurt = [Clsn::new(-18.0, -70.0, 18.0, 0.0)];
        // P2 far away at x=300 -> hurt world 282..318, no overlap with 10..55.
        assert!(!any_clsn_overlap(
            &attack,
            Vec2::new(0.0, 0.0),
            Facing::Right,
            &hurt,
            Vec2::new(300.0, 0.0),
            Facing::Left,
        ));
    }

    /// Concrete regression case: two characters in a realistic clinch.
    ///
    /// P1 (axis x=80, on ground) jabs forward facing right. P2 (axis x=120, on ground)
    /// facing left has a standing hurt box. The jab's front edge reaches P2's hurt box
    /// by a few pixels — this exact configuration must register as a hit, and shifting
    /// P2 back by 6px (just past the reach) must register as a miss.
    #[test]
    fn regression_jab_clinch() {
        // P1 jab: local x 18..46, y -76..-58 (head-height jab), facing right.
        let p1_clsn1 = [Clsn::new(18.0, -76.0, 46.0, -58.0)];
        let p1_pos = Vec2::new(80.0, 0.0);
        // -> world x 98..126, y -76..-58.
        let placed = place_clsn(p1_clsn1[0], p1_pos, Facing::Right);
        assert!(rect_eq(placed, Rect::new(98.0, -76.0, 28.0, 18.0)));

        // P2 hurt: local x -14..14, y -80..0, facing left (mirror keeps it symmetric).
        let p2_clsn2 = [Clsn::new(-14.0, -80.0, 14.0, 0.0)];

        // Hit: P2 axis at x=120 -> hurt world x 106..134, overlaps 98..126 in x and
        // -80..0 vs -76..-58 in y.
        assert!(any_clsn_overlap(
            &p1_clsn1,
            p1_pos,
            Facing::Right,
            &p2_clsn2,
            Vec2::new(120.0, 0.0),
            Facing::Left,
        ));

        // Miss: push P2 back so its hurt box starts at x=126 (edge-touch) -> no hit.
        // P2 axis at x=140 -> hurt world x 126..154; shares only the x=126 edge.
        assert!(!any_clsn_overlap(
            &p1_clsn1,
            p1_pos,
            Facing::Right,
            &p2_clsn2,
            Vec2::new(140.0, 0.0),
            Facing::Left,
        ));
    }

    #[test]
    fn facing_helpers() {
        assert_eq!(Facing::Right.sign(), 1.0);
        assert_eq!(Facing::Left.sign(), -1.0);
        assert_eq!(Facing::Right.flipped(), Facing::Left);
        assert_eq!(Facing::Left.flipped(), Facing::Right);
        assert_eq!(Facing::default(), Facing::Right);
    }

    // ---------------------------------------------------------------------
    // Proctor-added tests: additional edge cases, error paths, and
    // MUGEN-semantics coverage layered on top of Forge's originals.
    // ---------------------------------------------------------------------

    #[test]
    fn to_rect_negative_only_coordinates() {
        // A box entirely in the upper-left quadrant (all coords negative, Y down).
        // Normalization must still yield non-negative w/h with the correct min corner.
        let c = Clsn::new(-30.0, -40.0, -10.0, -20.0);
        assert!(rect_eq(c.to_rect(), Rect::new(-30.0, -40.0, 20.0, 20.0)));
        // Reversed corner order produces the identical rect.
        let rev = Clsn::new(-10.0, -20.0, -30.0, -40.0);
        assert!(rect_eq(rev.to_rect(), c.to_rect()));
    }

    #[test]
    fn rects_overlap_containment() {
        // A small box fully inside a large box overlaps (positive shared area),
        // and the relation is symmetric.
        let outer = Rect::new(0.0, 0.0, 100.0, 100.0);
        let inner = Rect::new(40.0, 40.0, 10.0, 10.0);
        assert!(rects_overlap(&outer, &inner));
        assert!(rects_overlap(&inner, &outer));
    }

    #[test]
    fn rects_overlap_identical_rects() {
        let a = Rect::new(3.0, -7.0, 12.0, 9.0);
        assert!(rects_overlap(&a, &a));
    }

    #[test]
    fn rects_overlap_one_pixel_sliver_hits() {
        // 1px of shared area on each axis must count as a hit (strict-inequality
        // boundary: any positive overlap, however small, is a hit).
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(9.0, 9.0, 10.0, 10.0); // overlaps x 9..10, y 9..10
        assert!(rects_overlap(&a, &b));
    }

    #[test]
    fn rects_overlap_shares_one_axis_only_is_miss() {
        // Boxes whose X ranges overlap but whose Y ranges are disjoint do not hit
        // (and vice versa). Guards against an accidental OR instead of AND.
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let same_x_below = Rect::new(0.0, 20.0, 10.0, 10.0); // x overlaps, y disjoint
        let same_y_right = Rect::new(20.0, 0.0, 10.0, 10.0); // y overlaps, x disjoint
        assert!(!rects_overlap(&a, &same_x_below));
        assert!(!rects_overlap(&a, &same_y_right));
    }

    #[test]
    fn clsn_overlaps_matches_rect_path() {
        // Clsn::overlaps must agree with normalizing both boxes and calling
        // rects_overlap, for any input corner ordering.
        let a = Clsn::new(20.0, 0.0, 0.0, -20.0); // reversed corners
        let b = Clsn::new(-10.0, -30.0, 10.0, -10.0);
        assert_eq!(a.overlaps(b), rects_overlap(&a.to_rect(), &b.to_rect()));
        assert!(a.overlaps(b)); // they do in fact overlap
    }

    #[test]
    fn degenerate_box_on_boundary_or_outside_is_miss() {
        // A degenerate (zero-area) box that lies ON an edge of another box, or
        // wholly outside it, does not overlap. (Edge-coincident => strict-inequality
        // miss; outside => trivially a miss.)
        let big = Clsn::new(0.0, 0.0, 100.0, 100.0);
        let edge_line = Clsn::new(0.0, 10.0, 0.0, 90.0); // w==0 on the left edge x=0
        let bottom_line = Clsn::new(10.0, 100.0, 90.0, 100.0); // h==0 on bottom edge
        let outside_point = Clsn::new(200.0, 200.0, 200.0, 200.0);
        assert!(!big.overlaps(edge_line));
        assert!(!big.overlaps(bottom_line));
        assert!(!big.overlaps(outside_point));
        // A degenerate box never overlaps itself (x < x is false on every axis).
        assert!(!outside_point.overlaps(outside_point));
    }

    #[test]
    fn degenerate_box_interior_overlaps_per_rect_semantics() {
        // Guards the documented degenerate-box semantics: a zero-area box (point or
        // line) lying *strictly interior* to another box DOES overlap it, because
        // fp-core's `Rect::overlaps` uses strict `<`/`>` against each rect's own
        // edges, so a width/height-0 rect strictly inside a larger one passes all
        // four tests. (The `Clsn::to_rect` doc was corrected to state this.)
        let big = Clsn::new(0.0, 0.0, 100.0, 100.0);
        let interior_vertical = Clsn::new(50.0, 10.0, 50.0, 90.0); // w == 0, inside
        let interior_horizontal = Clsn::new(10.0, 50.0, 90.0, 50.0); // h == 0, inside
        let interior_point = Clsn::new(50.0, 50.0, 50.0, 50.0); // point, inside
        assert!(big.overlaps(interior_vertical));
        assert!(big.overlaps(interior_horizontal));
        assert!(big.overlaps(interior_point));
    }

    #[test]
    fn place_at_nonzero_axis_offsets_both_axes() {
        // Y translation must apply (facing never touches Y, but pos.y does).
        let box_ = Clsn::new(10.0, -40.0, 30.0, 0.0);
        let r = place_clsn(box_, Vec2::new(100.0, -50.0), Facing::Right);
        // X: 110..130 ; Y: (-50 + -40)..(-50 + 0) = -90..-50
        assert!(rect_eq(r, Rect::new(110.0, -90.0, 20.0, 40.0)));
    }

    #[test]
    fn place_box_straddling_axis_facing_left_negates_correctly() {
        // A box that straddles the axis (x1 < 0 < x2). Mirroring about the axis
        // must reflect both edges; the box stays straddling but the asymmetric
        // overhang flips sides.
        let box_ = Clsn::new(-5.0, -10.0, 25.0, 0.0); // local x -5..25
        let pos = Vec2::new(0.0, 0.0);
        let left = place_clsn(box_, pos, Facing::Left);
        // Mirror about x=0: -25..5
        assert!(rect_eq(left, Rect::new(-25.0, -10.0, 30.0, 10.0)));
    }

    #[test]
    fn place_is_idempotent_for_reflection() {
        // Placing left-facing twice (reflecting twice) about the same axis returns
        // a box congruent to the right-facing placement, confirming sign(Left) == -1
        // is an involution about pos.x.
        let box_ = Clsn::new(7.0, -3.0, 19.0, 11.0);
        let pos = Vec2::new(42.0, 5.0);
        let right = place_clsn(box_, pos, Facing::Right);
        // Reflect the *world* right-facing box back about pos.x by hand and compare
        // to the left-facing placement.
        let left = place_clsn(box_, pos, Facing::Left);
        assert!((left.x - (2.0 * pos.x - right.right())).abs() < EPS);
        assert!((left.right() - (2.0 * pos.x - right.x)).abs() < EPS);
    }

    #[test]
    fn place_clsn_via_method_matches_free_function() {
        let box_ = Clsn::new(3.0, -9.0, 21.0, 4.0);
        let pos = Vec2::new(15.0, -2.0);
        for facing in [Facing::Right, Facing::Left] {
            assert!(rect_eq(box_.place(pos, facing), place_clsn(box_, pos, facing)));
        }
    }

    #[test]
    fn any_overlap_is_symmetric() {
        // any_overlap(a, b) == any_overlap(b, a) since rects_overlap is symmetric.
        let a = [Rect::new(0.0, 0.0, 10.0, 10.0), Rect::new(50.0, 50.0, 5.0, 5.0)];
        let b = [Rect::new(5.0, 5.0, 3.0, 3.0)];
        assert_eq!(any_overlap(&a, &b), any_overlap(&b, &a));
        assert!(any_overlap(&a, &b));
    }

    #[test]
    fn any_overlap_first_box_hits_short_circuit_ok() {
        // The very first pair overlapping must still return true (covers the
        // .any() short-circuit returning early without skipping a real hit).
        let a = [Rect::new(0.0, 0.0, 10.0, 10.0), Rect::new(500.0, 0.0, 1.0, 1.0)];
        let b = [Rect::new(1.0, 1.0, 1.0, 1.0), Rect::new(900.0, 0.0, 1.0, 1.0)];
        assert!(any_overlap(&a, &b));
    }

    #[test]
    fn any_overlap_many_vs_many_all_disjoint() {
        let a: Vec<Rect> = (0..8).map(|i| Rect::new(i as f32 * 100.0, 0.0, 10.0, 10.0)).collect();
        let b: Vec<Rect> = (0..8).map(|i| Rect::new(i as f32 * 100.0 + 50.0, 0.0, 10.0, 10.0)).collect();
        assert!(!any_overlap(&a, &b));
    }

    #[test]
    fn any_clsn_overlap_both_facing_right() {
        // Two right-facing characters overlapping (e.g. a cross-up / corner case).
        // Attacker axis x=0, box 10..30; defender axis x=15, hurt -5..25 -> world 10..40.
        let attack = [Clsn::new(10.0, -20.0, 30.0, 0.0)];
        let hurt = [Clsn::new(-5.0, -20.0, 25.0, 0.0)];
        assert!(any_clsn_overlap(
            &attack,
            Vec2::new(0.0, 0.0),
            Facing::Right,
            &hurt,
            Vec2::new(15.0, 0.0),
            Facing::Right,
        ));
    }

    #[test]
    fn any_clsn_overlap_empty_attack_or_hurt_is_miss() {
        let attack = [Clsn::new(0.0, -10.0, 10.0, 0.0)];
        let none: [Clsn; 0] = [];
        let pos = Vec2::new(0.0, 0.0);
        assert!(!any_clsn_overlap(&attack, pos, Facing::Right, &none, pos, Facing::Left));
        assert!(!any_clsn_overlap(&none, pos, Facing::Right, &attack, pos, Facing::Left));
        assert!(!any_clsn_overlap(&none, pos, Facing::Right, &none, pos, Facing::Left));
    }

    #[test]
    fn any_clsn_overlap_facing_changes_outcome() {
        // Same geometry, only the attacker's facing differs: the punch reaches the
        // defender when facing toward it, and whiffs when facing away.
        let attack = [Clsn::new(10.0, -60.0, 55.0, -40.0)]; // forward of axis
        let hurt = [Clsn::new(-18.0, -70.0, 18.0, 0.0)];
        let a_pos = Vec2::new(0.0, 0.0);
        let d_pos = Vec2::new(60.0, 0.0);
        // Facing right (toward defender on the +X side): hit.
        assert!(any_clsn_overlap(&attack, a_pos, Facing::Right, &hurt, d_pos, Facing::Left));
        // Facing left (away from defender): attack box mirrors to -55..-10, whiff.
        assert!(!any_clsn_overlap(&attack, a_pos, Facing::Left, &hurt, d_pos, Facing::Left));
    }

    #[test]
    fn never_panics_on_non_finite_inputs() {
        // Acceptance criterion 3: "never panics". Feed NaN / infinities through the
        // whole pipeline; results are unspecified but the calls must not panic.
        let nan = f32::NAN;
        let inf = f32::INFINITY;
        let weird = Clsn::new(nan, -inf, inf, nan);
        let _ = weird.to_rect();
        let _ = weird.overlaps(Clsn::new(0.0, 0.0, 1.0, 1.0));
        let placed = place_clsn(weird, Vec2::new(inf, nan), Facing::Left);
        let _ = rects_overlap(&placed, &Rect::new(0.0, 0.0, 1.0, 1.0));
        let _ = any_overlap(&[placed], &[Rect::new(0.0, 0.0, 1.0, 1.0)]);
        let _ = any_clsn_overlap(
            &[weird],
            Vec2::new(nan, inf),
            Facing::Right,
            &[Clsn::new(0.0, 0.0, 1.0, 1.0)],
            Vec2::new(0.0, 0.0),
            Facing::Left,
        );
    }

    #[test]
    fn deterministic_repeated_calls() {
        // Acceptance criterion 3: pure & deterministic — identical inputs always
        // yield identical outputs across repeated invocations.
        let attack = [Clsn::new(10.0, -60.0, 55.0, -40.0)];
        let hurt = [Clsn::new(-18.0, -70.0, 18.0, 0.0)];
        let call = || {
            any_clsn_overlap(
                &attack,
                Vec2::new(0.0, 0.0),
                Facing::Right,
                &hurt,
                Vec2::new(60.0, 0.0),
                Facing::Left,
            )
        };
        let first = call();
        for _ in 0..16 {
            assert_eq!(call(), first);
        }
        assert!(first);
    }

    // ---- Real-fixture test (gated: skips cleanly when test-assets/ is absent) ----

    /// Minimal test-only parser for `ClsnN[i] = x1,y1,x2,y2` lines in a `.air` file.
    /// Returns all boxes matching the given prefix (`"Clsn1["` or `"Clsn2["`).
    /// This lives in tests only; impl code is untouched.
    fn parse_clsn_lines(text: &str, prefix: &str) -> Vec<Clsn> {
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if !line.starts_with(prefix) {
                continue;
            }
            let Some((_, rhs)) = line.split_once('=') else {
                continue;
            };
            let nums: Vec<f32> = rhs
                .split(',')
                .filter_map(|t| t.trim().parse::<f32>().ok())
                .collect();
            if let [x1, y1, x2, y2] = nums[..] {
                out.push(Clsn::new(x1, y1, x2, y2));
            }
        }
        out
    }

    #[test]
    fn fixture_kfm_air_clsn_boxes() {
        // Resolve test-assets relative to the crate, then the workspace root.
        let candidates = [
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-assets/kfm/kfm.air"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/test-assets/kfm/kfm.air"),
        ];
        let path = candidates.iter().find(|p| std::path::Path::new(p).exists());
        let Some(path) = path else {
            eprintln!("skipping fixture_kfm_air_clsn_boxes: test-assets/ not present");
            return;
        };
        let text = std::fs::read_to_string(path).expect("read kfm.air");

        let clsn1 = parse_clsn_lines(&text, "Clsn1[");
        let clsn2 = parse_clsn_lines(&text, "Clsn2[");
        // The KFM fixture is known to contain both attack and hurt boxes.
        assert!(!clsn1.is_empty(), "expected Clsn1 attack boxes in kfm.air");
        assert!(!clsn2.is_empty(), "expected Clsn2 hurt boxes in kfm.air");

        // Every parsed box must normalize to a non-negative, finite rect and never
        // panic going through the placement pipeline (real-world corner orderings
        // in KFM include reversed pairs like `19,0,-10,-80`).
        for &c in clsn1.iter().chain(clsn2.iter()) {
            let r = c.to_rect();
            assert!(r.w >= 0.0 && r.h >= 0.0, "normalized rect has negative size: {r:?}");
            assert!(r.x.is_finite() && r.y.is_finite() && r.w.is_finite() && r.h.is_finite());
            let _ = place_clsn(c, Vec2::new(160.0, 0.0), Facing::Left);
        }

        // Concrete hit scenario from real data: KFM's first jab attack box is
        // `Clsn1[0] = 16,-80, 61,-71` (reaches out to local x=61). Pit it against a
        // KFM standing hurt box `Clsn2[0] = -13,0,16,-79` on an opponent facing left.
        let jab = Clsn::new(16.0, -80.0, 61.0, -71.0);
        assert!(clsn1.contains(&jab), "expected KFM jab Clsn1 box in fixture");
        let stand_hurt = Clsn::new(-13.0, 0.0, 16.0, -79.0);
        assert!(clsn2.contains(&stand_hurt), "expected KFM standing hurt box in fixture");

        // Attacker axis x=80 facing right -> jab world x 96..141, y -80..-71.
        // Defender facing left; place its axis so the hurt box reaches the jab.
        // hurt local x -13..16 mirrored about axis -> width 29 either side.
        let a_pos = Vec2::new(80.0, 0.0);
        // Defender at x=120 facing left: hurt world x = 120 - 16 .. 120 + 13 = 104..133.
        // Overlaps jab 96..141 in x; jab y -80..-71 within hurt y -79..0? -80..-71 vs
        // -79..0 overlaps on -79..-71. -> hit.
        assert!(any_clsn_overlap(
            &[jab],
            a_pos,
            Facing::Right,
            &[stand_hurt],
            Vec2::new(120.0, 0.0),
            Facing::Left,
        ));

        // Push the defender far to the right -> miss.
        assert!(!any_clsn_overlap(
            &[jab],
            a_pos,
            Facing::Right,
            &[stand_hurt],
            Vec2::new(400.0, 0.0),
            Facing::Left,
        ));
    }
}
