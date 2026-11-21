//! Glyph coverage masks and the runs that place them.
//!
//! Text is the case the whole project was built around, and it is also the case where the CPU/GPU
//! arbitrage is least obvious: a short run of small masks is dominated by the cost of handing work to
//! the GPU at all, while a full screen of large ones is dominated by the fill. The core therefore
//! models coverage as its own eight-bit plane, exactly as a real atlas would, rather than folding the
//! glyph into a colour: it keeps the mask reusable across colours, which is what makes a text cache
//! worth anything.

use crate::error::Result;
use crate::geom::{Point, Rect, Size};

/// An eight-bit coverage plane for a single glyph.
#[derive(Clone, PartialEq, Eq)]
pub struct GlyphMask {
    size: Size,
    coverage: Vec<u8>,
}

impl core::fmt::Debug for GlyphMask {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GlyphMask")
            .field("size", &self.size)
            .field("inked", &self.inked_pixels())
            .finish()
    }
}

impl GlyphMask {
    /// Wraps a tightly packed coverage plane.
    pub fn new(size: Size, coverage: Vec<u8>) -> Result<Self> {
        let expected = size.area() as usize;
        if coverage.len() != expected {
            return Err(crate::RasterError::AllocationFailed {
                bytes: expected as u64,
            });
        }
        Ok(Self { size, coverage })
    }

    /// An empty mask of a given size, every pixel fully transparent.
    pub fn blank(size: Size) -> Self {
        Self {
            size,
            coverage: vec![0; size.area() as usize],
        }
    }

    #[must_use]
    pub const fn size(&self) -> Size {
        self.size
    }

    #[must_use]
    pub fn coverage(&self) -> &[u8] {
        &self.coverage
    }

    #[must_use]
    pub fn coverage_mut(&mut self) -> &mut [u8] {
        &mut self.coverage
    }

    #[must_use]
    pub fn at(&self, x: u32, y: u32) -> u8 {
        if x >= self.size.width || y >= self.size.height {
            return 0;
        }
        self.coverage[(y * self.size.width + x) as usize]
    }

    pub fn set(&mut self, x: u32, y: u32, value: u8) {
        if x < self.size.width && y < self.size.height {
            self.coverage[(y * self.size.width + x) as usize] = value;
        }
    }

    /// Number of pixels with a non-zero coverage, which is what a backend actually has to blend.
    #[must_use]
    pub fn inked_pixels(&self) -> u64 {
        self.coverage.iter().filter(|value| **value != 0).count() as u64
    }

    /// The smallest rectangle holding every non-zero pixel.
    #[must_use]
    pub fn inked_bounds(&self) -> Option<Rect> {
        let mut left = self.size.width;
        let mut top = self.size.height;
        let mut right = 0u32;
        let mut bottom = 0u32;
        for y in 0..self.size.height {
            for x in 0..self.size.width {
                if self.at(x, y) != 0 {
                    left = left.min(x);
                    top = top.min(y);
                    right = right.max(x);
                    bottom = bottom.max(y);
                }
            }
        }
        if right < left || bottom < top {
            return None;
        }
        Some(Rect::new(
            left as i32,
            top as i32,
            right - left + 1,
            bottom - top + 1,
        ))
    }
}

/// Where one mask is placed on the surface.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GlyphSpan {
    /// Index into [`GlyphRun::masks`].
    pub mask: usize,
    /// Position of the mask's top-left corner on the target.
    pub origin: Point,
}

/// A colour and the placed masks that draw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlyphRun {
    pub masks: Vec<GlyphMask>,
    pub spans: Vec<GlyphSpan>,
    /// The ink. Premultiplied, and scaled per pixel by the mask's coverage.
    pub color: crate::image::Color,
}

impl GlyphRun {
    #[must_use]
    pub fn new(color: crate::image::Color) -> Self {
        Self {
            masks: Vec::new(),
            spans: Vec::new(),
            color,
        }
    }

    /// Adds a mask and places it, returning the span index.
    pub fn place(&mut self, mask: GlyphMask, origin: Point) -> usize {
        let index = self.masks.len();
        self.masks.push(mask);
        self.spans.push(GlyphSpan { mask: index, origin });
        index
    }

    /// Adds a mask that is already shared with other spans.
    pub fn reuse(&mut self, mask: usize, origin: Point) -> Result<()> {
        if mask >= self.masks.len() {
            return Err(crate::RasterError::UnknownGlyphMask {
                span: mask,
                masks: self.masks.len(),
            });
        }
        self.spans.push(GlyphSpan { mask, origin });
        Ok(())
    }

    /// Total pixels a backend has to blend to draw this run.
    #[must_use]
    pub fn inked_pixels(&self) -> u64 {
        self.spans
            .iter()
            .map(|span| self.masks[span.mask].inked_pixels())
            .sum()
    }

    /// Total bounding box of every span, or `None` when the run draws nothing.
    #[must_use]
    pub fn bounds(&self) -> Option<Rect> {
        let mut bounds: Option<Rect> = None;
        for span in &self.spans {
            let mask = &self.masks[span.mask];
            if mask.inked_bounds().is_none() {
                continue;
            }
            let placed = Rect::new(span.origin.x, span.origin.y, mask.size.width, mask.size.height);
            bounds = Some(match bounds {
                None => placed,
                Some(current) => current.union(placed),
            });
        }
        bounds
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Color;

    #[test]
    fn inked_bounds_ignore_the_transparent_border() {
        let mut mask = GlyphMask::blank(Size::new(8, 8));
        mask.set(3, 4, 255);
        mask.set(5, 6, 200);
        assert_eq!(mask.inked_bounds(), Some(Rect::new(3, 4, 3, 3)));
    }

    #[test]
    fn a_blank_mask_has_no_bounds() {
        let mask = GlyphMask::blank(Size::new(4, 4));
        assert!(mask.inked_bounds().is_none());
        assert_eq!(mask.inked_pixels(), 0);
    }

    #[test]
    fn reusing_an_unknown_mask_is_refused() {
        let mut run = GlyphRun::new(Color::WHITE);
        assert!(run.reuse(3, Point::ORIGIN).is_err());
    }
}
