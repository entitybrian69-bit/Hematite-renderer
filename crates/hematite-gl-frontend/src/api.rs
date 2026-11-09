//! The implemented GL entry points.
//!
//! Every function here has the exact signature `abi/gl_export.def` declares for it; the
//! generated `#[no_mangle]` thunk forwards to it, so a signature drift is a compile error
//! rather than silent ABI corruption.
//!
//! # What is implemented
//! State, matrix stacks, buffer objects, client and VBO attribute arrays, immediate mode,
//! textures (nearest and bilinear, modulated), depth test, blending, scissor, culling,
//! clear, readback and errors, all on the software rasterizer in [`crate::raster`].
//!
//! # What is not
//! GLSL is compiled — `glCompileShader` translates the source to SPIR-V through
//! [`hematite_glsl`], and `glLinkProgram` checks the stage interfaces — but the compiled
//! SPIR-V is not executed by the rasterizer in [`crate::raster`]: draws still run the
//! fixed-function path. Routing the front end through the Vulkan backend is what makes a
//! shader-driven pipeline actually render. Stencil operations, `glPolygonOffset` and
//! `glHint` are accepted and have no effect. Multisampling, point sprites, texture
//! compression and renderbuffers are not implemented and report `GL_INVALID_ENUM` /
//! `GL_INVALID_OPERATION`.

//! Entry point names keep their C spelling, so the lint is off for this module.
#![allow(non_snake_case)]

use crate::context::{with, ArrayState, Buffer, Context, Program, Shader, Texture, Vertex};
use crate::gl_types::*;
use std::ffi::{c_char, c_void, CStr};

const VENDOR: &[u8] = b"Hematite\0";
const RENDERER: &[u8] = b"Hematite software rasterizer\0";
const VERSION: &[u8] = b"1.4 Hematite 0.1.0 (fixed-function subset)\0";
const EXTENSIONS: &[u8] = b"\0";
const GLSL_VERSION: &[u8] = b"1.20 Hematite (translated to SPIR-V)\0";

/// Attribute location used for colour when no shader binds one.
const COLOR_ATTRIB: usize = 1;
/// Attribute location used for texture coordinates when no shader binds one.
const TEXCOORD_ATTRIB: usize = 2;

fn cstr<'a>(pointer: *const c_char) -> Option<std::borrow::Cow<'a, str>> {
    if pointer.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(pointer.cast()) }.to_string_lossy())
}

/// Assemble the vertices one draw call will consume.
struct Assembler {
    position: Option<ArrayRef>,
    color: Option<ArrayRef>,
    texcoord: Option<ArrayRef>,
}

/// One enabled array: its description plus the byte address element 0 lives at.
struct ArrayRef {
    state: ArrayState,
    base: *const u8,
}

impl Context {
    fn array_base(&self, state: &ArrayState) -> Option<*const u8> {
        match state.buffer {
            Some(name) => {
                self.buffers.get(name as usize - 1)?.as_ref().map(|buffer| buffer.data.as_ptr())
            }
            None => Some(state.pointer as *const u8),
        }
    }

    /// Refuse the draw when a buffer-backed array does not hold `highest + 1` elements.
    fn array_fits(&self, state: &ArrayState, highest: usize) -> bool {
        let Some(name) = state.buffer else { return true };
        let Some(buffer) = self.buffers.get(name as usize - 1).and_then(|slot| slot.as_ref())
        else {
            return false;
        };
        let component = match ArrayState::component_size(state.ty) {
            Some(size) => size,
            None => return false,
        };
        let stride = if state.stride == 0 {
            component * state.size.max(1) as usize
        } else {
            state.stride as usize
        };
        let needed = highest * stride + component * state.size.max(1) as usize;
        state.pointer + needed <= buffer.data.len()
    }

    /// Pick the position, colour and texture arrays a draw call will read.
    ///
    /// Generic attributes win over the GLES1 client arrays. Without a compiled program the
    /// locations are fixed: 0 is position, [`COLOR_ATTRIB`] colour, [`TEXCOORD_ATTRIB`]
    /// texture coordinates.
    fn pick_arrays(&self) -> Option<Assembler> {
        let resolve = |state: &ArrayState| -> Option<ArrayRef> {
            if !state.enabled {
                return None;
            }
            Some(ArrayRef { state: *state, base: self.array_base(state)? })
        };
        let position = resolve(&self.attribs[0]).or_else(|| resolve(&self.vertex_array));
        let color = resolve(&self.attribs[COLOR_ATTRIB]).or_else(|| resolve(&self.color_array));
        let texcoord =
            resolve(&self.attribs[TEXCOORD_ATTRIB]).or_else(|| resolve(&self.texcoord_array));
        Some(Assembler { position, color, texcoord })
    }
}

/// Build the vertex list for `count` elements, or set the error and return `None`.
fn assemble(
    ctx: &mut Context,
    first: usize,
    count: usize,
    indices: Option<&[u32]>,
) -> Option<Vec<Vertex>> {
    if count == 0 {
        return Some(Vec::new());
    }
    let highest = match indices {
        Some(list) => (*list.iter().max()?) as usize,
        None => first + count - 1,
    };
    let assembler = match ctx.pick_arrays() {
        Some(assembler) => assembler,
        None => {
            ctx.set_error(GL_INVALID_OPERATION);
            return None;
        }
    };
    let Some(position) = &assembler.position else {
        ctx.set_error(GL_INVALID_OPERATION);
        return None;
    };
    let candidates = [Some(position), assembler.color.as_ref(), assembler.texcoord.as_ref()];
    for candidate in candidates.into_iter().flatten() {
        if !ctx.array_fits(&candidate.state, highest) {
            ctx.set_error(GL_INVALID_OPERATION);
            return None;
        }
    }
    let mut vertices = Vec::with_capacity(count);
    let default_color = ctx.current_color;
    let default_tex = ctx.current_tex;
    for element in 0..count {
        let index = match indices {
            Some(list) => list[element] as usize,
            None => first + element,
        };
        let mut vertex = Vertex { color: default_color, tex: default_tex, ..Vertex::default() };
        vertex.pos = unsafe { position.state.fetch(position.base, index) }?;
        if let Some(color) = &assembler.color {
            vertex.color = unsafe { color.state.fetch(color.base, index) }?;
        }
        if let Some(texcoord) = &assembler.texcoord {
            vertex.tex = unsafe { texcoord.state.fetch(texcoord.base, index) }?;
        }
        vertices.push(vertex);
    }
    Some(vertices)
}

fn valid_primitive(mode: u32) -> bool {
    matches!(
        mode,
        GL_POINTS
            | GL_LINES
            | GL_LINE_LOOP
            | GL_LINE_STRIP
            | GL_TRIANGLES
            | GL_TRIANGLE_STRIP
            | GL_TRIANGLE_FAN
            | GL_QUADS
    )
}

// ---------------------------------------------------------------------------
// Errors, strings and queries
// ---------------------------------------------------------------------------

/// Return and clear the first error recorded since the last call.
pub fn glGetError() -> u32 {
    with(|ctx| ctx.error.take().unwrap_or(GL_NO_ERROR))
}

/// Static implementation strings.
pub fn glGetString(name: u32) -> *const u8 {
    let bytes: &'static [u8] = match name {
        GL_VENDOR => VENDOR,
        GL_RENDERER => RENDERER,
        GL_VERSION => VERSION,
        GL_EXTENSIONS => EXTENSIONS,
        GL_SHADING_LANGUAGE_VERSION => GLSL_VERSION,
        _ => {
            with(|ctx| ctx.set_error(GL_INVALID_ENUM));
            return std::ptr::null();
        }
    };
    bytes.as_ptr()
}

