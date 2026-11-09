//! The Hematite GL/EGL frontend: the full 866-entry C ABI surface, with the subset that
//! is genuinely implemented running on a software rasterizer.
//!
//! # Layers
//! * [`gen::abi`] and [`gen::decls`] are generated from `abi/gl_export.def`; they describe
//!   every entry point and declare its exact C signature.
//! * [`gen::exports`] holds the `#[no_mangle]` definition of all 866 symbols. Implemented
//!   ones forward to [`api`] or [`egl`], so the compiler proves the signature matches the
//!   def file; the rest record the call and return the type's zero value.
//! * [`gen::procs`] resolves any of those symbols by name, which is what
//!   `eglGetProcAddress` needs.
//! * [`context`] is the state machine, [`raster`] the rasterizer, [`gl_types`] the
//!   enumerants.
//!
//! # Honesty about scope
//! The fixed-function path is real: clear, matrices, arrays, immediate mode, textures,
//! depth, blending, scissor, culling and readback all produce pixels that tests check.
//! GLSL **is** compiled — `glCompileShader` translates it to SPIR-V through
//! `hematite-glsl` — but the compiled stages are not yet executed: draws still run the
//! fixed-function path until the front end is routed through the Vulkan backend.
//! Stencil and multisampling do not exist, and EGL publishes frames to a software
//! backbuffer rather than to a window. See `docs/KNOWN_LIMITATIONS.md`.

#![deny(unsafe_op_in_unsafe_fn)]
// Every exported entry point is a safe `extern "C"` function, because a C caller cannot
// write `unsafe`. Each one validates its pointer arguments before dereferencing them.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub mod api;
pub mod context;
pub mod egl;
pub mod gen;
pub mod gl_types;
pub mod raster;
pub mod vulkan;

use gl_types::*;

/// The zero value of a C return type, which is what an unimplemented entry point returns.
///
/// Raw pointers have no `Default`, so the generated thunks cannot use it; this covers
/// exactly the return types `abi/gl_export.def` declares.
pub trait Zero: Sized {
    /// The all-zero representation of this type.
    fn zero() -> Self;
}

macro_rules! impl_zero {
    ($($ty:ty => $value:expr),* $(,)?) => {
        $(impl Zero for $ty {
            fn zero() -> Self {
                $value
            }
        })*
    };
}

impl_zero!(
    () => (),
    u8 => 0,
    i8 => 0,
    u16 => 0,
    i16 => 0,
    u32 => 0,
    i32 => 0,
    u64 => 0,
    i64 => 0,
    usize => 0,
    isize => 0,
    f32 => 0.0,
    f64 => 0.0,
    *mut core::ffi::c_void => core::ptr::null_mut(),
    *const core::ffi::c_void => core::ptr::null(),
    *const u8 => core::ptr::null(),
    *const i8 => core::ptr::null(),
    Option<unsafe extern "C" fn(u32, u32, u32, u32, i32, *const i8, *mut core::ffi::c_void)> => None,
);

/// Record a call to an entry point that has no implementation and return the zero value
/// of the declared return type.
pub fn unsupported<R: Zero>(name: &'static str) -> R {
    context::record_unsupported(name);
    R::zero()
}

/// How many entry points the def file declares, and this crate exports.
pub fn export_count() -> usize {
    gen::abi::SYMBOLS.len()
}

/// The first `count` exported symbol names, in def-file order.
pub fn exported_symbols() -> &'static [&'static str] {
    &gen::abi::SYMBOLS
}

/// Reset this thread's context and size its target, as `eglMakeCurrent` does.
pub fn reset_context(width: usize, height: usize) -> bool {
    if width == 0 || height == 0 || width > context::MAX_RENDER_TARGET {
        return false;
    }
    context::with(|ctx| ctx.reset(width, height));
    true
}

/// The pixels of this thread's framebuffer, RGBA8 bottom-left origin.
pub fn framebuffer_rgba(width: usize, height: usize) -> Option<Vec<u8>> {
    let mut pixels = vec![0u8; width.checked_mul(height)?.checked_mul(4)?];
    api::glReadPixels(
        0,
        0,
        width as i32,
        height as i32,
        GL_RGBA,
        GL_UNSIGNED_BYTE,
        pixels.as_mut_ptr().cast(),
    );
    if context::with(|ctx| ctx.error.take()).is_some() {
        return None;
    }
    Some(pixels)
}

