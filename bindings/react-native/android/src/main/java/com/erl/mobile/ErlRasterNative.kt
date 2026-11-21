package com.erl.mobile

import android.view.Surface
import java.nio.ByteBuffer

/**
 * The ERL Raster library, as the virtual machine sees it.
 *
 * This object is the JNI surface and nothing else. Every function below has a counterpart in
 * `crates/erl-raster-ffi/src/jni.rs`, and the names are the contract between the two: renaming this
 * object renames those entry points, because JNI resolves native methods by the class and method name
 * they were declared with.
 *
 * The library is loaded once, lazily, and a failure to load it is not fatal. A device whose ABI the
 * application did not ship falls back to whatever the application did before, which is the whole point
 * of a library whose worst case is meant to be "as good as it was without me". Every caller checks
 * [available] before it calls anything here.
 */
object ErlRasterNative {

  /** Whether the native library was found and loaded. */
  val available: Boolean

  init {
    var loaded = false
    try {
      System.loadLibrary("erl_raster")
      loaded = true
    } catch (_: UnsatisfiedLinkError) {
      // The usual cause is an ABI the application did not build the library for. Nothing to do here:
      // the caller asks [available] and takes the old path.
    } catch (_: Throwable) {
      // A library that is present but cannot be initialised is the same situation from here.
    }
    available = loaded
  }

  /** The version of the ABI this library implements, to be compared with the one the caller expects. */
  external fun abiVersion(): Int

  /**
   * Builds a runtime and answers its handle, or zero with a Java exception raised.
   *
   * `profile` may be null, in which case the runtime starts with no measurements. A profile that is
   * present and cannot be read is refused rather than ignored: a runtime that believed it held
   * measurements it does not would take every later decision on a lie.
   */
  external fun open(device: String, platform: String, thermal: Int, profile: String?): Long

  /** Releases a runtime. Zero is accepted and does nothing. */
  external fun close(handle: Long)

  /** Declares the start of a frame. Call it once per operation, with the current thermal reading. */
  external fun beginFrame(handle: Long, thermal: Int, cold: Boolean, critical: Boolean): Int

  /**
   * Blurs a rectangle of a direct buffer in place, and answers the nanoseconds it took, or a negative
   * code. The buffer must be direct: a heap buffer has no address the core can be given.
   */
  external fun blur(
    handle: Long,
    pixels: ByteBuffer,
    width: Int,
    height: Int,
    x: Int,
    y: Int,
    regionWidth: Int,
    regionHeight: Int,
    radius: Int,
    passes: Int,
  ): Long

  /** Scales `source` into `pixels`, both direct, and answers the nanoseconds or a negative code. */
  external fun resample(
    handle: Long,
    source: ByteBuffer,
    sourceWidth: Int,
    sourceHeight: Int,
    pixels: ByteBuffer,
    width: Int,
    height: Int,
    destinationX: Int,
    destinationY: Int,
    destinationWidth: Int,
    destinationHeight: Int,
    filter: Int,
  ): Long

  /** How many pipelines the runtime holds. */
  external fun pipelineCount(handle: Long): Int

  /** The identifier of one pipeline, or null when the index is out of range. */
  external fun pipeline(handle: Long, index: Int): String?

  /** The pipeline that ran the most recent operation, or null when nothing has run yet. */
  external fun lastBackend(handle: Long): String?

  /** The nanoseconds the most recent operation took. */
  external fun lastElapsedNs(handle: Long): Long

  /** The bytes the most recent operation moved. This is what explains a small batch losing to the CPU. */
  external fun lastTransferredBytes(handle: Long): Long

  /** Whether the most recent operation had to block before returning. */
  external fun lastSynchronised(handle: Long): Boolean

  /** Whether the profile still describes the condition the device is in. */
  external fun isTrusted(handle: Long): Boolean

  /** The reason the most recent call failed, or null when it did not. */
  external fun lastError(handle: Long): String?

  // -----------------------------------------------------------------------------------------------
  // The display path
  // -----------------------------------------------------------------------------------------------

  /**
   * Builds a view over the window behind `surface` and answers its handle, or zero with a Java
   * exception raised.
   *
   * `width` and `height` are the surface's size in *physical* pixels, so this is called from the
   * holder's `surfaceChanged` and not from `surfaceCreated`: a surface has no size until it has
   * changed once, and a swapchain of zero extent is refused by the driver.
   *
   * The native side takes its own reference to the window behind the surface and drops it again in
   * [viewDestroy], so the caller keeps the surface alive for at least that long and no longer.
   */
  external fun viewCreate(surface: Surface, width: Int, height: Int): Long

  /** Releases a view. The surface is the caller's, and is not released here. Zero does nothing. */
  external fun viewDestroy(view: Long)

  /** Resizes the view. A no-op when the size has not changed. */
  external fun viewResize(view: Long, width: Int, height: Int): Int

  /**
   * Shows a picture in the view, uploading it only if the device does not already hold it.
   *
   * The buffer must be direct and hold packed RGBA8, `width * height * 4` bytes. `identifier` is the
   * name the picture was filed under, and it is what lets two views of one picture share a single
   * upload: the first to ask for it moves the bytes, and every other one borrows that texture.
   *
   * Answers the bytes this call moved, or a negative code. Zero is not a failure — it is a second view
   * being handed a picture the device already has. This is the only call in the display path that can
   * move the picture; every later [viewDraw] that shows a different part of it moves nothing.
   */
  external fun viewSetSource(view: Long, identifier: Long, pixels: ByteBuffer, width: Int, height: Int): Long

  /** Forgets the uploaded picture, so the view draws its background alone. */
  external fun viewClearSource(view: Long): Int

  /**
   * Draws the picture into the view and presents it, answering the nanoseconds it took or a negative
   * code.
   *
   * The source rectangle names the part of the *picture* to show and the destination rectangle names
   * where it lands in the *view*. Both are clipped rather than refused, so a crop that falls outside
   * the picture clears the view instead of failing. `background` is the packed `0xAARRGGBB` the rest
   * of the view is filled with.
   */
  external fun viewDraw(
    view: Long,
    sourceX: Int,
    sourceY: Int,
    sourceWidth: Int,
    sourceHeight: Int,
    destinationX: Int,
    destinationY: Int,
    destinationWidth: Int,
    destinationHeight: Int,
    filter: Int,
    background: Int,
  ): Long

  /** The ABI's success code, which every function that answers a code answers when it worked. */
  const val OK = 0

  /** The identifier of the CPU reference, which is always present. */
  const val PIPELINE_REFERENCE = "cpu-scalar"

  /** Nearest-neighbour resampling, which copies pixels rather than blending them. */
  const val FILTER_NEAREST = 0

  /** Bilinear resampling, which is what a thumbnail wants. */
  const val FILTER_BILINEAR = 1

  /** The thermal codes, numbered the way Android's `PowerManager` reports them. */
  const val THERMAL_NOMINAL = 0
  const val THERMAL_FAIR = 1
  const val THERMAL_SERIOUS = 2
  const val THERMAL_CRITICAL = 3
  const val THERMAL_UNKNOWN = 4
}