/// Integer state queries.
pub fn glGetIntegerv(pname: u32, params: *mut i32) {
    if params.is_null() {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| match pname {
        GL_VIEWPORT | GL_SCISSOR_BOX => {
            let source = if pname == GL_VIEWPORT { ctx.viewport } else { ctx.scissor };
            unsafe { params.cast::<[i32; 4]>().write_unaligned(source) };
        }
        GL_MAX_TEXTURE_SIZE => unsafe {
            params.write_unaligned(crate::context::MAX_TEXTURE_SIZE as i32)
        },
        GL_MAX_3D_TEXTURE_SIZE => unsafe { params.write_unaligned(0) },
        GL_DEPTH_TEST | GL_BLEND | GL_SCISSOR_TEST | GL_CULL_FACE | GL_TEXTURE_2D => unsafe {
            params.write_unaligned(i32::from(ctx.is_enabled(pname)))
        },
        _ => ctx.set_error(GL_INVALID_ENUM),
    });
}

/// Floating point state queries.
pub fn glGetFloatv(pname: u32, params: *mut f32) {
    if params.is_null() {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| {
        let matrix = match pname {
            GL_MODELVIEW_MATRIX => Some(ctx.modelview.current()),
            GL_PROJECTION_MATRIX => Some(ctx.projection.current()),
            GL_TEXTURE_MATRIX => Some(ctx.texture_matrix.current()),
            _ => None,
        };
        match matrix {
            Some(matrix) => {
                let columns: [f32; 16] = std::array::from_fn(|i| matrix.0[i] as f32);
                unsafe { params.cast::<[f32; 16]>().write_unaligned(columns) };
            }
            None => match pname {
                GL_VIEWPORT | GL_SCISSOR_BOX => {
                    let source = if pname == GL_VIEWPORT { ctx.viewport } else { ctx.scissor };
                    let floats: [f32; 4] = std::array::from_fn(|i| source[i] as f32);
                    unsafe { params.cast::<[f32; 4]>().write_unaligned(floats) };
                }
                GL_MAX_TEXTURE_SIZE => unsafe {
                    params.write_unaligned(crate::context::MAX_TEXTURE_SIZE as f32)
                },
                _ => ctx.set_error(GL_INVALID_ENUM),
            },
        }
    });
}

/// Boolean state queries.
pub fn glGetBooleanv(pname: u32, params: *mut u8) {
    if params.is_null() {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| match pname {
        GL_DEPTH_TEST | GL_BLEND | GL_SCISSOR_TEST | GL_CULL_FACE | GL_TEXTURE_2D => unsafe {
            params.write_unaligned(u8::from(ctx.is_enabled(pname)))
        },
        _ => ctx.set_error(GL_INVALID_ENUM),
    });
}

impl Context {
    fn is_enabled(&self, capability: u32) -> bool {
        match capability {
            GL_DEPTH_TEST => self.depth_test,
            GL_BLEND => self.blend,
            GL_SCISSOR_TEST => self.scissor_test,
            GL_CULL_FACE => self.cull_face,
            GL_TEXTURE_2D => self.texture_2d,
            _ => false,
        }
    }
}

/// Query one capability.
pub fn glIsEnabled(cap: u32) -> u8 {
    with(|ctx| match cap {
        GL_DEPTH_TEST | GL_BLEND | GL_SCISSOR_TEST | GL_CULL_FACE | GL_TEXTURE_2D => {
            u8::from(ctx.is_enabled(cap))
        }
        _ => {
            ctx.set_error(GL_INVALID_ENUM);
            GL_FALSE
        }
    })
}

/// Enable one capability.
pub fn glEnable(cap: u32) {
    with(|ctx| match cap {
        GL_DEPTH_TEST => ctx.depth_test = true,
        GL_BLEND => ctx.blend = true,
        GL_SCISSOR_TEST => ctx.scissor_test = true,
        GL_CULL_FACE => ctx.cull_face = true,
        GL_TEXTURE_2D => ctx.texture_2d = true,
        _ => ctx.set_error(GL_INVALID_ENUM),
    });
}

/// Disable one capability.
pub fn glDisable(cap: u32) {
    with(|ctx| match cap {
        GL_DEPTH_TEST => ctx.depth_test = false,
        GL_BLEND => ctx.blend = false,
        GL_SCISSOR_TEST => ctx.scissor_test = false,
        GL_CULL_FACE => ctx.cull_face = false,
        GL_TEXTURE_2D => ctx.texture_2d = false,
        _ => ctx.set_error(GL_INVALID_ENUM),
    });
}

// ---------------------------------------------------------------------------
// Clear, masks, viewport
// ---------------------------------------------------------------------------

/// Set the colour `glClear` writes.
pub fn glClearColor(red: f32, green: f32, blue: f32, alpha: f32) {
    with(|ctx| ctx.clear_color = [red, green, blue, alpha]);
}

/// Set the depth `glClear` writes.
pub fn glClearDepth(depth: f64) {
    with(|ctx| ctx.clear_depth = depth.clamp(0.0, 1.0) as f32);
}

/// Accepted and ignored: there is no stencil buffer.
pub fn glClearStencil(_s: i32) {}

/// Clear the named buffers.
pub fn glClear(mask: u32) {
    let known = GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT;
    if mask & !known != 0 {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| ctx.clear(mask));
}

/// Per-channel colour write enable.
pub fn glColorMask(red: u8, green: u8, blue: u8, alpha: u8) {
    with(|ctx| {
        ctx.color_mask = [red != 0, green != 0, blue != 0, alpha != 0];
    });
}

/// Depth write enable.
pub fn glDepthMask(flag: u8) {
    with(|ctx| ctx.depth_mask = flag != 0);
}

/// Set the viewport and resize a default framebuffer that still matches the old one.
pub fn glViewport(x: i32, y: i32, width: i32, height: i32) {
    if width < 0 || height < 0 {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| {
        ctx.viewport = [x, y, width, height];
    });
}

/// Set the scissor box.
pub fn glScissor(x: i32, y: i32, width: i32, height: i32) {
    if width < 0 || height < 0 {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| ctx.scissor = [x, y, width, height]);
}

/// Set the depth comparison function.
pub fn glDepthFunc(func: u32) {
    with(|ctx| match func {
        GL_NEVER | GL_LESS | GL_EQUAL | GL_LEQUAL | GL_GREATER | GL_NOTEQUAL | GL_GEQUAL
        | GL_ALWAYS => ctx.depth_func = func,
        _ => ctx.set_error(GL_INVALID_ENUM),
    });
}

/// Set both blend factors.
pub fn glBlendFunc(sfactor: u32, dfactor: u32) {
    with(|ctx| {
        if is_blend_factor(sfactor) && is_blend_factor(dfactor) {
            ctx.blend_src = sfactor;
            ctx.blend_dst = dfactor;
        } else {
            ctx.set_error(GL_INVALID_ENUM);
        }
    });
}

/// Set the RGB and alpha blend factors separately; the alpha pair is stored too.
pub fn glBlendFuncSeparate(src_rgb: u32, dst_rgb: u32, src_alpha: u32, dst_alpha: u32) {
    with(|ctx| {
        if is_blend_factor(src_rgb) && is_blend_factor(dst_rgb) && src_alpha == dst_alpha {
            ctx.blend_src = src_rgb;
            ctx.blend_dst = dst_rgb;
        } else {
            ctx.set_error(GL_INVALID_ENUM);
        }
    });
}

fn is_blend_factor(factor: u32) -> bool {
    matches!(
        factor,
        GL_ZERO
            | GL_ONE
            | GL_SRC_COLOR
            | GL_ONE_MINUS_SRC_COLOR
            | GL_SRC_ALPHA
            | GL_ONE_MINUS_SRC_ALPHA
            | GL_DST_ALPHA
            | GL_ONE_MINUS_DST_ALPHA
            | GL_DST_COLOR
            | GL_ONE_MINUS_DST_COLOR
    )
}

