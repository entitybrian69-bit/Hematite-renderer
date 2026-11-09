//! Routing draws through the Vulkan backend instead of the software rasterizer.
//!
//! The rasterizer in [`crate::raster`] stays the reference implementation: the frontend's
//! tests compare against it, and it runs whenever the device path would not produce the same
//! pixels. This module decides, per draw, which of the two runs, and it is deliberately
//! conservative — a draw that would not come out the same is left to the CPU.
//!
//! ## What is routed
//!
//! A draw goes to the device when all of these hold:
//!
//! * the mode is `GL_TRIANGLES`, `GL_TRIANGLE_STRIP`, `GL_TRIANGLE_FAN` or `GL_QUADS`
//!   (lines and points are shaded differently by the rasterizer and are not reproduced);
//! * the depth test is off, because the device resolves depth inside its own render pass and
//!   does not read or write the frontend's depth buffer;
//! * texturing is off, because GL textures are not uploaded to the device yet;
//! * face culling is off, because the pipeline is built with `CULL_MODE_NONE`;
//! * the viewport is the whole framebuffer, because the pipeline's viewport is baked in and
//!   sized to the target;
//! * blending is off and every vertex alpha is 1.0, so that compositing the device result into
//!   the framebuffer reproduces the host's colour writes exactly;
//! * `HEMATITE_VULKAN` is not `off`.
//!
//! ## How a device frame maps onto the framebuffer
//!
//! The first routable draw opens a device frame cleared to transparent black. Later routable
//! draws are recorded into the same frame while their host-side state matches, so a run of
//! draws costs one submission and one readback instead of one each. The frame is closed and
//! composited into the framebuffer as soon as anything else needs those pixels: a software
//! draw, `glClear`, `glReadPixels`, a resize, or a context reset. That is [`flush_pending`],
//! and it is a no-op when nothing is open.
//!
//! Compositing skips pixels whose alpha is zero, which is how "the draws did not cover this
//! pixel" is distinguished from "the draws wrote black" — the fragment stage forces alpha to
//! 1.0, which is exactly the vertex alpha the eligibility rules require.
//!
//! Blending and the colour mask stay on the host: there is one implementation of those rules
//! rather than two that can drift, and it is the one the tests already exercise.
//!
//! ## What is not
//!
//! Blended, textured, depth-tested, culled, sub-viewport and non-triangle draws still run on
//! the CPU, and so does every draw while a pipeline is being built on the worker thread. The
//! rings, the pipeline cache and the async builds are the batched design; the pieces that keep
//! more of Minecraft on the device are texture upload, a depth attachment and framebuffer
//! objects, in that order.

use crate::context::{Context, Vertex};
use crate::gl_types::*;
use hematite_fixed_function::matrix::Mat4;
use hematite_vk_backend::session::{
    PipelineState, Session, ShaderStages, Uniforms, VertexAttribute, VertexLayout,
};
use hematite_vk_backend::types::{Format, PrimitiveTopology};
use std::cell::RefCell;

/// Bytes of one uniform block; must match `hematite_vk_backend::session::UNIFORM_BLOCK_BYTES`.
/// Asserted against the backend by a test, so a change there fails here rather than drawing a
/// misaligned matrix.
pub const UNIFORM_BLOCK_BYTES: usize = 608;
/// Offset of the modelview-projection matrix inside a block.
pub const MVP_OFFSET: usize = 32;
/// Vertex layout: `vec2 position` at 0, `vec4 colour` at 8, 24 bytes per vertex.
const VERTEX_STRIDE: usize = 24;

/// The fixed-function stages, written against the uniform block the session fills in.
///
/// Both stages declare the same block, because a driver is entitled to assume they agree.
const FIXED_VERTEX: &str = "#version 450 core
layout(std140, binding = 0) uniform HematiteFrame {
    vec4 lightModelProduct;
    vec4 fog;
    mat4 mvp;
    vec4 u0;
    vec4 u1;
} hematiteFrame;
layout(location = 0) in vec2 inPosition;
layout(location = 1) in vec4 inColor;
layout(location = 1) out vec4 color;
void main() {
    color = inColor;
    gl_Position = hematiteFrame.mvp * vec4(inPosition, 0.0, 1.0);
}
";
const FIXED_FRAGMENT: &str = "#version 450 core
layout(location = 1) in vec4 color;
layout(location = 0) out vec4 outColor;
void main() {
    outColor = vec4(color.rgb, 1.0);
}
";

