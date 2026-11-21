//! The deterministic reference path.
//!
//! Every kernel here is integer-only, and the scalar CPU backend runs these very functions. That is
//! the point of the arrangement: the reference is not a second implementation kept in step by
//! discipline, it is *the* implementation, and every vectorised or GPU path is required to reproduce
//! its output exactly. A backend that cannot is wrong, and the dispatcher is told so rather than being
//! asked to look the other way.
//!
//! Two consequences are worth stating plainly, because they constrain everything that follows.
//!
//! The first is that [`fill_glyphs_with`] takes the row blender as a parameter. The scalar path passes
//! the scalar blender, a vectorised backend passes a NEON one, and the traversal, the clipping and the
//! accounting stay in one place. A duplication of the loop would be a duplication of the clipping
//! rules, and clipping is where implementations quietly disagree.
//!
//! The second is that a blur is in place. It reads a region of the target and writes the same region
//! back, which forces whoever implements it to own a scratch buffer and to pay for it inside the
//! measured region, exactly as the GPU would.

use crate::error::{RasterError, Result};
use crate::fixed::{div_round, mul_u8, sample_fixed, split_fixed, HALF, ONE};
use crate::geom::Rect;
use crate::glyph::GlyphRun;
use crate::image::{Color, Surface, CHANNELS};
use crate::op::{BlurOp, Batch, Op, ResampleFilter, ResampleOp, Work};

/// Blends one row of a glyph coverage plane onto a row of the target.
///
/// This is the single hottest inner loop in the library, and the one the vector path replaces. The
/// signature is fixed so that both implementations are interchangeable: a mutable target buffer, the
/// byte offset the span starts at, the coverage bytes, the ink, and how many pixels to blend.
///
/// # Panics
///
/// Panics if the offset, the length or the coverage slice run past the buffer. Callers clip first;
/// this is an internal contract, not a boundary the outside world can reach.
pub fn blend_span_row(
    target: &mut [u8],
    offset: usize,
    coverage: &[u8],
    color: Color,
    len: usize,
) {
    debug_assert!(coverage.len() >= len);
    debug_assert!(offset + len * CHANNELS <= target.len());
    for index in 0..len {
        let cov = coverage[index];
        if cov == 0 {
            // Fully transparent coverage is the common case on the edges of a glyph and the only
            // case where a read-modify-write cycle can be skipped outright.
            continue;
        }
        let at = offset + index * CHANNELS;
        let ink = color.scaled(cov);
        let inverse = 255 - cov;
        target[at] = ink.r.saturating_add(mul_u8(target[at], inverse));
        target[at + 1] = ink.g.saturating_add(mul_u8(target[at + 1], inverse));
        target[at + 2] = ink.b.saturating_add(mul_u8(target[at + 2], inverse));
        target[at + 3] = ink.a.saturating_add(mul_u8(target[at + 3], inverse));
    }
}

/// Draws a glyph run, using the supplied row blender.
///
/// Returns the number of pixels that actually carried ink. The clipping is done once per span, so the
/// inner loop carries no bounds test and the count is exact rather than an upper bound.
pub fn fill_glyphs_with<F>(target: &mut Surface, run: &GlyphRun, blend_row: F) -> Result<u64>
where
    F: Fn(&mut [u8], usize, &[u8], Color, usize),
{
    let width = target.width() as i32;
    let height = target.height() as i32;
    let stride = target.stride();
    let mut touched = 0u64;

    for (index, span) in run.spans.iter().enumerate() {
        let mask = run.masks.get(span.mask).ok_or(RasterError::UnknownGlyphMask {
            span: index,
            masks: run.masks.len(),
        })?;

        let mask_width = mask.size().width as usize;
        let mask_height = mask.size().height as i32;
        if mask_width == 0 || mask_height == 0 {
            continue;
        }

        let first_row = (0 - span.origin.y).max(0);
        let last_row = mask_height.min(height - span.origin.y);
        let first_column = (0 - span.origin.x).max(0);
        let last_column = mask.size().width as i32;
        let last_column = last_column.min(width - span.origin.x);
        if last_row <= first_row || last_column <= first_column {
            continue;
        }

        let len = (last_column - first_column) as usize;
        for row in first_row..last_row {
            let destination_byte = (span.origin.y + row) as usize * stride
                + (span.origin.x + first_column) as usize * CHANNELS;
            let coverage_at = row as usize * mask_width + first_column as usize;
            let coverage = &mask.coverage()[coverage_at..coverage_at + len];
            blend_row(target.data_mut(), destination_byte, coverage, run.color, len);
            touched += coverage.iter().filter(|value| **value != 0).count() as u64;
        }
    }

    Ok(touched)
}

