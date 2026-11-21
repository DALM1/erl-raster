//! The Metal bridge: the first real implementation behind [`GpuSurfaceBridge`].
//!
//! Four decisions shape this file, and each of them was taken against a cheaper alternative.
//!
//! **The kernels are compute dispatches, not render passes.** The reference is integer-only from end
//! to end, and that is the property the whole library is built on: it is what lets the conformance
//! test demand equality instead of a tolerance. A fragment shader could reproduce the same arithmetic,
//! but a glyph run has to read the pixel it is about to blend, and a render pass cannot read the
//! attachment it is writing without a framebuffer-fetch extension. The choice was therefore between a
//! ping-pong pair of textures, a copy per operation and a second format, or a storage buffer that is
//! read and written in place. The storage buffer wins, and it wins by a wide margin: one buffer, one
//! binding, no conversion, and the same eight-bit values the reference holds.
//!
//! **Everything is a buffer, nothing is a texture.** The target is a host-memory [`Surface`] whose
//! layout is already tightly packed four-byte texels, and a `u32` per pixel is that layout read as
//! words. A texture would force a staging buffer with rows padded to 256 bytes on the way out, which
//! means a de-striding pass, which means one more kernel and one more place for the two paths to part
//! company. Reading the surface as words removes all of it.
//!
//! **The work is reported from the batch, not counted on the device.** The `Work` a backend returns is
//! a property of the request, and the reference derives it from the same request. The GPU kernel could
//! count the pixels it blended and hand the count back through an atomic, but that would add a readback
//! to every frame to learn something the host already knows. The host therefore reproduces the
//! reference's clipping arithmetic in [`report_work`], once, in `prepare`, outside every measured
//! region. What it reports is what the reference would report, because it is computed the same way.
//!
//! **The measurement rule is the one the seam states.** The target arrives in host memory, so the
//! upload of the surface and the download of the result are both inside the region the harness times,
//! and `transferred_bytes` counts them. A backend that rendered into a texture it already owned would
//! post a far better number and describe a situation the caller is never in.
//!
//! The module is compiled only on macOS. It has to be: `wgpu`'s `metal` feature exists only under
//! `target_vendor = "apple"`, and `Instance::new` panics when no compiled backend is available for the
//! running platform. Gating the module rather than the call sites keeps a Linux or Android build from
//! even parsing an API that cannot exist there; the Android adapter will arrive as its own module with
//! its own gate.

use std::borrow::Cow;
use std::sync::Arc;

use wgpu::util::DeviceExt;

use erl_raster_core::geom::Size;
use erl_raster_core::image::{Surface, CHANNELS, MAX_DIMENSION, MAX_SURFACE_BYTES};
use erl_raster_core::op::{Batch, Op, Work};
use erl_raster_core::{RasterError, Result};

use crate::contract::{BackendId, Executed};
use crate::gpu::{GpuDeviceInfo, GpuSurfaceBridge};

/// The label every device object carries, so a Metal capture is readable.
const LABEL: &str = "erl-raster";

/// How many invocations a compute workgroup holds on each axis.
const WORKGROUP: u32 = 8;

// -------------------------------------------------------------------------------------------------
// Shaders
// -------------------------------------------------------------------------------------------------

/// The helpers every kernel shares.
///
/// These are transcriptions, not reimplementations. `div_255` is the same identity the core uses,
/// written so that a `u32` lane reproduces it exactly; `unpack` and `pack` are the little-endian view
/// of the four channel bytes the host buffer already holds. Nothing here converts to a float, because
/// a float is the one thing that would break the byte-for-byte claim: `mul_u8` has to round the way
/// `(x + 128 + ((x + 128) >> 8)) >> 8` rounds, and no hardware rounding mode promises that.
const PRELUDE: &str = r#"
fn div_255(n: u32) -> u32 {
    let biased = n + 128u;
    return (biased + (biased >> 8u)) >> 8u;
}

fn mul_u8(a: u32, b: u32) -> u32 {
    return div_255(a * b);
}

fn div_round(sum: u32, count: u32) -> u32 {
    return (sum + count / 2u) / count;
}

fn sat_add(a: u32, b: u32) -> u32 {
    let total = a + b;
    if (total > 255u) {
        return 255u;
    }
    return total;
}

fn unpack_pixel(pixel: u32) -> vec4<u32> {
    return vec4<u32>(
        pixel & 255u,
        (pixel >> 8u) & 255u,
        (pixel >> 16u) & 255u,
        (pixel >> 24u) & 255u,
    );
}

fn pack_pixel(color: vec4<u32>) -> u32 {
    return (color.x & 255u) | ((color.y & 255u) << 8u) | ((color.z & 255u) << 16u) | ((color.w & 255u) << 24u);
}
"#;

/// The glyph kernel.
///
/// One workgroup per span, and the span's own geometry is what the workgroup iterates. That shape is
/// deliberate: the alternative, a flat list of (pixel, coverage) pairs, is faster for a dense run and
/// impossible for a sparse one, because a sparse run would spend its invocations on the branch that
/// skips. Here every invocation that exists has a mask pixel under it, and the skip is a single test
/// against zero.
///
/// The clipping is the reference's, arithmetic included. `first_row`, `last_row`, `first_column` and
/// `last_column` are computed the same way, so a span that hangs off the surface blends exactly the
/// pixels the reference blends, and no others. A span entirely off the surface returns before any
/// invocation reads a pixel.
///
/// The one thing this does not reproduce is the order two *overlapping* spans of the same run are
/// applied in: the reference applies them in sequence, while these workgroups run concurrently. The
/// published load never overlaps, because text laid out by a compositor does not overlap itself, and a
/// run that did would have to be serialised into one workgroup per span. It is written down here
/// because a silent difference is worse than a stated one.
const GLYPHS: &str = r#"
struct Span {
    mask_offset: u32,
    mask_width: u32,
    mask_height: u32,
    origin_x: i32,
    origin_y: i32,
    pad_a: u32,
    pad_b: u32,
    pad_c: u32,
};

struct GlyphParams {
    target_width: u32,
    target_height: u32,
    ink: u32,
    span_count: u32,
};

@group(0) @binding(0) var<storage, read_write> pixels: array<u32>;
@group(0) @binding(1) var<storage, read> masks: array<u32>;
@group(0) @binding(2) var<storage, read> spans: array<Span>;
@group(0) @binding(3) var<uniform> params: GlyphParams;

