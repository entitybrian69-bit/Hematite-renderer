//! The implemented EGL entry points.
//!
//! This is a *software* EGL: `eglGetDisplay` returns a handle for a built-in display,
//! configs come from one static RGBA8 entry, pbuffer surfaces own a real framebuffer and
//! `eglSwapBuffers` publishes the frame so a host or a test can read it back.
//!
//! What it does **not** do is put pixels on a screen. `eglCreateWindowSurface` accepts an
//! `ANativeWindow` pointer but cannot query or lock it — that needs the NDK, which this
//! build never links — so a window surface behaves like a pbuffer and nothing is
//! presented. See `docs/KNOWN_LIMITATIONS.md`.

//! Entry point names keep their C spelling, so the lint is off for this module.
#![allow(non_snake_case)]

use crate::context::{with, Framebuffer};
use crate::gl_types::*;
use std::collections::HashSet;
use std::ffi::{c_char, c_void, CStr};
use std::sync::{Mutex, MutexGuard};

/// The client API Hematite implements.
pub const EGL_OPENGL_ES_API: i32 = 0x30A0;

const VENDOR: &[u8] = b"Hematite\0";
const VERSION: &[u8] = b"1.4 Hematite software EGL (no window system)\0";
const EXTENSIONS: &[u8] = b"EGL_KHR_surfaceless_context\0";
const CLIENT_APIS: &[u8] = b"OpenGL_ES\0";

/// The single display handle. Its address is the handle; it is never freed.
static DISPLAY: u8 = 0;
/// The single config handle.
static CONFIG: u8 = 0;
/// Live surface handles, so a stale pointer is rejected instead of dereferenced.
static SURFACES: Mutex<Option<HashSet<usize>>> = Mutex::new(None);
/// Live context handles.
static CONTEXTS: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

fn registry(
    live: &'static Mutex<Option<HashSet<usize>>>,
) -> MutexGuard<'static, Option<HashSet<usize>>> {
    live.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn is_live(live: &'static Mutex<Option<HashSet<usize>>>, handle: *mut c_void) -> bool {
    registry(live).as_ref().is_some_and(|set| set.contains(&(handle as usize)))
}

fn insert(live: &'static Mutex<Option<HashSet<usize>>>, handle: *mut c_void) {
    registry(live).get_or_insert_with(HashSet::new).insert(handle as usize);
}

fn remove(live: &'static Mutex<Option<HashSet<usize>>>, handle: *mut c_void) {
    if let Some(set) = registry(live).as_mut() {
        set.remove(&(handle as usize));
    }
}

thread_local! {
    static ERROR: std::cell::Cell<i32> = const { std::cell::Cell::new(EGL_SUCCESS) };
    static CURRENT_SURFACE: std::cell::Cell<*mut c_void> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    static CURRENT_CONTEXT: std::cell::Cell<*mut c_void> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    static SWAP_INTERVAL: std::cell::Cell<i32> = const { std::cell::Cell::new(1) };
    static API: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

fn fail(error: i32) -> u32 {
    ERROR.with(|cell| cell.set(error));
    EGL_FALSE
}

/// An owned render target plus what has been presented into it.
struct Surface {
    width: i32,
    height: i32,
    swaps: u64,
    /// The frame `eglSwapBuffers` published most recently.
    frame: Option<Framebuffer>,
}

fn attributes(list: *const i32) -> std::collections::HashMap<i32, i32> {
    let mut map = std::collections::HashMap::new();
    if list.is_null() {
        return map;
    }
    let mut index = 0;
    loop {
        let key = unsafe { list.add(index).read_unaligned() };
        if key == EGL_NONE {
            break;
        }
        let value = unsafe { list.add(index + 1).read_unaligned() };
        map.insert(key, value);
        index += 2;
        if index > 512 {
            break;
        }
    }
    map
}

/// Return the built-in display for any native display value.
pub fn eglGetDisplay(_display_id: *mut c_void) -> *mut c_void {
    std::ptr::from_ref(&DISPLAY).cast_mut().cast()
}

/// Report the supported version and mark the display usable.
pub fn eglInitialize(dpy: *mut c_void, major: *mut i32, minor: *mut i32) -> u32 {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        return fail(EGL_BAD_DISPLAY);
    }
    if !major.is_null() {
        unsafe { major.write_unaligned(1) };
    }
    if !minor.is_null() {
        unsafe { minor.write_unaligned(4) };
    }
    EGL_TRUE
}

