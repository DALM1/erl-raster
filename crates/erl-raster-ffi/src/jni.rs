//! The Android entry points, written directly on the JNI function table.
//!
//! Everything below this file is the same C ABI a Swift or C++ caller uses, reached through the same
//! functions. The entry points here therefore do not reimplement anything: they translate, they check
//! what the virtual machine hands them, and they call the ABI. That is deliberate. A second copy of
//! the raster logic living behind JNI would be a second place for it to be wrong, and it would be the
//! copy the conformance tests do not cover.
//!
//! Six rules hold, and each one is here because the alternative is a crash or a lie.
//!
//! **No panic crosses the boundary.** A panic unwinding into the virtual machine is undefined
//! behaviour, and a release build that aborts on a bad argument is only marginally better. Every
//! pointer from the virtual machine is checked before it is used, every table entry is checked before
//! it is called, and the only failure that can come out of this file is a return code or a Java
//! exception raised deliberately through `ThrowNew`.
//!
//! **The direct buffer is addressed, not copied.** Kotlin hands a `ByteBuffer.allocateDirect`, whose
//! address the virtual machine already knows, and it is that address the ABI is given. A caller who
//! hands a heap buffer by mistake gets the same answer a null pointer gets, because the virtual
//! machine returns null for one, rather than a silent transfer.
//!
//! **Strings are read as modified UTF-8, and the library only ever writes ASCII.** Device and platform
//! names arrive through `GetStringUTFChars`, which is CESU-8 rather than UTF-8 for anything outside
//! the BMP. Every name a caller passes here is an ASCII device identifier, and every string this file
//! returns is an identifier or a message the core builds from ASCII, so the distinction cannot bite.
//!
//! **The class name is part of the contract.** The entry points are named for
//! `com.erl.mobile.ErlRasterNative`, which is the Kotlin object that declares them. Renaming that
//! object renames these functions.
//!
//! **The thermal state is read, not assumed.** The Kotlin side passes what `PowerManager` reports at
//! the moment of the call, and the ABI refuses to trust a profile taken in any other state. Nothing
//! in this file invents a reading.
//!
//! **Nothing here is thread safe, and nothing pretends to be.** A handle belongs to the thread that
//! opened it. The virtual machine may call these entry points from any thread it likes, so the caller
//! is the one who must not hand the same handle to two of them.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use jni_sys::{
    jboolean, jint, jlong, jobject, jstring, JNIEnv, JNINativeInterface_,
};

use crate::{
    erl_raster_abi_version, erl_raster_begin_frame, erl_raster_blur, erl_raster_close,
    erl_raster_is_trusted, erl_raster_last_backend, erl_raster_last_elapsed_ns, erl_raster_last_error,
    erl_raster_last_synchronised, erl_raster_last_transferred_bytes, erl_raster_open,
    erl_raster_pipeline, erl_raster_pipeline_count, erl_raster_resample, ErlRasterRuntime,
    ERL_RASTER_OK,
};

/// The pointer the virtual machine hands a native method, which is a pointer to the envelope, which is
/// a pointer to the function table. Both levels are checked before either is used.
type Env = *mut JNIEnv;

/// The function table behind the envelope, or nothing when either level is null.
///
/// # Safety
///
/// `env` must be the pointer the virtual machine passed to a native method.
unsafe fn table(env: Env) -> Option<&'static JNINativeInterface_> {
    if env.is_null() {
        return None;
    }
    // Safety: the virtual machine's envelope is valid for the whole call.
    let inner: *const JNINativeInterface_ = unsafe { *env };
    if inner.is_null() {
        return None;
    }
    // Safety: the table lives as long as the virtual machine, which outlives every call made through
    // it. The `'static` is a statement about that, not about the pointer being a leaked allocation.
    Some(unsafe { &*inner })
}

