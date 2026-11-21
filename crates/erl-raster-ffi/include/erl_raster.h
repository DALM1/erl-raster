/*
 * erl_raster.h - the C surface of the ERL Raster adaptive engine.
 *
 * This header is the whole of what a platform needs in order to call the library. Kotlin reaches it
 * through JNI, Swift through a bridging header, and the C++ of a host view can include it directly.
 * It is written by hand and kept in step with `crates/erl-raster-ffi/src/lib.rs`, which is the
 * authority: if the two ever disagree, the Rust source is right and this file is a bug.
 *
 * Four properties hold on every function below, and a caller may rely on all four.
 *
 *   No panic crosses the boundary. Every pointer is checked and every size is checked before it
 *   reaches an allocation. A failure comes back as a negative return code, never as an unwinding
 *   frame, because unwinding into Kotlin or Swift is undefined behaviour.
 *
 *   One handle, no global state. A runtime is an opaque pointer. Two screens in one process get two
 *   runtimes and neither one's decisions move the other's frames. The handle is not a lock: it is
 *   single threaded, and the caller is the one who knows which thread it drives a raster from.
 *
 *   Host-memory targets. Buffers travel as plain pointers to packed RGBA8, premultiplied, which is
 *   what an Android `Bitmap` and an iOS `CGContext` already hold. Adopting one into the core costs
 *   one copy in and one copy out; those copies are the price of a boundary a platform can cross, and
 *   they are named here rather than discovered later in a profile.
 *
 *   The profile and the thermal state are the caller's. Neither is guessed. The profile is produced
 *   on the device it describes by the benchmark binary, and the thermal state is what the platform
 *   reports at the moment of the call. A runtime asked to run in a thermal state other than the one
 *   its profile was taken in reports itself untrusted and stops moving between pipelines, which is
 *   the honest reading: its numbers describe a device that no longer exists.
 */

#ifndef ERL_RASTER_H
#define ERL_RASTER_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The version of this ABI. Call `erl_raster_abi_version` once at start-up and compare. The handle is
 * opaque and nothing else crosses the boundary, so this number is the entire compatibility contract,
 * and it changes whenever a signature below changes. */
#define ERL_RASTER_ABI_VERSION 1u

/* The return codes. Zero is success; everything else is a failure a caller can branch on. */
#define ERL_RASTER_OK 0
/* A pointer was null, a length did not match, or a profile could not be read. */
#define ERL_RASTER_INVALID_ARGUMENT (-1)
/* The surface is larger than the core will allocate, or the allocator refused. */
#define ERL_RASTER_UNSUPPORTED_SIZE (-2)
/* No pipeline willing to serve this operation is left in the runtime. */
#define ERL_RASTER_NO_PIPELINE (-3)

/* The value `erl_raster_last_synchronised` and friends answer when the handle was null. */
#define ERL_RASTER_INVALID_HANDLE (-1)

/* Thermal codes, numbered the way Android's PowerManager reports them. 0 is the only state in which
 * an absolute measurement is permitted; anything the library does not recognise becomes "unknown",
 * and unknown is deliberately not nominal. */
#define ERL_RASTER_THERMAL_NOMINAL 0u
#define ERL_RASTER_THERMAL_FAIR 1u
#define ERL_RASTER_THERMAL_SERIOUS 2u
#define ERL_RASTER_THERMAL_CRITICAL 3u
#define ERL_RASTER_THERMAL_UNKNOWN 4u

/* Resampling filters, for `erl_raster_resample`. */
#define ERL_RASTER_FILTER_NEAREST 0u
#define ERL_RASTER_FILTER_BILINEAR 1u

/* The runtime, as an application sees it. Opaque on purpose: a caller holds a pointer and never a
 * value, and its fields are none of the caller's business. */
typedef struct ErlRasterRuntime ErlRasterRuntime;

/*
 * The version of the linked ABI.
 */
uint32_t erl_raster_abi_version(void);

/*
 * Builds a runtime. On success writes the handle to `out` and returns ERL_RASTER_OK. On failure leaves
 * `out` as null and returns a negative code.
 *
 * `device` and `platform` are the identity the profile will be keyed by. They are the authority when
 * `profile_text` is absent, and are ignored when it is present, because a profile carries the identity
 * it was measured under. `profile_text` may be null or `profile_length` zero, in which case the
 * runtime starts with no measurements and serves the CPU reference until it has evidence for anything
 * else. A profile that is present and cannot be read is refused rather than ignored: a runtime that
 * believed it held measurements it does not would take every later decision on a lie.
 *
 * `thermal` is the thermal code in effect when the profile was taken.
 */
int32_t erl_raster_open(const char *device,
                        const char *platform,
                        uint32_t thermal,
                        const char *profile_text,
                        size_t profile_length,
                        ErlRasterRuntime **out);

/*
 * Releases a runtime. Passing null is legal and does nothing. Using the handle afterwards is
 * undefined behaviour.
 */
void erl_raster_close(ErlRasterRuntime *runtime);