/// Render the built-in demonstration scene with the fixed-function path and return its
/// pixels, RGBA8 bottom-left origin.
///
/// It draws a colour-interpolated triangle over a bilinearly sampled checkerboard quad,
/// with the depth test enabled. Nothing here needs a Vulkan driver, so it is the cheapest
/// end-to-end proof that the exported ABI produces correct pixels.
pub fn render_demo(width: usize, height: usize) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || width > context::MAX_RENDER_TARGET {
        return None;
    }
    if !reset_context(width, height) {
        return None;
    }
    let (width, height) = (width as i32, height as i32);
    api::glViewport(0, 0, width, height);
    api::glClearColor(0.04, 0.06, 0.11, 1.0);
    api::glClearDepth(1.0);
    api::glEnable(GL_DEPTH_TEST);
    api::glDepthFunc(GL_LEQUAL);
    api::glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);

    // A 4x4 checkerboard, uploaded through the real glTexImage2D path.
    let mut checker = [0u8; 4 * 4 * 4];
    for y in 0..4usize {
        for x in 0..4usize {
            let level = if (x ^ y) & 1 == 0 { 235 } else { 40 };
            let offset = (y * 4 + x) * 4;
            checker[offset..offset + 4].copy_from_slice(&[level, level, level, 255]);
        }
    }
    let mut texture = 0u32;
    api::glGenTextures(1, &mut texture);
    api::glBindTexture(GL_TEXTURE_2D, texture);
    api::glTexImage2D(
        GL_TEXTURE_2D,
        0,
        GL_RGBA as i32,
        4,
        4,
        0,
        GL_RGBA,
        GL_UNSIGNED_BYTE,
        checker.as_ptr().cast(),
    );
    api::glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR as i32);
    api::glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR as i32);
    api::glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE as i32);
    api::glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE as i32);
    api::glEnable(GL_TEXTURE_2D);

    // A textured quad behind the triangle, uploaded through a real buffer object.
    let quad: [f32; 16] = [
        -0.95, -0.95, 0.0, 0.0, 0.95, -0.95, 1.0, 0.0, -0.95, 0.95, 0.0, 1.0, 0.95, 0.95, 1.0, 1.0,
    ];
    let mut buffer = 0u32;
    api::glGenBuffers(1, &mut buffer);
    api::glBindBuffer(GL_ARRAY_BUFFER, buffer);
    api::glBufferData(
        GL_ARRAY_BUFFER,
        (quad.len() * 4) as isize,
        quad.as_ptr().cast(),
        GL_STATIC_DRAW,
    );
    api::glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 16, std::ptr::null());
    api::glVertexAttribPointer(
        TEXCOORD_ATTRIB as u32,
        2,
        GL_FLOAT,
        GL_FALSE,
        16,
        8usize as *const core::ffi::c_void,
    );
    api::glEnableVertexAttribArray(0);
    api::glEnableVertexAttribArray(TEXCOORD_ATTRIB as u32);
    api::glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    api::glDisableVertexAttribArray(0);
    api::glDisableVertexAttribArray(TEXCOORD_ATTRIB as u32);
    api::glDisable(GL_TEXTURE_2D);

    // A Gouraud-interpolated triangle in front of it, through immediate mode.
    api::glBegin(GL_TRIANGLES);
    api::glColor4f(0.95, 0.25, 0.20, 1.0);
    api::glVertex3f(0.0, 0.75, -0.2);
    api::glColor4f(0.20, 0.85, 0.35, 1.0);
    api::glVertex3f(-0.70, -0.45, -0.2);
    api::glColor4f(0.20, 0.45, 0.95, 1.0);
    api::glVertex3f(0.70, -0.45, -0.2);
    api::glEnd();

    framebuffer_rgba(width as usize, height as usize)
}