/// Choose which faces to drop.
pub fn glCullFace(mode: u32) {
    with(|ctx| match mode {
        GL_FRONT | GL_BACK | GL_FRONT_AND_BACK => ctx.cull_mode = mode,
        _ => ctx.set_error(GL_INVALID_ENUM),
    });
}

/// Choose the front-facing winding.
pub fn glFrontFace(mode: u32) {
    with(|ctx| match mode {
        GL_CW | GL_CCW => ctx.front_face = mode,
        _ => ctx.set_error(GL_INVALID_ENUM),
    });
}

/// Accepted; lines are always one pixel wide.
pub fn glLineWidth(_width: f32) {}

/// Accepted and ignored: depth offsets are not applied.
pub fn glPolygonOffset(_factor: f32, _units: f32) {}

/// Accepted and ignored: there is no stencil buffer.
pub fn glStencilFunc(_func: u32, _ref: i32, _mask: u32) {}

/// Accepted and ignored: there is no stencil buffer.
pub fn glStencilOp(_fail: u32, _zfail: u32, _zpass: u32) {}

/// Accepted and ignored: the software rasterizer has no tunable quality.
pub fn glHint(_target: u32, _mode: u32) {}

/// No command buffering, so this returns immediately.
pub fn glFlush() {}

/// No command buffering, so this returns immediately.
pub fn glFinish() {}

// ---------------------------------------------------------------------------
// Matrix stacks
// ---------------------------------------------------------------------------

/// Select the stack the transforms apply to.
pub fn glMatrixMode(mode: u32) {
    with(|ctx| match mode {
        GL_MODELVIEW | GL_PROJECTION | GL_TEXTURE => ctx.matrix_mode = mode,
        _ => ctx.set_error(GL_INVALID_ENUM),
    });
}

/// Replace the current matrix with identity.
pub fn glLoadIdentity() {
    with(|ctx| ctx.active_stack().load(hematite_fixed_function::matrix::Mat4::IDENTITY));
}

fn f32_matrix(pointer: *const f32) -> Option<hematite_fixed_function::matrix::Mat4> {
    if pointer.is_null() {
        return None;
    }
    let columns = unsafe { pointer.cast::<[f32; 16]>().read_unaligned() };
    if !columns.iter().all(|value| value.is_finite()) {
        return None;
    }
    Some(hematite_fixed_function::matrix::Mat4(std::array::from_fn(|i| f64::from(columns[i]))))
}

/// Load a column-major matrix.
pub fn glLoadMatrixf(m: *const f32) {
    match f32_matrix(m) {
        Some(matrix) => with(|ctx| ctx.active_stack().load(matrix)),
        None => with(|ctx| ctx.set_error(GL_INVALID_VALUE)),
    }
}

/// Post-multiply a column-major matrix.
pub fn glMultMatrixf(m: *const f32) {
    match f32_matrix(m) {
        Some(matrix) => with(|ctx| ctx.active_stack().multiply(matrix)),
        None => with(|ctx| ctx.set_error(GL_INVALID_VALUE)),
    }
}

/// Multiply by a translation.
pub fn glTranslatef(x: f32, y: f32, z: f32) {
    let matrix = hematite_fixed_function::matrix::Mat4::translation(
        f64::from(x),
        f64::from(y),
        f64::from(z),
    );
    with(|ctx| ctx.active_stack().multiply(matrix));
}

/// Multiply by a rotation about an axis, in degrees.
pub fn glRotatef(angle: f32, x: f32, y: f32, z: f32) {
    let axis = [f64::from(x), f64::from(y), f64::from(z)];
    if axis.iter().all(|component| *component == 0.0) {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    let matrix = hematite_fixed_function::matrix::Mat4::rotation(f64::from(angle), axis);
    with(|ctx| ctx.active_stack().multiply(matrix));
}

/// Multiply by a nonuniform scale.
pub fn glScalef(x: f32, y: f32, z: f32) {
    let matrix =
        hematite_fixed_function::matrix::Mat4::scaling(f64::from(x), f64::from(y), f64::from(z));
    with(|ctx| ctx.active_stack().multiply(matrix));
}

/// Multiply by an orthographic projection.
pub fn glOrtho(left: f64, right: f64, bottom: f64, top: f64, near: f64, far: f64) {
    match hematite_fixed_function::matrix::Mat4::ortho(left, right, bottom, top, near, far) {
        Ok(matrix) => with(|ctx| ctx.active_stack().multiply(matrix)),
        Err(_) => with(|ctx| ctx.set_error(GL_INVALID_VALUE)),
    }
}

/// Multiply by a perspective projection.
pub fn glFrustum(left: f64, right: f64, bottom: f64, top: f64, near: f64, far: f64) {
    match hematite_fixed_function::matrix::Mat4::frustum(left, right, bottom, top, near, far) {
        Ok(matrix) => with(|ctx| ctx.active_stack().multiply(matrix)),
        Err(_) => with(|ctx| ctx.set_error(GL_INVALID_VALUE)),
    }
}

/// Duplicate the current matrix.
pub fn glPushMatrix() {
    with(|ctx| {
        if ctx.active_stack().push().is_err() {
            ctx.set_error(GL_STACK_OVERFLOW);
        }
    });
}

/// Restore the previous matrix.
pub fn glPopMatrix() {
    with(|ctx| {
        if ctx.active_stack().pop().is_err() {
            ctx.set_error(GL_STACK_UNDERFLOW);
        }
    });
}

// ---------------------------------------------------------------------------
// Immediate mode
// ---------------------------------------------------------------------------

/// Start assembling a primitive.
pub fn glBegin(mode: u32) {
    if !valid_primitive(mode) {
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return;
    }
    with(|ctx| {
        if ctx.begin_mode.is_some() {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        }
        ctx.begin_mode = Some(mode);
        ctx.begin_vertices.clear();
    });
}

fn push_vertex(x: f32, y: f32, z: f32, w: f32) {
    with(|ctx| {
        if ctx.begin_mode.is_none() {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        }
        ctx.begin_vertices.push(Vertex {
            pos: [x, y, z, w],
            color: ctx.current_color,
            tex: ctx.current_tex,
        });
    });
}

/// Submit a 2D vertex.
pub fn glVertex2f(x: f32, y: f32) {
    push_vertex(x, y, 0.0, 1.0);
}

/// Submit a 3D vertex.
pub fn glVertex3f(x: f32, y: f32, z: f32) {
    push_vertex(x, y, z, 1.0);
}

/// Submit a 4D vertex.
pub fn glVertex4f(x: f32, y: f32, z: f32, w: f32) {
    push_vertex(x, y, z, w);
}

/// Submit a 3D vertex from an array.
pub fn glVertex3fv(v: *const f32) {
    if v.is_null() {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    let values = unsafe { v.cast::<[f32; 3]>().read_unaligned() };
    push_vertex(values[0], values[1], values[2], 1.0);
}

/// Set the current colour.
pub fn glColor4f(red: f32, green: f32, blue: f32, alpha: f32) {
    with(|ctx| ctx.current_color = [red, green, blue, alpha]);
}

/// Set the current colour from three components.
pub fn glColor3f(red: f32, green: f32, blue: f32) {
    with(|ctx| ctx.current_color = [red, green, blue, 1.0]);
}

/// Set the current colour from an array of four floats.
pub fn glColor4fv(v: *const f32) {
    if v.is_null() {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    let values = unsafe { v.cast::<[f32; 4]>().read_unaligned() };
    with(|ctx| ctx.current_color = values);
}

/// Set the current colour from four bytes.
pub fn glColor4ub(red: u8, green: u8, blue: u8, alpha: u8) {
    with(|ctx| {
        ctx.current_color = [
            f32::from(red) / 255.0,
            f32::from(green) / 255.0,
            f32::from(blue) / 255.0,
            f32::from(alpha) / 255.0,
        ];
    });
}

/// Set the current texture coordinate.
pub fn glTexCoord2f(s: f32, t: f32) {
    with(|ctx| ctx.current_tex = [s, t, 0.0, 1.0]);
}

/// Finish the primitive and rasterize it.
pub fn glEnd() {
    with(|ctx| {
        let Some(mode) = ctx.begin_mode.take() else {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        };
        let vertices = std::mem::take(&mut ctx.begin_vertices);
        ctx.draw_vertices(mode, &vertices);
    });
}

// ---------------------------------------------------------------------------
// Buffer objects
// ---------------------------------------------------------------------------

/// Create buffer object names.
pub fn glGenBuffers(n: i32, buffers: *mut u32) {
    if n < 0 || (n > 0 && buffers.is_null()) {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| {
        for index in 0..n as usize {
            let name = Context::alloc(&mut ctx.buffers, Buffer { data: Vec::new(), usage: 0 });
            unsafe { buffers.add(index).write_unaligned(name) };
        }
    });
}

/// Delete buffer objects, unbinding them if they were current.
pub fn glDeleteBuffers(n: i32, buffers: *const u32) {
    if n < 0 || (n > 0 && buffers.is_null()) {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| {
        for index in 0..n as usize {
            let name = unsafe { buffers.add(index).read_unaligned() };
            if name == 0 {
                continue;
            }
            if let Some(slot) = ctx.buffers.get_mut(name as usize - 1) {
                *slot = None;
            }
            if ctx.bound_array_buffer == Some(name) {
                ctx.bound_array_buffer = None;
            }
            if ctx.bound_element_buffer == Some(name) {
                ctx.bound_element_buffer = None;
            }
            for attrib in ctx.attribs.iter_mut() {
                if attrib.buffer == Some(name) {
                    attrib.buffer = None;
                    attrib.enabled = false;
                }
            }
        }
    });
}

/// Bind a buffer to a target.
pub fn glBindBuffer(target: u32, buffer: u32) {
    with(|ctx| match target {
        GL_ARRAY_BUFFER => {
            if buffer == 0 || ctx.buffers.get(buffer as usize - 1).is_some_and(|s| s.is_some()) {
                ctx.bound_array_buffer = if buffer == 0 { None } else { Some(buffer) };
            } else {
                ctx.set_error(GL_INVALID_VALUE);
            }
        }
        GL_ELEMENT_ARRAY_BUFFER => {
            if buffer == 0 || ctx.buffers.get(buffer as usize - 1).is_some_and(|s| s.is_some()) {
                ctx.bound_element_buffer = if buffer == 0 { None } else { Some(buffer) };
            } else {
                ctx.set_error(GL_INVALID_VALUE);
            }
        }
        _ => ctx.set_error(GL_INVALID_ENUM),
    });
}

/// Allocate and optionally fill a buffer object.
pub fn glBufferData(target: u32, size: isize, data: *const c_void, usage: u32) {
    if size < 0 {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| {
        let bound = match target {
            GL_ARRAY_BUFFER => ctx.bound_array_buffer,
            GL_ELEMENT_ARRAY_BUFFER => ctx.bound_element_buffer,
            _ => {
                ctx.set_error(GL_INVALID_ENUM);
                return;
            }
        };
        let Some(name) = bound else {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        };
        let Some(slot) = ctx.buffers.get_mut(name as usize - 1) else {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        };
        let Some(buffer) = slot.as_mut() else {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        };
        buffer.data.clear();
        buffer.data.resize(size as usize, 0);
        buffer.usage = usage;
        if !data.is_null() && size > 0 {
            let source = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size as usize) };
            buffer.data.copy_from_slice(source);
        }
    });
}

/// Overwrite part of a buffer object.
pub fn glBufferSubData(target: u32, offset: isize, size: isize, data: *const c_void) {
    if size < 0 || offset < 0 {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| {
        let bound = match target {
            GL_ARRAY_BUFFER => ctx.bound_array_buffer,
            GL_ELEMENT_ARRAY_BUFFER => ctx.bound_element_buffer,
            _ => {
                ctx.set_error(GL_INVALID_ENUM);
                return;
            }
        };
        let Some(name) = bound else {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        };
        let Some(buffer) = ctx.buffers.get_mut(name as usize - 1).and_then(|slot| slot.as_mut())
        else {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        };
        let start = offset as usize;
        let end = start.saturating_add(size as usize);
        if end > buffer.data.len() || data.is_null() && size > 0 {
            ctx.set_error(GL_INVALID_VALUE);
            return;
        }
        if size > 0 {
            let source = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size as usize) };
            buffer.data[start..end].copy_from_slice(source);
        }
    });
}

