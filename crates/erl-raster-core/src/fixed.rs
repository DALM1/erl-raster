//! Fixed-point arithmetic shared by every deterministic kernel.
//!
//! Two helpers carry the whole core: the rounded division by 255 used everywhere a coverage or an
//! alpha blends a channel, and the 16.16 mapping from a destination index to the source position it
//! samples. Both are written so that an eight-lane NEON register reproduces the scalar value exactly,
//! which is what lets the conformance test demand equality rather than a tolerance.

/// Fractional bits of every fixed-point product in the core.
pub const FRAC_BITS: u32 = 16;

/// One, in 16.16.
pub const ONE: i32 = 1 << FRAC_BITS;

/// Half of one, in 16.16. Used to round a sample position to the nearest texel.
pub const HALF: i32 = ONE / 2;

/// Divides by 255 with round-to-nearest.
///
/// The numerator must not exceed `255 * 255`. The identity used is
/// `x / 255 == (x + 128 + ((x + 128) >> 8)) >> 8`, which is exact over that range and, more
/// importantly here, expresses itself as add and shift on sixteen-bit lanes.
#[inline]
#[must_use]
pub const fn div_255(numerator: u32) -> u8 {
    let biased = numerator + 128;
    ((biased + (biased >> 8)) >> 8) as u8
}

/// Multiplies two channel values, both scaled to `0..=255`.
#[inline]
#[must_use]
pub const fn mul_u8(a: u8, b: u8) -> u8 {
    div_255(a as u32 * b as u32)
}

/// Divides a running sum by the number of taps it accumulated, rounding to nearest.
///
/// The sum must be bounded by `255 * count`. Callers enforce the radius ceiling that guarantees it.
#[inline]
#[must_use]
pub const fn div_round(sum: u32, count: u32) -> u8 {
    ((sum + count / 2) / count) as u8
}

/// Multiplies two 16.16 values.
#[inline]
#[must_use]
pub const fn mul_fixed(a: i32, b: i32) -> i32 {
    (((a as i64) * (b as i64)) >> FRAC_BITS) as i32
}

/// Maps an index in `0..dst_len` to the source position it samples, in 16.16.
///
/// The mapping is centre-aligned: destination pixel `i` covers `[i, i+1)` and samples the centre of
/// the corresponding source interval. Without that half-texel alignment, scaling up and back down
/// shifts the whole image by half a pixel at every step, which is exactly the kind of drift the
/// conformance test would then have to forgive.
#[inline]
#[must_use]
pub fn sample_fixed(dst_index: u32, src_len: u32, dst_len: u32) -> i32 {
    debug_assert!(dst_len > 0 && src_len > 0);
    let numerator = (2 * dst_index as u64 + 1) * src_len as u64 * (1 << (FRAC_BITS - 1));
    let position = (numerator / dst_len as u64) as i64 - (1 << (FRAC_BITS - 1));
    position as i32
}

/// Splits a 16.16 value into its integral part and its `0..=65535` fraction.
#[inline]
#[must_use]
pub const fn split_fixed(value: i32) -> (i32, i32) {
    (value >> FRAC_BITS, value & (ONE - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn division_by_255_is_exact_on_every_product_of_two_channels() {
        // The identity is only claimed over this range, so this is the range that gets checked, and
        // exhaustively: a sampled test would leave exactly the boundary cases that matter.
        for a in 0..=255u32 {
            for b in 0..=255u32 {
                let expected = ((a * b) as f64 / 255.0).round() as u8;
                assert_eq!(div_255(a * b), expected, "div_255({a} * {b})");
            }
        }
    }

    #[test]
    fn centre_mapping_is_symmetric() {
        // The first and the last destination pixel of a symmetric scale must be symmetric about the
        // centre of the source, otherwise the sampler is off by a half texel. The mapping is expressed
        // in texel-centre coordinates, where texel `j` sits at integer `j`, so the source's own centre
        // is `(src_len - 1) / 2` and the two ends have to add up to twice that. The tempting `src_len`
        // is one texel too far: it would only hold if texels sat at `j + 0.5`, and it is the identity
        // resample, which samples exact texel centres, that rules that convention out.
        let src_len = 4;
        let first = sample_fixed(0, src_len, 8);
        let last = sample_fixed(7, src_len, 8);
        assert_eq!(
            first + last,
            (src_len as i32 - 1) * ONE,
            "the two ends should average to the source centre"
        );
    }

    #[test]
    fn splitting_recovers_integral_and_fraction() {
        let value = (3 << FRAC_BITS) | 0x7FFF;
        let (integral, fraction) = split_fixed(value);
        assert_eq!(integral, 3);
        assert_eq!(fraction, 0x7FFF);
    }
}
