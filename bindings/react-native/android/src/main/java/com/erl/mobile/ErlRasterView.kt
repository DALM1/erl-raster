package com.erl.mobile

import android.content.Context
import android.util.Log
import android.view.SurfaceHolder
import android.view.SurfaceView
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * A picture on the screen, drawn by the engine rather than by Android's canvas.
 *
 * This is the zero-copy half of the library's story. The measured half takes a host buffer, computes
 * into another one and hands it back, which is what makes it byte-exact and what makes it expensive:
 * an image that goes through it crosses the bus twice. This view takes a picture *once*, into a
 * texture the GPU holds, and every later frame — a resize, a scroll, a re-crop — presents it without
 * moving a byte of it. That is the whole reason the path exists, and the reason it is not offered as
 * a measured one: the filtering is the sampler's rather than the reference's, so the pixels it shows
 * are right to the eye and not to the byte.
 *
 * The surface is the caller's to keep alive; the native side takes its own reference to the window
 * behind it, which is why creation happens in `surfaceChanged` and release in `surfaceDestroyed`.
 * Those two are the only places the native handle is built or dropped, and the ordering matters: the
 * window must outlive the view that borrows it.
 *
 * Every native call below is made on the thread Android calls these callbacks on, which is the same
 * thread the properties are set on, so a single view is never driven from two threads at once.
 */
class ErlRasterView(context: Context) : SurfaceView(context), SurfaceHolder.Callback {

  /** The native view, or zero when there is no surface to draw into. */
  private var handle: Long = 0L

  /** The surface's size in physical pixels, which is what a destination rectangle is measured in. */
  private var surfaceWidth: Int = 0
  private var surfaceHeight: Int = 0

  /** The picture to show, and whether the view has already been given it. */
  private var picture: ErlRasterBuffer? = null
  private var uploaded: Boolean = false

  /** How the picture is sampled, how it is fitted, and what the rest of the view is filled with. */
  private var filter: Int = ErlRasterNative.FILTER_BILINEAR
  private var contentFit: Int = FIT_CONTAIN
  private var background: Int = DEFAULT_BACKGROUND

  /** What the last draw cost, and what went wrong if anything did. */
  private var lastDrawNs: Long = 0L
  private var lastError: String? = null

  init {
    holder.addCallback(this)
  }

  // -----------------------------------------------------------------------------------------------
  // What the view manager sets
  // -----------------------------------------------------------------------------------------------

  /**
   * Shows the picture, or clears the view when it is null.
   *
   * Internal because the buffer registry is: a caller outside this package names a picture by its
   * identifier and never holds one, so the only thing that hands a picture in is the view manager
   * beside this class.
   */
  internal fun setPicture(picture: ErlRasterBuffer?) {
    this.picture = picture
    // A new picture is not in the texture yet; the frame that showed the old one must be replaced
    // rather than left on the screen while a smaller one is uploaded behind it.
    uploaded = false
    draw()
  }

  /** Sets the sampler. Nearest copies pixels, bilinear blends them; a thumbnail wants the latter. */
  fun setFilter(value: Int) {
    if (filter == value) return
    filter = value
    draw()
  }

  /** Sets whether the picture is fitted inside the view or cropped to fill it. */
  fun setContentFit(value: Int) {
    if (contentFit == value) return
    contentFit = value
    draw()
  }

  /** Sets the colour the rest of the view is filled with, packed as Android packs a colour. */
  fun setBackground(value: Int) {
    if (background == value) return
    background = value
    draw()
  }

  /** The nanoseconds the most recent draw took, or zero when nothing has been drawn yet. */
  fun lastDrawNanoseconds(): Long = lastDrawNs

  /** The reason the most recent operation failed, or null when nothing has failed. */
  fun lastErrorMessage(): String? = lastError

  // -----------------------------------------------------------------------------------------------
  // The surface
  // -----------------------------------------------------------------------------------------------

  /** Nothing to do yet: a surface has no size until it has changed once. */
  override fun surfaceCreated(holder: SurfaceHolder) = Unit

