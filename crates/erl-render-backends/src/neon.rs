//! The vectorised compositing row.
//!
//! NEON is architecture-guaranteed on `aarch64`: unlike 32-bit ARM, where Advanced SIMD is an optional
//! extension that has to be detected at run time, every AArch64 core has it. There is therefore no
//! runtime probe here and no fallback path that has to be reachable on the target architecture, only a
//! compile-time one for the machines the open-source benchmark also has to build on.
//!
//! The loop is exact, not approximate. It computes the same `(x + 128 + ((x + 128) >> 8)) >> 8` the
//! scalar path computes, on eight lanes instead of one, and it blends pixels with a zero coverage
//! instead of skipping them, which yields the identical byte because zero ink plus a full-strength
//! backdrop is the backdrop. That is what lets the conformance test demand equality rather than a
//! tolerance, and it is worth more than any amount of extra speed: an approximate fast path is a
//! backend the dispatcher cannot be allowed to pick.

use erl_raster_core::image::Color;

/// Whether a vectorised path exists for this build.
#[must_use]
pub const fn is_available() -> bool {
    cfg!(target_arch = "aarch64")
}

/// The name this path reports, so a report on a laptop does not claim a NEON pipeline it never ran.
#[must_use]
pub const fn report_name() -> &'static str {
    if is_available() {
        "aarch64 NEON"
    } else {
        "scalar reference (no vector path for this architecture)"
    }
}

/// Blends a row of glyph coverage onto the target, vectorised where possible.
///
/// The tail is handed back to the reference. A mask is 26 pixels wide in the published load, so the
/// tail is almost always non-empty and writing a second, nearly identical epilogue would only be a
/// second place for the two paths to drift apart.
pub fn blend_span_row(target: &mut [u8], offset: usize, coverage: &[u8], color: Color, len: usize) {
    #[cfg(target_arch = "aarch64")]
    {
        unsafe {
            imp::blend_span_row(target, offset, coverage, color, len);
        }
    }

    #[cfg(not(target_arch = "aarch64"))]
    {
        // Reached only if a caller builds a vectorised backend on an architecture that has no vector
        // path. `CpuBackend::neon` refuses to exist there, so this is a safety net rather than a route
        // the runtime takes: it produces the right pixels slowly instead of failing.
        erl_raster_core::reference::blend_span_row(target, offset, coverage, color, len);
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use core::arch::aarch64::*;
    use erl_raster_core::image::{Color, CHANNELS};

    /// The scalar division by 255, on eight lanes.
    ///
    /// The inputs never exceed `255 * 255`, so the biased intermediate stays under `65536` and sixteen
    /// bits are enough. Widening to thirty-two would halve the throughput for a range that cannot
    /// occur.
    #[inline]
    unsafe fn divide_by_255(value: uint16x8_t) -> uint16x8_t {
        let biased = vaddq_u16(value, vdupq_n_u16(128));
        vshrq_n_u16::<8>(vaddq_u16(biased, vshrq_n_u16::<8>(biased)))
    }

    /// Blends one channel of eight pixels: ink scaled by coverage, plus the backdrop attenuated by the
    /// complement of that coverage.
    #[inline]
    unsafe fn blend_channel(
        backdrop: uint8x8_t,
        coverage: uint16x8_t,
        channel: u8,
    ) -> uint8x8_t {
        let complement = vsubq_u16(vdupq_n_u16(255), coverage);
        let ink = divide_by_255(vmulq_u16(vdupq_n_u16(u16::from(channel)), coverage));
        let remainder = divide_by_255(vmulq_u16(vmovl_u8(backdrop), complement));
        // Saturating, exactly like the scalar path: rounding both halves up can reach 256 and wrapping
        // there would turn a near-white pixel black.
        vmovn_u16(vqaddq_u16(ink, remainder))
    }

    pub unsafe fn blend_span_row(
        target: &mut [u8],
        offset: usize,
        coverage: &[u8],
        color: Color,
        len: usize,
    ) {
        let mut index = 0usize;

        while index + 8 <= len {
            // Eight pixels is four channels times eight, which is one `LD4`/`ST4` pair: the interleaved
            // layout deinterleaves for free, and the four planes come back in registers.
            let destination = target.as_mut_ptr().add(offset + index * CHANNELS);
            let lanes = vld1_u8(coverage.as_ptr().add(index));
            let lanes = vmovl_u8(lanes);

            let pixels = vld4_u8(destination);
            let blended = uint8x8x4_t(
                blend_channel(pixels.0, lanes, color.r),
                blend_channel(pixels.1, lanes, color.g),
                blend_channel(pixels.2, lanes, color.b),
                blend_channel(pixels.3, lanes, color.a),
            );
            vst4_u8(destination, blended);

            index += 8;
        }

        if index < len {
            erl_raster_core::reference::blend_span_row(
                target,
                offset + index * CHANNELS,
                &coverage[index..],
                color,
                len - index,
            );
        }
    }
}
