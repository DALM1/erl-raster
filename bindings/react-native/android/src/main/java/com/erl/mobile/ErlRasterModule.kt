package com.erl.mobile

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Matrix
import android.os.Build
import android.os.PowerManager
import androidx.exifinterface.media.ExifInterface
import com.facebook.react.bridge.Promise
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.bridge.ReactContextBaseJavaModule
import com.facebook.react.bridge.ReactMethod
import com.facebook.react.bridge.WritableNativeArray
import com.facebook.react.bridge.WritableNativeMap
import java.io.File
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * The raster engine, reachable from JavaScript by identifier rather than by buffer.
 *
 * The pixels never cross the bridge. A caller hands the module a file path, gets back an integer that
 * names a buffer living in native memory, does whatever it wants to that identifier, and hands a path
 * back when it wants the result written out. Nothing in this class marshals an image through
 * JavaScript, because the cost of doing so would exceed everything the engine saves: encoding a
 * screen-sized buffer to base64 and decoding it again in the JavaScript thread is slower than the
 * downscale it would be paying for.
 *
 * Three properties are worth stating outright.
 *
 * **Every operation runs on this module's own thread.** Nothing here touches the JavaScript thread and
 * nothing here touches the main thread. The engine is single threaded by construction, which is what
 * the handle has always required, and the queue is the one place that is enforced.
 *
 * **The thermal state is read at the moment of the call.** The arbitration refuses to act on a profile
 * taken in another thermal state, and it can only know that if it is told the truth each time. The
 * reading comes from `PowerManager`, is translated into the codes the ABI documents, and an Android
 * too old to report one is answered `unknown`, which is deliberately not `nominal`.
 *
 * **A missing library is not a failure.** A device whose ABI was not shipped answers `available:
 * false` and every call rejects with a message that says so, so the caller keeps the path it had
 * before rather than crashing on a library it never had.
 */
