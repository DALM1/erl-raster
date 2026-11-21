//! The surface: one tightly packed four-channel buffer, premultiplied, eight bits per channel.
//!
//! Premultiplied is the only format the core accepts, and it is not negotiable. Every compositor the
//! library feeds (Core Animation through `CGImage`, HWUI through a `Bitmap`, both GPU backends) wants
//! premultiplied data, so the moment a straight-alpha surface existed the pipeline would have to
//! convert somewhere, and the conversion would land either in the measured region or in a second
//! blend path that the conformance test would then have to cover twice.

use crate::error::{RasterError, Result};
use crate::fixed::mul_u8;
use crate::geom::{Rect, Size};

/// Channels per texel, always.
pub const CHANNELS: usize = 4;

/// The largest single dimension the core will allocate.
///
/// This is not a hardware limit, it is a guard against an arithmetic mistake upstream turning into a
/// multi-gigabyte allocation on a phone. Real surfaces on both target platforms are far below it.
pub const MAX_DIMENSION: u32 = 8192;

/// The largest single surface the core will allocate, in bytes.
pub const MAX_SURFACE_BYTES: u64 = 256 * 1024 * 1024;

/// A premultiplied, eight-bit-per-channel colour.
///
/// `r`, `g` and `b` are already multiplied by `a`. A straight-alpha colour is built through
/// [`Color::from_straight`], and there is no way back: unpremultiplying an eight-bit value loses
/// information, and a round trip would silently change the reference output.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Self = Self { r: 0, g: 0, b: 0, a: 0 };
    pub const OPAQUE_BLACK: Self = Self { r: 0, g: 0, b: 0, a: 255 };
    pub const WHITE: Self = Self { r: 255, g: 255, b: 255, a: 255 };

    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Builds a premultiplied colour from straight-alpha components.
    #[must_use]
    pub const fn from_straight(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self {
            r: mul_u8(r, a),
            g: mul_u8(g, a),
            b: mul_u8(b, a),
            a,
        }
    }

    /// Scales every channel by a coverage value, which is how a glyph's mask becomes an ink.
    #[must_use]
    pub const fn scaled(self, coverage: u8) -> Self {
        Self {
            r: mul_u8(self.r, coverage),
            g: mul_u8(self.g, coverage),
            b: mul_u8(self.b, coverage),
            a: mul_u8(self.a, coverage),
        }
    }

    /// Source-over, with `self` as the source and `backdrop` as the destination.
    ///
    /// Both are premultiplied, so the destination is simply attenuated by the source's alpha and the
    /// result added. The saturating add is not defensive: rounding both halves up can reach 256, and
    /// wrapping there would turn a near-white pixel black.
    #[must_use]
    pub const fn over(self, backdrop: Self) -> Self {
        let inverse = 255 - self.a;
        Self {
            r: self.r.saturating_add(mul_u8(backdrop.r, inverse)),
            g: self.g.saturating_add(mul_u8(backdrop.g, inverse)),
            b: self.b.saturating_add(mul_u8(backdrop.b, inverse)),
            a: self.a.saturating_add(mul_u8(backdrop.a, inverse)),
        }
    }
}

/// A tightly packed RGBA8 premultiplied pixel buffer.
#[derive(Clone, PartialEq, Eq)]
pub struct Surface {
    size: Size,
    data: Vec<u8>,
}

impl core::fmt::Debug for Surface {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The buffer is megabytes, so printing it would be useless. The checksum is printed instead,
        // and it is the thing a conformance failure actually wants to see.
        f.debug_struct("Surface")
            .field("size", &self.size)
            .field("checksum", &self.checksum())
            .finish()
    }
}

impl Surface {
    /// Allocates a transparent surface.
    pub fn try_new(size: Size) -> Result<Self> {
        if size.width == 0 || size.height == 0 {
            // A zero-sized surface is legal and useful: an operation clipped to nothing produces one.
            return Ok(Self { size, data: Vec::new() });
        }
        if size.width > MAX_DIMENSION || size.height > MAX_DIMENSION {
            return Err(RasterError::SizeOutOfRange {
                width: size.width,
                height: size.height,
            });
        }
        let bytes = size.byte_len();
        if bytes > MAX_SURFACE_BYTES {
            return Err(RasterError::AllocationFailed { bytes });
        }
        let mut data = Vec::new();
        data.try_reserve_exact(bytes as usize)
            .map_err(|_| RasterError::AllocationFailed { bytes })?;
        data.resize(bytes as usize, 0);
        Ok(Self { size, data })
    }

    /// Allocates a surface and fills it with one colour.
    pub fn filled(size: Size, color: Color) -> Result<Self> {
        let mut surface = Self::try_new(size)?;
        let texel = [color.r, color.g, color.b, color.a];
        for pixel in surface.data.chunks_exact_mut(CHANNELS) {
            pixel.copy_from_slice(&texel);
        }
        Ok(surface)
    }