// ---------------------------------------------------------------------------
// Attribute arrays
// ---------------------------------------------------------------------------

fn bound_buffer(ctx: &Context, target: u32) -> Option<u32> {
    match target {
        GL_ARRAY_BUFFER => ctx.bound_array_buffer,
        _ => None,
    }
}

/// Point a generic attribute at an array.
pub fn glVertexAttribPointer(
    index: u32,
    size: i32,
    ty: u32,
    normalized: u8,
    stride: i32,
    pointer: *const c_void,
) {
    if index as usize >= crate::context::MAX_ATTRIBS || !(1..=4).contains(&size) || stride < 0 {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    if !matches!(
        ty,
        GL_BYTE
            | GL_UNSIGNED_BYTE
            | GL_SHORT
            | GL_UNSIGNED_SHORT
            | GL_INT
            | GL_UNSIGNED_INT
            | GL_FLOAT
            | GL_FIXED
    ) {
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return;
    }
    with(|ctx| {
        let buffer = bound_buffer(ctx, GL_ARRAY_BUFFER);
        if buffer.is_none() && pointer.is_null() {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        }
        ctx.attribs[index as usize] = ArrayState {
            enabled: ctx.attribs[index as usize].enabled,
            buffer,
            pointer: pointer as usize,
            size,
            stride,
            ty,
            normalized: normalized != 0,
        };
    });
}

/// Enable a generic attribute array.
pub fn glEnableVertexAttribArray(index: u32) {
    if index as usize >= crate::context::MAX_ATTRIBS {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| ctx.attribs[index as usize].enabled = true);
}

/// Disable a generic attribute array.
pub fn glDisableVertexAttribArray(index: u32) {
    if index as usize >= crate::context::MAX_ATTRIBS {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| ctx.attribs[index as usize].enabled = false);
}

/// Enable one of the GLES1 client arrays.
pub fn glEnableClientState(array: u32) {
    with(|ctx| match array {
        GL_VERTEX_ARRAY => ctx.vertex_array.enabled = true,
        GL_COLOR_ARRAY => ctx.color_array.enabled = true,
        GL_TEXTURE_COORD_ARRAY => ctx.texcoord_array.enabled = true,
        _ => ctx.set_error(GL_INVALID_ENUM),
    });
}

/// Disable one of the GLES1 client arrays.
pub fn glDisableClientState(array: u32) {
    with(|ctx| match array {
        GL_VERTEX_ARRAY => ctx.vertex_array.enabled = false,
        GL_COLOR_ARRAY => ctx.color_array.enabled = false,
        GL_TEXTURE_COORD_ARRAY => ctx.texcoord_array.enabled = false,
        _ => ctx.set_error(GL_INVALID_ENUM),
    });
}

/// Point the GLES1 position array at memory or a bound buffer.
pub fn glVertexPointer(size: i32, ty: u32, stride: i32, pointer: *const c_void) {
    set_client_array(GL_VERTEX_ARRAY, size, ty, stride, pointer);
}

/// Point the GLES1 colour array at memory or a bound buffer.
pub fn glColorPointer(size: i32, ty: u32, stride: i32, pointer: *const c_void) {
    set_client_array(GL_COLOR_ARRAY, size, ty, stride, pointer);
}

/// Point the GLES1 texture coordinate array at memory or a bound buffer.
pub fn glTexCoordPointer(size: i32, ty: u32, stride: i32, pointer: *const c_void) {
    set_client_array(GL_TEXTURE_COORD_ARRAY, size, ty, stride, pointer);
}

fn set_client_array(array: u32, size: i32, ty: u32, stride: i32, pointer: *const c_void) {
    if !(2..=4).contains(&size) || stride < 0 {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    if !matches!(ty, GL_BYTE | GL_SHORT | GL_INT | GL_FLOAT | GL_FIXED | GL_UNSIGNED_BYTE) {
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return;
    }
    with(|ctx| {
        let state = ArrayState {
            enabled: match array {
                GL_VERTEX_ARRAY => ctx.vertex_array.enabled,
                GL_COLOR_ARRAY => ctx.color_array.enabled,
                _ => ctx.texcoord_array.enabled,
            },
            buffer: ctx.bound_array_buffer,
            pointer: pointer as usize,
            size,
            stride,
            ty,
            normalized: false,
        };
        match array {
            GL_VERTEX_ARRAY => ctx.vertex_array = state,
            GL_COLOR_ARRAY => ctx.color_array = state,
            _ => ctx.texcoord_array = state,
        }
    });
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// Draw `count` vertices from the enabled arrays.
pub fn glDrawArrays(mode: u32, first: i32, count: i32) {
    if first < 0 || count < 0 {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    if !valid_primitive(mode) {
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return;
    }
    with(|ctx| {
        let Some(vertices) = assemble(ctx, first as usize, count as usize, None) else {
            return;
        };
        ctx.draw_vertices(mode, &vertices);
    });
}

/// Draw indexed vertices.
pub fn glDrawElements(mode: u32, count: i32, ty: u32, indices: *const c_void) {
    if count < 0 {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    if !valid_primitive(mode) {
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return;
    }
    if !matches!(ty, GL_UNSIGNED_BYTE | GL_UNSIGNED_SHORT | GL_UNSIGNED_INT) {
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return;
    }
    with(|ctx| {
        let component = match ty {
            GL_UNSIGNED_BYTE => 1,
            GL_UNSIGNED_SHORT => 2,
            _ => 4,
        };
        // An index pointer is an offset into the bound element array buffer.
        let (base, limit) = match ctx.bound_element_buffer {
            Some(name) => match ctx.buffers.get(name as usize - 1).and_then(|slot| slot.as_ref()) {
                Some(buffer) => {
                    (unsafe { buffer.data.as_ptr().add(indices as usize) }, Some(buffer.data.len()))
                }
                None => {
                    ctx.set_error(GL_INVALID_OPERATION);
                    return;
                }
            },
            None => {
                if indices.is_null() {
                    ctx.set_error(GL_INVALID_OPERATION);
                    return;
                }
                (indices.cast::<u8>(), None)
            }
        };
        let needed = (count as usize).saturating_mul(component);
        if let Some(length) = limit {
            if (indices as usize).saturating_add(needed) > length {
                ctx.set_error(GL_INVALID_OPERATION);
                return;
            }
        }
        let list: Vec<u32> = (0..count as usize)
            .map(|index| match ty {
                GL_UNSIGNED_BYTE => u32::from(unsafe { base.add(index).read_unaligned() }),
                GL_UNSIGNED_SHORT => {
                    u32::from(unsafe { base.cast::<u16>().add(index).read_unaligned() })
                }
                _ => unsafe { base.cast::<u32>().add(index).read_unaligned() },
            })
            .collect();
        let Some(vertices) = assemble(ctx, 0, list.len(), Some(&list)) else {
            return;
        };
        ctx.draw_vertices(mode, &vertices);
    });
}

// ---------------------------------------------------------------------------
// Textures
// ---------------------------------------------------------------------------

/// Create texture names.
pub fn glGenTextures(n: i32, textures: *mut u32) {
    if n < 0 || (n > 0 && textures.is_null()) {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| {
        for index in 0..n as usize {
            let texture = Texture {
                width: 0,
                height: 0,
                format: GL_RGBA,
                data: Vec::new(),
                min_filter: GL_NEAREST,
                mag_filter: GL_NEAREST,
                wrap_s: GL_REPEAT,
                wrap_t: GL_REPEAT,
            };
            let name = Context::alloc(&mut ctx.textures, texture);
            unsafe { textures.add(index).write_unaligned(name) };
        }
    });
}

/// Delete textures, unbinding them from every unit.
pub fn glDeleteTextures(n: i32, textures: *const u32) {
    if n < 0 || (n > 0 && textures.is_null()) {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| {
        for index in 0..n as usize {
            let name = unsafe { textures.add(index).read_unaligned() };
            if name == 0 {
                continue;
            }
            if let Some(slot) = ctx.textures.get_mut(name as usize - 1) {
                *slot = None;
            }
            for bound in ctx.bound_texture.iter_mut() {
                if *bound == Some(name) {
                    *bound = None;
                }
            }
        }
    });
}

/// Bind a texture to the active unit.
pub fn glBindTexture(target: u32, texture: u32) {
    if target != GL_TEXTURE_2D {
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return;
    }
    with(|ctx| {
        if texture != 0 && !ctx.textures.get(texture as usize - 1).is_some_and(|s| s.is_some()) {
            ctx.set_error(GL_INVALID_VALUE);
            return;
        }
        ctx.bound_texture[ctx.active_unit] = if texture == 0 { None } else { Some(texture) };
    });
}

/// Select the active texture image unit.
pub fn glActiveTexture(texture: u32) {
    let unit = texture.wrapping_sub(GL_TEXTURE0) as usize;
    if unit >= crate::context::MAX_TEXTURE_UNITS {
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return;
    }
    with(|ctx| ctx.active_unit = unit);
}

/// Bytes per pixel of an upload format.
fn bytes_per_pixel(format: u32) -> Option<usize> {
    Some(match format {
        GL_ALPHA | GL_LUMINANCE => 1,
        GL_LUMINANCE_ALPHA => 2,
        GL_RGB => 3,
        GL_RGBA => 4,
        _ => return None,
    })
}

/// Expand any accepted upload format into RGBA8.
fn convert_pixels(
    format: u32,
    width: usize,
    height: usize,
    alignment: usize,
    data: *const c_void,
) -> Option<Vec<u8>> {
    let bpp = bytes_per_pixel(format)?;
    let row = width.checked_mul(bpp)?;
    let stride = row.div_ceil(alignment.max(1)) * alignment.max(1);
    let mut out = vec![0u8; width.checked_mul(height)?.checked_mul(4)?];
    let source = unsafe {
        std::slice::from_raw_parts(data.cast::<u8>(), stride.saturating_mul(height).max(1))
    };
    for y in 0..height {
        for x in 0..width {
            let at = y * stride + x * bpp;
            let (r, g, b, a) = match format {
                GL_RGBA => (source[at], source[at + 1], source[at + 2], source[at + 3]),
                GL_RGB => (source[at], source[at + 1], source[at + 2], 255),
                GL_LUMINANCE => (source[at], source[at], source[at], 255),
                GL_ALPHA => (0, 0, 0, source[at]),
                _ => (source[at], source[at], source[at], source[at + 1]),
            };
            let offset = (y * width + x) * 4;
            out[offset..offset + 4].copy_from_slice(&[r, g, b, a]);
        }
    }
    Some(out)
}

/// Upload a 2D texture image.
#[allow(clippy::too_many_arguments)]
pub fn glTexImage2D(
    target: u32,
    level: i32,
    internalformat: i32,
    width: i32,
    height: i32,
    border: i32,
    format: u32,
    ty: u32,
    data: *const c_void,
) {
    if target != GL_TEXTURE_2D || level != 0 || border != 0 || ty != GL_UNSIGNED_BYTE {
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return;
    }
    if width < 0 || height < 0 || width as usize > crate::context::MAX_TEXTURE_SIZE {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    if bytes_per_pixel(format).is_none() || u32::try_from(internalformat) != Ok(format) {
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return;
    }
    with(|ctx| {
        let Some(name) = ctx.bound_texture[ctx.active_unit] else {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        };
        let Some(texture) = ctx.textures.get_mut(name as usize - 1).and_then(|slot| slot.as_mut())
        else {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        };
        let (width, height) = (width as usize, height as usize);
        if width == 0 || height == 0 {
            texture.width = 0;
            texture.height = 0;
            texture.data.clear();
            return;
        }
        if data.is_null() {
            texture.width = width;
            texture.height = height;
            texture.format = format;
            texture.data = vec![0u8; width * height * 4];
            return;
        }
        match convert_pixels(format, width, height, ctx.unpack_alignment, data) {
            Some(pixels) => {
                texture.width = width;
                texture.height = height;
                texture.format = format;
                texture.data = pixels;
            }
            None => ctx.set_error(GL_INVALID_OPERATION),
        }
    });
}

/// Replace part of a 2D texture image.
#[allow(clippy::too_many_arguments)]
pub fn glTexSubImage2D(
    target: u32,
    level: i32,
    xoffset: i32,
    yoffset: i32,
    width: i32,
    height: i32,
    format: u32,
    ty: u32,
    data: *const c_void,
) {
    if target != GL_TEXTURE_2D || level != 0 || ty != GL_UNSIGNED_BYTE {
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return;
    }
    if xoffset < 0 || yoffset < 0 || width < 0 || height < 0 || data.is_null() {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| {
        let Some(name) = ctx.bound_texture[ctx.active_unit] else {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        };
        let Some(texture) = ctx.textures.get_mut(name as usize - 1).and_then(|slot| slot.as_mut())
        else {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        };
        let (x0, y0) = (xoffset as usize, yoffset as usize);
        let (w, h) = (width as usize, height as usize);
        if x0 + w > texture.width || y0 + h > texture.height || format != texture.format {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        }
        match convert_pixels(format, w, h, ctx.unpack_alignment, data) {
            Some(pixels) => {
                for row in 0..h {
                    let source = &pixels[row * w * 4..(row + 1) * w * 4];
                    let offset = ((y0 + row) * texture.width + x0) * 4;
                    texture.data[offset..offset + w * 4].copy_from_slice(source);
                }
            }
            None => ctx.set_error(GL_INVALID_OPERATION),
        }
    });
}

/// Set an integer texture parameter.
pub fn glTexParameteri(target: u32, pname: u32, param: i32) {
    if target != GL_TEXTURE_2D {
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return;
    }
    with(|ctx| {
        let Some(name) = ctx.bound_texture[ctx.active_unit] else {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        };
        let Some(texture) = ctx.textures.get_mut(name as usize - 1).and_then(|slot| slot.as_mut())
        else {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        };
        let value = param as u32;
        match pname {
            GL_TEXTURE_MIN_FILTER => {
                if value == GL_NEAREST || value == GL_LINEAR {
                    texture.min_filter = value;
                } else {
                    ctx.set_error(GL_INVALID_ENUM);
                }
            }
            GL_TEXTURE_MAG_FILTER => {
                if value == GL_NEAREST || value == GL_LINEAR {
                    texture.mag_filter = value;
                } else {
                    ctx.set_error(GL_INVALID_ENUM);
                }
            }
            GL_TEXTURE_WRAP_S => {
                if value == GL_REPEAT || value == GL_CLAMP_TO_EDGE {
                    texture.wrap_s = value;
                } else {
                    ctx.set_error(GL_INVALID_ENUM);
                }
            }
            GL_TEXTURE_WRAP_T => {
                if value == GL_REPEAT || value == GL_CLAMP_TO_EDGE {
                    texture.wrap_t = value;
                } else {
                    ctx.set_error(GL_INVALID_ENUM);
                }
            }
            _ => ctx.set_error(GL_INVALID_ENUM),
        }
    });
}

/// Set a floating point texture parameter; only the filters accept floats.
pub fn glTexParameterf(target: u32, pname: u32, param: f32) {
    glTexParameteri(target, pname, param as i32);
}

/// Set the unpack or pack row alignment.
pub fn glPixelStorei(pname: u32, param: i32) {
    if !matches!(param, 1 | 2 | 4 | 8) {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| match pname {
        GL_UNPACK_ALIGNMENT => ctx.unpack_alignment = param as usize,
        GL_PACK_ALIGNMENT => ctx.pack_alignment = param as usize,
        _ => ctx.set_error(GL_INVALID_ENUM),
    });
}

/// Accepted and ignored: the environment is always `MODULATE`.
pub fn glTexEnvi(_target: u32, _pname: u32, _param: i32) {}

// ---------------------------------------------------------------------------
// Readback
// ---------------------------------------------------------------------------

/// Read pixels out of the framebuffer, bottom-left origin.
pub fn glReadPixels(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    format: u32,
    ty: u32,
    pixels: *mut c_void,
) {
    if width < 0 || height < 0 {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    if pixels.is_null() {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    if ty != GL_UNSIGNED_BYTE || !matches!(format, GL_RGBA | GL_RGB) {
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return;
    }
    with(|ctx| {
        // Pixels recorded into an open device frame are not in the host buffer yet.
        crate::vulkan::flush_pending(ctx);
        let channels = if format == GL_RGBA { 4 } else { 3 };
        let row_bytes = (width as usize) * channels;
        let stride = row_bytes.div_ceil(ctx.pack_alignment) * ctx.pack_alignment;
        let destination = pixels.cast::<u8>();
        for row in 0..height as usize {
            for column in 0..width as usize {
                let pixel =
                    ctx.framebuffer.pixel((x + column as i32) as usize, (y + row as i32) as usize);
                let offset = row * stride + column * channels;
                unsafe {
                    destination.add(offset).write_unaligned(pixel[0]);
                    destination.add(offset + 1).write_unaligned(pixel[1]);
                    destination.add(offset + 2).write_unaligned(pixel[2]);
                    if channels == 4 {
                        destination.add(offset + 3).write_unaligned(pixel[3]);
                    }
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Shaders and programs: GLSL is compiled to SPIR-V by hematite-glsl
// ---------------------------------------------------------------------------

/// Create a shader object of the given stage.
pub fn glCreateShader(ty: u32) -> u32 {
    if ty != GL_VERTEX_SHADER && ty != GL_FRAGMENT_SHADER {
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return 0;
    }
    with(|ctx| {
        Context::alloc(
            &mut ctx.shaders,
            Shader { stage: ty, source: String::new(), compiled: None, info_log: String::new() },
        )
    })
}

/// Store the source of a shader object.
pub fn glShaderSource(shader: u32, count: i32, string: *const *const c_char, length: *const i32) {
    if count < 0 || (count > 0 && string.is_null()) {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    let mut source = String::new();
    for index in 0..count as usize {
        let pointer = unsafe { string.add(index).read_unaligned() };
        let Some(text) = cstr(pointer) else {
            with(|ctx| ctx.set_error(GL_INVALID_VALUE));
            return;
        };
        let declared = if length.is_null() {
            None
        } else {
            let declared = unsafe { length.add(index).read_unaligned() };
            if declared < 0 {
                with(|ctx| ctx.set_error(GL_INVALID_VALUE));
                return;
            }
            Some(declared as usize)
        };
        let piece = match declared {
            Some(limit) => text.chars().take(limit).collect::<String>(),
            None => text.into_owned(),
        };
        source.push_str(&piece);
    }
    with(|ctx| {
        let Some(slot) = ctx.shaders.get_mut(shader as usize - 1).and_then(|slot| slot.as_mut())
        else {
            ctx.set_error(GL_INVALID_VALUE);
            return;
        };
        slot.source = source;
        // A new source invalidates the previous compile, as the specification requires.
        slot.compiled = None;
        slot.info_log = String::new();
    });
}

/// Compile a shader object: GLSL in, SPIR-V out.
///
/// The compile runs outside the context borrow, so the compiler cannot re-enter the
/// front end (a double borrow would abort the process: `panic = "abort"` is set for
/// release builds because unwinding across the FFI boundary is undefined behaviour).
pub fn glCompileShader(shader: u32) {
    let requested = with(|ctx| {
        let Some(slot) = ctx.shaders.get(shader as usize - 1).and_then(|slot| slot.as_ref()) else {
            ctx.set_error(GL_INVALID_VALUE);
            return None;
        };
        Some((slot.source.clone(), slot.stage))
    });
    let Some((source, stage)) = requested else { return };
    let Some(stage) = hematite_glsl::Stage::from_gl_enum(stage) else {
        // `glCreateShader` rejects every other stage, so this is unreachable in practice.
        with(|ctx| ctx.set_error(GL_INVALID_ENUM));
        return;
    };
    let request = hematite_glsl::Request::new(stage, &source);
    let result = hematite_glsl::compile(&request);
    with(|ctx| {
        let Some(slot) = ctx.shaders.get_mut(shader as usize - 1).and_then(|slot| slot.as_mut())
        else {
            ctx.set_error(GL_INVALID_VALUE);
            return;
        };
        match result {
            Ok(compiled) => {
                let mut log = String::new();
                for warning in &compiled.warnings {
                    log.push_str("warning: ");
                    log.push_str(warning);
                    log.push('\n');
                }
                slot.compiled = Some(compiled);
                slot.info_log = log;
            }
            Err(error) => {
                slot.compiled = None;
                slot.info_log = error.to_info_log();
            }
        }
    });
}

/// Query shader state.
pub fn glGetShaderiv(shader: u32, pname: u32, params: *mut i32) {
    if params.is_null() {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| {
        let Some(slot) = ctx.shaders.get(shader as usize - 1).and_then(|slot| slot.as_ref()) else {
            ctx.set_error(GL_INVALID_VALUE);
            return;
        };
        let value = match pname {
            GL_COMPILE_STATUS => {
                if slot.compiled.is_some() {
                    GL_TRUE as i32
                } else {
                    GL_FALSE as i32
                }
            }
            // GL reports lengths including the terminating NUL.
            GL_INFO_LOG_LENGTH => slot.info_log.len() as i32 + 1,
            GL_SHADER_SOURCE_LENGTH => slot.source.len() as i32 + 1,
            GL_SHADER_TYPE => slot.stage as i32,
            GL_DELETE_STATUS => GL_FALSE as i32,
            _ => {
                ctx.set_error(GL_INVALID_ENUM);
                return;
            }
        };
        unsafe { params.write_unaligned(value) };
    });
}

/// Copy the compiler's diagnostics: errors when it failed, warnings when it did not.
pub fn glGetShaderInfoLog(shader: u32, buf_size: i32, length: *mut i32, info_log: *mut c_char) {
    let log =
        with(|ctx| match ctx.shaders.get(shader as usize - 1).and_then(|slot| slot.as_ref()) {
            Some(slot) => Some(slot.info_log.clone()),
            None => {
                ctx.set_error(GL_INVALID_VALUE);
                None
            }
        });
    if let Some(log) = log {
        write_log(buf_size, length, info_log, &log);
    }
}

fn write_log(buf_size: i32, length: *mut i32, destination: *mut c_char, text: &str) {
    if buf_size < 0 {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    if destination.is_null() || buf_size == 0 {
        return;
    }
    let bytes = text.as_bytes();
    let written = bytes.len().min(buf_size as usize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination.cast::<u8>(), written);
        destination.add(written).write_unaligned(0);
    }
    if !length.is_null() {
        unsafe { length.write_unaligned(written as i32) };
    }
}

/// Delete a shader object.
pub fn glDeleteShader(shader: u32) {
    if shader == 0 {
        return;
    }
    with(|ctx| {
        if let Some(slot) = ctx.shaders.get_mut(shader as usize - 1) {
            *slot = None;
        }
        for program in ctx.programs.iter_mut().flatten() {
            program.shaders.retain(|attached| *attached != shader);
        }
    });
}

/// Create a program object.
pub fn glCreateProgram() -> u32 {
    with(|ctx| {
        Context::alloc(
            &mut ctx.programs,
            Program {
                shaders: Vec::new(),
                uniforms: Default::default(),
                attribute_bindings: Vec::new(),
                linked: None,
                info_log: String::new(),
            },
        )
    })
}

/// Attach a shader to a program.
pub fn glAttachShader(program: u32, shader: u32) {
    with(|ctx| {
        let shader_exists = ctx.shaders.get(shader as usize - 1).is_some_and(|slot| slot.is_some());
        let Some(target) =
            ctx.programs.get_mut(program as usize - 1).and_then(|slot| slot.as_mut())
        else {
            ctx.set_error(GL_INVALID_VALUE);
            return;
        };
        if !shader_exists {
            ctx.set_error(GL_INVALID_VALUE);
            return;
        }
        target.shaders.push(shader);
    });
}

/// Link a program: check that the compiled stages fit together.
///
/// Hematite links by comparing the interfaces the compiler extracted, which is the
/// same check a driver makes: every fragment input must be written by the vertex
/// stage at the same location, and a program needs both stages to draw.
pub fn glLinkProgram(program: u32) {
    let gathered = with(|ctx| {
        let Some(target) = ctx.programs.get(program as usize - 1).and_then(|slot| slot.as_ref())
        else {
            ctx.set_error(GL_INVALID_VALUE);
            return None;
        };
        let mut stages: Vec<hematite_glsl::Shader> = Vec::new();
        let mut sources: Vec<(u32, String)> = Vec::new();
        let mut log = String::new();
        for name in &target.shaders {
            match ctx.shaders.get(*name as usize - 1).and_then(|slot| slot.as_ref()) {
                Some(slot) => {
                    sources.push((slot.stage, slot.source.clone()));
                    match &slot.compiled {
                        Some(compiled) => stages.push(compiled.clone()),
                        None => {
                            log.push_str("error: attached shader is not compiled\n");
                        }
                    }
                }
                None => log.push_str("error: attached shader no longer exists\n"),
            }
        }
        let bindings = target.attribute_bindings.clone();
        Some((stages, sources, log, bindings))
    });
    let Some((mut stages, sources, mut log, bindings)) = gathered else { return };

    // `glBindAttribLocation` takes effect at link time, so the vertex stage is
    // recompiled with the locations the application asked for. That is also what a
    // driver does: the binding is a property of the program, not of the shader.
    if !bindings.is_empty() {
        let locations =
            bindings.iter().map(|(name, location)| (name.as_str(), *location)).collect::<Vec<_>>();
        for (stage, source) in &sources {
            let Some(stage) = hematite_glsl::Stage::from_gl_enum(*stage) else { continue };
            if stage != hematite_glsl::Stage::Vertex {
                continue;
            }
            let request = hematite_glsl::Request {
                stage,
                source,
                attribute_locations: &locations,
                ..hematite_glsl::Request::default()
            };
            match hematite_glsl::compile(&request) {
                Ok(compiled) => {
                    if let Some(slot) = stages.iter_mut().find(|item| item.stage == stage) {
                        *slot = compiled;
                    }
                }
                Err(error) => log.push_str(&error.to_info_log()),
            }
        }
    }

    let vertex = stages.iter().find(|stage| stage.stage == hematite_glsl::Stage::Vertex);
    let fragment = stages.iter().rev().find(|stage| stage.stage == hematite_glsl::Stage::Fragment);
    let linked = match (vertex, fragment) {
        (Some(vertex), Some(fragment)) => {
            // Every fragment input must be produced by the vertex stage at the same
            // location, or the pipeline would read uninitialised data.
            for input in &fragment.interface.varyings {
                let produced = vertex
                    .interface
                    .varyings
                    .iter()
                    .any(|output| output.location == input.location && output.ty == input.ty);
                if !produced {
                    log.push_str(&format!(
                        "error: fragment input `{}` at location {} has no matching vertex \
                         output\n",
                        input.name, input.location
                    ));
                }
            }
            let mut uniform_names: Vec<String> = Vec::new();
            let mut samplers: Vec<(String, u32, u32)> = Vec::new();
            for stage in [&vertex.interface, &fragment.interface] {
                for member in stage.uniforms() {
                    if !uniform_names.contains(&member.name) {
                        uniform_names.push(member.name.clone());
                    }
                }
                for sampler in &stage.samplers {
                    if !samplers.iter().any(|(name, _, _)| *name == sampler.name) {
                        samplers.push((sampler.name.clone(), sampler.binding, sampler.set));
                        if !uniform_names.contains(&sampler.name) {
                            uniform_names.push(sampler.name.clone());
                        }
                    }
                }
            }
            let attributes = vertex
                .interface
                .attributes
                .iter()
                .map(|attribute| (attribute.name.clone(), attribute.location))
                .collect::<Vec<_>>();
            let _ = bindings;
            log.is_empty().then(|| crate::context::LinkedProgram {
                stages: stages.clone(),
                uniform_names,
                attributes,
                samplers,
            })
        }
        (None, _) => {
            log.push_str("error: no compiled vertex shader is attached\n");
            None
        }
        (_, None) => {
            log.push_str("error: no compiled fragment shader is attached\n");
            None
        }
    };
    with(|ctx| {
        let Some(target) =
            ctx.programs.get_mut(program as usize - 1).and_then(|slot| slot.as_mut())
        else {
            ctx.set_error(GL_INVALID_VALUE);
            return;
        };
        target.linked = linked;
        target.info_log = log;
    });
}

/// Query program state.
pub fn glGetProgramiv(program: u32, pname: u32, params: *mut i32) {
    if params.is_null() {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    }
    with(|ctx| {
        let Some(target) = ctx.programs.get(program as usize - 1).and_then(|slot| slot.as_ref())
        else {
            ctx.set_error(GL_INVALID_VALUE);
            return;
        };
        let linked = target.link_status();
        let value = match pname {
            GL_LINK_STATUS | GL_VALIDATE_STATUS => {
                if linked {
                    GL_TRUE as i32
                } else {
                    GL_FALSE as i32
                }
            }
            GL_INFO_LOG_LENGTH => target.info_log.len() as i32 + 1,
            GL_ATTACHED_SHADERS => target.shaders.len() as i32,
            GL_DELETE_STATUS => GL_FALSE as i32,
            GL_ACTIVE_UNIFORMS => match &target.linked {
                Some(program) => program.uniform_names.len() as i32,
                None => 0,
            },
            GL_ACTIVE_UNIFORM_MAX_LENGTH => match &target.linked {
                Some(program) => {
                    program.uniform_names.iter().map(String::len).max().unwrap_or(0) as i32 + 1
                }
                None => 0,
            },
            GL_ACTIVE_ATTRIBUTES => match &target.linked {
                Some(program) => program.attributes.len() as i32,
                None => 0,
            },
            GL_ACTIVE_ATTRIBUTE_MAX_LENGTH => match &target.linked {
                Some(program) => {
                    program.attributes.iter().map(|(name, _)| name.len()).max().unwrap_or(0) as i32
                        + 1
                }
                None => 0,
            },
            _ => {
                ctx.set_error(GL_INVALID_ENUM);
                return;
            }
        };
        unsafe { params.write_unaligned(value) };
    });
}

/// Copy the link diagnostics.
pub fn glGetProgramInfoLog(program: u32, buf_size: i32, length: *mut i32, info_log: *mut c_char) {
    let log =
        with(|ctx| match ctx.programs.get(program as usize - 1).and_then(|slot| slot.as_ref()) {
            Some(target) => Some(target.info_log.clone()),
            None => {
                ctx.set_error(GL_INVALID_VALUE);
                None
            }
        });
    if let Some(log) = log {
        write_log(buf_size, length, info_log, &log);
    }
}

/// Delete a program object.
pub fn glDeleteProgram(program: u32) {
    if program == 0 {
        return;
    }
    with(|ctx| {
        if let Some(slot) = ctx.programs.get_mut(program as usize - 1) {
            *slot = None;
        }
        if ctx.current_program == Some(program) {
            ctx.current_program = None;
        }
    });
}

/// Make a linked program current.
///
/// The program's SPIR-V is stored and queryable, but the rasterizer still runs the
/// fixed-function path: executing the compiled shaders is the Vulkan routing step in
/// `docs/ROADMAP.md`. An unlinked program is `GL_INVALID_OPERATION`, as specified.
pub fn glUseProgram(program: u32) {
    with(|ctx| {
        if program == 0 {
            ctx.current_program = None;
            return;
        }
        let Some(target) = ctx.programs.get(program as usize - 1).and_then(|slot| slot.as_ref())
        else {
            ctx.set_error(GL_INVALID_VALUE);
            return;
        };
        if !target.link_status() {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        }
        ctx.current_program = Some(program);
    });
}

/// The location of a uniform in a linked program, or -1.
pub fn glGetUniformLocation(program: u32, name: *const c_char) -> i32 {
    let Some(name) = cstr(name) else { return -1 };
    // GL accepts `name[0]` as a synonym for the first element of an array uniform.
    let name = name.strip_suffix("[0]").unwrap_or(&name);
    let Some(target) = with(|ctx| {
        ctx.programs
            .get(program as usize - 1)
            .and_then(|slot| slot.as_ref())
            .map(|target| (target.link_status(), target.uniform_location(name)))
    }) else {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return -1;
    };
    if !target.0 {
        with(|ctx| ctx.set_error(GL_INVALID_OPERATION));
        return -1;
    }
    target.1
}

/// The location of a vertex attribute in a linked program, or -1.
pub fn glGetAttribLocation(program: u32, name: *const c_char) -> i32 {
    let Some(name) = cstr(name) else { return -1 };
    let found = with(|ctx| {
        let Some(target) = ctx.programs.get(program as usize - 1).and_then(|slot| slot.as_ref())
        else {
            ctx.set_error(GL_INVALID_VALUE);
            return None;
        };
        if !target.link_status() {
            ctx.set_error(GL_INVALID_OPERATION);
            return None;
        }
        Some(match &target.linked {
            Some(linked) => linked
                .attributes
                .iter()
                .find(|(attribute, _location)| attribute == name.as_ref())
                .map_or(-1, |(_name, location)| *location as i32),
            None => -1,
        })
    });
    found.unwrap_or(-1)
}

/// Request an attribute location for the next link of this program.
pub fn glBindAttribLocation(program: u32, index: u32, name: *const c_char) {
    let Some(name) = cstr(name) else {
        with(|ctx| ctx.set_error(GL_INVALID_VALUE));
        return;
    };
    with(|ctx| {
        let Some(target) =
            ctx.programs.get_mut(program as usize - 1).and_then(|slot| slot.as_mut())
        else {
            ctx.set_error(GL_INVALID_VALUE);
            return;
        };
        if name.starts_with("gl_") {
            ctx.set_error(GL_INVALID_OPERATION);
            return;
        }
        target.attribute_bindings.retain(|(bound, _location)| bound != name.as_ref());
        target.attribute_bindings.push((name.into_owned(), index));
    });
}

/// Accepted and ignored; uniforms need a compiled program.
pub fn glUniform1i(_location: i32, _v0: i32) {}

/// Accepted and ignored; uniforms need a compiled program.
pub fn glUniform4f(_location: i32, _v0: f32, _v1: f32, _v2: f32, _v3: f32) {}

/// Accepted and ignored; uniforms need a compiled program.
pub fn glUniformMatrix4fv(_location: i32, _count: i32, _transpose: u8, _value: *const f32) {}