fn coverage_at(byte_index: u32) -> u32 {
    let word = masks[byte_index >> 2u];
    return (word >> ((byte_index & 3u) * 8u)) & 255u;
}

@compute @workgroup_size(8, 8)
fn main(@builtin(workgroup_id) group: vec3<u32>, @builtin(local_invocation_id) local: vec3<u32>) {
    if (group.x >= params.span_count) {
        return;
    }
    let span = spans[group.x];
    let mask_width = i32(span.mask_width);
    let mask_height = i32(span.mask_height);
    if (mask_width == 0 || mask_height == 0) {
        return;
    }
    let width = i32(params.target_width);
    let height = i32(params.target_height);
    let first_row = max(0, -span.origin_y);
    let last_row = min(mask_height, height - span.origin_y);
    let first_column = max(0, -span.origin_x);
    let last_column = min(mask_width, width - span.origin_x);
    if (last_row <= first_row || last_column <= first_column) {
        return;
    }

    let ink = unpack_pixel(params.ink);
    var row = first_row + i32(local.y);
    while (row < last_row) {
        var column = first_column + i32(local.x);
        while (column < last_column) {
            let coverage = coverage_at(span.mask_offset + u32(row * mask_width + column));
            if (coverage != 0u) {
                let at = u32((span.origin_y + row) * width + span.origin_x + column);
                let backdrop = unpack_pixel(pixels[at]);
                let inverse = 255u - coverage;
                let blended = vec4<u32>(
                    sat_add(mul_u8(ink.x, coverage), mul_u8(backdrop.x, inverse)),
                    sat_add(mul_u8(ink.y, coverage), mul_u8(backdrop.y, inverse)),
                    sat_add(mul_u8(ink.z, coverage), mul_u8(backdrop.z, inverse)),
                    sat_add(mul_u8(ink.w, coverage), mul_u8(backdrop.w, inverse)),
                );
                pixels[at] = pack_pixel(blended);
            }
            column = column + 8;
        }
        row = row + 8;
    }
}
"#;

/// The resample kernel.
///
/// The fixed-point mapping is the interesting part. `sample_fixed` in the core is `u64` arithmetic:
/// `(2 * dst + 1) * src_len * 32768 / dst_len - 32768`, and a `u32` version of that expression
/// overflows the moment a source is more than a few hundred pixels wide. The reformulation below keeps
/// every intermediate inside a `u32` without changing a single result: dividing `(2 * dst + 1) *
/// src_len` by `dst_len` first yields a quotient and a remainder whose scaled contributions rebuild
/// the original fixed-point value exactly, because `floor((q * n + r) * 32768 / n) = q * 32768 +
/// floor(r * 32768 / n)`. The quotient is bounded by twice the source length, so `q * 32768` cannot
/// overflow, and `r * 32768` is bounded by the destination length, which the core caps at `8192`.
///
/// The bilinear weights are computed in `u32` for the same reason: the core multiplies two `i32`
/// values, and `65535 * 65536` is just past `i32::MAX`. The product fits a `u32` with room to spare,
/// and every operand is non-negative, so the shift gives the same answer the core's `i64` shift does.
const RESAMPLE: &str = r#"
struct ResampleParams {
    target_width: u32,
    target_height: u32,
    source_width: u32,
    source_height: u32,
    region_x: u32,
    region_y: u32,
    region_width: u32,
    region_height: u32,
    destination_x: i32,
    destination_y: i32,
    destination_width: u32,
    destination_height: u32,
    filter_mode: u32,
    pad_a: u32,
    pad_b: u32,
    pad_c: u32,
};

@group(0) @binding(0) var<storage, read_write> pixels: array<u32>;
@group(0) @binding(1) var<storage, read> source: array<u32>;
@group(0) @binding(2) var<uniform> params: ResampleParams;

fn sample_fixed(dst_index: u32, src_len: u32, dst_len: u32) -> i32 {
    let numerator = (2u * dst_index + 1u) * src_len;
    let quotient = numerator / dst_len;
    let remainder = numerator % dst_len;
    let fraction = (remainder * 32768u) / dst_len;
    return i32(quotient * 32768u + fraction) - 32768;
}

fn split_fixed(value: i32) -> vec2<i32> {
    return vec2<i32>(value >> 16, value & 65535);
}

fn texel(x: u32, y: u32) -> vec4<u32> {
    return unpack_pixel(source[y * params.source_width + x]);
}

fn sample_nearest(fixed_x: i32, fixed_y: i32) -> vec4<u32> {
    let x = clamp((fixed_x + 32768) >> 16, 0, i32(params.source_width) - 1);
    let y = clamp((fixed_y + 32768) >> 16, 0, i32(params.source_height) - 1);
    return texel(u32(x), u32(y));
}

fn blend_channel(a: u32, b: u32, c: u32, d: u32, w00: i32, w10: i32, w01: i32, w11: i32) -> u32 {
    let sum = i32(a) * w00 + i32(b) * w10 + i32(c) * w01 + i32(d) * w11;
    return u32((sum + 32768) >> 16);
}

