//! The current GL context: state machine, object tables and the framebuffer the
//! rasterizer writes into.
//!
//! One context per thread, matching GL's "current context" model. Nothing here allocates
//! per draw call except the clip/polygon scratch buffers, which are reused.

use crate::gl_types::*;
use hematite_fixed_function::matrix::{Mat4, MatrixStack};
use std::cell::RefCell;
use std::collections::HashMap;

/// Attribute slots addressed by `glVertexAttribPointer`.
pub const MAX_ATTRIBS: usize = 8;
/// Texture image units.
pub const MAX_TEXTURE_UNITS: usize = 8;
/// Modelview stack depth; GL guarantees at least 32.
const MODELVIEW_DEPTH: usize = 32;
/// Projection and texture stack depth; GL guarantees at least 2.
const OTHER_DEPTH: usize = 2;
/// Largest framebuffer edge the software backend will allocate.
pub const MAX_RENDER_TARGET: usize = 8192;
/// Largest texture edge accepted by `glTexImage2D`.
pub const MAX_TEXTURE_SIZE: usize = 4096;

/// One transformed-independent vertex: object space position, colour and texture coords.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vertex {
    /// Object space position, `w` defaults to 1.
    pub pos: [f32; 4],
    /// Linear RGBA, premultiplied nowhere.
    pub color: [f32; 4],
    /// Texture coordinates `(s, t, r, q)`.
    pub tex: [f32; 4],
}

impl Default for Vertex {
    fn default() -> Self {
        Self { pos: [0.0, 0.0, 0.0, 1.0], color: [1.0; 4], tex: [0.0, 0.0, 0.0, 1.0] }
    }
}

/// An RGBA8 colour buffer plus a 32-bit float depth buffer.
#[derive(Debug, Clone)]
pub struct Framebuffer {
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// Interleaved RGBA8, bottom-left origin like `glReadPixels`.
    pub color: Vec<u8>,
    /// Depth per pixel, cleared to 1.0.
    pub depth: Vec<f32>,
}

impl Framebuffer {
    /// Allocate a cleared target, or `None` for an empty or absurdly large size.
    pub fn new(width: usize, height: usize) -> Option<Self> {
        if width == 0 || height == 0 || width > MAX_RENDER_TARGET || height > MAX_RENDER_TARGET {
            return None;
        }
        let pixels = width.checked_mul(height)?;
        let mut color = vec![0u8; pixels.checked_mul(4)?];
        for (index, byte) in color.iter_mut().enumerate() {
            if index % 4 == 3 {
                *byte = 255;
            }
        }
        Some(Self { width, height, color, depth: vec![1.0; pixels] })
    }

    /// Resize, discarding contents. False leaves the target untouched.
    pub fn resize(&mut self, width: usize, height: usize) -> bool {
        match Self::new(width, height) {
            Some(target) => {
                *self = target;
                true
            }
            None => false,
        }
    }

    /// The pixel at `(x, y)`, or transparent black outside the target.
    pub fn pixel(&self, x: usize, y: usize) -> [u8; 4] {
        if x >= self.width || y >= self.height {
            return [0, 0, 0, 0];
        }
        let offset = (y * self.width + x) * 4;
        [self.color[offset], self.color[offset + 1], self.color[offset + 2], self.color[offset + 3]]
    }
}

/// A buffer object: an owned byte range plus the hint it was created with.
#[derive(Debug, Clone)]
pub struct Buffer {
    /// Contents.
    pub data: Vec<u8>,
    /// Usage hint, for diagnostics only.
    pub usage: u32,
}

/// A 2D texture image with its sampling state.
#[derive(Debug, Clone)]
pub struct Texture {
    /// Width in texels.
    pub width: usize,
    /// Height in texels.
    pub height: usize,
    /// Source format the pixels were uploaded as.
    pub format: u32,
    /// Stored as RGBA8 regardless of the upload format.
    pub data: Vec<u8>,
    /// Minification filter.
    pub min_filter: u32,
    /// Magnification filter.
    pub mag_filter: u32,
    /// Horizontal wrap.
    pub wrap_s: u32,
    /// Vertical wrap.
    pub wrap_t: u32,
}

