//! The C ABI an application links against.
//!
//! Everything below this crate is a Rust library: it takes batches, surfaces and profiles as Rust
//! values. An application cannot. This is the seam that makes the arbitration reachable from the
//! languages that actually hold the pixels on a phone, namely Kotlin and Swift, and from the C++ that
//! React Native and a plain native application both speak.
//!
//! Four decisions shape it, and each was taken against a friendlier alternative.
//!
//! **One handle, and no global state.** The runtime owns the pipelines and the decision history, so
//! there is exactly one of it per caller and it is handed out as an opaque pointer. A global would
//! make two screens in one process share a tenant, and the first one to switch would move the second
//! one's frames. The pointer is not a lock: the handle is single-threaded, and the caller is the one
//! who knows which thread it drives a raster from.
//!
//! **Host-memory targets, and the copy at the boundary is admitted rather than hidden.** The buffers
//! travel as plain pointers to packed RGBA8, which is what a `Bitmap` on Android and a `CGContext` on
//! iOS already hold. Adopting one into the core costs one copy in and one copy out, and those copies
//! are the price of a boundary a platform can actually cross. They are small next to what the
//! operations do to the pixels and they are written down here rather than discovered in a profile.
//!
//! **The profile is the caller's, and so is the thermal state.** Neither is guessed. A profile is
//! produced by `erl-raster-bench` on the device it describes, and the thermal state is what the
//! platform reports at the moment of the call. A runtime whose profile was taken in a different
//! thermal state says so through [`erl_raster_is_trusted`] and stops moving between pipelines, which
//! is the honest reading: the numbers describe a device that no longer exists.
//!
//! **Nothing here can panic across the boundary.** Every pointer is checked, every size is checked
//! before it reaches an allocation, and every failure comes back as a negative code. A panic that
//! unwound into Kotlin would be undefined behaviour, and a library whose worst case is supposed to be
//! "as fast as the reference" cannot have that worst case be a crash.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::time::Instant;

use erl_adaptive_runtime::{AdaptiveRuntime, DispatchPolicy, FrameContext, RuntimeError};
use erl_benchmark::{DeviceProfile, Regime};
use erl_raster_core::geom::{Rect, Size};
use erl_raster_core::image::{Surface, MAX_DIMENSION, MAX_SURFACE_BYTES};
use erl_raster_core::op::{Batch, BlurOp, Op, ResampleFilter, ResampleOp};
use erl_raster_core::system::{SystemState, ThermalState};
use erl_raster_core::RasterError;
use erl_render_backends::{default_registry, Backend, BackendId, GpuBackend, SwitchCostModel};

// The Android entry points. They are written on the JNI table directly rather than through a wrapper
// crate, and they call the ABI below rather than reimplementing it, so the path Kotlin takes and the
// path a C caller takes are the same path.
#[cfg(all(feature = "jni", target_os = "android"))]
mod jni;

// The display path, as its own ABI. It is gated exactly as the presenter it wraps is: it can only be
// compiled where a `wgpu` backend exists for the platform, and only where the graphics feature is on.
// A build without either answers no view symbols at all, which is the honest shape — a caller that
// links against one gets a link error rather than a stub that refuses at run time.
#[cfg(all(feature = "gpu", any(target_vendor = "apple", target_os = "android")))]
mod view;

// -------------------------------------------------------------------------------------------------
// The codes a caller branches on
// -------------------------------------------------------------------------------------------------

/// The call succeeded.
pub const ERL_RASTER_OK: c_int = 0;
/// A pointer was null, a length did not match, or a profile could not be read.
pub const ERL_RASTER_INVALID_ARGUMENT: c_int = -1;
/// The surface is larger than the core will allocate, or the allocator refused.
pub const ERL_RASTER_UNSUPPORTED_SIZE: c_int = -2;
/// No pipeline willing to serve this operation is left in the runtime.
pub const ERL_RASTER_NO_PIPELINE: c_int = -3;

/// The version of this ABI.
///
/// A caller checks it rather than assuming it. The handle is opaque and the layout of nothing else
/// crosses the boundary, so this number is the whole compatibility contract, and it changes whenever
/// a signature below changes.
const ABI_VERSION: u32 = 1;

/// Which filter [`erl_raster_resample`] is asked for.
const FILTER_NEAREST: u32 = 0;
const FILTER_BILINEAR: u32 = 1;