fn sample_bilinear(fixed_x: i32, fixed_y: i32) -> vec4<u32> {
    if (params.source_width == 1u && params.source_height == 1u) {
        return texel(0u, 0u);
    }

    let last_x = i32(params.source_width) - 1;
    let last_y = i32(params.source_height) - 1;
    let split_x = split_fixed(fixed_x);
    let split_y = split_fixed(fixed_y);
    let ix = clamp(split_x.x, 0, last_x);
    let iy = clamp(split_y.x, 0, last_y);
    let fraction_x = split_x.y;
    let fraction_y = split_y.y;
    let x1 = min(ix + 1, last_x);
    let y1 = min(iy + 1, last_y);

    let inverse_x = 65536 - fraction_x;
    let inverse_y = 65536 - fraction_y;
    let weight_10 = i32((u32(fraction_x) * u32(inverse_y)) >> 16u);
    let weight_01 = i32((u32(inverse_x) * u32(fraction_y)) >> 16u);
    let weight_11 = i32((u32(fraction_x) * u32(fraction_y)) >> 16u);
    let weight_00 = 65536 - weight_10 - weight_01 - weight_11;

    let top_left = texel(u32(ix), u32(iy));
    let top_right = texel(u32(x1), u32(iy));
    let bottom_left = texel(u32(ix), u32(y1));
    let bottom_right = texel(u32(x1), u32(y1));

    return vec4<u32>(
        blend_channel(top_left.x, top_right.x, bottom_left.x, bottom_right.x, weight_00, weight_10, weight_01, weight_11),
        blend_channel(top_left.y, top_right.y, bottom_left.y, bottom_right.y, weight_00, weight_10, weight_01, weight_11),
        blend_channel(top_left.z, top_right.z, bottom_left.z, bottom_right.z, weight_00, weight_10, weight_01, weight_11),
        blend_channel(top_left.w, top_right.w, bottom_left.w, bottom_right.w, weight_00, weight_10, weight_01, weight_11),
    );
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= params.region_width || id.y >= params.region_height) {
        return;
    }

    let destination_x = u32(i32(params.region_x) - params.destination_x + i32(id.x));
    let destination_y = u32(i32(params.region_y) - params.destination_y + i32(id.y));
    let fixed_x = sample_fixed(destination_x, params.source_width, params.destination_width);
    let fixed_y = sample_fixed(destination_y, params.source_height, params.destination_height);

    var sampled = vec4<u32>(0u, 0u, 0u, 0u);
    if (params.filter_mode == 1u) {
        sampled = sample_bilinear(fixed_x, fixed_y);
    } else {
        sampled = sample_nearest(fixed_x, fixed_y);
    }

    let at = (params.region_y + id.y) * params.target_width + params.region_x + id.x;
    let backdrop = unpack_pixel(pixels[at]);
    let inverse = 255u - sampled.w;
    pixels[at] = pack_pixel(vec4<u32>(
        sat_add(sampled.x, mul_u8(backdrop.x, inverse)),
        sat_add(sampled.y, mul_u8(backdrop.y, inverse)),
        sat_add(sampled.z, mul_u8(backdrop.z, inverse)),
        sat_add(sampled.w, mul_u8(backdrop.w, inverse)),
    ));
}
"#;

/// The box blur kernel, one axis per dispatch.
///
/// The reference chains two scratch planes and copies the second back into the target after every
/// pass, and this does the same with one scratch buffer instead of two. The horizontal dispatch reads
/// the target and writes the scratch, the vertical one reads the scratch and writes the target, and the
/// two are separate passes because a single pass gives no guarantee that one dispatch's writes are
/// visible to the next. Separate passes are the barrier.
///
/// Edge pixels duplicate the border rather than dividing by a smaller tap count, exactly as the
/// reference does. That is not laziness about the edge: a renormalised divisor changes per pixel,
/// which costs a division per channel and gives a wide register nothing to line up on.
const BLUR: &str = r#"
struct BlurParams {
    target_width: u32,
    target_height: u32,
    region_x: u32,
    region_y: u32,
    region_width: u32,
    region_height: u32,
    radius: u32,
    axis: u32,
};

@group(0) @binding(0) var<storage, read_write> pixels: array<u32>;
@group(0) @binding(1) var<storage, read_write> scratch: array<u32>;
@group(0) @binding(2) var<uniform> params: BlurParams;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= params.region_width || id.y >= params.region_height) {
        return;
    }

    let width = i32(params.region_width);
    let height = i32(params.region_height);
    let radius = i32(params.radius);
    let count = params.radius * 2u + 1u;
    var total = vec4<u32>(0u, 0u, 0u, 0u);

    if (params.axis == 0u) {
        let base = (params.region_y + id.y) * params.target_width + params.region_x;
        for (var tap = -radius; tap <= radius; tap = tap + 1) {
            let sampled = clamp(i32(id.x) + tap, 0, width - 1);
            total = total + unpack_pixel(pixels[base + u32(sampled)]);
        }
        scratch[id.y * params.region_width + id.x] = pack_pixel(vec4<u32>(
            div_round(total.x, count),
            div_round(total.y, count),
            div_round(total.z, count),
            div_round(total.w, count),
        ));
    } else {
        let stride = params.region_width;
        for (var tap = -radius; tap <= radius; tap = tap + 1) {
            let sampled = clamp(i32(id.y) + tap, 0, height - 1);
            total = total + unpack_pixel(scratch[u32(sampled) * stride + id.x]);
        }
        let at = (params.region_y + id.y) * params.target_width + params.region_x + id.x;
        pixels[at] = pack_pixel(vec4<u32>(
            div_round(total.x, count),
            div_round(total.y, count),
            div_round(total.z, count),
            div_round(total.w, count),
        ));
    }
}
"#;

// -------------------------------------------------------------------------------------------------
// The bridge
// -------------------------------------------------------------------------------------------------

/// One prepared operation, holding the bind group the dispatch needs and nothing else.
enum PreparedOp {
    Glyphs {
        spans: u32,
        bind_group: wgpu::BindGroup,
    },
    Resample {
        region: Size,
        bind_group: wgpu::BindGroup,
    },
    Blur {
        passes: u8,
        region: Size,
        horizontal: wgpu::BindGroup,
        vertical: wgpu::BindGroup,
    },
}

impl core::fmt::Debug for PreparedOp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Glyphs { spans, .. } => f.debug_struct("Glyphs").field("spans", spans).finish(),
            Self::Resample { region, .. } => {
                f.debug_struct("Resample").field("region", region).finish()
            }
            Self::Blur { passes, region, .. } => f
                .debug_struct("Blur")
                .field("region", region)
                .field("passes", passes)
                .finish(),
        }
    }
}

/// Everything `execute` needs, built once by `prepare`.
///
/// The device buffers live here rather than in the bridge because they are a function of the batch:
/// its target size, its masks, its sources, its regions. `prepare` is called before the warmup and
/// never inside a measured region, so this is the one place where allocating and filling is free.
struct Prepared {
    target_size: Size,
    operation_count: u32,
    work: Work,
    byte_len: u64,
    /// The host surface, as `u32` words. Read and written by every kernel.
    target: wgpu::Buffer,
    /// Where the result lands before it is mapped back into the host surface.
    staging: wgpu::Buffer,
    ops: Vec<PreparedOp>,
}

impl core::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Prepared")
            .field("target_size", &self.target_size)
            .field("byte_len", &self.byte_len)
            .field("work", &self.work)
            .field("ops", &self.ops)
            .finish()
    }
}