/// Accepted; there is nothing to release.
pub fn eglTerminate(dpy: *mut c_void) -> u32 {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        return fail(EGL_BAD_DISPLAY);
    }
    EGL_TRUE
}

/// Static implementation strings.
pub fn eglQueryString(dpy: *mut c_void, name: i32) -> *const c_char {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        fail(EGL_BAD_DISPLAY);
        return std::ptr::null();
    }
    let bytes: &'static [u8] = match name {
        EGL_VENDOR => VENDOR,
        EGL_VERSION => VERSION,
        EGL_EXTENSIONS => EXTENSIONS,
        EGL_CLIENT_APIS => CLIENT_APIS,
        _ => {
            fail(EGL_BAD_PARAMETER);
            return std::ptr::null();
        }
    };
    bytes.as_ptr().cast()
}

/// Return and clear the last EGL error.
pub fn eglGetError() -> i32 {
    ERROR.with(|cell| cell.replace(EGL_SUCCESS))
}

/// The one config Hematite offers: RGBA8 with a 24-bit depth buffer.
pub fn eglGetConfigs(
    dpy: *mut c_void,
    configs: *mut *mut c_void,
    config_size: i32,
    num_config: *mut i32,
) -> u32 {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        return fail(EGL_BAD_DISPLAY);
    }
    if num_config.is_null() {
        return fail(EGL_BAD_PARAMETER);
    }
    let offered = if configs.is_null() || config_size < 1 { 0 } else { 1 };
    if offered == 1 {
        unsafe { configs.write_unaligned(std::ptr::from_ref(&CONFIG).cast_mut().cast()) };
    }
    unsafe { num_config.write_unaligned(offered) };
    EGL_TRUE
}

/// Match the attribute list against the single config.
pub fn eglChooseConfig(
    dpy: *mut c_void,
    attrib_list: *const i32,
    configs: *mut *mut c_void,
    config_size: i32,
    num_config: *mut i32,
) -> u32 {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        return fail(EGL_BAD_DISPLAY);
    }
    if num_config.is_null() {
        return fail(EGL_BAD_PARAMETER);
    }
    let requested = attributes(attrib_list);
    let acceptable = requested.iter().all(|(key, value)| match *key {
        EGL_SURFACE_TYPE => value & EGL_PBUFFER_BIT != 0 || value & EGL_WINDOW_BIT != 0,
        EGL_RENDERABLE_TYPE => value & EGL_OPENGL_ES2_BIT != 0,
        EGL_RED_SIZE | EGL_GREEN_SIZE | EGL_BLUE_SIZE | EGL_ALPHA_SIZE => *value <= 8,
        EGL_DEPTH_SIZE => *value <= 24,
        EGL_STENCIL_SIZE | EGL_BUFFER_SIZE => true,
        _ => true,
    });
    let offered = if acceptable && !configs.is_null() && config_size >= 1 { 1 } else { 0 };
    if offered == 1 {
        unsafe { configs.write_unaligned(std::ptr::from_ref(&CONFIG).cast_mut().cast()) };
    }
    unsafe { num_config.write_unaligned(offered) };
    if acceptable {
        EGL_TRUE
    } else {
        fail(EGL_BAD_ATTRIBUTE)
    }
}

/// Read one config attribute.
pub fn eglGetConfigAttrib(
    dpy: *mut c_void,
    config: *mut c_void,
    attribute: i32,
    value: *mut i32,
) -> u32 {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        return fail(EGL_BAD_DISPLAY);
    }
    if config != std::ptr::from_ref(&CONFIG).cast_mut().cast() {
        return fail(EGL_BAD_CONFIG);
    }
    if value.is_null() {
        return fail(EGL_BAD_PARAMETER);
    }
    let result = match attribute {
        EGL_RED_SIZE | EGL_GREEN_SIZE | EGL_BLUE_SIZE | EGL_ALPHA_SIZE => 8,
        EGL_DEPTH_SIZE => 24,
        EGL_STENCIL_SIZE => 0,
        EGL_BUFFER_SIZE => 32,
        EGL_SURFACE_TYPE => EGL_PBUFFER_BIT | EGL_WINDOW_BIT,
        EGL_RENDERABLE_TYPE => EGL_OPENGL_ES2_BIT,
        _ => {
            return fail(EGL_BAD_ATTRIBUTE);
        }
    };
    unsafe { value.write_unaligned(result) };
    EGL_TRUE
}

fn create_surface(width: i32, height: i32) -> *mut c_void {
    if width <= 0 || height <= 0 {
        return std::ptr::null_mut();
    }
    let surface = Box::new(Surface { width, height, swaps: 0, frame: None });
    let handle = Box::into_raw(surface).cast::<c_void>();
    insert(&SURFACES, handle);
    handle
}

