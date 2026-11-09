//! GL and EGL enumerants used by the frontend.
//!
//! Only the values the implementation actually branches on are declared here; the full
//! set of entry points comes from `abi/gl_export.def` via `cargo run -p xtask -- gen-abi`.

#![allow(clippy::identity_op, clippy::eq_op)]

/// Boolean false, as GL spells it.
pub const GL_FALSE: u8 = 0;
/// Boolean true, as GL spells it.
pub const GL_TRUE: u8 = 1;

// Primitives.
/// Points.
pub const GL_POINTS: u32 = 0x0000;
/// Line list.
pub const GL_LINES: u32 = 0x0001;
/// Closed line loop.
pub const GL_LINE_LOOP: u32 = 0x0002;
/// Line strip.
pub const GL_LINE_STRIP: u32 = 0x0003;
/// Triangle list.
pub const GL_TRIANGLES: u32 = 0x0004;
/// Triangle strip.
pub const GL_TRIANGLE_STRIP: u32 = 0x0005;
/// Triangle fan.
pub const GL_TRIANGLE_FAN: u32 = 0x0006;
/// Quad list (desktop GL only).
pub const GL_QUADS: u32 = 0x0007;

// Clear bits.
/// Depth buffer bit.
pub const GL_DEPTH_BUFFER_BIT: u32 = 0x0000_0100;
/// Stencil buffer bit.
pub const GL_STENCIL_BUFFER_BIT: u32 = 0x0000_0400;
/// Colour buffer bit.
pub const GL_COLOR_BUFFER_BIT: u32 = 0x0000_4000;

// Errors.
/// No error pending.
pub const GL_NO_ERROR: u32 = 0;
/// An enumerant was not recognised.
pub const GL_INVALID_ENUM: u32 = 0x0500;
/// A value was out of range.
pub const GL_INVALID_VALUE: u32 = 0x0501;
/// The call is illegal in the current state.
pub const GL_INVALID_OPERATION: u32 = 0x0502;
/// A matrix stack was pushed past its depth.
pub const GL_STACK_OVERFLOW: u32 = 0x0503;
/// A matrix stack was popped past its base.
pub const GL_STACK_UNDERFLOW: u32 = 0x0504;
/// Allocation failed.
pub const GL_OUT_OF_MEMORY: u32 = 0x0505;

// Depth functions.
/// Never pass.
pub const GL_NEVER: u32 = 0x0200;
/// Pass if incoming < stored.
pub const GL_LESS: u32 = 0x0201;
/// Pass if equal.
pub const GL_EQUAL: u32 = 0x0202;
/// Pass if incoming <= stored.
pub const GL_LEQUAL: u32 = 0x0203;
/// Pass if incoming > stored.
pub const GL_GREATER: u32 = 0x0204;
/// Pass if different.
pub const GL_NOTEQUAL: u32 = 0x0205;
/// Pass if incoming >= stored.
pub const GL_GEQUAL: u32 = 0x0206;
/// Always pass.
pub const GL_ALWAYS: u32 = 0x0207;

// Blend factors.
/// Zero.
pub const GL_ZERO: u32 = 0;
/// One.
pub const GL_ONE: u32 = 1;
/// Source colour.
pub const GL_SRC_COLOR: u32 = 0x0300;
/// One minus source colour.
pub const GL_ONE_MINUS_SRC_COLOR: u32 = 0x0301;
/// Source alpha.
pub const GL_SRC_ALPHA: u32 = 0x0302;
/// One minus source alpha.
pub const GL_ONE_MINUS_SRC_ALPHA: u32 = 0x0303;
/// Destination alpha.
pub const GL_DST_ALPHA: u32 = 0x0304;
/// One minus destination alpha.
pub const GL_ONE_MINUS_DST_ALPHA: u32 = 0x0305;
/// Destination colour.
pub const GL_DST_COLOR: u32 = 0x0306;
/// One minus destination colour.
pub const GL_ONE_MINUS_DST_COLOR: u32 = 0x0307;

// Capabilities.
/// Depth test.
pub const GL_DEPTH_TEST: u32 = 0x0B71;
/// Blending.
pub const GL_BLEND: u32 = 0x0BE2;
/// Scissor test.
pub const GL_SCISSOR_TEST: u32 = 0x0C11;
/// Face culling.
pub const GL_CULL_FACE: u32 = 0x0B44;
/// 2D texturing.
pub const GL_TEXTURE_2D: u32 = 0x0DE1;

