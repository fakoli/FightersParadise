//! Math primitives for 2D game engine operations.

use serde::{Deserialize, Serialize};
use std::ops::{Add, AddAssign, Div, Mul, Neg, Sub, SubAssign};

/// A 2D vector with generic scalar type.
///
/// Used throughout the engine for positions, velocities, sizes, and offsets.
/// The type parameter `T` is typically `f32` for physics and rendering, or
/// `i32` for pixel-level sprite coordinates.
///
/// # Examples
///
/// ```
/// use fp_core::Vec2;
///
/// let position = Vec2::new(100.0_f32, 200.0);
/// let velocity = Vec2::new(3.0_f32, -5.0);
/// let next_pos = position + velocity;
/// assert_eq!(next_pos, Vec2::new(103.0, 195.0));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Vec2<T> {
    /// The horizontal component.
    pub x: T,
    /// The vertical component.
    pub y: T,
}

impl<T> Vec2<T> {
    /// Creates a new 2D vector from x and y components.
    pub const fn new(x: T, y: T) -> Self {
        Self { x, y }
    }
}

impl<T: Default> Default for Vec2<T> {
    fn default() -> Self {
        Self {
            x: T::default(),
            y: T::default(),
        }
    }
}

impl Vec2<f32> {
    /// A zero vector `(0.0, 0.0)`.
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    /// Returns the squared length of this vector.
    ///
    /// Avoids the square root computation; useful for distance comparisons.
    pub fn length_squared(self) -> f32 {
        self.x * self.x + self.y * self.y
    }

    /// Returns the length (magnitude) of this vector.
    pub fn length(self) -> f32 {
        self.length_squared().sqrt()
    }

    /// Returns a unit vector in the same direction, or zero if the length is zero.
    pub fn normalized(self) -> Self {
        let len = self.length();
        if len == 0.0 {
            Self::ZERO
        } else {
            Self {
                x: self.x / len,
                y: self.y / len,
            }
        }
    }

    /// Returns the dot product of two vectors.
    pub fn dot(self, other: Self) -> f32 {
        self.x * other.x + self.y * other.y
    }
}

impl Vec2<i32> {
    /// A zero vector `(0, 0)`.
    pub const ZERO: Self = Self { x: 0, y: 0 };
}

impl<T: Add<Output = T>> Add for Vec2<T> {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self {
            x: self.x + rhs.x,
            y: self.y + rhs.y,
        }
    }
}

impl<T: AddAssign> AddAssign for Vec2<T> {
    fn add_assign(&mut self, rhs: Self) {
        self.x += rhs.x;
        self.y += rhs.y;
    }
}

impl<T: Sub<Output = T>> Sub for Vec2<T> {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self {
            x: self.x - rhs.x,
            y: self.y - rhs.y,
        }
    }
}

impl<T: SubAssign> SubAssign for Vec2<T> {
    fn sub_assign(&mut self, rhs: Self) {
        self.x -= rhs.x;
        self.y -= rhs.y;
    }
}

impl<T: Neg<Output = T>> Neg for Vec2<T> {
    type Output = Self;
    fn neg(self) -> Self {
        Self {
            x: -self.x,
            y: -self.y,
        }
    }
}

impl<T: Mul<Output = T> + Copy> Mul<T> for Vec2<T> {
    type Output = Self;
    fn mul(self, scalar: T) -> Self {
        Self {
            x: self.x * scalar,
            y: self.y * scalar,
        }
    }
}

impl<T: Div<Output = T> + Copy> Div<T> for Vec2<T> {
    type Output = Self;
    fn div(self, scalar: T) -> Self {
        Self {
            x: self.x / scalar,
            y: self.y / scalar,
        }
    }
}