/// A shader object. GLSL is **not** compiled; see `crate::api::compile_shader`.
#[derive(Debug, Clone)]
pub struct Shader {
    /// `GL_VERTEX_SHADER` or `GL_FRAGMENT_SHADER`.
    pub stage: u32,
    /// Source as handed to `glShaderSource`.
    pub source: String,
    /// SPIR-V and interface from the last `glCompileShader`, if it succeeded.
    pub compiled: Option<hematite_glsl::Shader>,
    /// What `glGetShaderInfoLog` returns; empty until the shader is compiled.
    pub info_log: String,
}

/// A program object: the shaders attached to it and its uniform values.
#[derive(Debug, Clone)]
pub struct Program {
    /// Attached shader names.
    pub shaders: Vec<u32>,
    /// Uniform values set through `glUniform*`.
    pub uniforms: HashMap<String, Uniform>,
    /// Locations requested with `glBindAttribLocation` before the last link.
    pub attribute_bindings: Vec<(String, u32)>,
    /// The compiled stages and merged interface of the last successful link.
    pub linked: Option<LinkedProgram>,
    /// What `glGetProgramInfoLog` returns.
    pub info_log: String,
}

/// A linked program: compiled stages plus the interface they share.
#[derive(Debug, Clone)]
pub struct LinkedProgram {
    /// The compiled stages, in the order they were attached.
    pub stages: Vec<hematite_glsl::Shader>,
    /// Uniform fields, in interface order; the index is the `GLint` location.
    pub uniform_names: Vec<String>,
    /// Vertex attribute locations by name.
    pub attributes: Vec<(String, u32)>,
    /// Sampler bindings by uniform name.
    pub samplers: Vec<(String, u32, u32)>,
}

impl Program {
    /// True when the last `glLinkProgram` on this program succeeded.
    pub fn link_status(&self) -> bool {
        self.linked.is_some()
    }

    /// The uniform location for a name, or -1 as GL requires for an unknown name.
    pub fn uniform_location(&self, name: &str) -> i32 {
        match self.linked {
            Some(ref linked) => match linked.uniform_names.iter().position(|item| item == name) {
                Some(index) => index as i32,
                None => -1,
            },
            None => -1,
        }
    }
}

/// A uniform value the fixed-function path can honour.
#[derive(Debug, Clone, Copy)]
pub enum Uniform {
    /// A single integer, as bound samplers use.
    Int(i32),
    /// A four component float vector.
    Vec4([f32; 4]),
    /// A column-major 4×4 matrix.
    Mat4([f32; 16]),
}

/// Description of one enabled attribute array, GLES1 client arrays included.
#[derive(Debug, Clone, Copy)]
pub struct ArrayState {
    /// Whether the array contributes to a draw call.
    pub enabled: bool,
    /// Buffer object the pointer refers into, or `None` for a client pointer.
    pub buffer: Option<u32>,
    /// First byte of the array, as a machine address when `buffer` is `None`.
    pub pointer: usize,
    /// Components per element, 1-4.
    pub size: i32,
    /// Byte stride between elements; 0 means tightly packed.
    pub stride: i32,
    /// Component type.
    pub ty: u32,
    /// Whether integer components map to `[0, 1]` / `[-1, 1]`.
    pub normalized: bool,
}

impl Default for ArrayState {
    fn default() -> Self {
        Self {
            enabled: false,
            buffer: None,
            pointer: 0,
            size: 4,
            stride: 0,
            ty: GL_FLOAT,
            normalized: false,
        }
    }
}

impl ArrayState {
    /// Size of one component of `ty`, in bytes.
    pub(crate) fn component_size(ty: u32) -> Option<usize> {
        Some(match ty {
            GL_BYTE | GL_UNSIGNED_BYTE => 1,
            GL_SHORT | GL_UNSIGNED_SHORT => 2,
            GL_INT | GL_UNSIGNED_INT | GL_FLOAT | GL_FIXED => 4,
            _ => return None,
        })
    }