/// Reads a thermal code the way a platform reports it.
///
/// The numbering mirrors Android's `PowerManager` thermal status, which is the integer an adapter
/// reads, and it is deliberately open at the top: a platform that invents a seventh state gets
/// `Unknown`, and `Unknown` is not `Nominal`. That distinction is the entire reason this function is
/// not a `matches!` on zero: only zero permits an absolute measurement.
const fn thermal_from_code(code: u32) -> ThermalState {
    match code {
        0 => ThermalState::Nominal,
        1 => ThermalState::Fair,
        2 => ThermalState::Serious,
        3 => ThermalState::Critical,
        _ => ThermalState::Unknown,
    }
}

// -------------------------------------------------------------------------------------------------
// The handle
// -------------------------------------------------------------------------------------------------

/// What the most recent call did, kept so a caller can log it without timing anything itself.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LastRun {
    backend: BackendId,
    elapsed_ns: u64,
    transferred_bytes: u64,
    pixels: u64,
    synchronised: bool,
}

/// The runtime, as an application sees it.
///
/// Opaque by construction: the struct is public only because the pointer type has to name something,
/// and every field is private. A caller holds an `ErlRasterRuntime*` and never a value of it.
pub struct ErlRasterRuntime {
    runtime: AdaptiveRuntime,
    /// The device and platform the profile belongs to. Held so `begin_frame` can rebuild a state that
    /// differs from the profile in nothing but its thermal reading, which is what `trusted` compares.
    device: String,
    platform: String,
    /// The pipelines that were linked into this handle, in report order.
    pipelines: Vec<BackendId>,
    last: Option<LastRun>,
    last_error: Option<String>,
}

impl core::fmt::Debug for ErlRasterRuntime {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ErlRasterRuntime")
            .field("runtime", &self.runtime)
            .field("device", &self.device)
            .field("platform", &self.platform)
            .field("pipelines", &self.pipelines)
            .field("last", &self.last)
            .field("last_error", &self.last_error)
            .finish()
    }
}

impl ErlRasterRuntime {
    /// Builds a handle around the pipelines this build can offer.
    ///
    /// `profile` is used as given, including the device, platform and thermal state it names, because
    /// `trusted` is decided by comparing those against what `begin_frame` is told. When there is no
    /// profile the three arguments are the authority instead, and the runtime starts with an empty
    /// entry list: the arbitration then runs on the pipelines' own cost models and, since no switch
    /// has ever been measured, it stays on the reference. That is the designed first-session
    /// behaviour, not a shortcoming of this function.
    fn open(
        device: &str,
        platform: &str,
        thermal: ThermalState,
        profile: Option<DeviceProfile>,
    ) -> Result<Self, RuntimeError> {
        let backends = pipelines();
        let pipelines: Vec<BackendId> = backends.iter().map(|backend| backend.id()).collect();

        let profile = profile.unwrap_or_else(|| DeviceProfile {
            state: SystemState::new(device, platform, thermal),
            entries: Vec::new(),
            switch_costs: SwitchCostModel::new(),
        });

        let state = profile.state.clone();
        let runtime = AdaptiveRuntime::new(backends, profile, DispatchPolicy::default())?;

        Ok(Self {
            runtime,
            device: state.device,
            platform: state.platform,
            pipelines,
            last: None,
            last_error: None,
        })
    }

    /// Runs one batch and copies the result back over the caller's buffer.
    ///
    /// The timer wraps the whole submission, which is the region the benchmark measures and therefore
    /// the only duration that is comparable with a profile. A shorter one, taken inside the backend,
    /// would be a number that looks better and means nothing.
    fn run(&mut self, batch: &Batch, target: &mut Surface, elapsed_ns: *mut u64) -> c_int {
        let started = Instant::now();
        match self.runtime.execute(batch, target) {
            Ok(executed) => {
                let elapsed = started.elapsed().as_nanos() as u64;
                self.last = Some(LastRun {
                    // The tenant is the pipeline that ran, and `execute` leaves it on the one that
                    // served the batch in every case except a fallback retry, which is exactly when
                    // its value is the interesting one.
                    backend: self.runtime.tenant(),
                    elapsed_ns: elapsed,
                    transferred_bytes: executed.transferred_bytes,
                    pixels: executed.work.pixels(),
                    synchronised: executed.synchronised,
                });
                self.last_error = None;
                if !elapsed_ns.is_null() {
                    // Safety: the caller promised a writable `u64` at this address, and it is checked
                    // for null immediately above. Nothing else in this function touches it.
                    unsafe { *elapsed_ns = elapsed };
                }
                ERL_RASTER_OK
            }
            Err(error) => {
                self.last = None;
                self.last_error = Some(error.to_string());
                code_for(&error)
            }
        }
    }
}

