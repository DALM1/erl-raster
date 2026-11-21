package com.erl.mobile

import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicInteger

/**
 * A picture the engine holds: the name it is filed under, the direct buffer, and the size it was
 * allocated for.
 *
 * The identifier lives here rather than only as a key of the table, because the picture is not the only
 * thing that needs it. A view showing the picture has to name it again — that name is what lets two
 * views of one picture share a single upload to the device — and a view holds a buffer, not a key.
 *
 * The buffer and the size travel together because the bytes alone cannot say how they are laid out. Two
 * numbers passed in from outside could name a rectangle that is not the one in the memory they point
 * at, and nothing in the ABI would notice.
 */
internal class ErlRasterBuffer(
  /** The name this picture is filed and shown under. Never zero, and never reused. */
  val identifier: Int,
  /** Packed RGBA8, `width * height * 4` bytes, in the platform's byte order. */
  val pixels: ByteBuffer,
  val width: Int,
  val height: Int,
)

/**
 * The pictures the engine holds, by the identifier a caller was given.
 *
 * A table of its own rather than a field of the module, because two things read it and only one of
 * them is the module. The module fills it when it decodes or resamples; the view that puts a picture
 * on the screen reads it by the identifier it was handed, from the thread the view lives on, without
 * asking the module anything. A view manager that reached for the module would be doing a registry
 * lookup — and possibly a module construction — on the main thread for every picture a list shows,
 * which is the kind of cost this library exists to remove.
 *
 * The identifier is a name, not a handle. What keeps the memory alive is a reference to the buffer, so
 * a view that holds one keeps its picture alive even after the caller has released the name, and a
 * name that was never held simply has nothing behind it.
 *
 * Nothing here marshals pixels. The table exists so that an image can be produced in native memory and
 * shown from that same memory, with no copy of it ever passing through JavaScript.
 */
internal object ErlRasterBuffers {

  /** The identifiers handed out so far. One, because zero is what a caller starts with. */
  private val identifiers = AtomicInteger(1)

  /** The pictures, by identifier. Concurrent because the module fills it and views read it. */
  private val held = ConcurrentHashMap<Int, ErlRasterBuffer>()

  /**
   * A direct buffer the engine can be given an address of, filed under a new identifier.
   *
   * Filed rather than merely allocated because this is the one path whose memory the caller will name
   * again: a buffer that is handed an identifier is one the caller has to be able to release.
   */
  fun allocate(width: Int, height: Int): ErlRasterBuffer {
    val identifier = identifiers.getAndIncrement()
    val buffer = ErlRasterBuffer(identifier, direct(width, height), width, height)
    held[identifier] = buffer
    return buffer
  }

  /**
   * A direct buffer of `width * height` RGBA8 pixels, which the caller owns outright.
   *
   * Used for the memory that never leaves the call that made it — the two buffers a thumbnail is
   * computed between, for instance. Filing those would keep a screen-sized allocation alive for every
   * image a list ever asked for, which is the pressure this library was built to avoid.
   */
  fun direct(width: Int, height: Int): ByteBuffer =
    ByteBuffer.allocateDirect(width * height * 4).order(ByteOrder.nativeOrder())

  /** The picture behind an identifier, or null when it is not held. */
  fun find(identifier: Int): ErlRasterBuffer? = held[identifier]

  /** Drops one identifier. A view still showing the picture is not affected: it holds the buffer. */
  fun release(identifier: Int) {
    held.remove(identifier)
  }

  /** Drops every identifier, which is what a torn-down module leaves behind. */
  fun releaseAll() {
    held.clear()
  }
}
