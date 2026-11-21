//! The view ABI: the display path, reachable from Kotlin and Swift.
//!
//! The measured half of this crate takes a host buffer and hands one back. This half takes a *window*
//! and draws into it, which is what lets the library replace a view rather than only compute for one.
//! The two are separate handles on purpose. A runtime is built around an arbiter that has to know
//! which pipeline ran and what it moved; a view has no arbiter, because there is nothing to arbitrate
//! — a picture goes to the screen one way, and the only question is whether it went.
//!
//! Three things differ from the measured ABI, and each was taken deliberately.
//!
//! **The window is an argument, not a global.** `erl_raster_view_create` is handed whatever pointer
//! the platform's surface offers — an `ANativeWindow*` on Android, a `CAMetalLayer*` on Apple — and
//! the view borrows it for its whole life. Releasing the window before the view is released is
//! undefined behaviour, and the ordering is the caller's to keep because no C signature can express
//! it. On Android that means `ANativeWindow_release` happens after [`erl_raster_view_destroy`].
//!
//! **A picture is uploaded once.** `erl_raster_view_set_source` is the only function that touches host
//! memory, and it is the only one that has to be called again when the picture changes. A view that is
//! resized, scrolled, or asked for a different crop of the picture it already holds re-renders with
//! [`erl_raster_view_draw`] alone and moves no bytes, which is the whole reason this path exists. And
//! because a picture is named by the identifier its caller gave it, a second view asking for the same
//! one borrows the texture the first already made: several views of one picture cost one upload.
//!
//! **This path is not byte-exact, and is not offered as a measured one.** The filtering is the
//! sampler's rather than the reference's integer arithmetic, so the pixels it shows are correct to the
//! eye and not to the byte. That is fine for a screen and would not be fine for a conformance report,
//! which is why the conformance rule stays where it was written and this file claims no measurement.
//! What it does report is the traffic it caused, because that number is the one that decides whether
//! the path was worth building.

use std::ffi::c_void;
use std::os::raw::c_int;

use erl_raster_core::geom::{Rect, Size};
use erl_render_backends::{PresentFilter, Presenter, WindowHandle};

use crate::{
    checked_layout, code_for, ERL_RASTER_INVALID_ARGUMENT, ERL_RASTER_OK,
    ERL_RASTER_UNSUPPORTED_SIZE, FILTER_BILINEAR, FILTER_NEAREST,
};

/// The presentation pipeline, as an application sees it.
///
/// Opaque by construction: the struct is public only because the pointer type has to name something,
/// and its one field is private. A caller holds an `ErlRasterView*` and never a value of it.
pub struct ErlRasterView {
    presenter: Presenter,
}

impl core::fmt::Debug for ErlRasterView {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The presenter is what a reader wants to see; wrapping it in a second struct name would only
        // add a line to a log without saying anything.
        core::fmt::Debug::fmt(&self.presenter, f)
    }
}

/// The window handle, in the variant this platform's surfaces come in.
///
/// One `cfg` pair rather than a trait: there are exactly two platforms, and the compiler picks the
/// variant, so a handle of the wrong kind cannot be written down, let alone passed.
#[cfg(target_os = "android")]
fn window_handle(window: *mut c_void) -> WindowHandle {
    WindowHandle::AndroidNdk(window)
}

/// The window handle, in the variant this platform's surfaces come in.
#[cfg(target_vendor = "apple")]
fn window_handle(window: *mut c_void) -> WindowHandle {
    WindowHandle::CoreAnimationLayer(window)
}

/// Unpacks the colour a caller passed into straight eight-bit channels.
///
/// The packing is `0xAARRGGBB`, which is what an Android `Color` int carries and what a caller on any
/// platform can build with three shifts. It arrives packed rather than as four arguments because the
/// alternative is a signature with four more numbers that mean nothing individually.
fn channels(background: u32) -> [u8; 4] {
    [
        (background >> 16) as u8,
        (background >> 8) as u8,
        background as u8,
        (background >> 24) as u8,
    ]
}

/// Turns a filter code into the sampler's, or refuses an unknown one.
fn filter_of(filter: u32) -> Option<PresentFilter> {
    match filter {
        FILTER_NEAREST => Some(PresentFilter::Nearest),
        FILTER_BILINEAR => Some(PresentFilter::Bilinear),
        _ => None,
    }
}

// -------------------------------------------------------------------------------------------------
// The ABI
// -------------------------------------------------------------------------------------------------