/// The pipelines this build can offer, in report order.
///
/// The GPU is linked where a bridge exists for the platform and named absent where it does not, so a
/// caller reading the pipeline list can tell a device that has no graphics adapter from a build that
/// has no bridge for it. The CPU reference is always present: the library must never be able to make
/// an application worse than it would have been without it.
///
/// A build compiled without the `gpu` feature reports the same GPU entry as an absent backend, with a
/// reason that names the build rather than the device. That is deliberate: the shape of the pipeline
/// list is part of what a caller branches on, so it must not depend on which artefact was linked.
fn pipelines() -> Vec<Box<dyn Backend>> {
    let mut backends = default_registry();

    #[cfg(all(feature = "gpu", any(target_vendor = "apple", target_os = "android")))]
    {
        match erl_render_backends::WgpuBridge::create() {
            Ok(bridge) => backends = erl_render_backends::registry_with_gpu(Box::new(bridge)),
            Err(_) => backends.push(Box::new(GpuBackend::absent(
                gpu_bridge_id(),
                "the graphics device could not be opened on this machine",
            ))),
        }
    }

    #[cfg(not(all(feature = "gpu", any(target_vendor = "apple", target_os = "android"))))]
    {
        backends.push(Box::new(GpuBackend::absent(gpu_bridge_id(), GPU_ABSENT_REASON)));
    }

    backends
}

/// Why the GPU entry is absent, when the build itself cannot carry one.
#[cfg(not(all(feature = "gpu", any(target_vendor = "apple", target_os = "android"))))]
const GPU_ABSENT_REASON: &str = if cfg!(feature = "gpu") {
    "no GPU bridge is compiled for this platform"
} else {
    "this artefact was built without the graphics bridge"
};

/// The identifier the GPU entry carries, whether or not a device opened.
fn gpu_bridge_id() -> BackendId {
    #[cfg(target_os = "android")]
    {
        BackendId::GpuVulkan
    }
    #[cfg(not(target_os = "android"))]
    {
        BackendId::GpuMetal
    }
}

/// The negative code that stands for a failure.
fn code_for(error: &RasterError) -> c_int {
    match error {
        RasterError::BackendUnavailable { .. } => ERL_RASTER_NO_PIPELINE,
        RasterError::SizeOutOfRange { .. } | RasterError::AllocationFailed { .. } => {
            ERL_RASTER_UNSUPPORTED_SIZE
        }
        RasterError::TargetMismatch { .. }
        | RasterError::UnknownGlyphMask { .. }
        | RasterError::UnsupportedOperation { .. } => ERL_RASTER_INVALID_ARGUMENT,
    }
}

/// The byte length of a surface the core will accept, or `None` when it will not.
///
/// Checked before anything is allocated or handed to a slice, so a width a caller got wrong becomes a
/// refusal rather than an allocation of several gigabytes.
fn checked_layout(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return None;
    }
    let bytes = Size::new(width, height).byte_len();
    if bytes > MAX_SURFACE_BYTES || bytes > usize::MAX as u64 {
        return None;
    }
    Some(bytes as usize)
}

/// Copies a NUL-terminated string out of the caller's buffer.
///
/// # Safety
///
/// `pointer` must either be null or point to a NUL-terminated string that stays valid for the call.
unsafe fn borrowed_str<'a>(pointer: *const c_char) -> Option<&'a str> {
    if pointer.is_null() {
        return None;
    }
    // Safety: the caller promised a NUL-terminated string at this address.
    unsafe { CStr::from_ptr(pointer) }.to_str().ok()
}

/// Writes a string into a caller-owned buffer and answers the length it needed, terminator included.
///
/// Answering the needed length even when the buffer was too small is what lets a caller retry with a
/// buffer that fits instead of guessing. The return is negative only when the arguments themselves
/// are unusable.
///
/// # Safety
///
/// `buffer` must point to `capacity` writable bytes.
unsafe fn write_c_string(text: &str, buffer: *mut c_char, capacity: usize) -> c_int {
    if buffer.is_null() {
        return ERL_RASTER_INVALID_ARGUMENT;
    }
    let needed = text.len() + 1;
    if capacity < needed {
        return needed as c_int;
    }
    // Safety: the caller promised `capacity` writable bytes and `needed` is within them.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buffer.cast::<u8>(), text.len());
        *buffer.add(text.len()) = 0;
    }
    needed as c_int
}