/// Create a pbuffer surface of the requested size.
pub fn eglCreatePbufferSurface(
    dpy: *mut c_void,
    config: *mut c_void,
    attrib_list: *const i32,
) -> *mut c_void {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        fail(EGL_BAD_DISPLAY);
        return std::ptr::null_mut();
    }
    if config != std::ptr::from_ref(&CONFIG).cast_mut().cast() {
        fail(EGL_BAD_CONFIG);
        return std::ptr::null_mut();
    }
    let requested = attributes(attrib_list);
    let width = requested.get(&EGL_WIDTH).copied().unwrap_or(1);
    let height = requested.get(&EGL_HEIGHT).copied().unwrap_or(1);
    let handle = create_surface(width, height);
    if handle.is_null() {
        fail(EGL_BAD_ATTRIBUTE);
    }
    handle
}

/// Accept a native window, but do not attach to it: see the module documentation.
pub fn eglCreateWindowSurface(
    dpy: *mut c_void,
    config: *mut c_void,
    win: *mut c_void,
    attrib_list: *const i32,
) -> *mut c_void {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        fail(EGL_BAD_DISPLAY);
        return std::ptr::null_mut();
    }
    if config != std::ptr::from_ref(&CONFIG).cast_mut().cast() {
        fail(EGL_BAD_CONFIG);
        return std::ptr::null_mut();
    }
    if win.is_null() {
        fail(EGL_BAD_NATIVE_WINDOW);
        return std::ptr::null_mut();
    }
    let requested = attributes(attrib_list);
    let handle = create_surface(
        requested.get(&EGL_WIDTH).copied().unwrap_or(1),
        requested.get(&EGL_HEIGHT).copied().unwrap_or(1),
    );
    if handle.is_null() {
        fail(EGL_BAD_ALLOC);
    }
    handle
}

/// Free a surface, releasing the current binding if it was current.
pub fn eglDestroySurface(dpy: *mut c_void, surface: *mut c_void) -> u32 {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        return fail(EGL_BAD_DISPLAY);
    }
    if surface.is_null() {
        return EGL_TRUE;
    }
    if !is_live(&SURFACES, surface) {
        return fail(EGL_BAD_SURFACE);
    }
    remove(&SURFACES, surface);
    CURRENT_SURFACE.with(|cell| {
        if cell.get() == surface {
            cell.set(std::ptr::null_mut());
        }
    });
    drop(unsafe { Box::from_raw(surface.cast::<Surface>()) });
    EGL_TRUE
}

/// Read a surface attribute.
pub fn eglQuerySurface(
    dpy: *mut c_void,
    surface: *mut c_void,
    attribute: i32,
    value: *mut i32,
) -> u32 {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        return fail(EGL_BAD_DISPLAY);
    }
    if !is_live(&SURFACES, surface) {
        return fail(EGL_BAD_SURFACE);
    }
    if value.is_null() {
        return fail(EGL_BAD_PARAMETER);
    }
    let result = match attribute {
        EGL_WIDTH => unsafe { (*surface.cast::<Surface>()).width },
        EGL_HEIGHT => unsafe { (*surface.cast::<Surface>()).height },
        _ => return fail(EGL_BAD_ATTRIBUTE),
    };
    unsafe { value.write_unaligned(result) };
    EGL_TRUE
}

/// Create a context for the requested client version.
pub fn eglCreateContext(
    dpy: *mut c_void,
    config: *mut c_void,
    _share_context: *mut c_void,
    attrib_list: *const i32,
) -> *mut c_void {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        fail(EGL_BAD_DISPLAY);
        return std::ptr::null_mut();
    }
    if config != std::ptr::from_ref(&CONFIG).cast_mut().cast() {
        fail(EGL_BAD_CONFIG);
        return std::ptr::null_mut();
    }
    let requested = attributes(attrib_list);
    let version = requested.get(&EGL_CONTEXT_CLIENT_VERSION).copied().unwrap_or(1);
    if !(1..=3).contains(&version) {
        fail(EGL_BAD_ATTRIBUTE);
        return std::ptr::null_mut();
    }
    // A context is a distinct, non-null, opaque handle; one byte per context is enough.
    let handle = Box::into_raw(Box::new(version as u8)).cast::<c_void>();
    insert(&CONTEXTS, handle);
    handle
}

