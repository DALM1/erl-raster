//! Sizes, points and the integer rectangles every operation is clipped against.

use crate::image::CHANNELS;

/// Pixel dimensions. Sizes are unsigned because a surface of negative extent has no meaning, while a
/// rectangle's origin does, which is why [`Rect`] is signed and this is not.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub const ZERO: Self = Self { width: 0, height: 0 };

    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    #[must_use]
    pub const fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// Bytes a surface of this size occupies, at one four-channel texel per pixel.
    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        self.area() * CHANNELS as u64
    }

    /// Bytes of a single row, i.e. the stride of a tightly packed surface.
    #[must_use]
    pub const fn stride(&self) -> usize {
        self.width as usize * CHANNELS
    }
}

impl Default for Size {
    fn default() -> Self {
        Self::ZERO
    }
}

/// A signed position. Origins are signed because an operation may start partly off the surface, and
/// clipping is cheaper than the alternative of rejecting work the compositor would happily accept.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const ORIGIN: Self = Self { x: 0, y: 0 };

    #[must_use]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

impl Default for Point {
    fn default() -> Self {
        Self::ORIGIN
    }
}

/// An axis-aligned integer rectangle.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    /// A rectangle covering a whole surface.
    #[must_use]
    pub const fn from_size(size: Size) -> Self {
        Self::new(0, 0, size.width, size.height)
    }

    #[must_use]
    pub const fn right(&self) -> i32 {
        self.x + self.width as i32
    }

    #[must_use]
    pub const fn bottom(&self) -> i32 {
        self.y + self.height as i32
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    #[must_use]
    pub const fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    /// The part of this rectangle that lies inside a surface, or `None` if none of it does.
    ///
    /// Returning `None` rather than an empty rectangle keeps "this operation touches nothing" and
    /// "this operation is malformed" apart at the call site.
    #[must_use]
    pub fn clamp_to(&self, size: Size) -> Option<Self> {
        let left = self.x.max(0);
        let top = self.y.max(0);
        let right = self.right().min(size.width as i32);
        let bottom = self.bottom().min(size.height as i32);
        if right <= left || bottom <= top {
            return None;
        }
        Some(Self::new(left, top, (right - left) as u32, (bottom - top) as u32))
    }

    /// The intersection of two rectangles, or `None` when they do not overlap.
    #[must_use]
    pub fn intersect(&self, other: Self) -> Option<Self> {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= left || bottom <= top {
            return None;
        }
        Some(Self::new(left, top, (right - left) as u32, (bottom - top) as u32))
    }

    /// The smallest rectangle containing both, treating an empty rectangle as absent.
    #[must_use]
    pub fn union(&self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return *self;
        }
        let left = self.x.min(other.x);
        let top = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Self::new(left, top, (right - left) as u32, (bottom - top) as u32)
    }

    /// Translate the rectangle.
    #[must_use]
    pub const fn translated(&self, dx: i32, dy: i32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamping_removes_the_part_that_hangs_off_the_surface() {
        let surface = Size::new(100, 50);
        let clipped = Rect::new(-10, -5, 40, 30).clamp_to(surface).expect("overlaps");
        assert_eq!(clipped, Rect::new(0, 0, 30, 25));
    }

    #[test]
    fn a_rectangle_entirely_off_the_surface_clamps_to_nothing() {
        let surface = Size::new(100, 50);
        assert!(Rect::new(200, 0, 10, 10).clamp_to(surface).is_none());
        assert!(Rect::new(0, 0, 0, 10).clamp_to(surface).is_none());
    }

    #[test]
    fn intersection_of_disjoint_rectangles_is_absent() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(20, 20, 5, 5);
        assert!(a.intersect(b).is_none());
    }

    #[test]
    fn union_ignores_empty_operands() {
        let a = Rect::new(3, 4, 5, 6);
        assert_eq!(a.union(Rect::new(0, 0, 0, 0)), a);
        assert_eq!(Rect::new(0, 0, 0, 0).union(a), a);
    }
}