/// Draws a glyph run with the scalar reference blender.
pub fn fill_glyphs(target: &mut Surface, run: &GlyphRun) -> Result<u64> {
    fill_glyphs_with(target, run, blend_span_row)
}

/// Reads one texel, out of bounds yielding transparent rather than clamping.
///
/// Clamping is handled by the samplers, which know which side of the edge they are on. A blanket
/// clamp here would smear the last column across every out-of-range fetch, including the ones the
/// bilinear path makes on purpose.
#[inline]
#[must_use]
fn texel(source: &[u8], stride: usize, x: u32, y: u32) -> Color {
    let at = y as usize * stride + x as usize * CHANNELS;
    match source.get(at..at + CHANNELS) {
        Some(bytes) => Color::new(bytes[0], bytes[1], bytes[2], bytes[3]),
        None => Color::TRANSPARENT,
    }
}

#[inline]
#[must_use]
fn sample_nearest(
    source: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    fx: i32,
    fy: i32,
) -> Color {
    let x = ((fx + HALF) >> 16).clamp(0, width as i32 - 1) as u32;
    let y = ((fy + HALF) >> 16).clamp(0, height as i32 - 1) as u32;
    texel(source, stride, x, y)
}

#[inline]
#[must_use]
fn sample_bilinear(
    source: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    fx: i32,
    fy: i32,
) -> Color {
    if width == 1 && height == 1 {
        return texel(source, stride, 0, 0);
    }

    let last_x = width as i32 - 1;
    let last_y = height as i32 - 1;
    let (mut ix, fraction_x) = split_fixed(fx);
    let (mut iy, fraction_y) = split_fixed(fy);
    ix = ix.clamp(0, last_x);
    iy = iy.clamp(0, last_y);
    let x1 = (ix + 1).min(last_x);
    let y1 = (iy + 1).min(last_y);

    // Three weights are computed and the fourth is taken as the remainder, so the four always sum to
    // exactly one and the sampler cannot darken or lighten a flat region.
    let inverse_x = (ONE - fraction_x) as i64;
    let inverse_y = (ONE - fraction_y) as i64;
    let weight_10 = ((fraction_x as i64 * inverse_y) >> 16) as i32;
    let weight_01 = ((inverse_x * fraction_y as i64) >> 16) as i32;
    let weight_11 = ((fraction_x as i64 * fraction_y as i64) >> 16) as i32;
    let weight_00 = ONE - weight_10 - weight_01 - weight_11;

    let top_left = texel(source, stride, ix as u32, iy as u32);
    let top_right = texel(source, stride, x1 as u32, iy as u32);
    let bottom_left = texel(source, stride, ix as u32, y1 as u32);
    let bottom_right = texel(source, stride, x1 as u32, y1 as u32);

    let blend = |a: u8, b: u8, c: u8, d: u8| -> u8 {
        let sum = i64::from(a) * i64::from(weight_00)
            + i64::from(b) * i64::from(weight_10)
            + i64::from(c) * i64::from(weight_01)
            + i64::from(d) * i64::from(weight_11);
        ((sum + i64::from(HALF)) >> 16) as u8
    };

    Color::new(
        blend(top_left.r, top_right.r, bottom_left.r, bottom_right.r),
        blend(top_left.g, top_right.g, bottom_left.g, bottom_right.g),
        blend(top_left.b, top_right.b, bottom_left.b, bottom_right.b),
        blend(top_left.a, top_right.a, bottom_left.a, bottom_right.a),
    )
}