/// Builds a view over a window, or answers a negative code and leaves `out` untouched.
///
/// `window` is an `ANativeWindow*` on Android and a `CAMetalLayer*` on Apple, and must stay valid
/// until [`erl_raster_view_destroy`] is called on the value returned here. `width` and `height` are
/// the window's size in *physical* pixels and must not be zero: a zero-sized swapchain is refused by
/// the driver, and a view that has not been laid out has no size to give. A caller therefore creates
/// the view from the surface's size-changed callback, not from its creation one.
///
/// A window pointer that is null is refused rather than followed, and so is a size of zero. Both are
/// answered as codes because a caller that got neither a view nor a reason would have nothing to log.
///
/// # Safety
///
/// `window` must be null or a live window of this platform's kind, and `out` must point to a writable
/// `ErlRasterView*`.
#[no_mangle]
pub unsafe extern "C" fn erl_raster_view_create(
    window: *mut c_void,
    width: u32,
    height: u32,
    out: *mut *mut ErlRasterView,
) -> c_int {
    if out.is_null() {
        return ERL_RASTER_INVALID_ARGUMENT;
    }
    // Safety: the caller promised a writable pointer, and it is checked above.
    unsafe { *out = std::ptr::null_mut() };
    if window.is_null() {
        return ERL_RASTER_INVALID_ARGUMENT;
    }
    if width == 0 || height == 0 {
        return ERL_RASTER_UNSUPPORTED_SIZE;
    }

    // Safety: the caller promised the window behind the handle outlives the value built here, which is
    // the one obligation this function cannot check on its own.
    match unsafe { Presenter::create(window_handle(window), Size::new(width, height)) } {
        Ok(presenter) => {
            // Safety: the caller promised a writable pointer, and it is checked above.
            unsafe { *out = Box::into_raw(Box::new(ErlRasterView { presenter })) };
            ERL_RASTER_OK
        }
        Err(error) => code_for(&error),
    }
}

/// Releases a view. Passing null is legal and does nothing.
///
/// The window is *not* released here. It was the caller's before this view existed and it is the
/// caller's after, which on Android means `ANativeWindow_release` is the caller's next step and not
/// this function's.
///
/// # Safety
///
/// `view` must be null or a pointer returned by [`erl_raster_view_create`] that has not already been
/// released. Using the handle after this call is undefined behaviour.
#[no_mangle]
pub unsafe extern "C" fn erl_raster_view_destroy(view: *mut ErlRasterView) {
    if view.is_null() {
        return;
    }
    // Safety: the caller promised a pointer from `erl_raster_view_create` that is still live and is
    // not used again after this call.
    drop(unsafe { Box::from_raw(view) });
}

/// Resizes the view. A no-op when the size has not changed.
///
/// A no-op is the right answer rather than a shortcut: reconfiguring a surface that is still valid
/// discards the images the driver was holding, and a view that reconfigures on every layout pass would
/// flicker for no reason.
///
/// # Safety
///
/// `view` must be a live handle from [`erl_raster_view_create`].
#[no_mangle]
pub unsafe extern "C" fn erl_raster_view_resize(
    view: *mut ErlRasterView,
    width: u32,
    height: u32,
) -> c_int {
    // Safety: the caller promised a live handle.
    let Some(handle) = (unsafe { view.as_mut() }) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };
    match handle.presenter.resize(Size::new(width, height)) {
        Ok(()) => ERL_RASTER_OK,
        Err(error) => code_for(&error),
    }
}

/// Shows a picture in the view, uploading it only if the device does not already hold it.
///
/// `identifier` names the picture, and it is the caller's own name for it — the integer the module
/// answered when it decoded or resampled it. Two views given the same identifier and the same size
/// share one texture: the second one borrows the first one's, moves no bytes, and says so by reporting
/// `0` in `uploaded_bytes`. That is the whole reason the identifier is a parameter rather than
/// something this file invents: only the caller knows that two views mean the same picture.
///
/// `pixels` is packed RGBA8, `width * height * 4` readable bytes, which is the layout both target
/// platforms hand out. This is the only function in this file that touches host memory, and the one
/// place the path pays for the picture — and only the first time that picture is seen.
///
/// `uploaded_bytes` receives the bytes this call moved; it may be null when the caller does not want
/// it, and is left untouched when the call fails.
///
/// # Safety
///
/// `view` must be a live handle, `pixels` must point to `width * height * 4` readable bytes, and
/// `uploaded_bytes` must be null or point to a writable `u64`.
#[no_mangle]
pub unsafe extern "C" fn erl_raster_view_set_source(
    view: *mut ErlRasterView,
    identifier: u64,
    pixels: *const u8,
    width: u32,
    height: u32,
    uploaded_bytes: *mut u64,
) -> c_int {
    // Safety: the caller promised a live handle.
    let Some(handle) = (unsafe { view.as_mut() }) else {
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
    let caller_pixels = unsafe { std::slice::from_raw_parts(pixels, bytes) };
    match handle
        .presenter
        .set_source(identifier, Size::new(width, height), caller_pixels)
    {
        Ok(uploaded) => {
            if !uploaded_bytes.is_null() {
                // Safety: the caller promised a writable `u64` at this address.
                unsafe { *uploaded_bytes = uploaded };
            }
            ERL_RASTER_OK
        }
        Err(error) => code_for(&error),
    }
}

/// Forgets the uploaded picture, so the view draws its background alone.
///
/// # Safety
///
/// `view` must be a live handle from [`erl_raster_view_create`].
#[no_mangle]
pub unsafe extern "C" fn erl_raster_view_clear_source(view: *mut ErlRasterView) -> c_int {
    // Safety: the caller promised a live handle.
    let Some(handle) = (unsafe { view.as_mut() }) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };
    handle.presenter.clear_source();
    ERL_RASTER_OK
}