// Faces and winding.
/// Front faces.
pub const GL_FRONT: u32 = 0x0404;
/// Back faces.
pub const GL_BACK: u32 = 0x0405;
/// Both faces.
pub const GL_FRONT_AND_BACK: u32 = 0x0408;
/// Clockwise winding.
pub const GL_CW: u32 = 0x0900;
/// Counter-clockwise winding.
pub const GL_CCW: u32 = 0x0901;

// Matrix modes and queries.
/// Modelview stack.
pub const GL_MODELVIEW: u32 = 0x1700;
/// Projection stack.
pub const GL_PROJECTION: u32 = 0x1701;
/// Texture stack.
pub const GL_TEXTURE: u32 = 0x1702;
/// Current modelview matrix.
pub const GL_MODELVIEW_MATRIX: u32 = 0x0BA6;
/// Current projection matrix.
pub const GL_PROJECTION_MATRIX: u32 = 0x0BA7;
/// Current texture matrix.
pub const GL_TEXTURE_MATRIX: u32 = 0x0BA8;
/// Current viewport.
pub const GL_VIEWPORT: u32 = 0x0BA2;
/// Current scissor box.
pub const GL_SCISSOR_BOX: u32 = 0x0C10;
/// Largest supported texture edge.
pub const GL_MAX_TEXTURE_SIZE: u32 = 0x0D33;
/// Largest supported 3D texture edge.
pub const GL_MAX_3D_TEXTURE_SIZE: u32 = 0x8073;

// String queries.
/// Implementation vendor.
pub const GL_VENDOR: u32 = 0x1F00;
/// Renderer name.
pub const GL_RENDERER: u32 = 0x1F01;
/// GL version string.
pub const GL_VERSION: u32 = 0x1F02;
/// Extension string.
pub const GL_EXTENSIONS: u32 = 0x1F03;
/// Shading language version string.
pub const GL_SHADING_LANGUAGE_VERSION: u32 = 0x8B8C;

// Buffers.
/// Vertex data buffer.
pub const GL_ARRAY_BUFFER: u32 = 0x8892;
/// Index buffer.
pub const GL_ELEMENT_ARRAY_BUFFER: u32 = 0x8893;
/// Static draw usage.
pub const GL_STATIC_DRAW: u32 = 0x88E4;
/// Dynamic draw usage.
pub const GL_DYNAMIC_DRAW: u32 = 0x88E8;
/// Stream draw usage.
pub const GL_STREAM_DRAW: u32 = 0x88E0;

// Attribute component types.
/// Signed byte.
pub const GL_BYTE: u32 = 0x1400;
/// Unsigned byte.
pub const GL_UNSIGNED_BYTE: u32 = 0x1401;
/// Signed short.
pub const GL_SHORT: u32 = 0x1402;
/// Unsigned short.
pub const GL_UNSIGNED_SHORT: u32 = 0x1403;
/// Signed int.
pub const GL_INT: u32 = 0x1404;
/// Unsigned int.
pub const GL_UNSIGNED_INT: u32 = 0x1405;
/// Float.
pub const GL_FLOAT: u32 = 0x1406;
/// 16.16 fixed point.
pub const GL_FIXED: u32 = 0x140C;

// Client state arrays (GLES1).
/// Position array.
pub const GL_VERTEX_ARRAY: u32 = 0x8074;
/// Normal array.
pub const GL_NORMAL_ARRAY: u32 = 0x8075;
/// Colour array.
pub const GL_COLOR_ARRAY: u32 = 0x8076;
/// Texture coordinate array.
pub const GL_TEXTURE_COORD_ARRAY: u32 = 0x8078;

// Pixel formats.
/// Single alpha channel.
pub const GL_ALPHA: u32 = 0x1906;
/// Single luminance channel.
pub const GL_LUMINANCE: u32 = 0x1909;
/// Luminance and alpha.
pub const GL_LUMINANCE_ALPHA: u32 = 0x190A;
/// Three byte colour.
pub const GL_RGB: u32 = 0x1907;
/// Four byte colour.
pub const GL_RGBA: u32 = 0x1908;