// -------------------------------------------------------------------------------------------------
// The ABI
// -------------------------------------------------------------------------------------------------

/// The version of this ABI. A caller checks it once, at start-up.
#[no_mangle]
pub extern "C" fn erl_raster_abi_version() -> u32 {
    ABI_VERSION
}

/// Builds a runtime, or answers a negative code and leaves `out` untouched.
///
/// `device` and `platform` are the authority when `profile_text` is absent, and are ignored when it is
/// present, because the profile carries the identity it was measured under. `profile_text` may be null
/// or empty, in which case the runtime starts with no measurements. A profile that is present and
/// cannot be read is refused rather than ignored: a runtime that believed it held measurements it does
/// not would take every later decision on a lie.
///
/// # Safety
///
/// The three pointers must be null or valid for the call, `profile_length` must be the true length of
/// `profile_text`, and `out` must point to a writable `ErlRasterRuntime*`.
#[no_mangle]
pub unsafe extern "C" fn erl_raster_open(
    device: *const c_char,
    platform: *const c_char,
    thermal: u32,
    profile_text: *const c_char,
    profile_length: usize,
    out: *mut *mut ErlRasterRuntime,
) -> c_int {
    if out.is_null() {
        return ERL_RASTER_INVALID_ARGUMENT;
    }
    // Safety: the caller promised a writable pointer, and it is checked above.
    unsafe { *out = std::ptr::null_mut() };

    // Safety: the caller promised NUL-terminated strings where these are not null.
    let device = unsafe { borrowed_str(device) };
    let platform = unsafe { borrowed_str(platform) };
    let (Some(device), Some(platform)) = (device, platform) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };

    let profile = if profile_text.is_null() || profile_length == 0 {
        None
    } else {
        // Safety: the caller promised `profile_length` readable bytes at this address.
        let bytes = unsafe { std::slice::from_raw_parts(profile_text.cast::<u8>(), profile_length) };
        let Ok(text) = std::str::from_utf8(bytes) else {
            return ERL_RASTER_INVALID_ARGUMENT;
        };
        match DeviceProfile::from_text(text) {
            Ok(profile) => Some(profile),
            Err(_) => return ERL_RASTER_INVALID_ARGUMENT,
        }
    };

    let thermal = thermal_from_code(thermal);
    match ErlRasterRuntime::open(device, platform, thermal, profile) {
        Ok(runtime) => {
            // Safety: the caller promised a writable pointer, and it is checked above.
            unsafe { *out = Box::into_raw(Box::new(runtime)) };
            ERL_RASTER_OK
        }
        // The one way this can fail is a pipeline set with no permanent fallback, which this build
        // cannot produce. It is still answered rather than assumed away, because a caller that was
        // handed nothing deserves to be told why.
        Err(RuntimeError::NoFallback) => ERL_RASTER_NO_PIPELINE,
    }
}

/// Releases a runtime. Passing null is legal and does nothing.
///
/// # Safety
///
/// `runtime` must be null or a pointer returned by [`erl_raster_open`] that has not already been
/// released. Using the handle after this call is undefined behaviour.
#[no_mangle]
pub unsafe extern "C" fn erl_raster_close(runtime: *mut ErlRasterRuntime) {
    if runtime.is_null() {
        return;
    }
    // Safety: the caller promised a pointer from `erl_raster_open` that is still live and is not used
    // again after this call.
    drop(unsafe { Box::from_raw(runtime) });
}

/// Declares the start of a frame, and the conditions it runs in.
///
/// `cold` marks work whose sources have just arrived, as opposed to work on data already in flight;
/// the benchmark measured both and only the caller knows which one a frame is. `critical` says that
/// nothing may change pipeline during this frame, and a critical frame is one where the evidence for
/// a change stops accumulating instead of being discarded.
///
/// # Safety
///
/// `runtime` must be a live handle from [`erl_raster_open`].
#[no_mangle]
pub unsafe extern "C" fn erl_raster_begin_frame(
    runtime: *mut ErlRasterRuntime,
    thermal: u32,
    cold: c_int,
    critical: c_int,
) -> c_int {
    // Safety: the caller promised a live handle.
    let Some(handle) = (unsafe { runtime.as_mut() }) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };

    let state = SystemState::new(
        handle.device.clone(),
        handle.platform.clone(),
        thermal_from_code(thermal),
    );
    let context = FrameContext {
        regime: if cold != 0 { Regime::Cold } else { Regime::Hot },
        critical: critical != 0,
    };
    handle.runtime.begin_frame(&state, context);
    ERL_RASTER_OK
}