/// Reads a Java string as the library's own text.
///
/// # Safety
///
/// `value` must be null or a `java.lang.String` the virtual machine handed this call.
unsafe fn read_string(env: Env, value: jstring) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let table = unsafe { table(env) }?;
    let read = table.GetStringUTFChars?;
    let release = table.ReleaseStringUTFChars?;
    // Safety: `value` is a string, and the signature of the entry is fixed by the virtual machine.
    let chars = unsafe { read(env, value, std::ptr::null_mut()) };
    if chars.is_null() {
        return None;
    }
    // Safety: the entry returned a NUL-terminated string that stays valid until it is released.
    let text = unsafe { CStr::from_ptr(chars) }.to_string_lossy().into_owned();
    // Safety: `chars` came from the entry above and has not been released.
    unsafe { release(env, value, chars) };
    Some(text)
}

/// Hands a piece of the library's text back to Java.
///
/// The text is always ASCII, so the modified UTF-8 `NewStringUTF` expects and the UTF-8 the core
/// produces are the same bytes. A string containing a NUL cannot be built here, and is reported as a
/// missing string rather than truncated at the NUL.
///
/// # Safety
///
/// `env` must be the pointer the virtual machine passed to a native method.
unsafe fn give_string(env: Env, text: &str) -> jstring {
    let Some(table) = (unsafe { table(env) }) else {
        return std::ptr::null_mut();
    };
    let Some(create) = table.NewStringUTF else {
        return std::ptr::null_mut();
    };
    let Ok(text) = CString::new(text) else {
        return std::ptr::null_mut();
    };
    // Safety: the entry takes a NUL-terminated string and returns a local reference.
    unsafe { create(env, text.as_ptr()) }
}

/// The address of a direct buffer, or nothing when the buffer is null or not direct.
///
/// # Safety
///
/// `buffer` must be null or a `java.nio.ByteBuffer` the virtual machine handed this call.
unsafe fn direct_address(env: Env, buffer: jobject) -> Option<*mut u8> {
    if buffer.is_null() {
        return None;
    }
    let table = unsafe { table(env) }?;
    let address = table.GetDirectBufferAddress?;
    // Safety: `buffer` is not null and the entry is fixed by the virtual machine. A heap buffer makes
    // the entry answer null, which is treated as "not a buffer this can use".
    let pointer = unsafe { address(env, buffer) };
    if pointer.is_null() {
        None
    } else {
        Some(pointer.cast::<u8>())
    }
}

/// Raises a Java exception of the given class, giving up quietly when the class cannot be found.
///
/// # Safety
///
/// `env` must be the pointer the virtual machine passed to a native method.
unsafe fn throw(env: Env, class: &str, message: &str) {
    let Some(table) = (unsafe { table(env) }) else {
        return;
    };
    let (Some(find), Some(raise)) = (table.FindClass, table.ThrowNew) else {
        return;
    };
    let Ok(class) = CString::new(class) else {
        return;
    };
    let Ok(message) = CString::new(message) else {
        return;
    };
    // Safety: both entries take NUL-terminated strings, and the class is a name the virtual machine
    // resolves or refuses. A refusal leaves a pending exception of its own, which is the best answer
    // available and still better than returning silently.
    let found = unsafe { find(env, class.as_ptr()) };
    if found.is_null() {
        return;
    }
    // Safety: `found` is a class the entry just resolved.
    unsafe { raise(env, found, message.as_ptr()) };
}

/// The handle behind a `jlong`, or nothing when it is zero.
fn handle_of(raw: jlong) -> *mut ErlRasterRuntime {
    if raw == 0 {
        std::ptr::null_mut()
    } else {
        raw as usize as *mut ErlRasterRuntime
    }
}

/// A handle, as Java carries it.
fn as_jlong(handle: *mut ErlRasterRuntime) -> jlong {
    handle as usize as jlong
}