// Textures.
/// Unit 0.
pub const GL_TEXTURE0: u32 = 0x84C0;
/// Magnification filter.
pub const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
/// Minification filter.
pub const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
/// Horizontal wrap mode.
pub const GL_TEXTURE_WRAP_S: u32 = 0x2802;
/// Vertical wrap mode.
pub const GL_TEXTURE_WRAP_T: u32 = 0x2803;
/// Point sampling.
pub const GL_NEAREST: u32 = 0x2600;
/// Bilinear sampling.
pub const GL_LINEAR: u32 = 0x2601;
/// Clamp to the edge texel centre.
pub const GL_CLAMP_TO_EDGE: u32 = 0x812F;
/// Repeat the image.
pub const GL_REPEAT: u32 = 0x2901;
/// Unpacking alignment.
pub const GL_UNPACK_ALIGNMENT: u32 = 0x0CF5;
/// Packing alignment.
pub const GL_PACK_ALIGNMENT: u32 = 0x0D05;

// Shaders and programs.
/// Vertex shader stage.
pub const GL_VERTEX_SHADER: u32 = 0x8B31;
/// Fragment shader stage.
pub const GL_FRAGMENT_SHADER: u32 = 0x8B30;
/// Compile result.
pub const GL_COMPILE_STATUS: u32 = 0x8B81;
/// Link result.
pub const GL_LINK_STATUS: u32 = 0x8B82;
/// Info log length.
pub const GL_INFO_LOG_LENGTH: u32 = 0x8B84;

// EGL.
/// Success.
pub const EGL_SUCCESS: i32 = 0x3000;
/// Display not initialised.
pub const EGL_NOT_INITIALIZED: i32 = 0x3001;
/// Allocation failed.
pub const EGL_BAD_ALLOC: i32 = 0x3003;
/// Bad attribute list.
pub const EGL_BAD_ATTRIBUTE: i32 = 0x3004;
/// Bad config.
pub const EGL_BAD_CONFIG: i32 = 0x3005;
/// Bad context.
pub const EGL_BAD_CONTEXT: i32 = 0x3006;
/// Bad display handle.
pub const EGL_BAD_DISPLAY: i32 = 0x3008;
/// Bad native window handle.
pub const EGL_BAD_NATIVE_WINDOW: i32 = 0x300B;
/// Bad argument.
pub const EGL_BAD_PARAMETER: i32 = 0x300C;
/// Bad surface handle.
pub const EGL_BAD_SURFACE: i32 = 0x300D;
/// Boolean false.
pub const EGL_FALSE: u32 = 0;
/// Boolean true.
pub const EGL_TRUE: u32 = 1;
/// End of an attribute list.
pub const EGL_NONE: i32 = 0x3038;
/// Surface type attribute.
pub const EGL_SURFACE_TYPE: i32 = 0x3033;
/// Pbuffer surfaces.
pub const EGL_PBUFFER_BIT: i32 = 0x0001;
/// Window surfaces.
pub const EGL_WINDOW_BIT: i32 = 0x0004;
/// Renderable client API attribute.
pub const EGL_RENDERABLE_TYPE: i32 = 0x3040;
/// OpenGL ES 2 renderable.
pub const EGL_OPENGL_ES2_BIT: i32 = 0x0004;
/// Red channel size attribute.
pub const EGL_RED_SIZE: i32 = 0x3024;
/// Green channel size attribute.
pub const EGL_GREEN_SIZE: i32 = 0x3023;
/// Blue channel size attribute.
pub const EGL_BLUE_SIZE: i32 = 0x3022;
/// Alpha channel size attribute.
pub const EGL_ALPHA_SIZE: i32 = 0x3021;
/// Depth buffer size attribute.
pub const EGL_DEPTH_SIZE: i32 = 0x3025;
/// Stencil buffer size attribute.
pub const EGL_STENCIL_SIZE: i32 = 0x3026;
/// Framebuffer colour depth attribute.
pub const EGL_BUFFER_SIZE: i32 = 0x3020;
/// Surface width attribute.
pub const EGL_WIDTH: i32 = 0x3057;
/// Surface height attribute.
pub const EGL_HEIGHT: i32 = 0x3058;
/// Context client version attribute.
pub const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;
/// Vendor string.
pub const EGL_VENDOR: i32 = 0x3053;
/// Version string.
pub const EGL_VERSION: i32 = 0x3054;
/// Extension string.
pub const EGL_EXTENSIONS: i32 = 0x3055;
/// Client API query.
pub const EGL_CLIENT_APIS: i32 = 0x308D;