/// Applies a separable box blur in place over a rectangle of the caller's buffer.
///
/// The buffer is packed RGBA8, premultiplied, `width * height * 4` bytes, which is the layout both
/// target platforms hand out. `radius` and `passes` are clamped by the core to what it supports.
/// `elapsed_ns` may be null when the caller does not want the duration.
///
/// # Safety
///
/// `runtime` must be a live handle, and `pixels` must point to `width * height * 4` writable bytes.
/// `elapsed_ns` must be null or point to a writable `u64`.
#[no_mangle]
pub unsafe extern "C" fn erl_raster_blur(
    runtime: *mut ErlRasterRuntime,
    pixels: *mut u8,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    region_width: u32,
    region_height: u32,
    radius: u32,
    passes: u32,
    elapsed_ns: *mut u64,
) -> c_int {
    // Safety: the caller promised a live handle.
    let Some(handle) = (unsafe { runtime.as_mut() }) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };
    if pixels.is_null() {
        return ERL_RASTER_INVALID_ARGUMENT;
    }
    let Some(bytes) = checked_layout(width, height) else {
        return ERL_RASTER_UNSUPPORTED_SIZE;
    };

    // Safety: the layout was just checked against the core's own ceiling, and the caller promised the
    // buffer is at least that long.
    let caller_buffer = unsafe { std::slice::from_raw_parts_mut(pixels, bytes) };
    let size = Size::new(width, height);
    let Ok(mut target) = Surface::from_buffer(size, caller_buffer.to_vec()) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };

    let mut batch = Batch::new(size);
    batch.push(Op::Blur(BlurOp::new(
        Rect::new(x, y, region_width, region_height),
        radius,
        passes as u8,
    )));

    let code = handle.run(&batch, &mut target, elapsed_ns);
    if code == ERL_RASTER_OK {
        caller_buffer.copy_from_slice(target.data());
    }
    code
}

/// Scales a source buffer into the caller's buffer, compositing over what is already there.
///
/// `source` is never written to and is used for nothing else, so a caller may pass a buffer it does
/// not own. The destination rectangle is given in the target's coordinate space and is clipped to it,
/// exactly as the reference clips it.
///
/// # Safety
///
/// `runtime` must be a live handle, `source` must point to `source_width * source_height * 4`
/// readable bytes, `pixels` must point to `width * height * 4` writable bytes, and `elapsed_ns` must
/// be null or point to a writable `u64`.
#[no_mangle]
pub unsafe extern "C" fn erl_raster_resample(
    runtime: *mut ErlRasterRuntime,
    source: *const u8,
    source_width: u32,
    source_height: u32,
    pixels: *mut u8,
    width: u32,
    height: u32,
    destination_x: i32,
    destination_y: i32,
    destination_width: u32,
    destination_height: u32,
    filter: u32,
    elapsed_ns: *mut u64,
) -> c_int {
    // Safety: the caller promised a live handle.
    let Some(handle) = (unsafe { runtime.as_mut() }) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };
    if source.is_null() || pixels.is_null() {
        return ERL_RASTER_INVALID_ARGUMENT;
    }
    let (Some(source_bytes), Some(target_bytes)) = (
        checked_layout(source_width, source_height),
        checked_layout(width, height),
    ) else {
        return ERL_RASTER_UNSUPPORTED_SIZE;
    };

    // Safety: both layouts were checked against the core's ceiling and the caller promised buffers of
    // at least those lengths.
    let source_pixels = unsafe { std::slice::from_raw_parts(source, source_bytes) };
    let caller_buffer = unsafe { std::slice::from_raw_parts_mut(pixels, target_bytes) };

    let source_size = Size::new(source_width, source_height);
    let target_size = Size::new(width, height);
    let Ok(source_surface) = Surface::from_buffer(source_size, source_pixels.to_vec()) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };
    let Ok(mut target) = Surface::from_buffer(target_size, caller_buffer.to_vec()) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };

    let filter = if filter == FILTER_BILINEAR {
        ResampleFilter::Bilinear
    } else if filter == FILTER_NEAREST {
        ResampleFilter::Nearest
    } else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };

    let mut batch = Batch::new(target_size);
    batch.push(Op::Resample(ResampleOp::new(
        source_surface,
        Rect::new(
            destination_x,
            destination_y,
            destination_width,
            destination_height,
        ),
        filter,
    )));

    let code = handle.run(&batch, &mut target, elapsed_ns);
    if code == ERL_RASTER_OK {
        caller_buffer.copy_from_slice(target.data());
    }
    code
}

