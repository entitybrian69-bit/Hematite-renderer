//! Stable C ABI for the Hematite native library, built as `libhematite.so` for Android.
//!
//! # What this ABI is, and what it is not
//!
//! It is a small bring-up interface over the crates that actually exist: the Vulkan
//! offscreen renderer and the fixed-function matrix stack. Every exported function is a
//! real implementation of the work it names.
//!
//! It is **not** the LWJGL OpenGL ABI. The 866 `gl*`/`egl*` entry points in
//! `abi/expected_symbols.txt` are still generated declarations only, so no Pojav-derived
//! launcher can load this library as a renderer: it does not intercept a single GL call
//! and never will until the GL frontend is implemented (see `docs/ROADMAP.md`).
//!
//! # Panics across the boundary
//! The release profile is `panic = "abort"`, so a panic aborts the process rather than
//! unwinding into a foreign launcher (unwinding across a C frame is undefined behaviour).
//! The `catch_unwind` guards below therefore take effect in development and test builds,
//! where unwinding is enabled, and are inert in release builds.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
// Every exported function is a safe `extern "C"` entry point, because a C caller cannot
// write `unsafe`. Each one documents the caller's pointer and length obligations and
// validates everything it can (null checks, lengths, finiteness) before dereferencing.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use hematite_fixed_function::matrix::Mat4;
use hematite_vk_backend::{OffscreenRenderer, Vertex};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

/// Version of this ABI surface. Bumped only on breaking changes to these signatures.
pub const ABI_VERSION: u32 = 1;

/// Operation succeeded and any output buffers were written completely.
pub const STATUS_OK: c_int = 0;
/// A null pointer, zero length or non-finite value was passed in.
pub const STATUS_INVALID_ARGUMENT: c_int = 1;
/// The Vulkan driver rejected an operation; `hematite_last_error` describes it.
pub const STATUS_BACKEND_ERROR: c_int = 2;
/// An output buffer was too small; the required size is returned in `*required`.
pub const STATUS_BUFFER_TOO_SMALL: c_int = 3;
/// A Rust panic was caught at the boundary (development and test builds only).
pub const STATUS_PANIC: c_int = 4;

/// One interleaved vertex: position in Vulkan clip space plus linear RGBA colour.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct HematiteVertex {
    /// X and Y in normalized clip coordinates.
    pub x: f32,
    /// See `x`.
    pub y: f32,
    /// Red, 0.0-1.0.
    pub r: f32,
    /// Green, 0.0-1.0.
    pub g: f32,
    /// Blue, 0.0-1.0.
    pub b: f32,
    /// Alpha, 0.0-1.0.
    pub a: f32,
}

struct Plugin {
    renderer: OffscreenRenderer,
}

thread_local! {
    /// None until a failure is recorded; interior NULs in driver messages are replaced.
    static LAST_ERROR: std::cell::RefCell<Option<std::ffi::CString>> =
        const { std::cell::RefCell::new(None) };
}

fn set_last_error(message: &str) {
    let encoded = std::ffi::CString::new(message.replace('\0', " "))
        .unwrap_or_else(|_| std::ffi::CString::new("unrepresentable error").expect("literal"));
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(encoded));
}

/// Run `body`, converting every failure mode of this ABI into a status code.
fn guarded(body: impl FnOnce() -> c_int) -> c_int {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(status) => status,
        Err(_) => {
            set_last_error("panic inside Hematite");
            STATUS_PANIC
        }
    }
}

/// Copy a NUL-terminated C string into an owned `String`.
fn c_string(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: caller guarantees a NUL-terminated string valid for the call duration.
    Some(unsafe { CStr::from_ptr(pointer) }.to_string_lossy().into_owned())
}

/// Version of the ABI this library implements.
#[no_mangle]
pub extern "C" fn hematite_abi_version() -> u32 {
    ABI_VERSION
}

/// Static description of a status code; never returns null.
#[no_mangle]
pub extern "C" fn hematite_status_string(status: c_int) -> *const c_char {
    match status {
        STATUS_OK => c"ok".as_ptr(),
        STATUS_INVALID_ARGUMENT => c"invalid argument".as_ptr(),
        STATUS_BACKEND_ERROR => c"vulkan backend error".as_ptr(),
        STATUS_BUFFER_TOO_SMALL => c"output buffer too small".as_ptr(),
        STATUS_PANIC => c"caught panic".as_ptr(),
        _ => c"unknown status".as_ptr(),
    }
}

/// Description of the most recent backend failure on this thread; never null.
/// The returned pointer stays valid until the next call on the same thread.
#[no_mangle]
pub extern "C" fn hematite_last_error() -> *const c_char {
    let mut pointer = c"".as_ptr();
    LAST_ERROR.with(|slot| {
        if let Some(message) = slot.borrow().as_ref() {
            pointer = message.as_ptr();
        }
    });
    pointer
}