/*
 * Declares the start of a frame and the conditions it runs in. Call this once per frame, before the
 * operations below.
 *
 * `cold` marks work whose sources have just arrived, as opposed to work on data already in flight.
 * `critical` says that nothing may change pipeline during this frame; evidence still accumulates,
 * but no switch is acted on. Pass zero for both when neither applies.
 *
 * `thermal` is what the platform reports now, not what the profile was taken at.
 */
int32_t erl_raster_begin_frame(ErlRasterRuntime *runtime,
                               uint32_t thermal,
                               int32_t cold,
                               int32_t critical);

/*
 * Applies a separable box blur in place, over a rectangle of the caller's buffer.
 *
 * `pixels` is packed RGBA8, premultiplied, `width * height * 4` bytes. The region is given in the
 * buffer's coordinates and is clipped to it by the core. `radius` and `passes` are clamped by the
 * core to what it supports, so a caller need not pre-clamp them.
 *
 * `elapsed_ns` may be null when the caller does not want the duration. When it is not null it
 * receives the nanoseconds the operation took, measured around the whole of it.
 */
int32_t erl_raster_blur(ErlRasterRuntime *runtime,
                        uint8_t *pixels,
                        uint32_t width,
                        uint32_t height,
                        int32_t x,
                        int32_t y,
                        uint32_t region_width,
                        uint32_t region_height,
                        uint32_t radius,
                        uint32_t passes,
                        uint64_t *elapsed_ns);

/*
 * Scales `source` into the caller's buffer, compositing over what is already there.
 *
 * `source` is never written to and is used for nothing else, so a caller may pass a buffer it does not
 * own. Both buffers are packed RGBA8, premultiplied, as above. The destination rectangle is given in
 * the target's coordinates and is clipped to it, exactly as the reference clips it. `filter` is one of
 * the ERL_RASTER_FILTER_* values; anything else is refused.
 *
 * `elapsed_ns` may be null when the caller does not want the duration.
 */
int32_t erl_raster_resample(ErlRasterRuntime *runtime,
                            const uint8_t *source,
                            uint32_t source_width,
                            uint32_t source_height,
                            uint8_t *pixels,
                            uint32_t width,
                            uint32_t height,
                            int32_t destination_x,
                            int32_t destination_y,
                            uint32_t destination_width,
                            uint32_t destination_height,
                            uint32_t filter,
                            uint64_t *elapsed_ns);

/*
 * How many pipelines this handle holds. The list is stable for the life of the handle and is the order
 * `erl_raster_pipeline` indexes into.
 */
int32_t erl_raster_pipeline_count(ErlRasterRuntime *runtime);

/*
 * Writes the identifier of one pipeline into `buffer` and answers the length it needed, terminator
 * included. A return larger than `capacity` means the text was cut and the caller should retry with a
 * buffer that fits. A negative return means the arguments were unusable or `index` is out of range.
 *
 * Identifiers are the ones the library reports everywhere: "cpu-scalar", "cpu-neon", "gpu-metal",
 * "gpu-vulkan".
 */
int32_t erl_raster_pipeline(ErlRasterRuntime *runtime,
                            uint32_t index,
                            char *buffer,
                            size_t capacity);

/*
 * Writes the identifier of the pipeline that ran the most recent operation, or an empty string when
 * nothing has run yet. Answers the length it needed, as `erl_raster_pipeline` does.
 */
int32_t erl_raster_last_backend(ErlRasterRuntime *runtime, char *buffer, size_t capacity);

/*
 * The duration, in nanoseconds, of the most recent operation, or zero when it failed or nothing has
 * run yet.
 */
uint64_t erl_raster_last_elapsed_ns(ErlRasterRuntime *runtime);

/*
 * The bytes the most recent operation moved, or zero when it failed.
 *
 * This is the number that explains a small batch losing to the CPU on a GPU: a host-memory target is
 * uploaded and downloaded, and both are counted here. It is offered so a caller can see the cost
 * rather than wonder at it.
 */
uint64_t erl_raster_last_transferred_bytes(ErlRasterRuntime *runtime);

/*
 * Whether the most recent operation had to block before returning. True means the caller paid for a
 * pipeline drain on this frame, which is the cost a GPU submission carries and a CPU operation does
 * not. Answers a negative code when the handle is null.
 */
int32_t erl_raster_last_synchronised(ErlRasterRuntime *runtime);

/*
 * Whether the profile still describes the condition the device is in. False means the thermal reading
 * no longer matches the one the profile was taken at, so the arbitration has returned to the
 * reference and every number in the profile describes a device that no longer exists. A caller that
 * shows this to a person is showing them something true.
 */
int32_t erl_raster_is_trusted(ErlRasterRuntime *runtime);

/*
 * Writes the reason the most recent call failed, or an empty string when it did not. Answers the
 * length it needed, as `erl_raster_pipeline` does.
 */
int32_t erl_raster_last_error(ErlRasterRuntime *runtime, char *buffer, size_t capacity);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* ERL_RASTER_H */