/// How many pipelines this handle holds.
///
/// # Safety
///
/// `runtime` must be a live handle from [`erl_raster_open`].
#[no_mangle]
pub unsafe extern "C" fn erl_raster_pipeline_count(runtime: *mut ErlRasterRuntime) -> c_int {
    // Safety: the caller promised a live handle.
    match unsafe { runtime.as_ref() } {
        Some(handle) => handle.pipelines.len() as c_int,
        None => ERL_RASTER_INVALID_ARGUMENT,
    }
}

/// Writes the identifier of one pipeline into the caller's buffer and answers the length it needed,
/// terminator included. A length larger than `capacity` means the text was cut.
///
/// # Safety
///
/// `runtime` must be a live handle, and `buffer` must point to `capacity` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn erl_raster_pipeline(
    runtime: *mut ErlRasterRuntime,
    index: u32,
    buffer: *mut c_char,
    capacity: usize,
) -> c_int {
    // Safety: the caller promised a live handle.
    let Some(handle) = (unsafe { runtime.as_ref() }) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };
    let Some(id) = handle.pipelines.get(index as usize) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };
    // Safety: the caller promised `capacity` writable bytes at this address.
    unsafe { write_c_string(id.as_str(), buffer, capacity) }
}

/// Writes the identifier of the pipeline that ran the most recent operation, or an empty string when
/// nothing has run yet.
///
/// # Safety
///
/// `runtime` must be a live handle, and `buffer` must point to `capacity` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn erl_raster_last_backend(
    runtime: *mut ErlRasterRuntime,
    buffer: *mut c_char,
    capacity: usize,
) -> c_int {
    // Safety: the caller promised a live handle.
    let Some(handle) = (unsafe { runtime.as_ref() }) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };
    let name = match &handle.last {
        Some(last) => last.backend.as_str(),
        None => "",
    };
    // Safety: the caller promised `capacity` writable bytes at this address.
    unsafe { write_c_string(name, buffer, capacity) }
}

/// The duration, in nanoseconds, of the most recent operation, or zero when it failed.
///
/// # Safety
///
/// `runtime` must be a live handle from [`erl_raster_open`].
#[no_mangle]
pub unsafe extern "C" fn erl_raster_last_elapsed_ns(runtime: *mut ErlRasterRuntime) -> u64 {
    // Safety: the caller promised a live handle.
    match unsafe { runtime.as_ref() } {
        Some(handle) => handle.last.as_ref().map_or(0, |last| last.elapsed_ns),
        None => 0,
    }
}

/// The bytes the most recent operation moved, or zero when it failed.
///
/// This is the number that explains a small batch losing on a GPU: a host-memory target is uploaded
/// and downloaded, and both are counted here.
///
/// # Safety
///
/// `runtime` must be a live handle from [`erl_raster_open`].
#[no_mangle]
pub unsafe extern "C" fn erl_raster_last_transferred_bytes(runtime: *mut ErlRasterRuntime) -> u64 {
    // Safety: the caller promised a live handle.
    match unsafe { runtime.as_ref() } {
        Some(handle) => handle
            .last
            .as_ref()
            .map_or(0, |last| last.transferred_bytes),
        None => 0,
    }
}

/// Whether the most recent operation had to block before returning.
///
/// A caller that gets `1` here paid for a pipeline drain on this frame, which is the cost a GPU
/// submission carries and a CPU operation does not.
///
/// # Safety
///
/// `runtime` must be a live handle from [`erl_raster_open`].
#[no_mangle]
pub unsafe extern "C" fn erl_raster_last_synchronised(runtime: *mut ErlRasterRuntime) -> c_int {
    // Safety: the caller promised a live handle.
    match unsafe { runtime.as_ref() } {
        Some(handle) => handle
            .last
            .as_ref()
            .map_or(0, |last| c_int::from(last.synchronised)),
        None => ERL_RASTER_INVALID_ARGUMENT,
    }
}

/// Whether the profile still describes the condition the device is in.
///
/// False means the thermal reading no longer matches the one the profile was taken at, so the
/// arbitration has returned to the reference and every number in the profile describes a device that
/// no longer exists. A caller that shows this to a person is showing them something true.
///
/// # Safety
///
/// `runtime` must be a live handle from [`erl_raster_open`].
#[no_mangle]
pub unsafe extern "C" fn erl_raster_is_trusted(runtime: *mut ErlRasterRuntime) -> c_int {
    // Safety: the caller promised a live handle.
    match unsafe { runtime.as_ref() } {
        Some(handle) => c_int::from(handle.runtime.is_trusted()),
        None => ERL_RASTER_INVALID_ARGUMENT,
    }
}