/// Draws the picture into the view and presents it, answering the traffic the draw caused.
///
/// `source_x`, `source_y`, `source_width`, `source_height` name the part of the *picture* to show, in
/// the picture's own coordinates. `destination_x`, `destination_y`, `destination_width`,
/// `destination_height` name where it lands in the *view*, in the view's coordinates. `filter` is `0`
/// for nearest and `1` for bilinear, the same two codes the measured ABI uses. `background` is the
/// packed `0xAARRGGBB` the rest of the view is filled with.
///
/// Both rectangles are clipped rather than refused, and a crop that falls entirely outside the picture
/// is a legal thing to ask for — it is what a caller scrolling past a picture produces. The view is
/// still cleared and presented in that case, because the frame that came before must not be left on
/// the screen.
///
/// `uploaded_bytes` receives the bytes this call moved and `synchronised` whether it had to block
/// before returning; both may be null when the caller does not want them, and both are left untouched
/// when the call fails.
///
/// # Safety
///
/// `view` must be a live handle, and `uploaded_bytes` and `synchronised` must each be null or point to
/// a writable value of their type.
#[no_mangle]
pub unsafe extern "C" fn erl_raster_view_draw(
    view: *mut ErlRasterView,
    source_x: i32,
    source_y: i32,
    source_width: u32,
    source_height: u32,
    destination_x: i32,
    destination_y: i32,
    destination_width: u32,
    destination_height: u32,
    filter: u32,
    background: u32,
    uploaded_bytes: *mut u64,
    synchronised: *mut c_int,
) -> c_int {
    // Safety: the caller promised a live handle.
    let Some(handle) = (unsafe { view.as_mut() }) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };
    let Some(filter) = filter_of(filter) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };

    let region = Rect::new(source_x, source_y, source_width, source_height);
    let destination = Rect::new(destination_x, destination_y, destination_width, destination_height);
    match handle
        .presenter
        .draw(region, destination, filter, channels(background))
    {
        Ok(presented) => {
            if !uploaded_bytes.is_null() {
                // Safety: the caller promised a writable `u64` at this address.
                unsafe { *uploaded_bytes = presented.uploaded_bytes };
            }
            if !synchronised.is_null() {
                // Safety: the caller promised a writable `c_int` at this address.
                unsafe { *synchronised = c_int::from(presented.synchronised) };
            }
            ERL_RASTER_OK
        }
        Err(error) => code_for(&error),
    }
}

/// Writes the size of the view, in physical pixels, into the caller's two values.
///
/// # Safety
///
/// `view` must be a live handle, and `width` and `height` must point to writable `u32`s.
#[no_mangle]
pub unsafe extern "C" fn erl_raster_view_size(
    view: *mut ErlRasterView,
    width: *mut u32,
    height: *mut u32,
) -> c_int {
    // Safety: the caller promised a live handle.
    let Some(handle) = (unsafe { view.as_ref() }) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };
    if width.is_null() || height.is_null() {
        return ERL_RASTER_INVALID_ARGUMENT;
    }
    let size = handle.presenter.size();
    // Safety: the caller promised both writable `u32`s, and both were checked above.
    unsafe {
        *width = size.width;
        *height = size.height;
    }
    ERL_RASTER_OK
}

/// Writes the size of the picture currently uploaded into the caller's two values, answering `1` when
/// there is one and `0` when there is not.
///
/// A caller that keeps the crop across frames needs the picture's size rather than the view's, and
/// asking the view is cheaper than remembering what it was told — and cannot drift from what the view
/// actually holds. `width` and `height` are written only when the answer is `1`.
///
/// # Safety
///
/// `view` must be a live handle, and `width` and `height` must each be null or point to a writable
/// `u32`.
#[no_mangle]
pub unsafe extern "C" fn erl_raster_view_source_size(
    view: *mut ErlRasterView,
    width: *mut u32,
    height: *mut u32,
) -> c_int {
    // Safety: the caller promised a live handle.
    let Some(handle) = (unsafe { view.as_ref() }) else {
        return ERL_RASTER_INVALID_ARGUMENT;
    };
    let Some(size) = handle.presenter.source_size() else {
        return 0;
    };
    if !width.is_null() {
        // Safety: the caller promised a writable `u32` at this address.
        unsafe { *width = size.width };
    }
    if !height.is_null() {
        // Safety: the caller promised a writable `u32` at this address.
        unsafe { *height = size.height };
    }
    1
}