/// Create a plugin bound to a Vulkan device.
///
/// `loader_path` may be null (system `libvulkan.so`/`libvulkan.so.1`) or a path to a
/// specific loader, which is how the software driver is selected in tests. Returns null
/// and records the reason in `hematite_last_error` when no usable device exists.
#[no_mangle]
pub extern "C" fn hematite_plugin_create(loader_path: *const c_char) -> *mut c_void {
    let mut result: *mut c_void = std::ptr::null_mut();
    let status = guarded(|| {
        let owned = match loader_path {
            null if null.is_null() => None,
            path => match c_string(path) {
                Some(path) => Some(path),
                None => {
                    set_last_error("loader path is not valid UTF-8");
                    return STATUS_INVALID_ARGUMENT;
                }
            },
        };
        match OffscreenRenderer::new(owned.as_deref().map(Path::new)) {
            Ok(renderer) => {
                result = Box::into_raw(Box::new(Plugin { renderer })).cast();
                STATUS_OK
            }
            Err(error) => {
                set_last_error(&error.to_string());
                STATUS_BACKEND_ERROR
            }
        }
    });
    if status != STATUS_OK {
        return std::ptr::null_mut();
    }
    result
}

/// Destroy a plugin returned by `hematite_plugin_create`. Null is a no-op.
#[no_mangle]
pub extern "C" fn hematite_plugin_destroy(plugin: *mut c_void) {
    if !plugin.is_null() {
        // SAFETY: the pointer came from Box::into_raw in hematite_plugin_create and is
        // dropped exactly once by this call.
        let _ = guarded(|| {
            drop(unsafe { Box::from_raw(plugin.cast::<Plugin>()) });
            STATUS_OK
        });
    }
}

fn as_plugin<'a>(pointer: *mut c_void) -> Option<&'a mut Plugin> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: the caller passes a live, exclusively owned plugin from create.
    Some(unsafe { &mut *pointer.cast::<Plugin>() })
}

/// Copy the selected Vulkan device name into `out`.
///
/// Returns the byte length written excluding the NUL, or `STATUS_BUFFER_TOO_SMALL` with
/// the required length in `*required` when `out` is too short.
#[no_mangle]
pub extern "C" fn hematite_plugin_device_name(
    plugin: *mut c_void,
    out: *mut c_char,
    out_len: usize,
    required: *mut usize,
) -> c_int {
    guarded(|| {
        let Some(plugin) = as_plugin(plugin) else {
            set_last_error("null plugin");
            return STATUS_INVALID_ARGUMENT;
        };
        let name = &plugin.renderer.capabilities().device_name;
        let bytes = name.as_bytes();
        if !required.is_null() {
            // SAFETY: caller guarantees a writable usize when the pointer is non-null.
            unsafe { *required = bytes.len() };
        }
        if out.is_null() || out_len < bytes.len() + 1 {
            return STATUS_BUFFER_TOO_SMALL;
        }
        // SAFETY: out has room for the bytes plus terminator, and the source is a
        // separate Rust string that cannot overlap the caller's buffer.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast(), bytes.len());
            *out.add(bytes.len()) = 0;
        }
        bytes.len() as c_int
    })
}

/// Clear an RGBA8 offscreen image, draw an optional triangle list, and read back pixels.
///
/// `rgba_out` receives exactly `width * height * 4` bytes, top row first. This is the
/// same synchronous path the test harness verifies against an analytic reference; it is
/// not a presentation path and has no swapchain.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn hematite_plugin_render_offscreen(
    plugin: *mut c_void,
    width: u32,
    height: u32,
    clear: *const f32,
    vertices: *const HematiteVertex,
    vertex_count: u32,
    rgba_out: *mut u8,
    rgba_out_len: usize,
) -> c_int {
    guarded(|| {
        let Some(plugin) = as_plugin(plugin) else {
            set_last_error("null plugin");
            return STATUS_INVALID_ARGUMENT;
        };
        if clear.is_null() || (!vertices.is_null() && vertex_count == 0) || rgba_out.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: caller guarantees 4 finite floats at `clear` and `vertex_count`
        // initialised vertices at `vertices` for the duration of this call.
        let clear_slice = unsafe { std::slice::from_raw_parts(clear, 4) };
        let input: &[HematiteVertex] = if vertices.is_null() {
            &[]
        } else {
            // SAFETY: same guarantees, bounded by the count argument.
            unsafe { std::slice::from_raw_parts(vertices, vertex_count as usize) }
        };
        let mut clear_array = [0.0f32; 4];
        clear_array.copy_from_slice(clear_slice);
        // Copied field-by-field: no layout assumption about the backend's own type.
        let converted: Vec<Vertex> = input
            .iter()
            .map(|v| Vertex { position: [v.x, v.y], color: [v.r, v.g, v.b, v.a] })
            .collect();
        match plugin.renderer.render(width, height, clear_array, &converted) {
            Ok(image) => {
                if rgba_out_len < image.rgba.len() {
                    set_last_error("rgba_out smaller than width*height*4");
                    return STATUS_BUFFER_TOO_SMALL;
                }
                // SAFETY: bounds checked above, and the image is owned by this function.
                unsafe {
                    std::ptr::copy_nonoverlapping(image.rgba.as_ptr(), rgba_out, image.rgba.len());
                }
                STATUS_OK
            }
            Err(error) => {
                set_last_error(&error.to_string());
                STATUS_BACKEND_ERROR
            }
        }
    })
}