/// Scales a source surface into a destination rectangle, compositing source-over onto the target.
///
/// Returns the number of destination pixels written.
///
/// The mapping is derived from the *unclipped* destination rectangle, not from the part that survives
/// clipping. Deriving it from the clipped one would rescale the image as it slid off the edge of the
/// surface, which is the kind of subtle wrongness that only shows up in an animation.
pub fn resample(target: &mut Surface, op: &ResampleOp) -> Result<u64> {
    let Some(region) = op.destination.clamp_to(target.size()) else {
        return Ok(0);
    };

    let source = &op.source;
    let source_width = source.width();
    let source_height = source.height();
    if source_width == 0 || source_height == 0 {
        return Ok(0);
    }

    let destination_width = op.destination.width;
    let destination_height = op.destination.height;
    if destination_width == 0 || destination_height == 0 {
        return Ok(0);
    }

    let source_stride = source.stride();
    let source_data = source.data();
    let target_stride = target.stride();
    let mut written = 0u64;

    for row in 0..region.height {
        let destination_y = (region.y - op.destination.y) as u32 + row;
        let fixed_y = sample_fixed(destination_y, source_height, destination_height);
        let target_row = (region.y + row as i32) as usize * target_stride;

        for column in 0..region.width {
            let destination_x = (region.x - op.destination.x) as u32 + column;
            let fixed_x = sample_fixed(destination_x, source_width, destination_width);

            let sampled = match op.filter {
                ResampleFilter::Nearest => {
                    sample_nearest(source_data, source_stride, source_width, source_height, fixed_x, fixed_y)
                }
                ResampleFilter::Bilinear => {
                    sample_bilinear(source_data, source_stride, source_width, source_height, fixed_x, fixed_y)
                }
            };

            let at = target_row + (region.x + column as i32) as usize * CHANNELS;
            let composited = {
                let backdrop = Color::new(
                    target.data()[at],
                    target.data()[at + 1],
                    target.data()[at + 2],
                    target.data()[at + 3],
                );
                sampled.over(backdrop)
            };

            let destination = target.data_mut();
            destination[at] = composited.r;
            destination[at + 1] = composited.g;
            destination[at + 2] = composited.b;
            destination[at + 3] = composited.a;
            written += 1;
        }
    }

    Ok(written)
}

/// One horizontal box pass, edge pixels duplicating the border.
#[must_use]
fn box_horizontal(
    source: &[u8],
    source_stride: usize,
    region: Rect,
    radius: u32,
    output: &mut [u8],
) -> u64 {
    let width = region.width as i32;
    let row_bytes = region.width as usize * CHANNELS;
    let radius = radius as i32;
    let count = (radius * 2 + 1) as u32;

    for row in 0..region.height as i32 {
        let source_row = (region.y + row) as usize * source_stride + region.x as usize * CHANNELS;
        let output_row = row as usize * row_bytes;
        for column in 0..width {
            for channel in 0..CHANNELS {
                let mut sum = 0u32;
                for tap in -radius..=radius {
                    let sampled = (column + tap).clamp(0, width - 1) as usize;
                    sum += u32::from(source[source_row + sampled * CHANNELS + channel]);
                }
                output[output_row + column as usize * CHANNELS + channel] = div_round(sum, count);
            }
        }
    }

    region.size().area()
}

/// One vertical box pass over an already horizontally blurred plane.
#[must_use]
fn box_vertical(
    source: &[u8],
    source_stride: usize,
    width: usize,
    height: usize,
    radius: u32,
    output: &mut [u8],
) -> u64 {
    let radius = radius as i32;
    let count = (radius * 2 + 1) as u32;

    for row in 0..height as i32 {
        for column in 0..width {
            for channel in 0..CHANNELS {
                let mut sum = 0u32;
                for tap in -radius..=radius {
                    let sampled = (row + tap).clamp(0, height as i32 - 1) as usize;
                    sum += u32::from(source[sampled * source_stride + column * CHANNELS + channel]);
                }
                output[row as usize * source_stride + column * CHANNELS + channel] = div_round(sum, count);
            }
        }
    }

    (width * height) as u64
}

