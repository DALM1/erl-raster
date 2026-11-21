package com.erl.mobile

import android.util.Log
import com.facebook.react.uimanager.SimpleViewManager
import com.facebook.react.uimanager.ThemedReactContext
import com.facebook.react.uimanager.annotations.ReactProp

/**
 * Puts the engine's zero-copy presenter in the React Native tree.
 *
 * The component is handed the *identifier* of a picture the module already holds — the integer
 * `decodeImage` or `resample` answered — and nothing else. No path is read here, nothing is decoded
 * here, and no pixel crosses the bridge: the picture was produced in native memory, and this view
 * draws it from that same memory. That is the whole point of the display path, and it is why the
 * component takes a number rather than a uri.
 *
 * The view is registered as an ordinary legacy view manager, which is what the interop layer turns
 * into a Fabric component on the new architecture. Nothing here is codegen'd, so the properties below
 * are the complete surface: an identifier, a sampler, and a colour.
 */
class ErlRasterViewManager : SimpleViewManager<ErlRasterView>() {

  override fun getName(): String = "ErlRasterView"

  override fun createViewInstance(reactContext: ThemedReactContext): ErlRasterView =
    ErlRasterView(reactContext)

  /**
   * Shows the picture behind an identifier, or clears the view when there is none.
   *
   * An identifier that is not held is not an error: it is what a caller that released the picture
   * while the view was still showing it leaves behind, and the honest answer is an empty view rather
   * than a crash or a stale frame. `0` is the default the property starts at, and it means nothing is
   * shown.
   */
  @ReactProp(name = "sourceId", defaultInt = 0)
  fun setSourceId(view: ErlRasterView, identifier: Int) {
    if (identifier <= 0) {
      view.setPicture(null)
      return
    }
    val picture = ErlRasterBuffers.find(identifier)
    if (picture == null) {
      Log.w(ErlRasterView.TAG, "no picture is held for identifier $identifier")
    }
    view.setPicture(picture)
  }

  /**
   * Sets the sampler: nearest copies pixels and bilinear blends them.
   *
   * The two codes are the ABI's, shared with the measured path, so a caller that knows one knows the
   * other. Unknown values are refused by the native side rather than guessed at, and a draw that
   * fails says so in logcat and leaves the frame that was on the screen alone.
   */
  @ReactProp(name = "filter", defaultInt = ErlRasterNative.FILTER_BILINEAR)
  fun setFilter(view: ErlRasterView, filter: Int) {
    view.setFilter(filter)
  }

  /**
   * Sets whether the picture is fitted inside the view or cropped to fill it.
   *
   * The names are the ones React Native's own image components use, spelled the same way, so a caller
   * moving between them does not have to learn a second vocabulary. Anything that is not `cover` is
   * the default -- including the null the property arrives as when a caller never sets it: showing
   * the picture whole is the only behaviour that can never lose part of it, and an unknown value is
   * not a reason to crop.
   */
  @ReactProp(name = "contentFit")
  fun setContentFit(view: ErlRasterView, contentFit: String?) {
    view.setContentFit(
      if (contentFit.equals("cover", ignoreCase = true)) ErlRasterView.FIT_COVER
      else ErlRasterView.FIT_CONTAIN,
    )
  }

  /** Sets the colour the rest of the view is filled with, packed as Android packs a colour. */
  @ReactProp(name = "background", defaultInt = ErlRasterView.DEFAULT_BACKGROUND)
  fun setBackground(view: ErlRasterView, background: Int) {
    view.setBackground(background)
  }
}