/// Free a context, releasing the current binding if it was current.
pub fn eglDestroyContext(dpy: *mut c_void, ctx: *mut c_void) -> u32 {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        return fail(EGL_BAD_DISPLAY);
    }
    if ctx.is_null() {
        return EGL_TRUE;
    }
    if !is_live(&CONTEXTS, ctx) {
        return fail(EGL_BAD_CONTEXT);
    }
    remove(&CONTEXTS, ctx);
    CURRENT_CONTEXT.with(|cell| {
        if cell.get() == ctx {
            cell.set(std::ptr::null_mut());
        }
    });
    drop(unsafe { Box::from_raw(ctx.cast::<u8>()) });
    EGL_TRUE
}

/// Bind a context and surface to this thread and size the framebuffer to the surface.
pub fn eglMakeCurrent(
    dpy: *mut c_void,
    draw: *mut c_void,
    read: *mut c_void,
    ctx: *mut c_void,
) -> u32 {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        return fail(EGL_BAD_DISPLAY);
    }
    if draw.is_null() && read.is_null() && ctx.is_null() {
        CURRENT_SURFACE.with(|cell| cell.set(std::ptr::null_mut()));
        CURRENT_CONTEXT.with(|cell| cell.set(std::ptr::null_mut()));
        return EGL_TRUE;
    }
    if !is_live(&SURFACES, draw) || !is_live(&SURFACES, read) {
        return fail(EGL_BAD_SURFACE);
    }
    if !is_live(&CONTEXTS, ctx) {
        return fail(EGL_BAD_CONTEXT);
    }
    let (width, height) =
        unsafe { ((*draw.cast::<Surface>()).width, (*draw.cast::<Surface>()).height) };
    with(|context| context.reset(width.max(1) as usize, height.max(1) as usize));
    CURRENT_SURFACE.with(|cell| cell.set(draw));
    CURRENT_CONTEXT.with(|cell| cell.set(ctx));
    EGL_TRUE
}

/// Publish the current frame and count the swap.
pub fn eglSwapBuffers(dpy: *mut c_void, surface: *mut c_void) -> u32 {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        return fail(EGL_BAD_DISPLAY);
    }
    if !is_live(&SURFACES, surface) {
        return fail(EGL_BAD_SURFACE);
    }
    if CURRENT_SURFACE.with(|cell| cell.get()) != surface {
        return fail(EGL_BAD_SURFACE);
    }
    let frame = with(|context| context.framebuffer.clone());
    crate::context::present(&frame);
    let surface = unsafe { &mut *surface.cast::<Surface>() };
    surface.swaps += 1;
    surface.frame = Some(frame);
    EGL_TRUE
}

/// Record the requested swap interval; software rendering never waits.
pub fn eglSwapInterval(dpy: *mut c_void, interval: i32) -> u32 {
    if dpy != std::ptr::from_ref(&DISPLAY).cast_mut().cast() {
        return fail(EGL_BAD_DISPLAY);
    }
    SWAP_INTERVAL.with(|cell| cell.set(interval));
    EGL_TRUE
}

/// Select the client API; only OpenGL ES is implemented.
pub fn eglBindAPI(api: u32) -> u32 {
    if api as i32 != EGL_OPENGL_ES_API {
        return fail(EGL_BAD_PARAMETER);
    }
    API.with(|cell| cell.set(api as i32));
    EGL_TRUE
}

/// Nothing is queued, so this always succeeds.
pub fn eglWaitGL() -> u32 {
    EGL_TRUE
}

/// Drop this thread's bindings.
pub fn eglReleaseThread() -> u32 {
    CURRENT_SURFACE.with(|cell| cell.set(std::ptr::null_mut()));
    CURRENT_CONTEXT.with(|cell| cell.set(std::ptr::null_mut()));
    ERROR.with(|cell| cell.set(EGL_SUCCESS));
    EGL_TRUE
}

/// Resolve one entry point by name, exactly as a loader would.
pub fn eglGetProcAddress(procname: *const c_char) -> *mut c_void {
    if procname.is_null() {
        return std::ptr::null_mut();
    }
    let Ok(name) = unsafe { CStr::from_ptr(procname) }.to_str() else {
        return std::ptr::null_mut();
    };
    crate::gen::procs::proc_address(name)
}

/// How many frames `eglSwapBuffers` has published on a surface, for tests and hosts.
pub fn swap_count(surface: *mut c_void) -> Option<u64> {
    if !is_live(&SURFACES, surface) {
        return None;
    }
    Some(unsafe { (*surface.cast::<Surface>()).swaps })
}
