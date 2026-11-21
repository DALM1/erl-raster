/*
 * erl_raster_view.h - the C surface of the ERL Raster display path.
 *
 * The other header in this directory, `erl_raster.h`, is the *measured* ABI: a caller hands a buffer
 * of host memory to the library and gets one back, and the arbitration decides which pipeline drew it.
 * This header is the *display* ABI: a caller hands the library a window and a picture, and the library
 * draws into the window. The two are separate handles on purpose. A runtime is built around an arbiter
 * that has to know which pipeline ran and how much it moved; a view has no arbiter, because there is
 * nothing to arbitrate - a picture goes to the screen one way, and the only question is whether it
 * went.
 *
 * Three things differ from the measured ABI, and each is deliberate.
 *
 *   The window is an argument, not a global. `erl_raster_view_create` is handed whatever pointer the
 *   platform's surface offers - an `ANativeWindow*` on Android, a `CAMetalLayer*` on Apple - and the
 *   view borrows it for its whole life. Releasing that window before the view is released is undefined
 *   behaviour, and the ordering is the caller's to keep because no C signature can express it. On
 *   Android that means `ANativeWindow_release` runs *after* `erl_raster_view_destroy`.
 *
 *   A picture is uploaded once. `erl_raster_view_set_source` is the only function here that touches
 *   host memory, and the only one that has to be called again when the picture changes. A view that is
 *   resized, scrolled, or asked for a different crop of the picture it already holds re-renders with
 *   `erl_raster_view_draw` alone and moves no bytes, which is the entire reason this path exists. And
 *   because a picture is named by the identifier its caller gave it, a second view asking for the same
 *   one borrows the texture the first already made: several views of one picture cost one upload.
 *
 *   This path is not byte-exact, and is not offered as a measured one. The filtering is the sampler's
 *   rather than the reference's integer arithmetic, so the pixels it shows are correct to the eye and
 *   not to the byte. That is fine for a screen and would not be fine for a conformance report. What it
 *   does report is the traffic it caused, because that number is the one that decides whether the path
 *   was worth building.
 *
 * These symbols exist only in a build that carries the graphics feature and targets a platform with a
 * presenter, which is Android and Apple. A build without either answers no view symbols at all, so a
 * caller that links against one gets a link error rather than a stub that refuses at run time. The
 * header is written by hand and kept in step with `crates/erl-raster-ffi/src/view.rs`, which is the
 * authority: if the two ever disagree, the Rust source is right and this file is a bug.
 */

#ifndef ERL_RASTER_VIEW_H
#define ERL_RASTER_VIEW_H

/* The return codes, the thermal codes and the resampling filters are shared with the measured ABI and
 * are defined once, in that header. */
#include "erl_raster.h"