class ErlRasterModule(reactContext: ReactApplicationContext) :
  ReactContextBaseJavaModule(reactContext) {

  override fun getName(): String = "ErlRaster"

  /** The engine's thread. One, so a handle is never driven from two threads at once. */
  private val worker: ExecutorService = Executors.newSingleThreadExecutor { runnable ->
    Thread(runnable, "erl-raster").apply { priority = Thread.NORM_PRIORITY }
  }

  /** The runtime, opened on first use and released when the module is torn down. */
  private var handle: Long = 0L

  /**
   * The device, named the way the benchmark names it.
   *
   * The identity is not decoration: a profile is keyed by device, platform and thermal state, and a
   * runtime whose identity differs from the one its profile was measured under finds no entry and
   * falls back to the reference. The two must therefore agree to the letter, and this is the same pair
   * `erl-raster-bench` writes when it measures on this phone.
   */
  private fun deviceName(): String = "${Build.MANUFACTURER} ${Build.MODEL}"

  /** The platform, named the way the benchmark names it. */
  private fun platformName(): String = "Android ${Build.VERSION.RELEASE}"

  /**
   * The thermal state, in the codes the ABI documents.
   *
   * Android numbers its own states differently and reserves values above the ones the ABI knows, so
   * they are translated here rather than passed through. Anything from `THERMAL_STATUS_CRITICAL`
   * upward becomes `UNKNOWN`, which is the conservative reading: the engine will not treat a device
   * under that much pressure as if its measurements still described it.
   */
  private fun thermalCode(): Int {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return ErlRasterNative.THERMAL_UNKNOWN
    val manager =
      reactApplicationContext.getSystemService(Context.POWER_SERVICE) as? PowerManager
        ?: return ErlRasterNative.THERMAL_UNKNOWN
    return when (manager.currentThermalStatus) {
      PowerManager.THERMAL_STATUS_NONE -> ErlRasterNative.THERMAL_NOMINAL
      PowerManager.THERMAL_STATUS_LIGHT -> ErlRasterNative.THERMAL_FAIR
      PowerManager.THERMAL_STATUS_MODERATE -> ErlRasterNative.THERMAL_SERIOUS
      PowerManager.THERMAL_STATUS_SEVERE -> ErlRasterNative.THERMAL_CRITICAL
      else -> ErlRasterNative.THERMAL_UNKNOWN
    }
  }

  /** The runtime, opened on first use and closed when the module goes away. */
  private fun runtime(): Long {
    if (handle != 0L) return handle
    handle = ErlRasterNative.open(
      deviceName(),
      platformName(),
      thermalCode(),
      null,
    )
    return handle
  }

  /** Runs one operation on the engine's thread, turning a failure into a rejected promise. */
  private fun onWorker(promise: Promise, block: () -> WritableNativeMap) {
    worker.execute {
      try {
        promise.resolve(block())
      } catch (error: Throwable) {
        promise.reject("erl_raster", error.message ?: "the operation failed", error)
      }
    }
  }

  /** Refuses a call cleanly when the native library is not in this build. */
  private fun requireLibrary(promise: Promise): Boolean {
    if (ErlRasterNative.available) return true
    promise.reject(
      "erl_raster_unavailable",
      "the erl-raster native library is not part of this build for this ABI",
    )
    return false
  }

  /**
   * Announces a frame and runs one operation, keeping the two inseparable.
   *
   * The library is told the thermal state before every operation rather than once at start-up,
   * because a phone heats up while it is used and a runtime that was told `nominal` an hour ago would
   * keep acting on measurements that no longer apply.
   */
  private fun frame(): Long {
    val runtime = runtime()
    ErlRasterNative.beginFrame(runtime, thermalCode(), false, false)
    return runtime
  }

  /** Builds the answer every operation reports alongside its result. */
  private fun resultOf(elapsedNs: Long): WritableNativeMap {
    val runtime = handle
    return WritableNativeMap().apply {
      putDouble("elapsedNs", elapsedNs.toDouble())
      putString("backend", ErlRasterNative.lastBackend(runtime))
      putDouble("transferredBytes", ErlRasterNative.lastTransferredBytes(runtime).toDouble())
      putBoolean("synchronised", ErlRasterNative.lastSynchronised(runtime))
      putBoolean("trusted", ErlRasterNative.isTrusted(runtime))
    }
  }

  // -----------------------------------------------------------------------------------------------
  // Lifecycle and introspection
  // -----------------------------------------------------------------------------------------------

  @ReactMethod
  fun isAvailable(promise: Promise) {
    if (!ErlRasterNative.available) {
      val answer = WritableNativeMap()
      answer.putBoolean("available", false)
      answer.putString("reason", "the erl-raster native library is not part of this build for this ABI")
      promise.resolve(answer)
      return
    }
    val answer = WritableNativeMap()
    answer.putBoolean("available", true)
    answer.putInt("abiVersion", ErlRasterNative.abiVersion())
    promise.resolve(answer)
  }

  /**
   * Opens the runtime with a profile, or without one.
   *
   * A profile is a file the benchmark produced on this device, and `profileThermal` is the state it
   * was taken in. Passing the wrong one is not an error the module can detect: the runtime will simply
   * report itself untrusted, which is the honest answer and the one a caller can act on.
   */
  @ReactMethod
  fun open(profile: String?, profileThermal: Int, promise: Promise) {
    if (!requireLibrary(promise)) return
    onWorker(promise) {
      if (handle != 0L) {
        ErlRasterNative.close(handle)
        handle = 0L
      }
      handle = ErlRasterNative.open(deviceName(), platformName(), profileThermal, profile)
      if (handle == 0L) {
        throw IllegalStateException("the erl-raster runtime could not be opened")
      }
      val count = ErlRasterNative.pipelineCount(handle)
      val names = WritableNativeArray()
      for (index in 0 until count) {
        names.pushString(ErlRasterNative.pipeline(handle, index) ?: "")
      }
      WritableNativeMap().apply {
        putArray("pipelines", names)
        putString("device", deviceName())
        putString("platform", platformName())
        putInt("thermal", thermalCode())
      }
    }
  }

  @ReactMethod
  fun close(promise: Promise) {
    onWorker(promise) {
      ErlRasterBuffers.releaseAll()
      if (handle != 0L) {
        ErlRasterNative.close(handle)
        handle = 0L
      }
      WritableNativeMap()
    }
  }

  /**
   * The directory the module is allowed to write into.
   *
   * A thumbnail is produced by naming its destination, and JavaScript has no way to name a path this
   * application may write to: the module owns that answer, so it hands it over. The directory is the
   * application's own cache, which the system is free to reclaim and which needs no permission.
   */
  @ReactMethod
  fun scratchDirectory(promise: Promise) {
    onWorker(promise) {
      val directory = File(reactApplicationContext.cacheDir, "erl-raster")
      if (!directory.exists() && !directory.mkdirs()) {
        throw IllegalStateException("the scratch directory could not be created")
      }
      WritableNativeMap().apply { putString("path", directory.absolutePath) }
    }
  }

  /** The pipelines this build linked, the CPU reference first. */
  @ReactMethod
  fun pipelineNames(promise: Promise) {
    if (!requireLibrary(promise)) return
    onWorker(promise) {
      val runtime = runtime()
      val count = ErlRasterNative.pipelineCount(runtime)
      val names = WritableNativeArray()
      for (index in 0 until count) {
        names.pushString(ErlRasterNative.pipeline(runtime, index) ?: "")
      }
      WritableNativeMap().apply { putArray("pipelines", names) }
    }
  }

  /** What the engine is doing: which pipeline ran last, how long it took, and whether it is trusted. */
  @ReactMethod
  fun status(promise: Promise) {
    if (!requireLibrary(promise)) return
    onWorker(promise) {
      val runtime = runtime()
      WritableNativeMap().apply {
        putString("device", deviceName())
        putString("platform", platformName())
        putInt("thermal", thermalCode())
        putString("backend", ErlRasterNative.lastBackend(runtime))
        putDouble("elapsedNs", ErlRasterNative.lastElapsedNs(runtime).toDouble())
        putDouble("transferredBytes", ErlRasterNative.lastTransferredBytes(runtime).toDouble())
        putBoolean("synchronised", ErlRasterNative.lastSynchronised(runtime))
        putBoolean("trusted", ErlRasterNative.isTrusted(runtime))
        putString("error", ErlRasterNative.lastError(runtime))
      }
    }
  }

  // -----------------------------------------------------------------------------------------------
  // Native buffers
  // -----------------------------------------------------------------------------------------------

  /**
   * Decodes an image into native memory and answers the identifier of the buffer holding it.
   *
   * `maxDimension` is the longest edge the decode should land at, or zero for the image's own size.
   * It is a decode hint rather than the final size: the decoder subsamples in powers of two, so the
   * result is the smallest power-of-two reduction that still leaves the longest edge at or above
   * `maxDimension`. Scaling the rest of the way is the engine's job, and doing it on a buffer that is
   * already close to the target is what keeps a thumbnail cheap.
   *
   * The buffer holds premultiplied RGBA8, which is the layout the engine takes and the layout Android
   * decodes to. It is the caller's to release.
   */
  @ReactMethod
  fun decodeImage(path: String, maxDimension: Int, promise: Promise) {
    if (!requireLibrary(promise)) return
    onWorker(promise) {
      val file = File(path)
      if (!file.exists()) {
        throw IllegalArgumentException("no such file: $path")
      }
      val bitmap = decodeOriented(path, maxDimension)

      val width = bitmap.width
      val height = bitmap.height
      val held = ErlRasterBuffers.allocate(width, height)
      held.pixels.clear()
      bitmap.copyPixelsToBuffer(held.pixels)
      bitmap.recycle()

      WritableNativeMap().apply {
        putInt("id", held.identifier)
        putInt("width", width)
        putInt("height", height)
      }
    }
  }

  /** Releases a buffer. Releasing an identifier that is not held does nothing. */
  @ReactMethod
  fun release(id: Int, promise: Promise) {
    onWorker(promise) {
      ErlRasterBuffers.release(id)
      WritableNativeMap().apply { putBoolean("released", true) }
    }
  }

  /**
   * Scales a buffer into a new one of exactly the requested size, and answers the new identifier.
   *
   * The whole source is fitted into the whole target. The output is a new buffer so the caller keeps
   * both, which is what a cache of thumbnails wants.
   */
  @ReactMethod
  fun resample(
    sourceId: Int,
    sourceWidth: Int,
    sourceHeight: Int,
    width: Int,
    height: Int,
    filter: Int,
    promise: Promise,
  ) {
    if (!requireLibrary(promise)) return
    onWorker(promise) {
      val source = ErlRasterBuffers.find(sourceId)
        ?: throw IllegalArgumentException("no buffer with identifier $sourceId")
      val sourceBytes = sourceWidth * sourceHeight * 4
      if (source.pixels.capacity() < sourceBytes) {
        throw IllegalArgumentException(
          "buffer $sourceId holds ${source.pixels.capacity()} bytes, which is less than $sourceBytes",
        )
      }
      val runtime = frame()
      val target = ErlRasterBuffers.allocate(width, height)
      target.pixels.clear()
      val elapsed = ErlRasterNative.resample(
        runtime,
        source.pixels,
        sourceWidth,
        sourceHeight,
        target.pixels,
        width,
        height,
        0,
        0,
        width,
        height,
        filter,
      )
      if (elapsed < 0) {
        ErlRasterBuffers.release(target.identifier)
        throw IllegalStateException(
          "the resample failed with code $elapsed: ${ErlRasterNative.lastError(runtime)}",
        )
      }
      resultOf(elapsed).apply {
        putInt("id", target.identifier)
        putInt("width", width)
        putInt("height", height)
      }
    }
  }

  /**
   * Writes a buffer out as an image file and answers how many bytes were written.
   *
   * The bitmap the encoder is given borrows the buffer's memory rather than copying it, so a thumbnail
   * is encoded where the engine left it.
   */
  @ReactMethod
  fun encodeImage(
    id: Int,
    width: Int,
    height: Int,
    path: String,
    format: String,
    quality: Int,
    promise: Promise,
  ) {
    if (!requireLibrary(promise)) return
    onWorker(promise) {
      val held = ErlRasterBuffers.find(id)
        ?: throw IllegalArgumentException("no buffer with identifier $id")
      val bytes = width * height * 4
      if (held.pixels.capacity() < bytes) {
        throw IllegalArgumentException(
          "buffer $id holds ${held.pixels.capacity()} bytes, which is less than $bytes",
        )
      }
      val bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
      held.pixels.clear()
      bitmap.copyPixelsFromBuffer(held.pixels)
      val file = File(path)
      file.parentFile?.mkdirs()
      val compress =
        if (format.equals("png", ignoreCase = true)) Bitmap.CompressFormat.PNG
        else Bitmap.CompressFormat.JPEG
      file.outputStream().use { stream ->
        if (!bitmap.compress(compress, quality.coerceIn(1, 100), stream)) {
          bitmap.recycle()
          throw IllegalStateException("the image could not be encoded: $path")
        }
      }
      bitmap.recycle()
      WritableNativeMap().apply {
        putString("path", path)
        putDouble("bytes", file.length().toDouble())
      }
    }
  }

  /**
   * The whole thumbnail path in one call: read, scale, write.
   *
   * This is the shape a list wants. The caller passes two paths and a box, and gets back a file whose
   * longest edge is at most `maxWidth` by `maxHeight` while the aspect ratio is preserved. The source
   * is stood upright first, so the box measures the image the way it is seen rather than the way it is
   * stored. The decode lands as close to the target as a power-of-two subsample allows, and the engine
   * does the rest of the scaling, which is the division of labour the measurements support: the
   * decoder is good at reading less, the engine is good at resampling what was read.
   */
  @ReactMethod
  fun thumbnail(
    sourcePath: String,
    targetPath: String,
    maxWidth: Int,
    maxHeight: Int,
    filter: Int,
    format: String,
    quality: Int,
    promise: Promise,
  ) {
    if (!requireLibrary(promise)) return
    onWorker(promise) {
      val box = max(maxWidth, maxHeight)
      if (box <= 0) {
        throw IllegalArgumentException("the thumbnail box must be positive")
      }
      val file = File(sourcePath)
      if (!file.exists()) {
        throw IllegalArgumentException("no such file: $sourcePath")
      }
      val decoded = decodeOriented(sourcePath, box)
      val sourceWidth = decoded.width
      val sourceHeight = decoded.height
      // The box is a ceiling rather than a size to grow into: an image already smaller than the box
      // keeps its own dimensions instead of being enlarged to fill it.
      val scale = min(
        1.0,
        min(
          maxWidth.toDouble() / sourceWidth.toDouble(),
          maxHeight.toDouble() / sourceHeight.toDouble(),
        ),
      )
      val targetWidth = max(1, (sourceWidth * scale).roundToInt())
      val targetHeight = max(1, (sourceHeight * scale).roundToInt())

      // Neither buffer is filed under an identifier: nothing here hands one back, and a thumbnail
      // that left the picture and its result in the registry would keep a screen-sized allocation
      // alive for every image a list ever asked for.
      val sourceBuffer = ErlRasterBuffers.direct(sourceWidth, sourceHeight)
      sourceBuffer.clear()
      decoded.copyPixelsToBuffer(sourceBuffer)
      decoded.recycle()

      val targetBuffer = ErlRasterBuffers.direct(targetWidth, targetHeight)
      targetBuffer.clear()
      val runtime = frame()
      val elapsed = ErlRasterNative.resample(
        runtime,
        sourceBuffer,
        sourceWidth,
        sourceHeight,
        targetBuffer,
        targetWidth,
        targetHeight,
        0,
        0,
        targetWidth,
        targetHeight,
        filter,
      )
      if (elapsed < 0) {
        throw IllegalStateException(
          "the resample failed with code $elapsed: ${ErlRasterNative.lastError(runtime)}",
        )
      }

      val bitmap = Bitmap.createBitmap(targetWidth, targetHeight, Bitmap.Config.ARGB_8888)
      targetBuffer.clear()
      bitmap.copyPixelsFromBuffer(targetBuffer)
      val output = File(targetPath)
      output.parentFile?.mkdirs()
      val compress =
        if (format.equals("png", ignoreCase = true)) Bitmap.CompressFormat.PNG
        else Bitmap.CompressFormat.JPEG
      output.outputStream().use { stream ->
        if (!bitmap.compress(compress, quality.coerceIn(1, 100), stream)) {
          bitmap.recycle()
          throw IllegalStateException("the thumbnail could not be encoded: $targetPath")
        }
      }
      bitmap.recycle()

      resultOf(elapsed).apply {
        putString("path", targetPath)
        putInt("width", targetWidth)
        putInt("height", targetHeight)
        putDouble("bytes", output.length().toDouble())
      }
    }
  }

  /**
   * The largest power-of-two reduction that still leaves the longest edge at or above the box.
   *
   * Powers of two are all the Android decoder offers, and asking for one that lands just under the
   * target would make the engine enlarge an image it should have read smaller.
   */
  private fun sampleSize(width: Int, height: Int, box: Int): Int {
    if (box <= 0) return 1
    var sample = 1
    while (max(width, height) / (sample * 2) >= box) {
      sample *= 2
    }
    return sample
  }

  /**
   * Decodes a file and stands it the way a viewer would.
   *
   * The Android decoder reads pixels and nothing else, so a photograph taken in portrait arrives
   * landscape and carries its rotation in the file's header. Every decode in this class goes through
   * here rather than calling the decoder directly, because a thumbnail lying on its side is worse
   * than no thumbnail at all.
   *
   * `box` is the longest edge the decode should land at, or zero for the file's own size. The decoder
   * subsamples in powers of two, so the result is the smallest reduction that still leaves the
   * longest edge at or above the box.
   */
  private fun decodeOriented(path: String, box: Int): Bitmap {
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    BitmapFactory.decodeFile(path, bounds)
    if (bounds.outWidth <= 0 || bounds.outHeight <= 0) {
      throw IllegalArgumentException("the image could not be read: $path")
    }
    val options = BitmapFactory.Options().apply {
      inPreferredConfig = Bitmap.Config.ARGB_8888
      inSampleSize = sampleSize(bounds.outWidth, bounds.outHeight, box)
    }
    val decoded = BitmapFactory.decodeFile(path, options)
      ?: throw IllegalArgumentException("the image could not be decoded: $path")
    val rotation = orientationMatrix(path)
    if (rotation == null) return decoded
    val upright = Bitmap.createBitmap(decoded, 0, 0, decoded.width, decoded.height, rotation, true)
    if (upright !== decoded) decoded.recycle()
    return upright
  }

  /**
   * The rotation the file's header records, as a matrix, or `null` when it is already upright.
   *
   * A file that carries no header, or one this build cannot read, is answered `null`, which is the
   * upright reading. Guessing a rotation would be worse than assuming there is none.
   */
  private fun orientationMatrix(path: String): Matrix? {
    val orientation = try {
      ExifInterface(path).getAttributeInt(
        ExifInterface.TAG_ORIENTATION,
        ExifInterface.ORIENTATION_NORMAL,
      )
    } catch (_: Throwable) {
      ExifInterface.ORIENTATION_NORMAL
    }
    return when (orientation) {
      ExifInterface.ORIENTATION_ROTATE_90 -> Matrix().apply { postRotate(90f) }
      ExifInterface.ORIENTATION_ROTATE_180 -> Matrix().apply { postRotate(180f) }
      ExifInterface.ORIENTATION_ROTATE_270 -> Matrix().apply { postRotate(270f) }
      ExifInterface.ORIENTATION_FLIP_HORIZONTAL -> Matrix().apply { postScale(-1f, 1f) }
      ExifInterface.ORIENTATION_FLIP_VERTICAL -> Matrix().apply { postScale(1f, -1f) }
      ExifInterface.ORIENTATION_TRANSPOSE -> Matrix().apply {
        postRotate(90f)
        postScale(-1f, 1f)
      }
      ExifInterface.ORIENTATION_TRANSVERSE -> Matrix().apply {
        postRotate(270f)
        postScale(-1f, 1f)
      }
      else -> null
    }
  }

  override fun invalidate() {
    ErlRasterBuffers.releaseAll()
    if (handle != 0L) {
      val runtime = handle
      handle = 0L
      worker.execute {
        ErlRasterNative.close(runtime)
      }
    }
    worker.shutdown()
    super.invalidate()
  }
}