    fn stride_or_tight(&self) -> Option<usize> {
        let packed = (self.size as usize).checked_mul(Self::component_size(self.ty)?)?;
        if self.stride == 0 {
            Some(packed)
        } else if self.stride as usize >= packed {
            Some(self.stride as usize)
        } else {
            None
        }
    }

    /// Read element `index` and widen it to four floats, padding with `(0, 0, 0, 1)`.
    ///
    /// # Safety
    /// `base` must point at `stride * index + component_size * size` readable bytes;
    /// `crate::api` computes that bound from the buffer or client pointer before calling.
    pub unsafe fn fetch(&self, base: *const u8, index: usize) -> Option<[f32; 4]> {
        let stride = self.stride_or_tight()?;
        let mut out = [0.0f32, 0.0, 0.0, 1.0];
        let components = self.size.clamp(1, 4) as usize;
        for (component, slot) in out.iter_mut().enumerate().take(components) {
            let at =
                unsafe { base.add(index * stride + component * Self::component_size(self.ty)?) };
            let raw = match self.ty {
                GL_FLOAT => f64::from(unsafe { at.cast::<f32>().read_unaligned() }),
                GL_UNSIGNED_BYTE => f64::from(unsafe { at.cast::<u8>().read_unaligned() }),
                GL_BYTE => f64::from(unsafe { at.cast::<i8>().read_unaligned() }),
                GL_UNSIGNED_SHORT => f64::from(unsafe { at.cast::<u16>().read_unaligned() }),
                GL_SHORT => f64::from(unsafe { at.cast::<i16>().read_unaligned() }),
                GL_UNSIGNED_INT => f64::from(unsafe { at.cast::<u32>().read_unaligned() }),
                GL_INT => f64::from(unsafe { at.cast::<i32>().read_unaligned() }),
                GL_FIXED => f64::from(unsafe { at.cast::<i32>().read_unaligned() }) / 65536.0,
                _ => return None,
            };
            let scaled = if self.normalized {
                match self.ty {
                    GL_UNSIGNED_BYTE => raw / 255.0,
                    GL_BYTE => (raw / 127.0).max(-1.0),
                    GL_UNSIGNED_SHORT => raw / 65535.0,
                    GL_SHORT => (raw / 32767.0).max(-1.0),
                    _ => raw,
                }
            } else {
                raw
            };
            *slot = scaled as f32;
        }
        Some(out)
    }
}

/// Counters that say what the frontend actually did, for tests and diagnostics.
#[derive(Debug, Default, Clone, Copy)]
pub struct Diagnostics {
    /// Calls to entry points that have no implementation.
    pub unsupported_calls: u64,
    /// Calls rejected with a GL error.
    pub error_calls: u64,
    /// `glDrawArrays` / `glDrawElements` / `glEnd` calls that produced geometry.
    pub draw_calls: u64,
    /// Triangles handed to the rasterizer.
    pub triangles: u64,
    /// Fragments that passed the depth test and were written.
    pub fragments_written: u64,
    /// Texture fetches performed.
    pub texture_fetches: u64,
    /// Draw calls whose geometry was produced by the Vulkan backend.
    pub routed_draws: u64,
    /// Draw calls that a Vulkan session declined, so the rasterizer ran instead.
    pub rasterizer_fallbacks: u64,
}