    /// Allocates a surface over an existing buffer.
    ///
    /// The buffer is adopted as is, which is what a caller does when the pixels already came from
    /// somewhere else: a decoded image, an `IOSurface`, an `AHardwareBuffer` copy.
    pub fn from_buffer(size: Size, data: Vec<u8>) -> Result<Self> {
        let expected = size.byte_len() as usize;
        if data.len() != expected {
            return Err(RasterError::TargetMismatch {
                batch: (expected as u32, 0),
                target: (data.len() as u32, 0),
            });
        }
        Ok(Self { size, data })
    }

    #[must_use]
    pub const fn size(&self) -> Size {
        self.size
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.size.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.size.height
    }

    /// Bytes per row.
    #[must_use]
    pub const fn stride(&self) -> usize {
        self.size.stride()
    }

    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    #[must_use]
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// The whole surface as a rectangle, in the coordinate space operations use.
    #[must_use]
    pub const fn bounds(&self) -> Rect {
        Rect::from_size(self.size)
    }

    /// Offset of a pixel inside the buffer, or `None` when it falls outside.
    #[must_use]
    pub fn offset(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x >= self.width() as i32 || y >= self.height() as i32 {
            return None;
        }
        Some(y as usize * self.stride() + x as usize * CHANNELS)
    }

    #[must_use]
    pub fn pixel(&self, x: i32, y: i32) -> Color {
        match self.offset(x, y) {
            None => Color::TRANSPARENT,
            Some(at) => Color::new(self.data[at], self.data[at + 1], self.data[at + 2], self.data[at + 3]),
        }
    }

    pub fn set_pixel(&mut self, x: i32, y: i32, color: Color) {
        if let Some(at) = self.offset(x, y) {
            self.data[at] = color.r;
            self.data[at + 1] = color.g;
            self.data[at + 2] = color.b;
            self.data[at + 3] = color.a;
        }
    }

    /// Fills a clipped rectangle with one colour, replacing what is there.
    pub fn clear(&mut self, rect: Rect, color: Color) {
        let Some(region) = rect.clamp_to(self.size) else {
            return;
        };
        let texel = [color.r, color.g, color.b, color.a];
        let stride = self.stride();
        let row_bytes = region.width as usize * CHANNELS;
        for y in 0..region.height as usize {
            let start = (region.y as usize + y) * stride + region.x as usize * CHANNELS;
            for pixel in self.data[start..start + row_bytes].chunks_exact_mut(CHANNELS) {
                pixel.copy_from_slice(&texel);
            }
        }
    }

    /// A cheap fingerprint of the buffer, used by conformance failures to say *where* two backends
    /// parted company rather than only that they did.
    #[must_use]
    pub fn checksum(&self) -> u64 {
        // FNV-1a. Not cryptographic, and it does not need to be: it only has to be stable and cheap
        // enough to run over a ten-megabyte surface in the assertion that reports the mismatch.
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in &self.data {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// Index of the first byte at which two surfaces differ.
    #[must_use]
    pub fn first_difference(&self, other: &Self) -> Option<usize> {
        if self.size != other.size {
            return Some(0);
        }
        self.data
            .iter()
            .zip(other.data.iter())
            .position(|(left, right)| left != right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_surface_is_transparent() {
        let surface = Surface::try_new(Size::new(4, 4)).expect("allocates");
        assert!(surface.data().iter().all(|byte| *byte == 0));
    }

    #[test]
    fn premultiplication_folds_alpha_into_every_channel() {
        let color = Color::from_straight(255, 128, 0, 128);
        assert_eq!(color.a, 128);
        assert_eq!(color.r, 128);
        assert_eq!(color.g, 64);
        assert_eq!(color.b, 0);
    }

    #[test]
    fn source_over_composites_the_expected_way() {
        let backdrop = Color::from_straight(0, 0, 0, 255);
        let source = Color::from_straight(255, 255, 255, 128);
        let result = source.over(backdrop);
        // White at half alpha over black leaves the backdrop at roughly half its value.
        assert_eq!(result.r, 128);
        assert_eq!(result.a, 255);
    }

    #[test]
    fn a_surface_outside_the_supported_range_is_refused() {
        let error = Surface::try_new(Size::new(MAX_DIMENSION + 1, 4)).expect_err("refused");
        assert!(matches!(error, RasterError::SizeOutOfRange { .. }));
    }

    #[test]
    fn clearing_clips_instead_of_panicking() {
        let mut surface = Surface::try_new(Size::new(8, 8)).expect("allocates");
        surface.clear(Rect::new(-4, -4, 8, 8), Color::WHITE);
        assert_eq!(surface.pixel(0, 0), Color::WHITE);
        assert_eq!(surface.pixel(4, 4), Color::TRANSPARENT);
    }
}