/// The Metal implementation of [`GpuSurfaceBridge`].
///
/// The pipelines are built in [`MetalBridge::create`] rather than in `prepare`, because shader
/// compilation is the single largest startup cost a GPU backend has and it does not depend on the
/// batch. `prepare` then only allocates and fills buffers, which is what the seam says it is for.
#[derive(Debug)]
pub struct MetalBridge {
    device: wgpu::Device,
    queue: wgpu::Queue,
    info: GpuDeviceInfo,
    /// The ceiling the device's own limits impose on a single buffer, kept so `prepare` can refuse an
    /// oversized target with an error instead of letting the allocator panic.
    buffer_ceiling: u64,
    glyph_pipeline: wgpu::ComputePipeline,
    glyph_layout: wgpu::BindGroupLayout,
    resample_pipeline: wgpu::ComputePipeline,
    resample_layout: wgpu::BindGroupLayout,
    blur_pipeline: wgpu::ComputePipeline,
    blur_layout: wgpu::BindGroupLayout,
    prepared: Option<Prepared>,
}

impl MetalBridge {
    /// Opens the first Metal adapter this machine offers and links it.
    ///
    /// Returns an error rather than panicking when there is no adapter: a Mac without a Metal device is
    /// not a normal machine, but a process running under an unusual sandbox is, and the caller's
    /// correct response is the one the registry already defines, namely to fall back to the CPU.
    pub fn create() -> Result<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });

        let adapter = instance
            .enumerate_adapters(wgpu::Backends::METAL)
            .into_iter()
            .next()
            .ok_or(RasterError::BackendUnavailable {
                backend: BackendId::GpuMetal.as_str(),
                reason: "no Metal adapter was found on this machine",
            })?;

        let descriptor = wgpu::DeviceDescriptor {
            label: Some(LABEL),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            memory_hints: wgpu::MemoryHints::default(),
        };

        // `wgpu-core` resolves this future synchronously: it performs the blocking device creation
        // itself and hands back a future that is already `Ready`. There is nothing to wait for and no
        // executor is needed, which is why the workspace does not carry one.
        let requested = resolve(adapter.request_device(&descriptor, None)).ok_or(
            RasterError::BackendUnavailable {
                backend: BackendId::GpuMetal.as_str(),
                reason: "the Metal device request did not resolve",
            },
        )?;
        let (device, queue) = requested.map_err(|_| RasterError::BackendUnavailable {
            backend: BackendId::GpuMetal.as_str(),
            reason: "the Metal device could not be created",
        })?;

        let adapter_info = adapter.get_info();
        let limits = adapter.limits();
        let buffer_ceiling = limits
            .max_buffer_size
            .min(u64::from(limits.max_storage_buffer_binding_size))
            .min(MAX_SURFACE_BYTES);

        let info = GpuDeviceInfo {
            id: BackendId::GpuMetal,
            name: if adapter_info.name.is_empty() {
                "Metal device".to_string()
            } else {
                adapter_info.name.clone()
            },
            max_surface: surface_ceiling(buffer_ceiling),
            // Every kernel in this file is a compute dispatch, so a device that cannot run compute is
            // a device this bridge cannot link at all.
            supports_compute: true,
            // The claim this whole design is built to earn. Every kernel is integer-only, so the
            // driver has no rounding decision to make and the conformance test compares checksums
            // rather than tolerances. If a driver ever broke that, the test would fail before the
            // backend was allowed to serve a frame, and this flag would become the honest `false`.
            bit_exact: true,
            supports_timestamps: adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY),
        };

        // The first binding of every layout is the target, which every kernel writes. The ones after
        // it are inputs except in the blur, whose scratch plane is written by the horizontal dispatch
        // and read by the vertical one.
        let glyph_layout = storage_pipeline_layout(&device, LABEL, &[true, false, false]);
        let resample_layout = storage_pipeline_layout(&device, LABEL, &[true, false]);
        let blur_layout = storage_pipeline_layout(&device, LABEL, &[true, true]);

        let glyph_pipeline = build_pipeline(
            &device,
            "erl-raster glyphs",
            PRELUDE,
            GLYPHS,
            &glyph_layout,
        );
        let resample_pipeline = build_pipeline(
            &device,
            "erl-raster resample",
            PRELUDE,
            RESAMPLE,
            &resample_layout,
        );
        let blur_pipeline = build_pipeline(&device, "erl-raster blur", PRELUDE, BLUR, &blur_layout);

        Ok(Self {
            device,
            queue,
            info,
            buffer_ceiling,
            glyph_pipeline,
            glyph_layout,
            resample_pipeline,
            resample_layout,
            blur_pipeline,
            blur_layout,
            prepared: None,
        })
    }

    /// Wraps a linked bridge into the backend the registry expects.
    pub fn linked() -> Result<crate::gpu::GpuBackend> {
        Ok(crate::gpu::GpuBackend::linked(Box::new(Self::create()?)))
    }

    /// Rebuilds the device state when `execute` is called without a matching `prepare`.
    ///
    /// The harness always prepares first, so this is a safety net for a caller that drives the bridge
    /// directly. It compares the batch length and the target size, which is cheap, rather than
    /// recomputing `report_work`, which would move host work into the measured region.
    fn ensure_prepared(&mut self, batch: &Batch) -> Result<()> {
        let stale = match &self.prepared {
            None => true,
            Some(prepared) => {
                prepared.target_size != batch.target()
                    || prepared.operation_count != batch.len() as u32
            }
        };
        if stale {
            self.prepare(batch)?;
        }
        Ok(())
    }

    /// Uploads a byte plane as a storage buffer, padding it to a whole word.
    fn storage_buffer(&self, label: &str, bytes: &[u8]) -> wgpu::Buffer {
        let mut padded = bytes.to_vec();
        while padded.is_empty() || padded.len() % 4 != 0 {
            padded.push(0);
        }
        self.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: &padded,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            })
    }
}

impl GpuSurfaceBridge for MetalBridge {
    fn device(&self) -> &GpuDeviceInfo {
        &self.info
    }

    fn prepare(&mut self, batch: &Batch) -> Result<()> {
        let target_size = batch.target();
        let byte_len = target_size.byte_len();
        if byte_len > self.buffer_ceiling {
            return Err(RasterError::AllocationFailed { bytes: byte_len });
        }

        // The work report is derived from the batch once, here, and reused by every `execute`. It has
        // to be computed outside the measured region because it must equal the reference's own count,
        // and the reference counts while it draws.
        let work = report_work(target_size, batch)?;

        // The target buffer is created before the operations, because every operation's bind group
        // binds it at index zero. Building those groups against a stand-in and swapping the buffer
        // afterwards is not possible: a bind group captures the buffer it was created with, and the
        // dispatch would read and write the stand-in while `execute` uploaded into the real one.
        let target = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("erl-raster target"),
            size: byte_len.max(4),
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("erl-raster download"),
            size: byte_len.max(4),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let mut ops = Vec::with_capacity(batch.len());
        for op in batch.ops() {
            ops.push(self.prepare_op(target_size, &target, op)?);
        }

        self.prepared = Some(Prepared {
            target_size,
            operation_count: batch.len() as u32,
            work,
            byte_len,
            target,
            staging,
            ops,
        });
        Ok(())
    }

