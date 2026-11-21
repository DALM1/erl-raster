//! The operations a backend is asked to perform, the batch they travel in, and the work they imply.
//!
//! The batch is the unit of decision, not the operation. That is a direct consequence of the second
//! ecueil: a single switch of pipeline pays for binding a pipeline, updating descriptors and
//! synchronising, so a decision taken per primitive would cost more than the primitive it accelerates.
//! Grouping a frame's drawing into one batch is what lets a switch be amortised over the ten or a
//! hundred operations that share it.

use crate::geom::{Rect, Size};
use crate::glyph::GlyphRun;
use crate::image::{Color, Surface, CHANNELS};

/// The largest box radius a blur may use.
///
/// The reference accumulates into a 32-bit sum, so the tap count has to stay far below
/// `u32::MAX / 255`. This ceiling is two orders of magnitude under that and is also far above any
/// radius a user interface would ask for.
pub const MAX_BLUR_RADIUS: u32 = 256;

/// The largest number of passes a blur may chain.
pub const MAX_BLUR_PASSES: u8 = 8;

/// Which kind of work an operation is.
///
/// The dispatcher decides per kind, because the three behave differently enough that one answer for
/// all of them would be wrong for two of them: a glyph run is small, scattered and latency-bound, a
/// resample is a streaming read, a blur is a two-pass sweep with a scratch buffer.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum OpKind {
    /// Blending glyph coverage masks.
    FillGlyphs,
    /// Scaling a source surface into a destination rectangle.
    Resample,
    /// A separable box blur, read from and written back to the target.
    Blur,
}

impl OpKind {
    /// Every kind, in the order a report should list them.
    pub const ALL: [Self; 3] = [Self::FillGlyphs, Self::Resample, Self::Blur];

    #[must_use]
    pub const fn bit(self) -> u8 {
        match self {
            Self::FillGlyphs => 1 << 0,
            Self::Resample => 1 << 1,
            Self::Blur => 1 << 2,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FillGlyphs => "fill-glyphs",
            Self::Resample => "resample",
            Self::Blur => "blur",
        }
    }

    #[must_use]
    pub const fn from_str(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"fill-glyphs" => Some(Self::FillGlyphs),
            b"resample" => Some(Self::Resample),
            b"blur" => Some(Self::Blur),
            _ => None,
        }
    }
}

/// A set of operation kinds, small enough to live in a register and to be built without allocating.
///
/// The dispatcher runs once per frame, and a frame path that allocates a vector is a frame path that
/// occasionally pauses to collect. Three bits cost nothing and cannot.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, Hash)]
pub struct OpKindSet(u8);

impl OpKindSet {
    pub const EMPTY: Self = Self(0);
    pub const ALL: Self = Self(0b111);

    #[must_use]
    pub const fn from_kind(kind: OpKind) -> Self {
        Self(kind.bit())
    }

    /// Rebuilds a set from its bit pattern, dropping bits that name no kind.
    ///
    /// Used when a persisted profile is read back. Masking rather than rejecting is right here: a
    /// profile written by a future version that knows a fourth kind should still yield the three this
    /// version understands, instead of being discarded whole.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & Self::ALL.0)
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(self, kind: OpKind) -> bool {
        self.0 & kind.bit() != 0
    }

    pub fn insert(&mut self, kind: OpKind) {
        self.0 |= kind.bit();
    }

    #[must_use]
    pub const fn count(self) -> u32 {
        self.0.count_ones()
    }

    pub fn iter(self) -> impl Iterator<Item = OpKind> {
        OpKind::ALL.into_iter().filter(move |kind| self.contains(*kind))
    }
}

impl core::fmt::Display for OpKindSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut first = true;
        for kind in self.iter() {
            if !first {
                f.write_str("+")?;
            }
            f.write_str(kind.as_str())?;
            first = false;
        }
        if first {
            f.write_str("none")?;
        }
        Ok(())
    }
}

/// Which filter a resample uses.
///
/// Nearest is kept because it is the fallback on the vector path and because a pixel-exact upscale is
/// a real case, not a degenerate one.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ResampleFilter {
    Nearest,
    Bilinear,
}

