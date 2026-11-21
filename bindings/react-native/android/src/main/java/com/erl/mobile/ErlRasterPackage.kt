package com.erl.mobile

import com.facebook.react.ReactPackage
import com.facebook.react.bridge.NativeModule
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.uimanager.ViewManager

/**
 * Puts the raster engine where the JavaScript side can find it.
 *
 * Two things are registered, and they are two halves of the same story. The module is how the engine
 * is *asked* for work: it is handed file paths, and it answers identifiers of buffers it owns, which
 * keeps every pixel out of the bridge. The view manager is how a result reaches the screen without
 * going back through Java at all: it takes one of those identifiers, and the engine draws the picture
 * from the memory it already sits in.
 *
 * Registering them in a package of its own, rather than folding them into a package that already
 * exists, keeps the engine removable: a build that does not want it drops one line from the
 * application and nothing else changes.
 *
 * The view is an ordinary legacy view manager, which the new architecture's interop layer turns into
 * a Fabric component. That is deliberate: a codegen'd Fabric component would need a spec on the
 * JavaScript side and a descriptor on the C++ side to expose a component whose entire surface is one
 * integer, a sampler and a colour.
 */
class ErlRasterPackage : ReactPackage {
  override fun createNativeModules(reactApplicationContext: ReactApplicationContext): List<NativeModule> =
    listOf(ErlRasterModule(reactApplicationContext))

  override fun createViewManagers(reactApplicationContext: ReactApplicationContext): List<ViewManager<*, *>> =
    listOf(ErlRasterViewManager())
}