    fn execute(&mut self, batch: &Batch, target: &mut Surface) -> Result<Executed> {
        if batch.target() != target.size() {
            return Err(RasterError::TargetMismatch {
                batch: (batch.target().width, batch.target().height),
                target: (target.width(), target.height()),
            });
        }
        self.ensure_prepared(batch)?;

        let Some(prepared) = self.prepared.as_ref() else {
            return Err(RasterError::BackendUnavailable {
                backend: self.info.id.as_str(),
                reason: "the bridge holds no prepared state",
            });
        };

        // A zero-sized surface has no pixels to draw into and no buffer to map. It is legal, and it is
        // the shape an operation clipped to nothing produces.
        if prepared.byte_len == 0 {
            return Ok(Executed {
                work: prepared.work,
                transferred_bytes: batch.transfer_bytes(),
                synchronised: false,
            });
        }

        // The upload. This is inside the measured region on purpose: the target arrives in host
        // memory, and a real caller pays for getting it to the device.
        self.queue.write_buffer(&prepared.target, 0, target.data());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some(LABEL) });

        for op in &prepared.ops {
            match op {
                PreparedOp::Glyphs { spans, bind_group } => {
                    if *spans == 0 {
                        continue;
                    }
                    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
                    pass.set_pipeline(&self.glyph_pipeline);
                    pass.set_bind_group(0, bind_group, &[]);
                    pass.dispatch_workgroups(*spans, 1, 1);
                }
                PreparedOp::Resample { region, bind_group } => {
                    // A resample clipped to nothing holds an empty group and a zero region. Binding
                    // it would put a buffer the shader never reads in front of the pipeline, so the
                    // dispatch is simply not encoded.
                    if region.width == 0 || region.height == 0 {
                        continue;
                    }
                    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
                    pass.set_pipeline(&self.resample_pipeline);
                    pass.set_bind_group(0, bind_group, &[]);
                    pass.dispatch_workgroups(workgroups(region.width), workgroups(region.height), 1);
                }
                PreparedOp::Blur {
                    passes,
                    region,
                    horizontal,
                    vertical,
                } => {
                    if *passes == 0 {
                        continue;
                    }
                    for _ in 0..*passes {
                        // Two passes, never one: a single compute pass gives no guarantee that the
                        // horizontal dispatch's writes to the scratch buffer are visible to the
                        // vertical dispatch that reads it.
                        {
                            let mut pass =
                                encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
                            pass.set_pipeline(&self.blur_pipeline);
                            pass.set_bind_group(0, horizontal, &[]);
                            pass.dispatch_workgroups(
                                workgroups(region.width),
                                workgroups(region.height),
                                1,
                            );
                        }
                        {
                            let mut pass =
                                encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
                            pass.set_pipeline(&self.blur_pipeline);
                            pass.set_bind_group(0, vertical, &[]);
                            pass.dispatch_workgroups(
                                workgroups(region.width),
                                workgroups(region.height),
                                1,
                            );
                        }
                    }
                }
            }
        }

        encoder.copy_buffer_to_buffer(&prepared.target, 0, &prepared.staging, 0, prepared.byte_len);
        self.queue.submit(Some(encoder.finish()));

        // The download. `map_async` has to go through a callback, so the result comes back over a
        // channel and the wait is an explicit `poll`. That wait is the reason `synchronised` is true:
        // it is the cost a caller inherits on the frame this backend runs, and hiding it would be
        // exactly the kind of measurement the harness exists to prevent.
        let slice = prepared.staging.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |outcome| {
            let _ = sender.send(outcome);
        });
        self.device.poll(wgpu::Maintain::Wait);
        let mapped = receiver.recv().map_err(|_| RasterError::BackendUnavailable {
            backend: self.info.id.as_str(),
            reason: "the download callback was dropped before it ran",
        })?;
        mapped.map_err(|_| RasterError::BackendUnavailable {
            backend: self.info.id.as_str(),
            reason: "the download buffer could not be mapped",
        })?;

        {
            let view = slice.get_mapped_range();
            // The staging buffer is the surface byte for byte, because a buffer copy has no row
            // padding requirement; only a texture copy would have forced a stride of 256.
            target.data_mut().copy_from_slice(&view);
        }
        prepared.staging.unmap();

        Ok(Executed {
            work: prepared.work,
            // Both directions of the transfer are counted, plus every source surface the batch reads.
            transferred_bytes: batch.transfer_bytes(),
            synchronised: true,
        })
    }

    fn release(&mut self) {
        self.prepared = None;
    }
}