impl ResampleFilter {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nearest => "nearest",
            Self::Bilinear => "bilinear",
        }
    }
}

/// Scaling a source surface into a destination rectangle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResampleOp {
    pub source: Surface,
    /// Where the result lands, before clipping. The scaling factor is derived from this rectangle and
    /// the source size, which is why the unclipped rectangle and not the clipped one has to be kept.
    pub destination: Rect,
    pub filter: ResampleFilter,
}

impl ResampleOp {
    #[must_use]
    pub fn new(source: Surface, destination: Rect, filter: ResampleFilter) -> Self {
        Self {
            source,
            destination,
            filter,
        }
    }

    /// Bytes the operation reads on top of the target it writes.
    #[must_use]
    pub fn source_bytes(&self) -> u64 {
        self.source.size().byte_len()
    }
}

/// A separable box blur applied in place over a rectangle of the target.
///
/// In place is deliberate. It is the honest shape of the operation: the compositor reads a region and
/// writes the same region back, which forces the backend to own a scratch buffer and makes the cost of
/// that buffer part of what the benchmark measures rather than a detail hidden inside a convenient
/// out-of-place signature.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BlurOp {
    pub region: Rect,
    /// Box radius of a single pass, in pixels.
    pub radius: u32,
    pub passes: u8,
}

impl BlurOp {
    /// Builds a blur, clamping the radius and the pass count to what the core supports.
    ///
    /// Clamping rather than rejecting is the right call here: the caller asked for a softer edge than
    /// the implementation can give, and the visible difference between radius 256 and radius 4096 is
    /// nothing, while a failure would take the frame down.
    #[must_use]
    pub fn new(region: Rect, radius: u32, passes: u8) -> Self {
        Self {
            region,
            radius: radius.min(MAX_BLUR_RADIUS),
            passes: passes.clamp(1, MAX_BLUR_PASSES),
        }
    }

    /// Number of source rows or columns each output pixel reads.
    #[must_use]
    pub const fn taps(&self) -> u32 {
        self.radius * 2 + 1
    }
}

/// One drawing operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    FillGlyphs(GlyphRun),
    Resample(ResampleOp),
    Blur(BlurOp),
}

impl Op {
    #[must_use]
    pub fn kind(&self) -> OpKind {
        match self {
            Self::FillGlyphs(_) => OpKind::FillGlyphs,
            Self::Resample(_) => OpKind::Resample,
            Self::Blur(_) => OpKind::Blur,
        }
    }

    /// What this operation costs a backend to run, in work units rather than in time.
    #[must_use]
    pub fn work(&self) -> Work {
        match self {
            Self::FillGlyphs(run) => Work {
                operations: 1,
                glyph_pixels: run.inked_pixels(),
                ..Work::default()
            },
            Self::Resample(op) => Work {
                operations: 1,
                resample_pixels: op.destination.size().area(),
                source_bytes: op.source_bytes(),
                ..Work::default()
            },
            Self::Blur(op) => Work {
                operations: 1,
                blur_pixels: op.region.size().area() * u64::from(op.passes) * u64::from(op.taps()) * 2,
                ..Work::default()
            },
        }
    }
}

/// An opaque colour fill, used by the harness to lay down a backdrop without going through a glyph
/// run. Kept out of [`Op`] on purpose: it is scaffolding for the benchmark, not a primitive the
/// dispatcher arbitrates.
#[must_use]
pub fn solid_texel(color: Color) -> [u8; CHANNELS] {
    [color.r, color.g, color.b, color.a]
}

/// The work an operation, or a batch of them, asks of a backend.
///
/// Deliberately not a duration. A backend knows how much work it was handed; how long that took is
/// something only the harness can measure, because only the harness sees the submission and the wait
/// that follows it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Work {
    pub operations: u32,
    /// Pixels actually blended for glyphs, i.e. non-zero coverage only.
    pub glyph_pixels: u64,
    pub resample_pixels: u64,
    /// Blur tap-hits: region area times passes times taps times two axes.
    pub blur_pixels: u64,
    /// Bytes a backend must have in flight beyond the target it writes.
    pub source_bytes: u64,
}