/// The message the most recent call left behind, for a caller that has a handle to ask.
///
/// # Safety
///
/// `handle` must be null or a live handle.
unsafe fn last_error_of(handle: *mut ErlRasterRuntime) -> String {
    if handle.is_null() {
        return String::new();
    }
    let mut buffer = [0 as c_char; 512];
    // Safety: the buffer is the length that was passed, and the handle was checked.
    let written = unsafe { erl_raster_last_error(handle, buffer.as_mut_ptr(), buffer.len()) };
    if written <= 0 {
        return String::new();
    }
    // Safety: the entry wrote a NUL-terminated string inside the buffer.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

// -------------------------------------------------------------------------------------------------
// The entry points
// -------------------------------------------------------------------------------------------------

/// `ErlRasterNative.abiVersion`
#[no_mangle]
pub extern "system" fn Java_com_erl_mobile_ErlRasterNative_abiVersion(
    _env: Env,
    _this: jobject,
) -> jint {
    erl_raster_abi_version() as jint
}

/// `ErlRasterNative.open`
///
/// Answers the handle, or zero with an exception raised. A profile that is present and unreadable is
/// refused rather than ignored, exactly as the C ABI refuses it.
#[no_mangle]
pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_open(
    env: Env,
    _this: jobject,
    device: jstring,
    platform: jstring,
    thermal: jint,
    profile: jstring,
) -> jlong {
    let (Some(device), Some(platform)) = (
        // Safety: the virtual machine handed both arguments to this call.
        unsafe { read_string(env, device) },
        unsafe { read_string(env, platform) },
    ) else {
        // Safety: the envelope was handed to this call.
        unsafe { throw(env, "java/lang/IllegalArgumentException", "device and platform must be strings") };
        return 0;
    };

    // Safety: the argument is null or a string the virtual machine handed this call.
    let profile = unsafe { read_string(env, profile) };
    let Ok(device) = CString::new(device) else {
        return 0;
    };
    let Ok(platform) = CString::new(platform) else {
        return 0;
    };
    let Ok(profile) = CString::new(profile.unwrap_or_default()) else {
        return 0;
    };

    let mut handle: *mut ErlRasterRuntime = std::ptr::null_mut();
    // Safety: the strings are NUL-terminated, the profile length is its true length, and the out
    // pointer is to a local that outlives the call.
    let code = unsafe {
        erl_raster_open(
            device.as_ptr(),
            platform.as_ptr(),
            thermal as u32,
            if profile.as_bytes().is_empty() {
                std::ptr::null()
            } else {
                profile.as_ptr()
            },
            profile.as_bytes().len(),
            &mut handle,
        )
    };
    if code != ERL_RASTER_OK {
        // Safety: the envelope was handed to this call.
        unsafe {
            throw(
                env,
                "java/lang/IllegalArgumentException",
                &format!("the erl-raster runtime could not be opened (code {code})"),
            )
        };
        return 0;
    }
    as_jlong(handle)
}

/// `ErlRasterNative.close`
#[no_mangle]
pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_close(
    _env: Env,
    _this: jobject,
    handle: jlong,
) {
    let handle = handle_of(handle);
    if handle.is_null() {
        return;
    }
    // Safety: the caller promised a pointer this library produced that is not used again afterwards.
    unsafe { erl_raster_close(handle) };
}

/// `ErlRasterNative.beginFrame`
#[no_mangle]
pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_beginFrame(
    _env: Env,
    _this: jobject,
    handle: jlong,
    thermal: jint,
    cold: jboolean,
    critical: jboolean,
) -> jint {
    // Safety: the caller promised a handle it still holds, or zero.
    unsafe {
        erl_raster_begin_frame(
            handle_of(handle),
            thermal as u32,
            if cold != 0 { 1 } else { 0 },
            if critical != 0 { 1 } else { 0 },
        ) as jint
    }
}

/// `ErlRasterNative.blur`
///
/// Answers the nanoseconds the operation took, or a negative code. The buffer must be direct: a heap
/// buffer has no address to give the core, and is answered the way a null buffer is.
#[no_mangle]
pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_blur(
    _env: Env,
    _this: jobject,
    handle: jlong,
    pixels: jobject,
    width: jint,
    height: jint,
    x: jint,
    y: jint,
    region_width: jint,
    region_height: jint,
    radius: jint,
    passes: jint,
) -> jlong {
    // Safety: the buffer is null or a direct buffer the virtual machine knows the address of.
    let Some(pixels) = (unsafe { direct_address(_env, pixels) }) else {
        return -1;
    };
    let mut elapsed: u64 = 0;
    // Safety: the handle is null or live, and the address came from a direct buffer the caller owns
    // and promised is `width * height * 4` bytes long.
    let code = unsafe {
        erl_raster_blur(
            handle_of(handle),
            pixels,
            width as u32,
            height as u32,
            x,
            y,
            region_width as u32,
            region_height as u32,
            radius as u32,
            passes as u32,
            &mut elapsed,
        )
    };
    if code == ERL_RASTER_OK {
        elapsed as jlong
    } else {
        code as jlong
    }
}

/// `ErlRasterNative.resample`
///
/// Answers the nanoseconds the operation took, or a negative code. Both buffers must be direct.
#[no_mangle]
pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_resample(
    _env: Env,
    _this: jobject,
    handle: jlong,
    source: jobject,
    source_width: jint,
    source_height: jint,
    pixels: jobject,
    width: jint,
    height: jint,
    destination_x: jint,
    destination_y: jint,
    destination_width: jint,
    destination_height: jint,
    filter: jint,
) -> jlong {
    // Safety: both buffers are null or direct buffers the virtual machine knows the address of.
    let (Some(source), Some(pixels)) = (
        unsafe { direct_address(_env, source) },
        unsafe { direct_address(_env, pixels) },
    ) else {
        return -1;
    };
    let mut elapsed: u64 = 0;
    // Safety: the handle is null or live, both addresses came from direct buffers, and the caller
    // promised each is at least its declared layout long.
    let code = unsafe {
        erl_raster_resample(
            handle_of(handle),
            source,
            source_width as u32,
            source_height as u32,
            pixels,
            width as u32,
            height as u32,
            destination_x,
            destination_y,
            destination_width as u32,
            destination_height as u32,
            filter as u32,
            &mut elapsed,
        )
    };
    if code == ERL_RASTER_OK {
        elapsed as jlong
    } else {
        code as jlong
    }
}

/// `ErlRasterNative.pipelineCount`
#[no_mangle]
pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_pipelineCount(
    _env: Env,
    _this: jobject,
    handle: jlong,
) -> jint {
    // Safety: the caller promised a handle it still holds, or zero.
    unsafe { erl_raster_pipeline_count(handle_of(handle)) as jint }
}

/// `ErlRasterNative.pipeline`
///
/// Answers the identifier at `index`, or null when the index is out of range.
#[no_mangle]
pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_pipeline(
    env: Env,
    _this: jobject,
    handle: jlong,
    index: jint,
) -> jstring {
    let mut buffer = [0 as c_char; 64];
    // Safety: the buffer is the length that was passed, and the handle is null or live.
    let written = unsafe {
        erl_raster_pipeline(
            handle_of(handle),
            index as u32,
            buffer.as_mut_ptr(),
            buffer.len(),
        )
    };
    if written <= 0 {
        return std::ptr::null_mut();
    }
    // Safety: the entry wrote a NUL-terminated identifier inside the buffer.
    let name = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy();
    // Safety: the envelope was handed to this call.
    unsafe { give_string(env, &name) }
}

/// `ErlRasterNative.lastBackend`
#[no_mangle]
pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_lastBackend(
    env: Env,
    _this: jobject,
    handle: jlong,
) -> jstring {
    let mut buffer = [0 as c_char; 64];
    // Safety: the buffer is the length that was passed, and the handle is null or live.
    let written =
        unsafe { erl_raster_last_backend(handle_of(handle), buffer.as_mut_ptr(), buffer.len()) };
    if written <= 0 {
        return std::ptr::null_mut();
    }
    // Safety: the entry wrote a NUL-terminated identifier inside the buffer.
    let name = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy();
    // Safety: the envelope was handed to this call.
    unsafe { give_string(env, &name) }
}

/// `ErlRasterNative.lastElapsedNs`
#[no_mangle]
pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_lastElapsedNs(
    _env: Env,
    _this: jobject,
    handle: jlong,
) -> jlong {
    // Safety: the caller promised a handle it still holds, or zero.
    unsafe { erl_raster_last_elapsed_ns(handle_of(handle)) as jlong }
}

/// `ErlRasterNative.lastTransferredBytes`
#[no_mangle]
pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_lastTransferredBytes(
    _env: Env,
    _this: jobject,
    handle: jlong,
) -> jlong {
    // Safety: the caller promised a handle it still holds, or zero.
    unsafe { erl_raster_last_transferred_bytes(handle_of(handle)) as jlong }
}

/// `ErlRasterNative.lastSynchronised`
#[no_mangle]
pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_lastSynchronised(
    _env: Env,
    _this: jobject,
    handle: jlong,
) -> jboolean {
    // Safety: the caller promised a handle it still holds, or zero.
    let answer = unsafe { erl_raster_last_synchronised(handle_of(handle)) };
    if answer > 0 {
        1
    } else {
        0
    }
}

/// `ErlRasterNative.isTrusted`
#[no_mangle]
pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_isTrusted(
    _env: Env,
    _this: jobject,
    handle: jlong,
) -> jboolean {
    // Safety: the caller promised a handle it still holds, or zero.
    if unsafe { erl_raster_is_trusted(handle_of(handle)) } == 1 {
        1
    } else {
        0
    }
}

/// `ErlRasterNative.lastError`
#[no_mangle]
pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_lastError(
    env: Env,
    _this: jobject,
    handle: jlong,
) -> jstring {
    // Safety: the handle is null or live.
    let message = unsafe { last_error_of(handle_of(handle)) };
    // Safety: the envelope was handed to this call.
    unsafe { give_string(env, &message) }
}

// -------------------------------------------------------------------------------------------------
// The view entry points
// -------------------------------------------------------------------------------------------------

/// The display path, as Kotlin reaches it.
///
/// A view is not a runtime and does not share its handle, so its entry points are grouped here rather
/// than appended to the list above. They exist only in a build that carries the graphics stack: a
/// build without it exports no `view*` symbol at all, which turns a mistake into a link error instead
/// of a call that refuses at run time.
///
/// The window is acquired here rather than passed in. Kotlin holds a `android.view.Surface`, and the
/// conversion to an `ANativeWindow*` is a native call it cannot make itself, so this layer makes it
/// and *owns* the reference it took: the view is released first, the window second, and the order
/// matters because the surface borrows the window until it is dropped.
#[cfg(feature = "gpu")]
mod view {
    use super::*;
    use std::ffi::c_void;
    use std::time::Instant;

    use crate::view::{
        erl_raster_view_clear_source, erl_raster_view_create, erl_raster_view_destroy,
        erl_raster_view_draw, erl_raster_view_resize, erl_raster_view_set_source, ErlRasterView,
    };
    use crate::ERL_RASTER_OK;

    // Two signatures and the library they live in. Declaring them here rather than pulling in a
    // bindings crate keeps the workspace to the dependencies it already has, and there are only two
    // of them: the window behind a surface, and the reference to it this layer holds.
    #[link(name = "android")]
    extern "C" {
        /// Takes a reference to the window behind a Java surface, which the caller must release.
        fn ANativeWindow_fromSurface(env: Env, surface: jobject) -> *mut c_void;
        /// Drops the reference [`ANativeWindow_fromSurface`] took.
        fn ANativeWindow_release(window: *mut c_void);
    }

    /// What Kotlin's `jlong` points at.
    ///
    /// The window is kept next to the view rather than inside it because it is this layer's reference
    /// and not the presenter's: the presenter borrows the window, and this is what keeps the borrow
    /// alive. Releasing them in the other order would be a use-after-free the presenter could not
    /// detect.
    struct JavaView {
        present: *mut ErlRasterView,
        window: *mut c_void,
    }

    /// A pointer, as Java carries it.
    fn as_long<T>(pointer: *mut T) -> jlong {
        pointer as usize as jlong
    }

    /// The pointer behind a `jlong`, or nothing when it is zero.
    fn from_long<T>(raw: jlong) -> *mut T {
        if raw == 0 {
            std::ptr::null_mut()
        } else {
            raw as usize as *mut T
        }
    }

    /// The view behind a `jlong`, or nothing when it is zero.
    ///
    /// # Safety
    ///
    /// `raw` must be zero or a value this module produced for a view that has not been released.
    unsafe fn java_view(raw: jlong) -> Option<&'static JavaView> {
        let pointer = from_long::<JavaView>(raw);
        if pointer.is_null() {
            return None;
        }
        // Safety: the caller promised a live view, and the value lives until `viewDestroy`.
        Some(unsafe { &*pointer })
    }

    /// `ErlRasterNative.viewCreate`
    ///
    /// Takes a reference to the surface's window and answers the view, or zero with an exception
    /// raised. The reference is dropped again by `viewDestroy`, so a caller never sees it.
    #[no_mangle]
    pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_viewCreate(
        env: Env,
        _this: jobject,
        surface: jobject,
        width: jint,
        height: jint,
    ) -> jlong {
        if surface.is_null() {
            // Safety: the envelope was handed to this call.
            unsafe { throw(env, "java/lang/IllegalArgumentException", "the surface must not be null") };
            return 0;
        }
        // Safety: `surface` is a surface the virtual machine handed this call, and the entry is fixed
        // by the NDK.
        let window = unsafe { ANativeWindow_fromSurface(env, surface) };
        if window.is_null() {
            // Safety: the envelope was handed to this call.
            unsafe {
                throw(
                    env,
                    "java/lang/IllegalStateException",
                    "the surface has no window yet; create the view from its size-changed callback",
                )
            };
            return 0;
        }

        let mut present: *mut ErlRasterView = std::ptr::null_mut();
        // Safety: the window is live and this layer keeps the reference it just took, which is what
        // the presenter requires of its caller.
        let code = unsafe {
            erl_raster_view_create(window, width as u32, height as u32, &mut present)
        };
        if code != ERL_RASTER_OK {
            // Safety: the reference was taken above and is not kept, because there is no view to
            // release it later.
            unsafe { ANativeWindow_release(window) };
            // Safety: the envelope was handed to this call.
            unsafe {
                throw(
                    env,
                    "java/lang/IllegalStateException",
                    &format!("the erl-raster view could not be created (code {code})"),
                )
            };
            return 0;
        }

        as_long(Box::into_raw(Box::new(JavaView { present, window })))
    }

    /// `ErlRasterNative.viewDestroy`
    ///
    /// Releases the view and then the window reference, in that order, because the presenter borrows
    /// the window until it is dropped.
    #[no_mangle]
    pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_viewDestroy(
        _env: Env,
        _this: jobject,
        view: jlong,
    ) {
        let pointer = from_long::<JavaView>(view);
        if pointer.is_null() {
            return;
        }
        // Safety: the caller promised a pointer this module produced and is not used again.
        let java = unsafe { Box::from_raw(pointer) };
        // Safety: the presenter was built over this window and has not been released.
        unsafe { erl_raster_view_destroy(java.present) };
        // Safety: the reference was taken by `viewCreate` and is released exactly once, here.
        unsafe { ANativeWindow_release(java.window) };
    }

    /// `ErlRasterNative.viewResize`
    #[no_mangle]
    pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_viewResize(
        _env: Env,
        _this: jobject,
        view: jlong,
        width: jint,
        height: jint,
    ) -> jint {
        // Safety: the caller promised a handle it still holds, or zero.
        let Some(java) = (unsafe { java_view(view) }) else {
            return -1;
        };
        // Safety: the view is live, and a size of zero is refused by the ABI rather than trusted.
        unsafe { erl_raster_view_resize(java.present, width as u32, height as u32) as jint }
    }

    /// `ErlRasterNative.viewSetSource`
    ///
    /// The buffer must be direct: a heap buffer has no address to give the presenter, and is answered
    /// the way a null buffer is.
    ///
    /// Answers the bytes this call moved, or a negative code. Zero is the interesting answer: it means
    /// the picture was already on the device under that identifier, so another view had uploaded it
    /// and this one borrowed it. Reporting the number rather than a bare success is what lets the
    /// Kotlin side log what actually happened instead of claiming an upload that did not occur.
    #[no_mangle]
    pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_viewSetSource(
        env: Env,
        _this: jobject,
        view: jlong,
        identifier: jlong,
        pixels: jobject,
        width: jint,
        height: jint,
    ) -> jlong {
        // Safety: the buffer is null or a direct buffer the virtual machine knows the address of.
        let Some(pixels) = (unsafe { direct_address(env, pixels) }) else {
            return -1;
        };
        // Safety: the caller promised a handle it still holds, or zero.
        let Some(java) = (unsafe { java_view(view) }) else {
            return -1;
        };
        let mut uploaded: u64 = 0;
        // Safety: the address came from a direct buffer the caller owns, and the caller promised it
        // is `width * height * 4` bytes long.
        let code = unsafe {
            erl_raster_view_set_source(
                java.present,
                identifier as u64,
                pixels,
                width as u32,
                height as u32,
                &mut uploaded,
            )
        };
        if code == ERL_RASTER_OK {
            uploaded as jlong
        } else {
            code as jlong
        }
    }

    /// `ErlRasterNative.viewClearSource`
    #[no_mangle]
    pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_viewClearSource(
        _env: Env,
        _this: jobject,
        view: jlong,
    ) -> jint {
        // Safety: the caller promised a handle it still holds, or zero.
        let Some(java) = (unsafe { java_view(view) }) else {
            return -1;
        };
        // Safety: the view is live.
        unsafe { erl_raster_view_clear_source(java.present) as jint }
    }

    /// `ErlRasterNative.viewDraw`
    ///
    /// Answers the nanoseconds the draw took, or a negative code. The duration is taken around the
    /// whole submission — the acquire, the pass and the present — because a shorter one, read from
    /// inside the driver, would be a number that looks better and describes nothing.
    #[no_mangle]
    pub unsafe extern "system" fn Java_com_erl_mobile_ErlRasterNative_viewDraw(
        _env: Env,
        _this: jobject,
        view: jlong,
        source_x: jint,
        source_y: jint,
        source_width: jint,
        source_height: jint,
        destination_x: jint,
        destination_y: jint,
        destination_width: jint,
        destination_height: jint,
        filter: jint,
        background: jint,
    ) -> jlong {
        // Safety: the caller promised a handle it still holds, or zero.
        let Some(java) = (unsafe { java_view(view) }) else {
            return -1;
        };

        let started = Instant::now();
        // Safety: the view is live, and neither output is asked for through this path.
        let code = unsafe {
            erl_raster_view_draw(
                java.present,
                source_x,
                source_y,
                source_width as u32,
                source_height as u32,
                destination_x,
                destination_y,
                destination_width as u32,
                destination_height as u32,
                filter as u32,
                background as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if code == ERL_RASTER_OK {
            started.elapsed().as_nanos() as jlong
        } else {
            code as jlong
        }
    }
}