impl MetalBridge {
    /// Builds the bind group and the uniform a single operation needs.
    ///
    /// `target` is the buffer every kernel reads and writes at binding zero. It is passed in rather
    /// than reached for so that the group is built against the buffer `execute` will upload into.
    fn prepare_op(&self, target_size: Size, target: &wgpu::Buffer, op: &Op) -> Result<PreparedOp> {
        match op {
            Op::FillGlyphs(run) => {
                let mut masks: Vec<u8> = Vec::new();
                let mut spans: Vec<SpanGpu> = Vec::with_capacity(run.spans.len());
                for (index, span) in run.spans.iter().enumerate() {
                    let mask = run
                        .masks
                        .get(span.mask)
                        .ok_or(RasterError::UnknownGlyphMask {
                            span: index,
                            masks: run.masks.len(),
                        })?;
                    let coverage = mask.coverage();
                    if coverage.is_empty() {
                        // A zero-area mask contributes nothing and has no bytes to place. The
                        // reference skips it too, so its span is simply not uploaded.
                        continue;
                    }
                    spans.push(SpanGpu {
                        mask_offset: masks.len() as u32,
                        mask_width: mask.size().width,
                        mask_height: mask.size().height,
                        origin_x: span.origin.x,
                        origin_y: span.origin.y,
                        pad: [0; 3],
                    });
                    masks.extend_from_slice(coverage);
                }

                let mut span_bytes = Vec::with_capacity(spans.len() * 32);
                for span in &spans {
                    span_bytes.extend_from_slice(&span.mask_offset.to_le_bytes());
                    span_bytes.extend_from_slice(&span.mask_width.to_le_bytes());
                    span_bytes.extend_from_slice(&span.mask_height.to_le_bytes());
                    span_bytes.extend_from_slice(&span.origin_x.to_le_bytes());
                    span_bytes.extend_from_slice(&span.origin_y.to_le_bytes());
                    span_bytes.extend_from_slice(&[0u8; 12]);
                }

                let mask_buffer = self.storage_buffer("erl-raster masks", &masks);
                let span_buffer = self.storage_buffer("erl-raster spans", &span_bytes);

                let params = [
                    target_size.width,
                    target_size.height,
                    pack_color(run.color.r, run.color.g, run.color.b, run.color.a),
                    spans.len() as u32,
                    0,
                    0,
                    0,
                    0,
                ];
                let uniform = self.uniform_buffer("erl-raster glyph params", &params);

                let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("erl-raster glyphs"),
                    layout: &self.glyph_layout,
                    entries: &[
                        storage_entry(0, target),
                        storage_read_entry(1, &mask_buffer),
                        storage_read_entry(2, &span_buffer),
                        uniform_entry(3, &uniform),
                    ],
                });

                Ok(PreparedOp::Glyphs {
                    spans: spans.len() as u32,
                    bind_group,
                })
            }
            Op::Resample(resample) => {
                let source = &resample.source;
                let Some(region) = resample.destination.clamp_to(target_size) else {
                    // Nothing lands, so nothing is dispatched. The work report already says zero.
                    return Ok(PreparedOp::Resample {
                        region: Size::ZERO,
                        bind_group: self.empty_bind_group(
                            &self.resample_layout,
                            3,
                            "erl-raster resample (empty)",
                        ),
                    });
                };
                if source.width() == 0
                    || source.height() == 0
                    || resample.destination.width == 0
                    || resample.destination.height == 0
                {
                    return Ok(PreparedOp::Resample {
                        region: Size::ZERO,
                        bind_group: self.empty_bind_group(
                            &self.resample_layout,
                            3,
                            "erl-raster resample (empty)",
                        ),
                    });
                }

                let source_buffer = self.storage_buffer("erl-raster source", source.data());
                let params = [
                    target_size.width,
                    target_size.height,
                    source.width(),
                    source.height(),
                    region.x as u32,
                    region.y as u32,
                    region.width,
                    region.height,
                    resample.destination.x as u32,
                    resample.destination.y as u32,
                    resample.destination.width,
                    resample.destination.height,
                    match resample.filter {
                        erl_raster_core::op::ResampleFilter::Nearest => 0,
                        erl_raster_core::op::ResampleFilter::Bilinear => 1,
                    },
                    0,
                    0,
                    0,
                ];
                let uniform = self.uniform_buffer("erl-raster resample params", &params);

                let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("erl-raster resample"),
                    layout: &self.resample_layout,
                    entries: &[
                        storage_entry(0, target),
                        storage_read_entry(1, &source_buffer),
                        uniform_entry(2, &uniform),
                    ],
                });

                Ok(PreparedOp::Resample {
                    region: region.size(),
                    bind_group,
                })
            }
            Op::Blur(blur) => {
                let Some(region) = blur.region.clamp_to(target_size) else {
                    return Ok(PreparedOp::Blur {
                        passes: 0,
                        region: Size::ZERO,
                        horizontal: self.empty_bind_group(
                            &self.blur_layout,
                            3,
                            "erl-raster blur (empty)",
                        ),
                        vertical: self.empty_bind_group(&self.blur_layout, 3, "erl-raster blur (empty)"),
                    });
                };
                if region.is_empty() || blur.radius == 0 {
                    return Ok(PreparedOp::Blur {
                        passes: 0,
                        region: Size::ZERO,
                        horizontal: self.empty_bind_group(
                            &self.blur_layout,
                            3,
                            "erl-raster blur (empty)",
                        ),
                        vertical: self.empty_bind_group(&self.blur_layout, 3, "erl-raster blur (empty)"),
                    });
                }

                let scratch_bytes = region.size().byte_len().max(4);
                let scratch = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("erl-raster scratch"),
                    size: scratch_bytes,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });

                let common = [
                    target_size.width,
                    target_size.height,
                    region.x as u32,
                    region.y as u32,
                    region.width,
                    region.height,
                    blur.radius,
                ];
                let horizontal_uniform = {
                    let mut params = common.to_vec();
                    params.push(0);
                    self.uniform_buffer("erl-raster blur horizontal", &params)
                };
                let vertical_uniform = {
                    let mut params = common.to_vec();
                    params.push(1);
                    self.uniform_buffer("erl-raster blur vertical", &params)
                };

                let make = |label: &str, uniform: &wgpu::Buffer| {
                    self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some(label),
                        layout: &self.blur_layout,
                        entries: &[
                            storage_entry(0, target),
                            storage_entry(1, &scratch),
                            uniform_entry(2, uniform),
                        ],
                    })
                };
                let horizontal = make("erl-raster blur horizontal", &horizontal_uniform);
                let vertical = make("erl-raster blur vertical", &vertical_uniform);

                Ok(PreparedOp::Blur {
                    passes: blur.passes,
                    region: region.size(),
                    horizontal,
                    vertical,
                })
            }
        }
    }

    /// A uniform buffer holding a run of 32-bit words.
    fn uniform_buffer(&self, label: &str, words: &[u32]) -> wgpu::Buffer {
        let mut bytes = Vec::with_capacity(words.len() * 4);
        for word in words {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        // A uniform binding has to be a whole number of 16-byte rows, and a struct with no `vec4`
        // member is not padded for us.
        while bytes.len() % 16 != 0 {
            bytes.push(0);
        }
        self.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: &bytes,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            })
    }

    /// A buffer bound by an operation that resolves to nothing.
    ///
    /// Such an operation is never dispatched, so nothing ever reads this memory. It exists only
    /// because a bind group has to name a buffer for every binding its layout declares.
    fn placeholder(&self) -> wgpu::Buffer {
        self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("erl-raster placeholder"),
            size: 4,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        })
    }

    /// A complete bind group for an operation that resolves to nothing.
    ///
    /// It exists so such an operation still holds a group the pipeline accepts, rather than one
    /// missing an entry, which is a validation error and would conflate a normal case with a
    /// malformed one. `entries` is the binding count the layout was built with; a layout cannot be
    /// queried for it, so it travels with the call.
    fn empty_bind_group(
        &self,
        layout: &wgpu::BindGroupLayout,
        entries: u32,
        label: &str,
    ) -> wgpu::BindGroup {
        let placeholder = self.placeholder();
        let uniform = self.uniform_buffer(label, &[0u32; 16]);
        let bindings: Vec<wgpu::BindGroupEntry<'_>> = (0..entries)
            .map(|binding| {
                if binding + 1 == entries {
                    // The last binding is the uniform, matching `storage_pipeline_layout`.
                    uniform_entry(binding, &uniform)
                } else if binding == 0 {
                    storage_entry(binding, &placeholder)
                } else {
                    storage_read_entry(binding, &placeholder)
                }
            })
            .collect();
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout,
            entries: &bindings,
        })
    }
}