/// An axis-aligned bounding box defined by its top-left corner and size.
///
/// Used for collision detection (Clsn1/Clsn2 boxes in MUGEN), sprite regions,
/// and camera bounds. Coordinates follow screen convention: Y increases downward.
///
/// # Examples
///
/// ```
/// use fp_core::Rect;
///
/// let a = Rect::new(0.0, 0.0, 10.0, 10.0);
/// let b = Rect::new(5.0, 5.0, 10.0, 10.0);
/// assert!(a.overlaps(&b));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    /// Left edge X coordinate.
    pub x: f32,
    /// Top edge Y coordinate.
    pub y: f32,
    /// Width of the rectangle.
    pub w: f32,
    /// Height of the rectangle.
    pub h: f32,
}

impl Rect {
    /// Creates a new rectangle from position and size.
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// A zero-sized rectangle at the origin.
    pub const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
    };

    /// Returns the right edge X coordinate (`x + w`).
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    /// Returns the bottom edge Y coordinate (`y + h`).
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    /// Returns the center point of this rectangle.
    pub fn center(&self) -> Vec2<f32> {
        Vec2::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// Tests whether this rectangle overlaps with another (AABB collision).
    ///
    /// Returns `true` if the two rectangles share any area. Touching edges
    /// (zero overlap) returns `false`.
    pub fn overlaps(&self, other: &Rect) -> bool {
        self.x < other.right()
            && self.right() > other.x
            && self.y < other.bottom()
            && self.bottom() > other.y
    }

    /// Returns `true` if the given point is inside this rectangle.
    pub fn contains_point(&self, point: Vec2<f32>) -> bool {
        point.x >= self.x && point.x < self.right() && point.y >= self.y && point.y < self.bottom()
    }

    /// Returns the intersection of two rectangles, or `None` if they don't overlap.
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());

        if right > x && bottom > y {
            Some(Rect::new(x, y, right - x, bottom - y))
        } else {
            None
        }
    }
}

impl Default for Rect {
    fn default() -> Self {
        Self::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec2_arithmetic() {
        let a = Vec2::new(3.0_f32, 4.0);
        let b = Vec2::new(1.0_f32, 2.0);

        assert_eq!(a + b, Vec2::new(4.0, 6.0));
        assert_eq!(a - b, Vec2::new(2.0, 2.0));
        assert_eq!(a * 2.0, Vec2::new(6.0, 8.0));
        assert_eq!(-a, Vec2::new(-3.0, -4.0));
    }

    #[test]
    fn vec2_length() {
        let v = Vec2::new(3.0_f32, 4.0);
        assert!((v.length() - 5.0).abs() < f32::EPSILON);
    }

    #[test]
    fn vec2_normalized() {
        let v = Vec2::new(0.0_f32, 5.0);
        let n = v.normalized();
        assert!((n.length() - 1.0).abs() < 1e-6);
        assert!((n.x).abs() < f32::EPSILON);
        assert!((n.y - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn vec2_zero_normalized() {
        let v = Vec2::<f32>::ZERO;
        assert_eq!(v.normalized(), Vec2::<f32>::ZERO);
    }

    #[test]
    fn rect_overlaps() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        let c = Rect::new(20.0, 20.0, 5.0, 5.0);

        assert!(a.overlaps(&b));
        assert!(b.overlaps(&a));
        assert!(!a.overlaps(&c));
    }

    #[test]
    fn rect_touching_does_not_overlap() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(10.0, 0.0, 10.0, 10.0);
        assert!(!a.overlaps(&b));
    }

    #[test]
    fn rect_contains_point() {
        let r = Rect::new(10.0, 10.0, 20.0, 20.0);
        assert!(r.contains_point(Vec2::new(15.0, 15.0)));
        assert!(!r.contains_point(Vec2::new(5.0, 5.0)));
        assert!(r.contains_point(Vec2::new(10.0, 10.0))); // top-left inclusive
        assert!(!r.contains_point(Vec2::new(30.0, 30.0))); // bottom-right exclusive
    }

    #[test]
    fn rect_intersection() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        let inter = a.intersection(&b).unwrap();
        assert_eq!(inter, Rect::new(5.0, 5.0, 5.0, 5.0));

        let c = Rect::new(20.0, 20.0, 5.0, 5.0);
        assert!(a.intersection(&c).is_none());
    }
}