  /**
   * Builds the native view over the surface, or resizes the one that is already there.
   *
   * The size is taken here rather than in `surfaceCreated` because a swapchain of zero extent is
   * refused by the driver, and a surface has no extent until this callback fires.
   */
  override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
    if (!ErlRasterNative.available) {
      fail("the erl-raster native library is not part of this build for this ABI")
      return
    }
    surfaceWidth = width
    surfaceHeight = height

    if (handle == 0L) {
      val created = try {
        ErlRasterNative.viewCreate(holder.surface, width, height)
      } catch (error: Throwable) {
        // The native side raises an exception when the window behind the surface is not there yet,
        // which is the case for a surface that has never been laid out. Nothing to recover from here.
        fail("the view could not be created: ${error.message}")
        return
      }
      if (created == 0L) {
        fail("the view could not be created")
        return
      }
      handle = created
      lastError = null
      Log.i(TAG, "created for a ${width}x$height surface")
    } else if (ErlRasterNative.viewResize(handle, width, height) != ErlRasterNative.OK) {
      fail("the view could not be resized to ${width}x$height")
      return
    }
    draw()
  }

  /** Releases the native view. The surface is Android's, and is not released here. */
  override fun surfaceDestroyed(holder: SurfaceHolder) {
    val releasing = handle
    handle = 0L
    uploaded = false
    if (releasing != 0L) {
      ErlRasterNative.viewDestroy(releasing)
      Log.i(TAG, "released")
    }
  }

  // -----------------------------------------------------------------------------------------------
  // Drawing
  // -----------------------------------------------------------------------------------------------

  /**
   * Draws what the view holds, uploading the picture first when the texture does not have it yet.
   *
   * A call with nothing to draw is not skipped: the frame that came before is still on the screen, and
   * leaving it there while the view is asked to be empty is how a list ends up showing a thumbnail
   * under the picture that replaced it.
   */
  fun draw() {
    val handle = handle
    if (handle == 0L) return

    val held = picture
    if (held == null) {
      ErlRasterNative.viewClearSource(handle)
      uploaded = false
      submit(handle, 0, 0, 0, 0, 0, 0, 0, 0)
      return
    }
    if (!uploaded && !upload(handle, held)) return

    val placement = placementOf(held.width, held.height)
    submit(
      handle,
      placement[0],
      placement[1],
      placement[2],
      placement[3],
      placement[4],
      placement[5],
      placement[6],
      placement[7],
    )
  }

  /**
   * Hands the picture to the native view. Answers whether the view now holds it.
   *
   * The picture is named by its identifier rather than handed over blind, which is what lets a second
   * view of the same picture borrow the texture the first one uploaded instead of uploading a copy of
   * its own. The native side answers how many bytes it moved, and the log says which of the two things
   * happened: an upload, or a borrowing of one that was already there.
   */
  private fun upload(handle: Long, held: ErlRasterBuffer): Boolean {
    val needed = held.width * held.height * 4
    if (held.width <= 0 || held.height <= 0 || held.pixels.capacity() < needed) {
      fail(
        "a ${held.width}x${held.height} picture holds ${held.pixels.capacity()} bytes, " +
          "which is less than the $needed the view would read",
      )
      return false
    }
    val moved = ErlRasterNative.viewSetSource(
      handle,
      held.identifier.toLong(),
      held.pixels,
      held.width,
      held.height,
    )
    if (moved < 0) {
      fail("the picture could not be uploaded (code $moved)")
      return false
    }
    uploaded = true
    lastError = null
    if (moved > 0) {
      Log.i(TAG, "uploaded ${held.width}x${held.height}, $moved bytes")
    } else {
      Log.i(TAG, "borrowed ${held.width}x${held.height} from the device, no bytes moved")
    }
    return true
  }

  /** Runs one draw and keeps what it cost. This is the only call a frame pays for. */
  private fun submit(
    handle: Long,
    sourceX: Int,
    sourceY: Int,
    sourceWidth: Int,
    sourceHeight: Int,
    destinationX: Int,
    destinationY: Int,
    destinationWidth: Int,
    destinationHeight: Int,
  ) {
    val elapsed = ErlRasterNative.viewDraw(
      handle,
      sourceX,
      sourceY,
      sourceWidth,
      sourceHeight,
      destinationX,
      destinationY,
      destinationWidth,
      destinationHeight,
      filter,
      background,
    )
    if (elapsed < 0) {
      fail("the draw failed with code $elapsed")
      return
    }
    lastDrawNs = elapsed
    lastError = null
    // One line per presented frame, which is what a probe reads. It is the only number this path can
    // report: the picture was moved in by `upload`, and a draw that moves nothing is the whole point.
    Log.d(
      TAG,
      "drew ${sourceWidth}x$sourceHeight at ${destinationX},${destinationY} " +
        "as ${destinationWidth}x$destinationHeight in ${elapsed / 1000}µs",
    )
  }

  /**
   * What the draw is given: which part of the picture to show, then where that part lands.
   *
   * The two rectangles are answered together because they have to agree, and that is the whole
   * difficulty of a fitting. `contain` shows the whole picture, enlarged or reduced until one of its
   * edges reaches the view, centred, with the background left around the other pair. `cover` fills
   * the view, which means cropping: the picture is enlarged until *both* edges reach, and the
   * overhang on each side is dropped.
   *
   * The crop is named as a source rectangle rather than as a destination hanging off the view,
   * because the native draw clips the destination against the view while still mapping the whole
   * source onto what survives the clip: a destination of minus two hundred pixels would come back as
   * a picture squashed into the view rather than as one cropped by it. So `cover` hands over the view
   * unchanged and narrows the source to the middle that actually lands inside it.
   */
  private fun placementOf(sourceWidth: Int, sourceHeight: Int): IntArray {
    if (sourceWidth <= 0 || sourceHeight <= 0 || surfaceWidth <= 0 || surfaceHeight <= 0) {
      return IntArray(8)
    }
    val byWidth = surfaceWidth.toDouble() / sourceWidth.toDouble()
    val byHeight = surfaceHeight.toDouble() / sourceHeight.toDouble()
    val scale = if (contentFit == FIT_COVER) max(byWidth, byHeight) else min(byWidth, byHeight)

    // The picture at that scale, as a floating point frame, so the cropping below is not read off a
    // rounded number: a pixel of error here is a pixel of the picture shown twice or not at all.
    val scaledWidth = sourceWidth * scale
    val scaledHeight = sourceHeight * scale

    if (contentFit != FIT_COVER) {
      val left = ((surfaceWidth - scaledWidth) / 2.0).roundToInt()
      val top = ((surfaceHeight - scaledHeight) / 2.0).roundToInt()
      return intArrayOf(
        0,
        0,
        sourceWidth,
        sourceHeight,
        left,
        top,
        max(1, scaledWidth.roundToInt()),
        max(1, scaledHeight.roundToInt()),
      )
    }

    val sourceX = max(0.0, (scaledWidth - surfaceWidth) / 2.0) / scale
    val sourceY = max(0.0, (scaledHeight - surfaceHeight) / 2.0) / scale
    val sourceRight = min(sourceWidth.toDouble(), sourceX + surfaceWidth.toDouble() / scale)
    val sourceBottom = min(sourceHeight.toDouble(), sourceY + surfaceHeight.toDouble() / scale)
    return intArrayOf(
      sourceX.roundToInt(),
      sourceY.roundToInt(),
      max(1, (sourceRight - sourceX).roundToInt()),
      max(1, (sourceBottom - sourceY).roundToInt()),
      0,
      0,
      surfaceWidth,
      surfaceHeight,
    )
  }

  /** Records a failure and says so once, so a caller reading logcat has a reason rather than a blank. */
  private fun fail(reason: String) {
    lastError = reason
    Log.w(TAG, reason)
  }

  companion object {
    /**
     * The tag every line from this view is written under, so a reader can follow one component's
     * frames without the rest of the log. The view manager writes under it too.
     */
    const val TAG = "ErlRasterView"

    /**
     * Opaque black, packed the way the ABI packs a colour: `0xAARRGGBB`.
     *
     * Negative because `0xFF000000` does not fit in an `Int` and a `const` cannot call `toInt`.
     */
    const val DEFAULT_BACKGROUND: Int = -0x1000000

    /** The picture is shown whole, centred, with the background left around the edges it misses. */
    const val FIT_CONTAIN: Int = 0

    /** The picture fills the view and the overhang on each side is cropped away. */
    const val FIT_COVER: Int = 1
  }
}