impl Work {
    #[must_use]
    pub const fn pixels(&self) -> u64 {
        self.glyph_pixels + self.resample_pixels + self.blur_pixels
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.operations == 0
    }
}

impl core::ops::AddAssign for Work {
    fn add_assign(&mut self, other: Self) {
        self.operations += other.operations;
        self.glyph_pixels += other.glyph_pixels;
        self.resample_pixels += other.resample_pixels;
        self.blur_pixels += other.blur_pixels;
        self.source_bytes += other.source_bytes;
    }
}

impl core::ops::Add for Work {
    type Output = Self;

    fn add(mut self, other: Self) -> Self {
        self += other;
        self
    }
}

/// One frame's worth of drawing, as handed to a backend in a single call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    target: Size,
    ops: Vec<Op>,
}

impl Batch {
    #[must_use]
    pub fn new(target: Size) -> Self {
        Self {
            target,
            ops: Vec::new(),
        }
    }

    #[must_use]
    pub const fn target(&self) -> Size {
        self.target
    }

    pub fn push(&mut self, op: Op) -> &mut Self {
        self.ops.push(op);
        self
    }

    pub fn extend(&mut self, ops: impl IntoIterator<Item = Op>) -> &mut Self {
        self.ops.extend(ops);
        self
    }

    #[must_use]
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Which kinds of operation this batch contains.
    #[must_use]
    pub fn kinds(&self) -> OpKindSet {
        let mut set = OpKindSet::EMPTY;
        for op in &self.ops {
            set.insert(op.kind());
        }
        set
    }

    /// Total work the batch asks for.
    #[must_use]
    pub fn work(&self) -> Work {
        let mut total = Work::default();
        for op in &self.ops {
            total += op.work();
        }
        total
    }

    /// Bytes the batch holds alive, used by the harness to report the footprint a pipeline needs.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        let mut bytes = 0;
        for op in &self.ops {
            if let Op::Resample(resample) = op {
                bytes += resample.source_bytes();
            }
        }
        bytes
    }

    /// Bytes this batch moves through the pipeline for one execution: what it reads, what it writes.
    #[must_use]
    pub fn transfer_bytes(&self) -> u64 {
        self.retained_bytes() + self.target.byte_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Size;

    #[test]
    fn a_kind_set_iterates_in_a_stable_order() {
        let mut set = OpKindSet::EMPTY;
        set.insert(OpKind::Blur);
        set.insert(OpKind::FillGlyphs);
        let collected: Vec<_> = set.iter().collect();
        assert_eq!(collected, vec![OpKind::FillGlyphs, OpKind::Blur]);
        assert_eq!(set.count(), 2);
    }

    #[test]
    fn a_blur_clamps_the_parameters_it_cannot_serve() {
        let blur = BlurOp::new(Rect::new(0, 0, 10, 10), 10_000, 99);
        assert_eq!(blur.radius, MAX_BLUR_RADIUS);
        assert_eq!(blur.passes, MAX_BLUR_PASSES);
        assert_eq!(BlurOp::new(Rect::new(0, 0, 10, 10), 4, 0).passes, 1);
    }

    #[test]
    fn blur_work_counts_both_axes_and_every_pass() {
        let blur = BlurOp::new(Rect::new(0, 0, 10, 10), 2, 3);
        // 100 pixels, three passes, five taps, two axes.
        assert_eq!(Op::Blur(blur).work().blur_pixels, 100 * 3 * 5 * 2);
    }

    #[test]
    fn kinds_are_reported_without_duplicates() {
        let mut batch = Batch::new(Size::new(16, 16));
        batch.push(Op::Blur(BlurOp::new(Rect::from_size(Size::new(16, 16)), 1, 1)));
        batch.push(Op::Blur(BlurOp::new(Rect::from_size(Size::new(16, 16)), 1, 1)));
        assert_eq!(batch.kinds().count(), 1);
        assert!(batch.kinds().contains(OpKind::Blur));
    }
}