thread_local! {
    static STATE: RefCell<Option<Routing>> = const { RefCell::new(None) };
}

/// Per-thread Vulkan routing state. One context per thread, like the rest of the frontend.
#[derive(Default)]
struct Routing {
    /// `None` until the first routed draw, so loading the library never opens a driver.
    session: Option<Session>,
    /// Why no session exists, once an attempt has failed.
    unavailable: Option<String>,
    stages: Option<ShaderStages>,
    /// A device frame with recorded draws, not yet composited.
    open: Option<Batch>,
    routed: u64,
    fallbacks: u64,
    batches: u64,
}

/// Host-side state that the composite step depends on. Two draws can share one device frame
/// only when compositing the combined result once equals compositing each of them in turn,
/// which holds for opaque draws with the same mask and scissor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct BatchKey {
    width: usize,
    height: usize,
    color_mask: [bool; 4],
    scissor: Option<[i32; 4]>,
}

/// An open device frame and the host state it was recorded with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Batch {
    key: BatchKey,
}

impl BatchKey {
    fn of(ctx: &Context) -> Self {
        Self {
            width: ctx.framebuffer.width,
            height: ctx.framebuffer.height,
            color_mask: ctx.color_mask,
            scissor: if ctx.scissor_test { Some(ctx.scissor) } else { None },
        }
    }
}

thread_local! {
    /// A per-thread override of the environment switch, so tests can compare the two paths
    /// without racing on a process-wide variable.
    static OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether this thread's draws may go to the device at all.
fn wanted() -> bool {
    OVERRIDE.with(|enabled| enabled.get()).unwrap_or_else(|| {
        std::env::var("HEMATITE_VULKAN").map(|value| value != "off").unwrap_or(true)
    })
}

/// Force the switch for this thread, ignoring `HEMATITE_VULKAN`.
#[cfg(test)]
fn set_enabled(enabled: bool) {
    OVERRIDE.with(|slot| slot.set(Some(enabled)));
}

/// How many draws this thread sent to the device, how many fell back, and how many device
/// frames were opened. Exposed for tests and diagnostics; these are the evidence that routing
/// happened at all.
pub fn statistics() -> (u64, u64, u64) {
    STATE.with(|state| {
        let state = state.borrow();
        match state.as_ref() {
            Some(routing) => (routing.routed, routing.fallbacks, routing.batches),
            None => (0, 0, 0),
        }
    })
}

/// Why the device is unavailable on this thread, if it was tried and failed.
pub fn unavailable_reason() -> Option<String> {
    STATE.with(|state| state.borrow().as_ref().and_then(|routing| routing.unavailable.clone()))
}

/// Forget the session, so the next draw opens a fresh one. Tests use this to compare a routed
/// frame with a software one in the same process.
pub fn reset() {
    STATE.with(|state| *state.borrow_mut() = None);
}

/// Whether this draw can go to the device without changing what the framebuffer ends up
/// holding.
fn eligible(mode: u32, ctx: &Context, vertices: &[Vertex]) -> bool {
    if !wanted()
        || vertices.len() < 3
        || ctx.depth_test
        || ctx.texture_2d
        || ctx.cull_face
        || ctx.blend
        || !ctx.color_mask.iter().all(|&channel| channel)
    {
        return false;
    }
    let [vx, vy, vw, vh] = ctx.viewport;
    if vx != 0
        || vy != 0
        || vw as usize != ctx.framebuffer.width
        || vh as usize != ctx.framebuffer.height
    {
        return false;
    }
    // The fragment stage forces alpha to 1.0 so that a covered pixel is distinguishable from
    // an uncovered one; that is only what the host would have written when the vertex alpha is
    // 1.0 everywhere, so anything else falls back.
    vertices.iter().all(|vertex| vertex.color[3] == 1.0)
        && matches!(mode, GL_TRIANGLES | GL_TRIANGLE_STRIP | GL_TRIANGLE_FAN | GL_QUADS)
}

/// Try to route one draw to the device. Returns true when the caller is done with it.
///
/// When this returns false nothing was written to the framebuffer and any open device frame has
/// already been composited, so the caller can rasterize into it safely.
pub fn draw(ctx: &mut Context, mode: u32, vertices: &[Vertex]) -> bool {
    if !eligible(mode, ctx, vertices) {
        flush_pending(ctx);
        return false;
    }
    let key = BatchKey::of(ctx);
    let outcome = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let routing = state.get_or_insert_with(Routing::default);
        // A draw whose composite state differs cannot share the open frame, because one
        // composite could not stand in for two.
        if routing.open.map(|batch| batch.key != key).unwrap_or(false) {
            close(routing, ctx);
        }
        route(routing, ctx, mode, vertices, key)
    });
    match outcome {
        Ok(true) => {
            ctx.diagnostics.draw_calls += 1;
            ctx.diagnostics.routed_draws += 1;
            true
        }
        Ok(false) => {
            // The pipeline is still building, or the driver refused this state. Nothing was
            // written, so the caller rasterizes.
            ctx.diagnostics.rasterizer_fallbacks += 1;
            flush_pending(ctx);
            false
        }
        Err(reason) => {
            // The device is unusable from here on: remember why once, then stop trying.
            ctx.diagnostics.rasterizer_fallbacks += 1;
            STATE.with(|state| {
                if let Some(routing) = state.borrow_mut().as_mut() {
                    routing.unavailable.get_or_insert(reason);
                }
            });
            flush_pending(ctx);
            false
        }
    }
}