/// Everything one GL context owns.
#[derive(Debug)]
pub struct Context {
    /// The colour and depth target every draw call writes to.
    pub framebuffer: Framebuffer,
    /// Viewport `(x, y, width, height)`.
    pub viewport: [i32; 4],
    /// Scissor box `(x, y, width, height)`.
    pub scissor: [i32; 4],
    /// `glScissor` test enabled.
    pub scissor_test: bool,
    /// Colour the colour buffer clears to.
    pub clear_color: [f32; 4],
    /// Value the depth buffer clears to.
    pub clear_depth: f32,
    /// Per-channel colour write enable.
    pub color_mask: [bool; 4],
    /// Depth write enable.
    pub depth_mask: bool,
    /// Depth test enable.
    pub depth_test: bool,
    /// Depth comparison function.
    pub depth_func: u32,
    /// Blend enable.
    pub blend: bool,
    /// Blend source factor.
    pub blend_src: u32,
    /// Blend destination factor.
    pub blend_dst: u32,
    /// Face culling enable.
    pub cull_face: bool,
    /// Which faces to drop.
    pub cull_mode: u32,
    /// Winding that counts as front-facing.
    pub front_face: u32,
    /// Texturing enable.
    pub texture_2d: bool,
    /// Stack `glMatrixMode` selects.
    pub matrix_mode: u32,
    /// Modelview matrices.
    pub modelview: MatrixStack,
    /// Projection matrices.
    pub projection: MatrixStack,
    /// Texture matrices.
    pub texture_matrix: MatrixStack,
    /// Current colour for immediate-mode vertices.
    pub current_color: [f32; 4],
    /// Current texture coordinate for immediate-mode vertices.
    pub current_tex: [f32; 4],
    /// Primitive being assembled by `glBegin`/`glEnd`, if any.
    pub begin_mode: Option<u32>,
    /// Vertices submitted since `glBegin`.
    pub begin_vertices: Vec<Vertex>,
    /// Buffer objects by name; index 0 is unused.
    pub buffers: Vec<Option<Buffer>>,
    /// Currently bound buffer per target.
    pub bound_array_buffer: Option<u32>,
    /// Currently bound element array buffer.
    pub bound_element_buffer: Option<u32>,
    /// Textures by name; index 0 is unused.
    pub textures: Vec<Option<Texture>>,
    /// Bound 2D texture per image unit.
    pub bound_texture: [Option<u32>; MAX_TEXTURE_UNITS],
    /// Active image unit.
    pub active_unit: usize,
    /// Attribute arrays, GLES2 style.
    pub attribs: [ArrayState; MAX_ATTRIBS],
    /// GLES1 position array.
    pub vertex_array: ArrayState,
    /// GLES1 colour array.
    pub color_array: ArrayState,
    /// GLES1 texture coordinate array.
    pub texcoord_array: ArrayState,
    /// Shader objects by name.
    pub shaders: Vec<Option<Shader>>,
    /// Program objects by name.
    pub programs: Vec<Option<Program>>,
    /// Program in use, if any.
    pub current_program: Option<u32>,
    /// Unpacking alignment for `glTexImage2D`.
    pub unpack_alignment: usize,
    /// Packing alignment for `glReadPixels`.
    pub pack_alignment: usize,
    /// Pending error, as `glGetError` reports it.
    pub error: Option<u32>,
    /// Counters.
    pub diagnostics: Diagnostics,
    /// Most recent entry point that had no implementation.
    pub last_unsupported: Option<&'static str>,
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Context {
    /// A context with a 1×1 target; `eglMakeCurrent` or `glViewport` sizes it properly.
    pub fn new() -> Self {
        Self {
            framebuffer: Framebuffer::new(1, 1).expect("a 1x1 target always fits"),
            viewport: [0, 0, 1, 1],
            scissor: [0, 0, 1, 1],
            scissor_test: false,
            clear_color: [0.0, 0.0, 0.0, 0.0],
            clear_depth: 1.0,
            color_mask: [true; 4],
            depth_mask: true,
            depth_test: false,
            depth_func: GL_LESS,
            blend: false,
            blend_src: GL_ONE,
            blend_dst: GL_ZERO,
            cull_face: false,
            cull_mode: GL_BACK,
            front_face: GL_CCW,
            texture_2d: false,
            matrix_mode: GL_MODELVIEW,
            modelview: MatrixStack::new(MODELVIEW_DEPTH).expect("32 is a valid depth"),
            projection: MatrixStack::new(OTHER_DEPTH).expect("2 is a valid depth"),
            texture_matrix: MatrixStack::new(OTHER_DEPTH).expect("2 is a valid depth"),
            current_color: [1.0; 4],
            current_tex: [0.0, 0.0, 0.0, 1.0],
            begin_mode: None,
            begin_vertices: Vec::new(),
            buffers: Vec::new(),
            bound_array_buffer: None,
            bound_element_buffer: None,
            textures: Vec::new(),
            bound_texture: [None; MAX_TEXTURE_UNITS],
            active_unit: 0,
            attribs: [ArrayState::default(); MAX_ATTRIBS],
            vertex_array: ArrayState::default(),
            color_array: ArrayState { size: 4, ..ArrayState::default() },
            texcoord_array: ArrayState { size: 2, ..ArrayState::default() },
            shaders: Vec::new(),
            programs: Vec::new(),
            current_program: None,
            unpack_alignment: 4,
            pack_alignment: 4,
            error: None,
            diagnostics: Diagnostics::default(),
            last_unsupported: None,
        }
    }

    /// Reset every piece of state a real `eglMakeCurrent` would reset.
    pub fn reset(&mut self, width: usize, height: usize) {
        // Any un-composited device frame belongs to the framebuffer being replaced.
        crate::vulkan::flush_pending(self);
        let diagnostics = self.diagnostics;
        *self = Self::new();
        self.diagnostics = diagnostics;
        let _ = self.framebuffer.resize(width, height);
        self.viewport = [0, 0, width as i32, height as i32];
        self.scissor = [0, 0, width as i32, height as i32];
    }

    /// Record the first error of a sequence, as GL requires.
    pub fn set_error(&mut self, error: u32) {
        if self.error.is_none() {
            self.error = Some(error);
            self.diagnostics.error_calls += 1;
        }
    }

    /// The stack `glMatrixMode` selected.
    pub fn active_stack(&mut self) -> &mut MatrixStack {
        match self.matrix_mode {
            GL_PROJECTION => &mut self.projection,
            GL_TEXTURE => &mut self.texture_matrix,
            _ => &mut self.modelview,
        }
    }

    /// Projection times modelview, the transform every vertex goes through.
    pub fn mvp(&self) -> Mat4 {
        self.projection.current().post_mul(self.modelview.current())
    }

    /// Allocate a name in a slot table; names start at 1 because 0 is reserved.
    pub fn alloc<T>(slots: &mut Vec<Option<T>>, value: T) -> u32 {
        if let Some(free) = slots.iter().position(|slot| slot.is_none()) {
            slots[free] = Some(value);
            return (free + 1) as u32;
        }
        slots.push(Some(value));
        slots.len() as u32
    }
}

thread_local! {
    static CURRENT: RefCell<Context> = RefCell::new(Context::new());
    static PRESENTED: RefCell<Option<Framebuffer>> = const { RefCell::new(None) };
}

/// Run `f` against this thread's context.
///
/// The closure must not call another `with`, because the `RefCell` is already borrowed;
/// every public entry point follows that rule.
pub fn with<R>(f: impl FnOnce(&mut Context) -> R) -> R {
    CURRENT.with(|cell| f(&mut cell.borrow_mut()))
}

/// Store the framebuffer an `eglSwapBuffers` presented, for tests and hosts to read back.
pub fn present(framebuffer: &Framebuffer) {
    PRESENTED.with(|cell| *cell.borrow_mut() = Some(framebuffer.clone()));
}

/// The framebuffer the last `eglSwapBuffers` presented, if any.
pub fn presented() -> Option<Framebuffer> {
    PRESENTED.with(|cell| cell.borrow().clone())
}

/// Note that an entry point with no implementation was called.
///
/// This also raises `GL_INVALID_OPERATION`, which is what D-002 in `docs/DECISIONS.md`
/// requires of an entry point that exists but cannot do the work: a caller probing
/// capabilities gets a legal GL error rather than a silent no-op.
pub fn record_unsupported(name: &'static str) {
    with(|context| {
        context.diagnostics.unsupported_calls += 1;
        context.last_unsupported = Some(name);
        context.set_error(crate::gl_types::GL_INVALID_OPERATION);
    });
}

/// How many calls hit an unimplemented entry point.
pub fn unsupported_count() -> u64 {
    with(|context| context.diagnostics.unsupported_calls)
}

/// The most recent unimplemented entry point that was called.
pub fn last_unsupported() -> Option<&'static str> {
    with(|context| context.last_unsupported)
}

/// A snapshot of the counters.
pub fn diagnostics() -> Diagnostics {
    with(|context| context.diagnostics)
}
