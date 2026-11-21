//! The published load.
//!
//! The benchmark is only comparable with itself if the work it runs never changes. Every workload
//! here is therefore fixed, procedural and free of any font file, image file or asset: the glyph masks
//! are generated from a seeded generator, the resample source likewise. Two runs six months apart, on
//! a laptop and on a phone, draw exactly the same pixels.
//!
//! The shapes are deliberately chosen to make the arbitrage visible rather than to look like a real
//! screen. `glyphs.tiny` exists to be a case the GPU loses: four small masks are so little work that
//! submitting them costs more than blending them. `mixed.screen` exists to be the case where a single
//! decision for the whole batch is either right for all of it or clearly wrong.

use crate::error::Result;
use crate::geom::{Point, Rect, Size};
use crate::glyph::{GlyphMask, GlyphRun};
use crate::image::{Color, Surface};
use crate::op::{Batch, BlurOp, Op, ResampleFilter, ResampleOp};

/// The reference screen size, a phone in portrait at three times density.
pub const SCREEN: Size = Size::new(1080, 2340);

/// Edge length of a generated glyph mask.
const GLYPH_SIZE: u32 = 26;

/// How many distinct masks the generated atlas holds. Real text caches sit in the hundreds; sixteen is
/// enough to exercise mask reuse without making the setup dominate the first iteration.
const ATLAS_LEN: usize = 16;

/// One named, fixed piece of drawing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workload {
    /// Stable identifier. It appears in the profile and in every report, so changing it invalidates a
    /// stored measurement.
    pub name: &'static str,
    /// What the shape is meant to represent, for whoever reads a report later.
    pub describes: &'static str,
    pub batch: Batch,
}

impl Workload {
    fn new(name: &'static str, describes: &'static str, target: Size) -> Self {
        Self {
            name,
            describes,
            batch: Batch::new(target),
        }
    }
}

/// A seeded generator, so that the published content is identical on every machine forever.
///
/// `xorshift64*` is chosen over anything with more statistical quality for exactly one reason: it is
/// seventeen lines, it is trivially portable, and no future compiler upgrade can change what it
/// produces.
#[derive(Clone, Debug)]
struct Generator(u64);

impl Generator {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u32(&mut self) -> u32 {
        let mut state = self.0;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.0 = state;
        (state >> 32) as u32
    }

    /// A value in `[0, 1)`.
    fn unit(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32
    }
}

/// Distance from a point to a segment, which is how the synthetic glyphs get their shape.
fn distance_to_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let edge_x = bx - ax;
    let edge_y = by - ay;
    let to_point_x = px - ax;
    let to_point_y = py - ay;
    let length_squared = edge_x * edge_x + edge_y * edge_y;
    let along = if length_squared <= f32::EPSILON {
        0.0
    } else {
        ((to_point_x * edge_x + to_point_y * edge_y) / length_squared).clamp(0.0, 1.0)
    };
    let closest_x = ax + along * edge_x;
    let closest_y = ay + along * edge_y;
    ((px - closest_x).powi(2) + (py - closest_y).powi(2)).sqrt()
}

/// Builds one coverage mask: three strokes with an anti-aliased edge.
///
/// The point is not to look like a letter. It is to produce a mask with the properties a real one has,
/// namely a mostly transparent plane with a thin inked region, sparse enough that a backend realises
/// most of its work is the branch that skips, dense enough that the blending loop still matters.
fn synthetic_mask(index: u32, size: u32) -> GlyphMask {
    let mut generator = Generator::new(0x9E37_79B9_7F4A_7C15 ^ (u64::from(index) + 1));
    let extent = size as f32;

    let mut strokes = [((0f32, 0f32), (0f32, 0f32)); 3];
    for stroke in strokes.iter_mut() {
        let ax = 0.15 * extent + generator.unit() * 0.7 * extent;
        let ay = 0.15 * extent + generator.unit() * 0.7 * extent;
        let bx = 0.15 * extent + generator.unit() * 0.7 * extent;
        let by = 0.15 * extent + generator.unit() * 0.7 * extent;
        *stroke = ((ax, ay), (bx, by));
    }

    let half_width = 0.085 * extent;
    let feather = 1.0f32;
    let mut mask = GlyphMask::blank(Size::new(size, size));

    for y in 0..size {
        for x in 0..size {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let mut coverage = 0f32;
            for ((ax, ay), (bx, by)) in strokes.iter() {
                let distance = distance_to_segment(px, py, *ax, *ay, *bx, *by);
                coverage = coverage.max((1.0 - (distance - half_width) / feather).clamp(0.0, 1.0));
            }
            mask.set(x, y, (coverage * 255.0 + 0.5) as u8);
        }
    }

    mask
}

/// Lays glyphs out in reading order over a surface, the way a paragraph would.
fn glyph_positions(target: Size, size: u32, count: u32) -> Vec<Point> {
    let margin = 24i32;
    let advance_x = size as i32 + 6;
    let advance_y = size as i32 + 12;
    let columns = ((target.width as i32 - 2 * margin) / advance_x).max(1) as u32;
    (0..count)
        .map(|index| {
            let column = (index % columns) as i32;
            let row = (index / columns) as i32;
            Point::new(margin + column * advance_x, margin + row * advance_y)
        })
        .collect()
}