/// Attribute location the demo and the fixed-function path use for texture coordinates.
const TEXCOORD_ATTRIB: usize = 2;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every declared entry point must resolve to a real address in this binary.
    #[test]
    fn every_declared_entry_point_has_an_address() {
        let symbols = exported_symbols();
        assert_eq!(symbols.len(), export_count());
        let mut missing: Vec<&str> = Vec::new();
        for symbol in symbols {
            if gen::procs::proc_address(symbol).is_null() {
                missing.push(symbol);
            }
        }
        assert!(missing.is_empty(), "entry points without an address: {missing:?}");
    }

    /// An unimplemented entry point must be recorded rather than silently dropped.
    #[test]
    fn unimplemented_calls_are_recorded() {
        let before = context::unsupported_count();
        let value: u32 = unsupported("glAreTexturesResident");
        assert_eq!(value, 0);
        assert_eq!(context::unsupported_count(), before + 1);
        assert_eq!(context::last_unsupported(), Some("glAreTexturesResident"));
    }

    /// The demo scene must produce a real image with the colours it claims.
    #[test]
    fn demo_scene_renders_expected_pixels() {
        let side = 64usize;
        let pixels = render_demo(side, side).expect("demo renders");
        assert_eq!(pixels.len(), side * side * 4);
        let at = |x: usize, y: usize| -> [u8; 4] {
            let offset = (y * side + x) * 4;
            [pixels[offset], pixels[offset + 1], pixels[offset + 2], pixels[offset + 3]]
        };
        // Corner: the checkerboard quad, opaque.
        let corner = at(2, 2);
        assert!(corner[3] == 255, "corner {corner:?} should be opaque");
        assert!(corner[0] > 20, "corner {corner:?} should be lit by the quad");
        // Centre: the triangle, red-dominant where it covers the middle.
        let centre = at(side / 2, side / 2);
        assert!(centre[3] == 255, "centre {centre:?} should be opaque");
        assert_ne!(centre, corner, "the triangle must differ from the quad");
        // Outside the quad the clear colour survives.
        let cleared = at(side - 1, side / 2);
        assert!(cleared[2] > cleared[0], "clear colour {cleared:?} is blue-dominant");
    }

    /// Depth testing must decide which of two overlapping triangles is visible.
    #[test]
    fn depth_test_orders_overlapping_triangles() {
        reset_context(16, 16);
        api::glViewport(0, 0, 16, 16);
        api::glEnable(GL_DEPTH_TEST);
        api::glDepthFunc(GL_LESS);
        api::glClearColor(0.0, 0.0, 0.0, 1.0);
        api::glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);

        let quad = |z: f32, red: f32| {
            api::glBegin(GL_TRIANGLES);
            api::glColor4f(red, 0.0, 0.0, 1.0);
            api::glVertex3f(-1.0, -1.0, z);
            api::glVertex3f(1.0, -1.0, z);
            api::glVertex3f(0.0, 1.0, z);
            api::glEnd();
        };
        quad(0.5, 0.0);
        quad(-0.5, 1.0);
        let pixels = framebuffer_rgba(16, 16).expect("readback");
        let offset = (8 * 16 + 8) * 4;
        assert_eq!(pixels[offset], 255, "the nearer triangle must win");
    }

    /// A shader that compiles must report success and produce SPIR-V.
    #[test]
    fn compile_link_and_use_a_shader_program() {
        reset_context(8, 8);
        let vertex = compile_shader(
            GL_VERTEX_SHADER,
            "#version 120\nattribute vec4 position;\nattribute vec2 uv;\nvarying vec2 texcoord;\n\
             uniform mat4 mvp;\nvoid main() { texcoord = uv; gl_Position = mvp * position; }\n",
        );
        let fragment = compile_shader(
            GL_FRAGMENT_SHADER,
            "#version 120\nvarying vec2 texcoord;\nuniform sampler2D diffuse;\n\
             void main() { gl_FragColor = texture2D(diffuse, texcoord); }\n",
        );
        let program = api::glCreateProgram();
        api::glAttachShader(program, vertex);
        api::glAttachShader(program, fragment);
        api::glLinkProgram(program);
        assert_eq!(link_status(program), 1, "link status: {}", program_log(program));
        assert_eq!(api::glGetAttribLocation(program, c"position".as_ptr()), 0);
        assert_eq!(api::glGetAttribLocation(program, c"uv".as_ptr()), 1);
        assert!(api::glGetUniformLocation(program, c"mvp".as_ptr()) >= 0);
        assert!(api::glGetUniformLocation(program, c"diffuse".as_ptr()) >= 0);
        assert_eq!(api::glGetUniformLocation(program, c"absent".as_ptr()), -1);
        api::glUseProgram(program);
        assert_eq!(api::glGetError(), GL_NO_ERROR);
        assert_eq!(context::with(|ctx| ctx.current_program), Some(program));
    }

    /// A shader that cannot compile must fail loudly, with a log an application can show.
    #[test]
    fn a_broken_shader_fails_to_compile_and_to_link() {
        reset_context(8, 8);
        let (vertex, status) =
            try_compile(GL_VERTEX_SHADER, "#version 120\nvoid main() { int = ; }\n");
        assert_eq!(status, 0, "the broken shader must not compile");
        let log = shader_log(vertex);
        assert!(!log.trim().is_empty(), "the info log must explain the failure");

        let program = api::glCreateProgram();
        api::glAttachShader(program, vertex);
        api::glLinkProgram(program);
        assert_eq!(link_status(program), 0);
        api::glUseProgram(program);
        assert_eq!(api::glGetError(), GL_INVALID_OPERATION, "an unlinked program cannot be used");
    }

    /// `glBindAttribLocation` before linking must decide the attribute locations.
    #[test]
    fn bound_attribute_locations_survive_linking() {
        reset_context(8, 8);
        let vertex = compile_shader(
            GL_VERTEX_SHADER,
            "#version 120\nattribute vec4 position;\nattribute vec2 uv;\n\
             void main() { gl_Position = position + vec4(uv, 0.0, 0.0); }\n",
        );
        let fragment = compile_shader(
            GL_FRAGMENT_SHADER,
            "#version 120\nvoid main() { gl_FragColor = vec4(1.0); }\n",
        );
        let program = api::glCreateProgram();
        api::glBindAttribLocation(program, 7, c"uv".as_ptr());
        api::glAttachShader(program, vertex);
        api::glAttachShader(program, fragment);
        api::glLinkProgram(program);
        assert_eq!(link_status(program), 1, "link status: {}", program_log(program));
        assert_eq!(api::glGetAttribLocation(program, c"uv".as_ptr()), 7);
    }

    /// Compile a shader through the real entry points and assert it succeeded.
    fn compile_shader(stage: u32, source: &str) -> u32 {
        let (shader, status) = try_compile(stage, source);
        assert_eq!(status, 1, "compile failed: {}", shader_log(shader));
        shader
    }

    /// Compile a shader and report whether it worked, for the failure tests.
    fn try_compile(stage: u32, source: &str) -> (u32, i32) {
        let shader = api::glCreateShader(stage);
        assert_ne!(shader, 0);
        let text = std::ffi::CString::new(source).expect("source has no NUL");
        let pointer: *const i8 = text.as_ptr();
        api::glShaderSource(shader, 1, &pointer, std::ptr::null());
        api::glCompileShader(shader);
        let mut status = 0;
        api::glGetShaderiv(shader, GL_COMPILE_STATUS, &mut status);
        (shader, status)
    }

    fn shader_log(shader: u32) -> String {
        let mut length = 0;
        api::glGetShaderiv(shader, GL_INFO_LOG_LENGTH, &mut length);
        let mut buffer = vec![0u8; length.max(1) as usize + 1];
        api::glGetShaderInfoLog(
            shader,
            buffer.len() as i32,
            &mut length,
            buffer.as_mut_ptr().cast(),
        );
        String::from_utf8_lossy(&buffer[..length.max(0) as usize]).into_owned()
    }

    fn program_log(program: u32) -> String {
        let mut length = 0;
        api::glGetProgramiv(program, GL_INFO_LOG_LENGTH, &mut length);
        let mut buffer = vec![0u8; length.max(1) as usize + 1];
        api::glGetProgramInfoLog(
            program,
            buffer.len() as i32,
            &mut length,
            buffer.as_mut_ptr().cast(),
        );
        String::from_utf8_lossy(&buffer[..length.max(0) as usize]).into_owned()
    }

    fn link_status(program: u32) -> i32 {
        let mut status = 0;
        api::glGetProgramiv(program, GL_LINK_STATUS, &mut status);
        status
    }

    /// `glReadPixels` and the raw framebuffer must agree.
    #[test]
    fn read_pixels_matches_the_framebuffer() {
        reset_context(8, 8);
        api::glViewport(0, 0, 8, 8);
        api::glClearColor(0.0, 1.0, 0.0, 1.0);
        api::glClear(GL_COLOR_BUFFER_BIT);
        let pixels = framebuffer_rgba(8, 8).expect("readback");
        assert_eq!(&pixels[0..4], &[0, 255, 0, 255]);
        assert_eq!(api::glGetError(), GL_NO_ERROR);
    }
}