/// Composite and close an open device frame, if there is one.
///
/// Call before anything reads or writes `ctx.framebuffer`: a software draw, `glClear`,
/// `glReadPixels`, a resize, a context reset.
pub fn flush_pending(ctx: &mut Context) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if let Some(routing) = state.as_mut() {
            if routing.open.is_some() {
                close(routing, ctx);
            }
        }
    });
}

/// Read the open device frame back and composite it into the framebuffer.
fn close(routing: &mut Routing, ctx: &mut Context) {
    let Some(batch) = routing.open.take() else { return };
    let Some(session) = routing.session.as_mut() else { return };
    match session.flush() {
        Ok(pixels) => composite(ctx, &pixels, batch.key),
        Err(error) => {
            let reason = format!("cannot read a Vulkan frame back: {error}");
            routing.unavailable.get_or_insert(reason);
        }
    }
}

/// Record one draw into the open device frame, opening one if needed.
fn route(
    routing: &mut Routing,
    ctx: &Context,
    mode: u32,
    vertices: &[Vertex],
    key: BatchKey,
) -> Result<bool, String> {
    let width = ctx.framebuffer.width as u32;
    let height = ctx.framebuffer.height as u32;
    if width == 0 || height == 0 {
        return Ok(false);
    }
    if routing.session.is_none() {
        // A session without a sampler: a draw that needs one is never routed.
        let loader = std::env::var_os("HEMATITE_VULKAN_LIBRARY").map(std::path::PathBuf::from);
        routing.session =
            Some(Session::new(loader.as_deref(), false).map_err(|error| format!("{error}"))?);
    }
    let session = routing.session.as_mut().expect("the session was just created");
    if routing.stages.is_none() {
        let compile = |stage: hematite_glsl::Stage, source: &str| {
            hematite_glsl::compile(&hematite_glsl::Request::new(stage, source))
                .map(|shader| shader.spirv)
                .map_err(|error| format!("the fixed-function stage does not compile: {error}"))
        };
        routing.stages = Some(ShaderStages {
            vertex: compile(hematite_glsl::Stage::Vertex, FIXED_VERTEX)?,
            fragment: compile(hematite_glsl::Stage::Fragment, FIXED_FRAGMENT)?,
        });
    }
    let stages = routing.stages.clone().expect("the stages were just compiled");
    let layout = vertex_layout().ok_or("the fixed-function vertex layout is not supported")?;
    let bytes = interleave(vertices);
    let state = PipelineState {
        viewport: [width, height],
        // The scissor origin flips: GL counts rows from the bottom, Vulkan from the top.
        scissor: key.scissor.map(|[x, y, w, h]| [x, height as i32 - (y + h), w.max(0), h.max(0)]),
        ..PipelineState::opaque(width, height)
    };
    let uniforms = Uniforms { state: [0.0; 8], mvp: mvp_columns(&ctx.mvp()), uniforms: Vec::new() };

    let opened = routing.open.is_none();
    if opened {
        session
            .begin(width, height, [0.0, 0.0, 0.0, 0.0])
            .map_err(|error| format!("cannot begin a Vulkan frame: {error}"))?;
        routing.open = Some(Batch { key });
    }
    let topology = match mode {
        GL_QUADS => PrimitiveTopology::TRIANGLE_LIST,
        GL_TRIANGLE_STRIP => PrimitiveTopology::TRIANGLE_STRIP,
        GL_TRIANGLE_FAN => PrimitiveTopology::TRIANGLE_FAN,
        _ => PrimitiveTopology::TRIANGLE_LIST,
    };
    let (topology, bytes) = if mode == GL_QUADS {
        // Vulkan has no quad topology, so quads become two triangles each here, the same way
        // the rasterizer expands them.
        (topology, quads_to_triangles(&bytes))
    } else {
        (topology, bytes)
    };
    let recorded = session
        .record_draw(&stages, layout, state, topology, &bytes, &uniforms)
        .map_err(|error| format!("cannot record a Vulkan draw: {error}"))?;
    if recorded {
        routing.routed += 1;
        if opened {
            // Only a frame that actually holds a draw counts; one opened and abandoned
            // because the pipeline was still building would inflate the number.
            routing.batches += 1;
        }
    } else {
        routing.fallbacks += 1;
    }
    Ok(recorded)
}