/// Blurs a region of the target in place.
///
/// Returns the number of region pixels the blur touched, before the pass and tap multiplier that turns
/// it into work units. Edge pixels duplicate the border rather than being renormalised by a smaller
/// tap count: a renormalisation makes the divisor vary per pixel, which costs a division per channel
/// and gives the vector path nothing to line up on.
pub fn blur(target: &mut Surface, op: &BlurOp) -> Result<u64> {
    let Some(region) = op.region.clamp_to(target.size()) else {
        return Ok(0);
    };
    if region.is_empty() || op.radius == 0 {
        return Ok(0);
    }

    let stride = target.stride();
    let row_bytes = region.width as usize * CHANNELS;
    let mut horizontal = vec![0u8; row_bytes * region.height as usize];
    let mut vertical = vec![0u8; row_bytes * region.height as usize];

    // The two passes report the area they touched, which is the same region on every pass, so the last
    // vertical pass already carries the answer this function returns.
    let mut touched = 0;
    for _ in 0..op.passes {
        {
            let source = target.data();
            let _ = box_horizontal(source, stride, region, op.radius, &mut horizontal);
        }
        touched = box_vertical(
            &horizontal,
            row_bytes,
            region.width as usize,
            region.height as usize,
            op.radius,
            &mut vertical,
        );
        let destination = target.data_mut();
        for row in 0..region.height as usize {
            let target_at = (region.y as usize + row) * stride + region.x as usize * CHANNELS;
            let source_at = row * row_bytes;
            destination[target_at..target_at + row_bytes]
                .copy_from_slice(&vertical[source_at..source_at + row_bytes]);
        }
    }

    Ok(touched)
}