/// The `Span` struct the glyph shader reads, laid out to match its WGSL twin.
#[repr(C)]
struct SpanGpu {
    mask_offset: u32,
    mask_width: u32,
    mask_height: u32,
    origin_x: i32,
    origin_y: i32,
    pad: [u32; 3],
}

/// Packs four channel bytes the way the kernels do.
fn pack_color(r: u8, g: u8, b: u8, a: u8) -> u32 {
    u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16) | (u32::from(a) << 24)
}

/// The number of workgroups needed to cover a span of `length` invocations.
fn workgroups(length: u32) -> u32 {
    length.div_ceil(WORKGROUP)
}

/// The largest surface the device's own buffer limits allow.
fn surface_ceiling(buffer_ceiling: u64) -> Size {
    let pixels = buffer_ceiling / CHANNELS as u64;
    let side = (pixels as f64).sqrt().floor() as u64;
    let side = side.clamp(1, u64::from(MAX_DIMENSION)) as u32;
    Size::new(side, side)
}

/// A bind group layout of storage bindings followed by one uniform binding.
///
/// `writable` describes each storage binding in order from binding zero, and it is a slice rather than
/// a count because the access class has to match the shader's declaration *exactly*: `wgpu` compares
/// them for equality, not for containment, so a layout that allows writes cannot serve a kernel that
/// only reads. The blur reads and writes both of its planes while the glyph and resample kernels write
/// the first buffer and only read the others, which is why the shape is written out per layout.
fn storage_pipeline_layout(
    device: &wgpu::Device,
    label: &str,
    writable: &[bool],
) -> wgpu::BindGroupLayout {
    let mut entries: Vec<wgpu::BindGroupLayoutEntry> = writable
        .iter()
        .enumerate()
        .map(|(binding, read_write)| wgpu::BindGroupLayoutEntry {
            binding: binding as u32,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage {
                    read_only: !read_write,
                },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        })
        .collect();
    entries.push(wgpu::BindGroupLayoutEntry {
        binding: writable.len() as u32,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    });
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &entries,
    })
}

/// Compiles one compute pipeline out of the shared prelude and its body.
fn build_pipeline(
    device: &wgpu::Device,
    label: &str,
    prelude: &str,
    body: &str,
    layout: &wgpu::BindGroupLayout,
) -> wgpu::ComputePipeline {
    let source = if body.contains("div_255") {
        body.to_string()
    } else {
        format!("{prelude}\n{body}")
    };
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(Cow::Owned(source)),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[layout],
        push_constant_ranges: &[],
    });
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some("main"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    })
}

/// A storage binding entry.
fn storage_entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

/// A read-only storage binding entry.
fn storage_read_entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

/// A uniform binding entry.
fn uniform_entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

/// What the reference would report for this batch, without drawing anything.
///
/// This is a transcription of the clipping arithmetic in `reference.rs`, and it has to stay one: the
/// `Work` a backend returns is compared between pipelines, so a GPU that reported a different count
/// from the CPU would look like a conformance failure even when its pixels matched. The clipping is
/// the only part that is not already a property of the operation, which is why it is reproduced here
/// rather than taken from [`Op::work`].
fn report_work(target: Size, batch: &Batch) -> Result<Work> {
    let mut work = Work::default();
    for op in batch.ops() {
        work += match op {
            Op::FillGlyphs(run) => {
                let mut glyph_pixels = 0u64;
                for (index, span) in run.spans.iter().enumerate() {
                    let mask = run
                        .masks
                        .get(span.mask)
                        .ok_or(RasterError::UnknownGlyphMask {
                            span: index,
                            masks: run.masks.len(),
                        })?;
                    let mask_width = mask.size().width as i32;
                    let mask_height = mask.size().height as i32;
                    if mask_width == 0 || mask_height == 0 {
                        continue;
                    }
                    let first_row = (0 - span.origin.y).max(0);
                    let last_row = mask_height.min(target.height as i32 - span.origin.y);
                    let first_column = (0 - span.origin.x).max(0);
                    let last_column = mask_width.min(target.width as i32 - span.origin.x);
                    if last_row <= first_row || last_column <= first_column {
                        continue;
                    }
                    let width = (last_column - first_column) as usize;
                    for row in first_row..last_row {
                        let start = row as usize * mask_width as usize + first_column as usize;
                        let coverage = &mask.coverage()[start..start + width];
                        glyph_pixels += coverage.iter().filter(|value| **value != 0).count() as u64;
                    }
                }
                Work {
                    operations: 1,
                    glyph_pixels,
                    ..Work::default()
                }
            }
            Op::Resample(resample) => {
                let pixels = match resample.destination.clamp_to(target) {
                    None => 0,
                    Some(region) => {
                        if resample.source.width() == 0
                            || resample.source.height() == 0
                            || resample.destination.width == 0
                            || resample.destination.height == 0
                        {
                            0
                        } else {
                            region.size().area()
                        }
                    }
                };
                Work {
                    operations: 1,
                    resample_pixels: pixels,
                    source_bytes: resample.source_bytes(),
                    ..Work::default()
                }
            }
            Op::Blur(blur) => {
                let area = match blur.region.clamp_to(target) {
                    None => 0,
                    Some(region) => {
                        if region.is_empty() || blur.radius == 0 {
                            0
                        } else {
                            region.size().area()
                        }
                    }
                };
                Work {
                    operations: 1,
                    blur_pixels: area * u64::from(blur.passes) * u64::from(blur.taps()) * 2,
                    ..Work::default()
                }
            }
        };
    }
    Ok(work)
}