#ifdef __cplusplus
extern "C" {
#endif

/*
 * A view, as an application sees it. Opaque on purpose: a caller holds a pointer and never a value,
 * and its fields are none of the caller's business.
 */
typedef struct ErlRasterView ErlRasterView;

/*
 * Builds a view over a window. On success writes the handle to `out` and returns ERL_RASTER_OK; on
 * failure leaves `out` as null and returns a negative code.
 *
 * `window` is an `ANativeWindow*` on Android and a `CAMetalLayer*` on Apple, and must stay valid until
 * `erl_raster_view_destroy` is called on the returned handle. A null window is refused rather than
 * followed.
 *
 * `width` and `height` are the window's size in *physical* pixels and must not be zero: a zero-sized
 * swapchain is refused by the driver, and a view that has not been laid out has no size to give. Create
 * the view from the surface's size-changed callback, not from its creation one.
 */
int32_t erl_raster_view_create(void *window,
                               uint32_t width,
                               uint32_t height,
                               ErlRasterView **out);

/*
 * Releases a view. Passing null is legal and does nothing. Using the handle afterwards is undefined
 * behaviour.
 *
 * The window is *not* released here. It was the caller's before this view existed and it remains the
 * caller's after, so on Android `ANativeWindow_release` is the caller's next step and not this
 * function's.
 */
void erl_raster_view_destroy(ErlRasterView *view);

/*
 * Resizes the view. A no-op when the size has not changed, which is the right answer rather than a
 * shortcut: reconfiguring a surface that is still valid discards the images the driver was holding, so
 * a view that reconfigured on every layout pass would flicker for no reason.
 */
int32_t erl_raster_view_resize(ErlRasterView *view, uint32_t width, uint32_t height);

/*
 * Shows a picture in the view, uploading it only if the device does not already hold it.
 *
 * `identifier` names the picture, and it is the caller's own name for it - the integer the module
 * answered when it decoded or resampled it. Two views given the same identifier and the same size
 * share one texture: the second borrows the first one's, moves no bytes, and says so by reporting `0`
 * in `uploaded_bytes`. That is why the identifier is a parameter rather than something this library
 * invents: only the caller knows that two views mean the same picture.
 *
 * `pixels` is packed RGBA8, `width * height * 4` readable bytes, which is the layout both target
 * platforms hand out. This is the only function here that touches host memory, and the one place the
 * path pays for the picture - and only the first time that picture is seen.
 *
 * `uploaded_bytes` receives the bytes this call moved. It may be null when the caller does not want it,
 * and it is left untouched when the call fails.
 */
int32_t erl_raster_view_set_source(ErlRasterView *view,
                                   uint64_t identifier,
                                   const uint8_t *pixels,
                                   uint32_t width,
                                   uint32_t height,
                                   uint64_t *uploaded_bytes);

/*
 * Forgets the uploaded picture, so the view draws its background alone.
 */
int32_t erl_raster_view_clear_source(ErlRasterView *view);

/*
 * Draws the picture into the view and presents it, answering the traffic the draw caused.
 *
 * `source_x`, `source_y`, `source_width`, `source_height` name the part of the *picture* to show, in
 * the picture's own coordinates. `destination_x`, `destination_y`, `destination_width`,
 * `destination_height` name where it lands in the *view*, in the view's coordinates. `filter` is
 * ERL_RASTER_FILTER_NEAREST or ERL_RASTER_FILTER_BILINEAR; anything else is refused. `background` is
 * the packed `0xAARRGGBB` the rest of the view is filled with.
 *
 * Both rectangles are clipped rather than refused, and a crop that falls entirely outside the picture
 * is a legal thing to ask for - it is what a caller scrolling past a picture produces. The view is
 * still cleared and presented in that case, because the frame that came before it must not be left on
 * the screen.
 *
 * `uploaded_bytes` receives the bytes this call moved and `synchronised` whether the call had to block
 * before returning. Both may be null when the caller does not want them, and both are left untouched
 * when the call fails.
 */
int32_t erl_raster_view_draw(ErlRasterView *view,
                             int32_t source_x,
                             int32_t source_y,
                             uint32_t source_width,
                             uint32_t source_height,
                             int32_t destination_x,
                             int32_t destination_y,
                             uint32_t destination_width,
                             uint32_t destination_height,
                             uint32_t filter,
                             uint32_t background,
                             uint64_t *uploaded_bytes,
                             int32_t *synchronised);

/*
 * Writes the size of the view, in physical pixels, into the caller's two values.
 */
int32_t erl_raster_view_size(ErlRasterView *view, uint32_t *width, uint32_t *height);

/*
 * Writes the size of the picture currently uploaded into the caller's two values, answering 1 when
 * there is one and 0 when there is not.
 *
 * A caller that keeps the crop across frames needs the picture's size rather than the view's, and
 * asking the view is cheaper than remembering what it was told - and cannot drift from what the view
 * actually holds. `width` and `height` are written only when the answer is 1.
 */
int32_t erl_raster_view_source_size(ErlRasterView *view, uint32_t *width, uint32_t *height);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* ERL_RASTER_VIEW_H */