/// Builds a run that draws `count` glyphs, reusing the atlas the way a text cache does.
fn glyph_run(target: Size, count: u32) -> GlyphRun {
    let positions = glyph_positions(target, GLYPH_SIZE, count);
    let mut run = GlyphRun::new(Color::from_straight(238, 240, 246, 255));

    let atlas_size = (count as usize).min(ATLAS_LEN);
    for index in 0..atlas_size {
        let mask = synthetic_mask(index as u32, GLYPH_SIZE);
        run.place(mask, positions[index]);
    }
    for index in atlas_size..positions.len() {
        run.reuse(index % atlas_size, positions[index])
            .expect("the index is taken modulo the atlas length, so the mask is always placed");
    }

    run
}

/// Builds a source image with a smooth gradient and a few hard edges.
///
/// The gradient exercises the interpolating part of the sampler; the discs give it a discontinuity,
/// which is where a bilinear filter and a nearest filter visibly disagree and where a divergence
/// between two GPU drivers would show up first.
fn synthetic_source(size: Size) -> Surface {
    let mut surface = Surface::try_new(size).expect("the published source fits the core limits");
    let mut generator = Generator::new(0xDEAD_BEEF_CAFE_1234);
    let discs: Vec<(f32, f32, f32, u8)> = (0..3)
        .map(|_| {
            (
                generator.unit() * size.width as f32,
                generator.unit() * size.height as f32,
                size.width as f32 * (0.12 + generator.unit() * 0.18),
                (128 + generator.next_u32() % 128) as u8,
            )
        })
        .collect();

    for y in 0..size.height {
        for x in 0..size.width {
            let red = (x * 255 / size.width) as u8;
            let green = (y * 255 / size.height) as u8;
            let mut blue = 40u8;
            let mut alpha = 255u8;
            for (cx, cy, radius, tint) in &discs {
                let dx = x as f32 - *cx;
                let dy = y as f32 - *cy;
                if dx * dx + dy * dy <= radius * radius {
                    blue = *tint;
                    alpha = 200;
                }
            }
            surface.set_pixel(x as i32, y as i32, Color::from_straight(red, green, blue, alpha));
        }
    }

    surface
}

/// The whole published set, in report order.
///
/// Every workload is built for the same screen-sized target, because the target is what a backend has
/// to allocate, clear and hand to the compositor; comparing pipelines across different target sizes
/// would fold the target cost into the comparison and make the result unreadable.
pub fn published_workloads() -> Result<Vec<Workload>> {
    let mut workloads = Vec::new();

    let mut tiny = Workload::new(
        "glyphs.tiny",
        "four short masks: the case where submitting to the GPU costs more than the drawing",
        SCREEN,
    );
    tiny.batch.push(Op::FillGlyphs(glyph_run(SCREEN, 4)));
    workloads.push(tiny);

    let mut paragraph = Workload::new(
        "glyphs.paragraph",
        "a screen of text: many small masks, mostly transparent plane",
        SCREEN,
    );
    paragraph.batch.push(Op::FillGlyphs(glyph_run(SCREEN, 96)));
    workloads.push(paragraph);

    let mut resample = Workload::new(
        "resample.card",
        "a 512 estimate scaled into a card: streaming read, hard edges",
        SCREEN,
    );
    resample.batch.push(Op::Resample(ResampleOp::new(
        synthetic_source(Size::new(512, 512)),
        Rect::new(40, 120, 420, 300),
        ResampleFilter::Bilinear,
    )));
    workloads.push(resample);

    let mut blur = Workload::new(
        "blur.panel",
        "a three-pass box blur over a panel: read-modify-write with a scratch buffer",
        SCREEN,
    );
    blur.batch.push(Op::Blur(BlurOp::new(Rect::new(60, 400, 320, 240), 6, 3)));
    workloads.push(blur);

    let mut mixed = Workload::new(
        "mixed.screen",
        "all three kinds in one batch, which is the granularity the dispatcher actually decides at",
        SCREEN,
    );
    mixed
        .batch
        .push(Op::FillGlyphs(glyph_run(SCREEN, 48)))
        .push(Op::Resample(ResampleOp::new(
            synthetic_source(Size::new(256, 256)),
            Rect::new(600, 120, 300, 220),
            ResampleFilter::Bilinear,
        )))
        .push(Op::Blur(BlurOp::new(Rect::new(100, 700, 280, 200), 8, 3)));
    workloads.push(mixed);

    Ok(workloads)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_published_set_is_the_same_every_time() {
        let first = published_workloads().expect("builds");
        let second = published_workloads().expect("builds");
        assert_eq!(first, second);
    }

    #[test]
    fn every_workload_reports_the_kinds_it_holds() {
        let workloads = published_workloads().expect("builds");
        let named = |name: &str| {
            workloads
                .iter()
                .find(|workload| workload.name == name)
                .expect("present")
        };
        assert_eq!(named("glyphs.tiny").batch.kinds().count(), 1);
        assert_eq!(named("mixed.screen").batch.kinds().count(), 3);
    }

    #[test]
    fn the_mixed_batch_is_not_the_sum_of_its_parts_by_accident() {
        let workloads = published_workloads().expect("builds");
        let mixed = workloads
            .iter()
            .find(|workload| workload.name == "mixed.screen")
            .expect("present");
        assert_eq!(mixed.batch.len(), 3);
        assert!(mixed.batch.work().pixels() > 0);
        assert!(mixed.batch.transfer_bytes() > mixed.batch.retained_bytes());
    }

    #[test]
    fn generated_glyphs_are_sparse_but_not_empty() {
        let mask = synthetic_mask(0, GLYPH_SIZE);
        let inked = mask.inked_pixels();
        assert!(inked > 0, "a glyph with no ink is not a glyph");
        assert!(
            inked < mask.size().area(),
            "a glyph that fills its whole box makes the transparent-plane fast path untestable"
        );
    }
}