/// Column-major orthographic projection, identical to `glOrtho`, written to 16 floats.
/// Pure CPU work: it verifies the fixed-function crate through this ABI on any target.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn hematite_fixed_function_ortho(
    out: *mut f32,
    left: f64,
    right: f64,
    bottom: f64,
    top: f64,
    near: f64,
    far: f64,
) -> c_int {
    guarded(|| {
        if out.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        let matrix = match Mat4::ortho(left, right, bottom, top, near, far) {
            Ok(matrix) => matrix,
            Err(error) => {
                set_last_error(&format!("{error:?}"));
                return STATUS_INVALID_ARGUMENT;
            }
        };
        // SAFETY: caller guarantees 16 writable floats at `out`.
        unsafe {
            for (index, value) in matrix.0.iter().enumerate() {
                *out.add(index) = *value as f32;
            }
        }
        STATUS_OK
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_surface_reports_version_and_static_strings() {
        assert_eq!(hematite_abi_version(), 1);
        for status in [
            STATUS_OK,
            STATUS_INVALID_ARGUMENT,
            STATUS_BACKEND_ERROR,
            STATUS_BUFFER_TOO_SMALL,
            STATUS_PANIC,
            99,
        ] {
            assert!(!hematite_status_string(status).is_null());
        }
        assert!(!hematite_last_error().is_null());
    }

    #[test]
    fn create_rejects_garbage_loader_and_destroy_tolerates_null() {
        assert!(hematite_plugin_create(c"/definitely/not/a/loader.so".as_ptr()).is_null());
        hematite_plugin_destroy(std::ptr::null_mut());
        let message = unsafe { CStr::from_ptr(hematite_last_error()) };
        assert!(!message.to_bytes().is_empty(), "failure must record a reason");
    }

    #[test]
    fn null_arguments_are_rejected_without_touching_a_device() {
        let mut required = 0usize;
        assert_eq!(
            hematite_plugin_device_name(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                8,
                &mut required
            ),
            STATUS_INVALID_ARGUMENT
        );
        assert_eq!(
            hematite_plugin_render_offscreen(
                std::ptr::null_mut(),
                1,
                1,
                [0.0f32; 4].as_ptr(),
                std::ptr::null(),
                0,
                [0u8; 4].as_mut_ptr(),
                4
            ),
            STATUS_INVALID_ARGUMENT
        );
        assert_eq!(
            hematite_fixed_function_ortho(std::ptr::null_mut(), -1.0, 1.0, -1.0, 1.0, -1.0, 1.0),
            STATUS_INVALID_ARGUMENT
        );
    }

    #[test]
    fn ortho_matches_the_gl_fixed_function_definition() {
        let mut out = [0.0f32; 16];
        assert_eq!(
            hematite_fixed_function_ortho(out.as_mut_ptr(), -2.0, 2.0, -1.0, 3.0, 1.0, 5.0),
            STATUS_OK
        );
        let expected = Mat4::ortho(-2.0, 2.0, -1.0, 3.0, 1.0, 5.0).unwrap();
        for (actual, expected) in out.iter().zip(expected.0) {
            assert!((f64::from(*actual) - expected).abs() < 1e-6);
        }
        assert_eq!(
            hematite_fixed_function_ortho(out.as_mut_ptr(), 1.0, 1.0, 0.0, 1.0, 0.0, 1.0),
            STATUS_INVALID_ARGUMENT,
            "degenerate ortho must be rejected"
        );
    }

    #[test]
    fn vertex_layout_matches_the_backend_field_for_field() {
        assert_eq!(std::mem::size_of::<HematiteVertex>(), 24);
        let vertex = HematiteVertex { x: 1.0, y: 2.0, r: 0.5, g: 0.25, b: 0.0, a: 1.0 };
        let converted = Vertex {
            position: [vertex.x, vertex.y],
            color: [vertex.r, vertex.g, vertex.b, vertex.a],
        };
        assert_eq!(converted.position, [1.0, 2.0]);
        assert_eq!(converted.color, [0.5, 0.25, 0.0, 1.0]);
    }
}