/// Writes the reason the most recent call failed, or an empty string when it did not.
///
/// # Safety
///
/// `runtime` must be a live handle, and `buffer` must point to `capacity` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn erl_raster_last_error(
    runtime: *mut ErlRasterRuntime,
    buffer: *mut c_char,
    capacity: usize,
) -> c_int {
    // Safety: the caller promised a live handle.
    let Some(handle) = (unsafe { runtime.as_ref() }) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };
    let message = handle.last_error.as_deref().unwrap_or("");
    // Safety: the caller promised `capacity` writable bytes at this address.
    unsafe { write_c_string(message, buffer, capacity) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use erl_raster_core::reference;
    use std::ffi::CString;

    /// A handle with no profile, which is what a first launch has.
    fn opened() -> *mut ErlRasterRuntime {
        let device = CString::new("test-device").expect("no interior nul");
        let platform = CString::new("test-platform").expect("no interior nul");
        let mut handle: *mut ErlRasterRuntime = std::ptr::null_mut();
        // Safety: every pointer is a live local for the duration of the call.
        let code = unsafe {
            erl_raster_open(
                device.as_ptr(),
                platform.as_ptr(),
                0,
                std::ptr::null(),
                0,
                &mut handle,
            )
        };
        assert_eq!(code, ERL_RASTER_OK);
        assert!(!handle.is_null());
        // Safety: the handle came from `erl_raster_open` and is used single-threaded in this test.
        unsafe { erl_raster_begin_frame(handle, 0, 0, 0) };
        handle
    }

    /// A deterministic pattern with alpha, so the blur has something that varies in every channel.
    fn pattern(size: Size) -> Vec<u8> {
        let mut pixels = vec![0u8; size.byte_len() as usize];
        for (index, pixel) in pixels.chunks_exact_mut(4).enumerate() {
            let value = (index as u8).wrapping_mul(37);
            pixel.copy_from_slice(&[value, value / 3, 255 - value, 0xff]);
        }
        pixels
    }

    #[test]
    fn a_blur_through_the_abi_lands_on_the_reference_bytes() {
        let size = Size::new(48, 32);
        let original = pattern(size);
        let mut pixels = original.clone();
        let mut elapsed = 0u64;

        let handle = opened();
        // Safety: the buffer is exactly `width * height * 4` bytes and outlives the call.
        let code = unsafe {
            erl_raster_blur(
                handle,
                pixels.as_mut_ptr(),
                size.width,
                size.height,
                0,
                0,
                size.width,
                size.height,
                3,
                1,
                &mut elapsed,
            )
        };
        assert_eq!(code, ERL_RASTER_OK);
        assert!(elapsed > 0, "a blur that took no time was not measured");

        let mut expected =
            Surface::from_buffer(size, original).expect("the pattern is exactly one surface");
        reference::blur(&mut expected, &BlurOp::new(Rect::from_size(size), 3, 1))
            .expect("the reference serves a blur");
        assert_eq!(pixels, expected.data());

        // Safety: the handle came from `erl_raster_open` and is not used again.
        unsafe { erl_raster_close(handle) };
    }

    #[test]
    fn a_resample_through_the_abi_lands_on_the_reference_bytes() {
        let source_size = Size::new(24, 16);
        let target_size = Size::new(64, 48);
        let source = pattern(source_size);
        let backdrop = pattern(target_size);
        let mut pixels = backdrop.clone();

        let handle = opened();
        // Safety: both buffers hold exactly one surface and outlive the call.
        let code = unsafe {
            erl_raster_resample(
                handle,
                source.as_ptr(),
                source_size.width,
                source_size.height,
                pixels.as_mut_ptr(),
                target_size.width,
                target_size.height,
                4,
                6,
                40,
                30,
                FILTER_BILINEAR,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(code, ERL_RASTER_OK);

        let mut expected = Surface::from_buffer(target_size, backdrop).expect("one surface");
        let op = ResampleOp::new(
            Surface::from_buffer(source_size, source).expect("one surface"),
            Rect::new(4, 6, 40, 30),
            ResampleFilter::Bilinear,
        );
        reference::resample(&mut expected, &op).expect("the reference serves a resample");
        assert_eq!(pixels, expected.data());

        // Safety: the handle came from `erl_raster_open` and is not used again.
        unsafe { erl_raster_close(handle) };
    }

    #[test]
    fn the_pipeline_list_names_the_reference_first() {
        let handle = opened();
        // Safety: the handle is live and the buffer holds more than one short identifier.
        let count = unsafe { erl_raster_pipeline_count(handle) };
        assert!(count >= 2, "the reference and the vector path are both expected");

        let mut buffer = [0 as c_char; 32];
        // Safety: the buffer has room for the longest identifier plus its terminator.
        let written = unsafe { erl_raster_pipeline(handle, 0, buffer.as_mut_ptr(), buffer.len()) };
        assert!(written > 0 && written as usize <= buffer.len());
        // Safety: the call above wrote a NUL-terminated string into the buffer.
        let name = unsafe { CStr::from_ptr(buffer.as_ptr()) };
        assert_eq!(name.to_str().expect("ascii"), "cpu-scalar");

        // Safety: the handle came from `erl_raster_open` and is not used again.
        unsafe { erl_raster_close(handle) };
    }

    #[test]
    fn a_buffer_the_core_will_not_allocate_is_refused_before_it_is_touched() {
        let handle = opened();
        let mut pixel = 0u8;
        // Safety: the call must refuse on the size alone, before reading the buffer it was handed,
        // which is why a single byte is a safe thing to pass here.
        let code = unsafe {
            erl_raster_blur(
                handle,
                &mut pixel,
                MAX_DIMENSION + 1,
                4,
                0,
                0,
                4,
                4,
                1,
                1,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(code, ERL_RASTER_UNSUPPORTED_SIZE);

        // Safety: the handle came from `erl_raster_open` and is not used again.
        unsafe { erl_raster_close(handle) };
    }

    #[test]
    fn a_null_handle_is_answered_rather_than_followed() {
        // Safety: every pointer is null or a local, so nothing is dereferenced.
        let code = unsafe {
            erl_raster_begin_frame(std::ptr::null_mut(), 0, 0, 0)
        };
        assert_eq!(code, ERL_RASTER_INVALID_ARGUMENT);
        // Safety: `erl_raster_close` accepts null by contract.
        unsafe { erl_raster_close(std::ptr::null_mut()) };
    }

    #[test]
    fn a_corrupt_profile_is_refused_rather_than_ignored() {
        let device = CString::new("test-device").expect("no interior nul");
        let platform = CString::new("test-platform").expect("no interior nul");
        let profile = CString::new("not a profile at all").expect("no interior nul");
        let mut handle: *mut ErlRasterRuntime = std::ptr::null_mut();

        // Safety: every pointer is a live local for the duration of the call.
        let code = unsafe {
            erl_raster_open(
                device.as_ptr(),
                platform.as_ptr(),
                0,
                profile.as_ptr(),
                profile.as_bytes().len(),
                &mut handle,
            )
        };
        assert_eq!(code, ERL_RASTER_INVALID_ARGUMENT);
        assert!(handle.is_null(), "a refused open must leave nothing behind");
    }

    #[test]
    fn a_profile_taken_in_another_thermal_state_stops_the_arbitration() {
        let device = CString::new("test-device").expect("no interior nul");
        let platform = CString::new("test-platform").expect("no interior nul");
        // A profile recorded at nominal, opened as if it were, then driven on a throttling device:
        // the reading no longer matches and the runtime has to say so.
        let profile = CString::new("erl-raster-profile 1\ndevice test-device\nplatform test-platform\nthermal nominal\n")
            .expect("no interior nul");
        let mut handle: *mut ErlRasterRuntime = std::ptr::null_mut();

        // Safety: every pointer is a live local for the duration of the call.
        let code = unsafe {
            erl_raster_open(
                device.as_ptr(),
                platform.as_ptr(),
                0,
                profile.as_ptr(),
                profile.as_bytes().len(),
                &mut handle,
            )
        };
        assert_eq!(code, ERL_RASTER_OK);

        // Safety: the handle is live. A frame declared at the state the profile was taken in is the
        // case the profile describes, so the arbitration is allowed to trust it.
        unsafe { erl_raster_begin_frame(handle, 0, 0, 0) };
        // Safety: the handle is live.
        assert_eq!(unsafe { erl_raster_is_trusted(handle) }, 1);
        // Safety: the handle is live, and 2 is the code for a device the platform is throttling.
        unsafe { erl_raster_begin_frame(handle, 2, 0, 0) };
        // Safety: the handle is live.
        assert_eq!(unsafe { erl_raster_is_trusted(handle) }, 0);
        // Safety: the handle is live and is not used again.
        unsafe { erl_raster_close(handle) };
    }
}