/// Polls a future once, spinning briefly on the off chance that it is not already resolved.
///
/// The workspace carries no async runtime, and this is the only future it ever has to drive. Writing a
/// no-op waker out of `std::task::Wake` keeps this safe: the alternative is a hand-rolled `RawWaker`
/// vtable, which is three unsafe functions to express "nobody needs to be woken".
fn resolve<F: core::future::Future>(future: F) -> Option<F::Output> {
    use core::task::{Context, Poll, Waker};

    struct Noop;

    impl std::task::Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }

    let waker = Waker::from(Arc::new(Noop));
    let mut context = Context::from_waker(&waker);
    let mut future = core::pin::pin!(future);
    for _ in 0..1_024 {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return Some(value),
            Poll::Pending => std::thread::yield_now(),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use erl_raster_core::geom::Rect;
    use erl_raster_core::op::{BlurOp, ResampleFilter, ResampleOp};
    use erl_raster_core::workload::published_workloads;
    use crate::contract::Backend;
    use crate::cpu::CpuBackend;

    /// Opens the bridge, or returns `None` when this machine has no Metal adapter.
    ///
    /// A test that cannot run should say so by returning, not by failing: a build machine without a
    /// GPU is a configuration, not a regression, and the CPU conformance tests already cover the
    /// reference. The moment an adapter exists, every assertion below is mandatory.
    fn bridge() -> Option<MetalBridge> {
        MetalBridge::create().ok()
    }

    #[test]
    fn the_metal_pipeline_reproduces_the_scalar_reference() {
        // The claim the whole design rests on, and the reason the kernels are integer-only. If this
        // ever fails, the device is not `bit_exact` and the conformance rule has to become a written
        // tolerance before the backend serves a frame, not after.
        let Some(bridge) = bridge() else {
            return;
        };
        assert!(bridge.device().bit_exact, "the bridge claims byte-for-byte equality");

        let mut cpu = CpuBackend::scalar();
        let mut gpu = crate::gpu::GpuBackend::linked(Box::new(bridge));

        for workload in published_workloads().expect("builds") {
            let size = workload.batch.target();
            let mut expected = Surface::filled(size, erl_raster_core::image::Color::OPAQUE_BLACK)
                .expect("fits");
            let mut actual = Surface::filled(size, erl_raster_core::image::Color::OPAQUE_BLACK)
                .expect("fits");

            cpu.execute(&workload.batch, &mut expected).expect("cpu runs");
            gpu.prepare(&workload.batch).expect("prepares");
            gpu.execute(&workload.batch, &mut actual).expect("gpu runs");

            assert_eq!(
                expected.checksum(),
                actual.checksum(),
                "{} diverged at {:?}",
                workload.name,
                expected.first_difference(&actual)
            );
        }
    }

    #[test]
    fn the_metal_pipeline_reports_the_work_the_reference_reports() {
        let Some(bridge) = bridge() else {
            return;
        };
        let mut cpu = CpuBackend::scalar();
        let mut gpu = crate::gpu::GpuBackend::linked(Box::new(bridge));

        for workload in published_workloads().expect("builds") {
            let size = workload.batch.target();
            let mut left = Surface::try_new(size).expect("fits");
            let mut right = Surface::try_new(size).expect("fits");

            let expected = cpu.execute(&workload.batch, &mut left).expect("cpu runs").work;
            gpu.prepare(&workload.batch).expect("prepares");
            let actual = gpu.execute(&workload.batch, &mut right).expect("gpu runs").work;
            assert_eq!(expected, actual, "{} reported different work", workload.name);
        }
    }

    #[test]
    fn the_metal_pipeline_reproduces_the_reference_where_the_published_set_does_not_look() {
        // The published set exercises the bilinear filter and rectangles that land inside the target.
        // Two live paths in the kernels sit outside it: the nearest filter, and an operation whose
        // rectangle clips away to nothing. A path that has never run is not a path known to be exact,
        // so both are compared the way the published loads are.
        let Some(bridge) = bridge() else {
            return;
        };
        let mut cpu = CpuBackend::scalar();
        let mut gpu = crate::gpu::GpuBackend::linked(Box::new(bridge));

        let target = Size::new(251, 173);
        // An opaque source, so the pattern is a legal premultiplied surface rather than a set of
        // values the pipeline would never be handed.
        let source = || {
            let size = Size::new(64, 48);
            let mut data = Vec::with_capacity(size.byte_len() as usize);
            for pixel in 0..size.area() as usize {
                data.push((pixel * 53 % 200) as u8);
                data.push((pixel * 97 % 200) as u8);
                data.push((pixel * 29 % 200) as u8);
                data.push(255);
            }
            Surface::from_buffer(size, data).expect("fits")
        };

        let cases: [(&str, Batch); 3] = [
            (
                "a nearest resample clipped on two edges",
                {
                    let mut batch = Batch::new(target);
                    batch.push(Op::Resample(ResampleOp::new(
                        source(),
                        Rect::new(-20, -10, 300, 200),
                        ResampleFilter::Nearest,
                    )));
                    batch
                },
            ),
            (
                "a blur with a zero radius",
                {
                    let mut batch = Batch::new(target);
                    batch.push(Op::Blur(BlurOp::new(Rect::new(10, 10, 100, 80), 0, 2)));
                    batch
                },
            ),
            (
                "a resample entirely off the target",
                {
                    let mut batch = Batch::new(target);
                    batch.push(Op::Resample(ResampleOp::new(
                        source(),
                        Rect::new(900, 900, 40, 40),
                        ResampleFilter::Nearest,
                    )));
                    batch
                },
            ),
        ];

        for (name, batch) in cases {
            let mut expected =
                Surface::filled(target, erl_raster_core::image::Color::OPAQUE_BLACK).expect("fits");
            let mut actual =
                Surface::filled(target, erl_raster_core::image::Color::OPAQUE_BLACK).expect("fits");

            cpu.execute(&batch, &mut expected).expect("cpu runs");
            gpu.prepare(&batch).expect("prepares");
            gpu.execute(&batch, &mut actual).expect("gpu runs");

            assert_eq!(
                expected.checksum(),
                actual.checksum(),
                "{name} diverged at {:?}",
                expected.first_difference(&actual)
            );
        }
    }

    #[test]
    fn a_batch_against_the_wrong_target_is_refused() {
        let Some(mut bridge) = bridge() else {
            return;
        };
        let batch = Batch::new(Size::new(4, 4));
        let mut target = Surface::try_new(Size::new(8, 8)).expect("fits");
        assert!(matches!(
            bridge.execute(&batch, &mut target),
            Err(RasterError::TargetMismatch { .. })
        ));
    }
}