/// `vec2 position` then `vec4 colour`, tightly packed.
fn vertex_layout() -> Option<VertexLayout> {
    let layout = VertexLayout::packed(&[
        VertexAttribute { location: 0, format: Format::R32G32_SFLOAT, offset: 0 },
        VertexAttribute { location: 1, format: Format::R32G32B32A32_SFLOAT, offset: 0 },
    ])?;
    debug_assert_eq!(layout.stride as usize, VERTEX_STRIDE);
    Some(layout)
}

/// Pack the frontend's vertices into the interleaved layout above, dropping the components the
/// fixed-function stages do not read.
fn interleave(vertices: &[Vertex]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vertices.len() * VERTEX_STRIDE);
    for vertex in vertices {
        for value in [vertex.pos[0], vertex.pos[1]] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for value in vertex.color {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    bytes
}

/// Re-index packed 24-byte quads as triangle pairs: `v0 v1 v2` and `v2 v3 v0`.
fn quads_to_triangles(quads: &[u8]) -> Vec<u8> {
    let mut triangles = Vec::with_capacity(quads.len() / 2);
    for quad in quads.chunks_exact(VERTEX_STRIDE * 4) {
        let at = |index: usize| &quad[index * VERTEX_STRIDE..(index + 1) * VERTEX_STRIDE];
        for triangle in [[0usize, 1, 2], [2, 3, 0]] {
            for index in triangle {
                triangles.extend_from_slice(at(index));
            }
        }
    }
    triangles
}

/// Composite one device frame into the framebuffer.
///
/// The rows are not flipped, and the reason is worth writing down because it looks wrong. The
/// frontend's window-space row is `(ndc_y / 2 + 0.5) * height`, counting up from the bottom
/// because that is what `glReadPixels` reports. Vulkan's viewport transform is
/// `y_f = (ndc_y / 2 + 0.5) * height` too, counting down from the top of the image. The two
/// disagree about which way the axis points and agree about which *index* a given `ndc_y`
/// lands on, so the readback and the framebuffer line up row for row with no transformation.
/// The routing test asserts this against the rasterizer for a scene whose primitives sit at
/// both ends of the frame.
fn composite(ctx: &mut Context, pixels: &[u8], key: BatchKey) {
    let width = ctx.framebuffer.width;
    let height = ctx.framebuffer.height;
    if pixels.len() < width * height * 4 || key.width != width || key.height != height {
        return;
    }
    for y in 0..height {
        let source_row = y * width;
        for x in 0..width {
            let source = (source_row + x) * 4;
            if pixels[source + 3] == 0 {
                // Transparent means no draw in this frame covered the pixel, so the stored
                // value stays.
                continue;
            }
            let shaded = [
                f32::from(pixels[source]) / 255.0,
                f32::from(pixels[source + 1]) / 255.0,
                f32::from(pixels[source + 2]) / 255.0,
                1.0,
            ];
            let destination = y * width + x;
            // Blending is off for every routed draw, so the stored colour is replaced
            // wholesale; the mask is honoured because a draw may have disabled a channel.
            for (channel, value) in shaded.iter().enumerate() {
                if ctx.color_mask[channel] {
                    ctx.framebuffer.color[destination * 4 + channel] =
                        (value.clamp(0.0, 1.0) * 255.0).round() as u8;
                }
            }
            ctx.diagnostics.fragments_written += 1;
        }
    }
}

/// The modelview-projection matrix in the column-major order the uniform block declares.
fn mvp_columns(matrix: &Mat4) -> [f32; 16] {
    std::array::from_fn(|index| matrix.0[index] as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One scene, so the routed and the rasterized frames can be compared pixel by pixel.
    ///
    /// Three draws in a row: one triangle, one strip and one quad. Nothing here changes state
    /// between them, so all three must land in a single device frame. The vertices avoid
    /// half-pixel positions so that no pixel centre sits exactly on an edge, which is the one
    /// case where Vulkan's top-left fill rule and the rasterizer's `w >= 0` test disagree.
    fn scene(width: usize, height: usize) -> Vec<u8> {
        assert!(crate::reset_context(width, height));
        crate::api::glClearColor(0.25, 0.5, 0.75, 1.0);
        crate::api::glClear(GL_COLOR_BUFFER_BIT);

        crate::api::glBegin(GL_TRIANGLES);
        crate::api::glColor3f(1.0, 0.0, 0.0);
        crate::api::glVertex2f(-0.9, -0.8);
        crate::api::glColor3f(0.0, 1.0, 0.0);
        crate::api::glVertex2f(0.1, -0.7);
        crate::api::glColor3f(0.0, 0.0, 1.0);
        crate::api::glVertex2f(-0.4, 0.3);
        crate::api::glEnd();

        crate::api::glBegin(GL_TRIANGLE_STRIP);
        crate::api::glColor3f(1.0, 1.0, 0.0);
        crate::api::glVertex2f(0.2, 0.1);
        crate::api::glColor3f(0.0, 1.0, 1.0);
        crate::api::glVertex2f(0.9, 0.2);
        crate::api::glColor3f(1.0, 0.0, 1.0);
        crate::api::glVertex2f(0.3, 0.9);
        crate::api::glColor3f(0.5, 0.5, 0.5);
        crate::api::glVertex2f(0.8, 0.8);
        crate::api::glEnd();

        crate::api::glBegin(GL_QUADS);
        crate::api::glColor3f(1.0, 1.0, 1.0);
        crate::api::glVertex2f(-0.8, 0.4);
        crate::api::glVertex2f(-0.3, 0.4);
        crate::api::glVertex2f(-0.3, 0.85);
        crate::api::glVertex2f(-0.8, 0.85);
        crate::api::glEnd();

        crate::framebuffer_rgba(width, height).expect("the scene renders")
    }

    /// The routed frame must match the rasterized one, and the draws must have gone to the
    /// device: a frame that silently fell back would pass a pixel comparison and prove nothing.
    ///
    /// Needs a Vulkan driver, so it is ignored and run by `./build.sh gpu`.
    #[test]
    #[ignore = "needs a Vulkan driver; run ./build.sh gpu"]
    fn routed_draws_match_the_rasterizer() {
        let (width, height) = (32, 24);

        reset();
        set_enabled(false);
        let rasterized = scene(width, height);
        assert_eq!(statistics(), (0, 0, 0), "nothing is routed while routing is off");

        reset();
        set_enabled(true);
        // The pipeline is built on a worker thread, so the first draws of a new state are
        // rasterized on the CPU while it compiles. Repeat the scene until the device path has
        // taken over: that is what an application sees across its first frames.
        let mut warmup = 0;
        while statistics().0 < 3 && warmup < 64 {
            warmup += 1;
            let _ = scene(width, height);
        }
        assert_eq!(unavailable_reason(), None, "the device session stayed usable");
        assert!(statistics().0 >= 3, "the device path took over after {warmup} scenes");
        let warmup_draws = statistics().0;

        // One more scene with a warm pipeline cache, to measure how that one is recorded.
        let (before, _, batches_before) = statistics();
        let routed = scene(width, height);
        let (draws, fallbacks, batches) = statistics();
        assert_eq!(draws - before, 3, "all three draws of one scene went to the device");
        assert_eq!(
            batches - batches_before,
            1,
            "one device frame for the whole scene, got {}",
            batches - batches_before
        );
        assert!(
            fallbacks >= warmup_draws / 3,
            "the draws that ran before the pipeline landed fell back, got {fallbacks}"
        );

        let mut differing = 0usize;
        let mut pixels_differing = 0usize;
        for pixel in 0..width * height {
            let offset = pixel * 4;
            let mut pixel_differs = false;
            for channel in 0..4 {
                let difference =
                    i32::from(routed[offset + channel]) - i32::from(rasterized[offset + channel]);
                if difference.abs() > 1 {
                    differing += 1;
                    pixel_differs = true;
                }
            }
            if pixel_differs {
                pixels_differing += 1;
                if pixels_differing <= 4 {
                    println!(
                        "  pixel ({}, {}): device {:?}, rasterizer {:?}",
                        pixel % width,
                        pixel / width,
                        &routed[offset..offset + 4],
                        &rasterized[offset..offset + 4],
                    );
                }
            }
        }
        let took_over_after = format!("{warmup} scene(s)");
        println!(
            "  {took_over_after} before the device took over; {pixels_differing} pixels differ \
             from the rasterizer across {differing} channels"
        );
        // The two rasterizers sample pixel centres with the same barycentric test, so they
        // disagree only where a centre lands on an edge, where f32 edge functions and
        // Vulkan's top-left fill rule can pick opposite sides. Everything else must match, and
        // the whole picture matching in extent is what pins down the row order, the matrix and
        // the interpolation all at once.
        assert!(
            pixels_differing <= (width * height) / 100 + 1,
            "{pixels_differing} pixels differ from the rasterizer"
        );
        // A pixel deep inside the first triangle must match exactly: no coverage rule is
        // involved there, only the vertex transform, the interpolation and the block offset.
        let inside = (5 * width + 10) * 4;
        assert_eq!(
            &routed[inside..inside + 4],
            &rasterized[inside..inside + 4],
            "the interior of the triangle is shaded identically"
        );
        let clear = [64u8, 128, 191, 255];
        assert_eq!(&routed[..4], &clear[..], "the clear colour survives the device frame");
    }

    /// A draw the device cannot reproduce must still come out right, through the rasterizer.
    ///
    /// Depth-tested, blended and textured draws are all outside the routed set; this covers the
    /// state transition in both directions, since the triangle before and after must land in
    /// separate device frames.
    #[test]
    #[ignore = "needs a Vulkan driver; run ./build.sh gpu"]
    fn draws_the_device_cannot_reproduce_still_render() {
        let (width, height) = (16, 16);
        reset();
        set_enabled(true);
        assert!(crate::reset_context(width, height));
        crate::api::glClearColor(0.0, 0.0, 0.0, 1.0);
        crate::api::glClear(GL_COLOR_BUFFER_BIT);

        crate::api::glEnable(GL_DEPTH_TEST);
        crate::api::glBegin(GL_TRIANGLES);
        crate::api::glColor3f(0.0, 1.0, 0.0);
        crate::api::glVertex2f(-1.0, -1.0);
        crate::api::glVertex2f(1.0, -1.0);
        crate::api::glVertex2f(0.0, 1.0);
        crate::api::glEnd();
        crate::api::glDisable(GL_DEPTH_TEST);

        let pixels = crate::framebuffer_rgba(width, height).expect("the scene renders");
        let centre = (8 * width + 8) * 4;
        assert_eq!(
            &pixels[centre..centre + 4],
            &[0, 255, 0, 255],
            "a depth-tested draw is rasterized on the CPU and still lands"
        );
        assert_eq!(statistics().0, 0, "a depth-tested draw is never routed");
    }

    /// The uniform block this module writes into must stay the size and shape the backend
    /// allocates, or every matrix would be read from the wrong offset.
    #[test]
    fn the_uniform_block_layout_agrees_with_the_backend() {
        assert_eq!(UNIFORM_BLOCK_BYTES, hematite_vk_backend::session::UNIFORM_BLOCK_BYTES as usize);
        assert_eq!(MVP_OFFSET, hematite_vk_backend::session::UNIFORM_MVP_OFFSET as usize);
    }
}