/// Runs a whole batch against a target, with the scalar reference kernels.
///
/// The target must be the size the batch was built for. That check is the only validation the hot
/// path performs, and it is worth its cost: a mismatched pair would otherwise write past the end of a
/// buffer, which is a crash rather than a wrong pixel.
pub fn execute(target: &mut Surface, batch: &Batch) -> Result<Work> {
    if batch.target() != target.size() {
        return Err(RasterError::TargetMismatch {
            batch: (batch.target().width, batch.target().height),
            target: (target.width(), target.height()),
        });
    }

    let mut work = Work::default();
    for op in batch.ops() {
        work += match op {
            Op::FillGlyphs(run) => Work {
                operations: 1,
                glyph_pixels: fill_glyphs(target, run)?,
                ..Work::default()
            },
            Op::Resample(resample_op) => Work {
                operations: 1,
                resample_pixels: resample(target, resample_op)?,
                source_bytes: resample_op.source_bytes(),
                ..Work::default()
            },
            Op::Blur(blur_op) => {
                let area = blur(target, blur_op)?;
                Work {
                    operations: 1,
                    blur_pixels: area
                        * u64::from(blur_op.passes)
                        * u64::from(blur_op.taps())
                        * 2,
                    ..Work::default()
                }
            }
        };
    }

    Ok(work)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::{Point, Size};
    use crate::glyph::GlyphMask;
    use crate::op::BlurOp;

    #[test]
    fn a_blur_leaves_a_flat_region_flat() {
        // The strongest statement a box blur has to satisfy: it averages, so a region where every
        // pixel is the same must come back unchanged. A rounding error anywhere shows up immediately.
        let size = Size::new(24, 24);
        let mut surface = Surface::filled(size, Color::from_straight(90, 140, 200, 255)).expect("allocates");
        let before = surface.clone();
        blur(&mut surface, &BlurOp::new(Rect::from_size(size), 5, 3)).expect("blurs");
        assert_eq!(surface, before);
    }

    #[test]
    fn a_blur_spreads_a_single_bright_pixel() {
        let size = Size::new(16, 16);
        let mut surface = Surface::try_new(size).expect("allocates");
        surface.set_pixel(8, 8, Color::from_straight(255, 255, 255, 255));
        blur(&mut surface, &BlurOp::new(Rect::from_size(size), 2, 1)).expect("blurs");
        // A box blur averages, and the average is a *separable* one: five taps across then five down
        // leave the single bright texel spread over twenty-five positions, so the centre comes back at
        // 255 divided twice, not at 255. Expecting the original value here would only be right for a
        // blur that sums instead of averaging, which the flat-region test already rules out.
        let expected = div_round(u32::from(div_round(255, 5)), 5);
        assert_eq!(surface.pixel(8, 8).a, expected);
        assert!(surface.pixel(7, 8).a > 0, "the blur should have spread outwards");
        assert_eq!(surface.pixel(0, 0).a, 0, "and not reached the far corner");
    }

    #[test]
    fn resampling_at_the_same_size_is_close_to_the_identity() {
        let size = Size::new(32, 32);
        let mut source = Surface::try_new(size).expect("allocates");
        for y in 0..32 {
            for x in 0..32 {
                source.set_pixel(x, y, Color::from_straight((x * 8) as u8, (y * 8) as u8, 10, 255));
            }
        }
        let mut target = Surface::try_new(size).expect("allocates");
        let op = ResampleOp::new(source.clone(), Rect::from_size(size), ResampleFilter::Bilinear);
        resample(&mut target, &op).expect("resamples");
        // An identity scale samples exactly on texel centres, so every pixel must come back exactly.
        assert_eq!(target, source);
    }

    #[test]
    fn a_nearest_resample_picks_the_source_pixel_under_each_destination_centre() {
        let source_size = Size::new(2, 1);
        let mut source = Surface::try_new(source_size).expect("allocates");
        source.set_pixel(0, 0, Color::new(10, 0, 0, 255));
        source.set_pixel(1, 0, Color::new(200, 0, 0, 255));

        let target_size = Size::new(4, 1);
        let mut target = Surface::try_new(target_size).expect("allocates");
        let op = ResampleOp::new(source, Rect::from_size(target_size), ResampleFilter::Nearest);
        resample(&mut target, &op).expect("resamples");

        assert_eq!(target.pixel(0, 0).r, 10);
        assert_eq!(target.pixel(1, 0).r, 10);
        assert_eq!(target.pixel(2, 0).r, 200);
        assert_eq!(target.pixel(3, 0).r, 200);
    }

    #[test]
    fn glyph_coverage_blends_proportionally() {
        let size = Size::new(4, 1);
        let mut target = Surface::filled(size, Color::OPAQUE_BLACK).expect("allocates");
        let mut mask = GlyphMask::blank(Size::new(4, 1));
        mask.set(0, 0, 255);
        mask.set(1, 0, 128);
        let mut run = GlyphRun::new(Color::WHITE);
        run.place(mask, Point::ORIGIN);

        let touched = fill_glyphs(&mut target, &run).expect("draws");
        assert_eq!(touched, 2);
        assert_eq!(target.pixel(0, 0).r, 255);
        assert_eq!(target.pixel(1, 0).r, 128);
        assert_eq!(target.pixel(2, 0).r, 0);
    }

    #[test]
    fn a_run_clipped_entirely_off_the_surface_changes_nothing() {
        let size = Size::new(8, 8);
        let mut target = Surface::filled(size, Color::OPAQUE_BLACK).expect("allocates");
        let before = target.clone();
        let mut mask = GlyphMask::blank(Size::new(4, 4));
        mask.set(1, 1, 255);
        let mut run = GlyphRun::new(Color::WHITE);
        run.place(mask, Point::new(100, 100));
        assert_eq!(fill_glyphs(&mut target, &run).expect("draws"), 0);
        assert_eq!(target, before);
    }

    #[test]
    fn a_run_partly_off_the_surface_clips_the_right_way() {
        // Only the two rightmost columns of the mask should land on the surface.
        let size = Size::new(2, 1);
        let mut target = Surface::try_new(size).expect("allocates");
        let mut mask = GlyphMask::blank(Size::new(4, 1));
        mask.set(0, 0, 255);
        mask.set(1, 0, 255);
        mask.set(2, 0, 255);
        mask.set(3, 0, 255);
        let mut run = GlyphRun::new(Color::WHITE);
        run.place(mask, Point::new(-2, 0));
        assert_eq!(fill_glyphs(&mut target, &run).expect("draws"), 2);
        assert_eq!(target.pixel(0, 0).a, 255);
        assert_eq!(target.pixel(1, 0).a, 255);
    }

    #[test]
    fn a_batch_built_for_another_size_is_refused() {
        let batch = Batch::new(Size::new(4, 4));
        let mut target = Surface::try_new(Size::new(8, 8)).expect("allocates");
        assert!(matches!(
            execute(&mut target, &batch),
            Err(RasterError::TargetMismatch { .. })
        ));
    }
}
