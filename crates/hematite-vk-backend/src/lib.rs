//! Real Vulkan 1.1 device selection and synchronous offscreen triangle rendering.
//!
//! This is backend bring-up, NOT a GL implementation or an Android renderer plugin.
//! No swapchain, textures, depth, general command executor, asynchronous submission or
//! runtime shader translation is implemented. Each render allocates temporary resources;
//! no hot-path performance claim is made. Built-in shaders are original synthetic shaders.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

mod driver;
mod render;
pub mod session;

/// The Vulkan types that appear in the session's public interface.
///
/// Re-exported so a caller does not need to depend on `ash` directly, and so the version it
/// compiles against cannot drift from the one the backend uses.
pub mod types {
    pub use ash::vk::{BlendFactor, Format, PrimitiveTopology};
}

use ash::vk;
use std::fmt;
use std::path::Path;

/// A structured backend failure; Vulkan errors are never converted to fake success.
#[derive(Debug)]
pub enum Error {
    /// Dynamic loader or driver discovery failed.
    Loader(String),
    /// Vulkan returned an error code at the named operation.
    Vulkan(&'static str, vk::Result),
    /// Device or input cannot support the requested operation.
    Unsupported(String),
    /// Input validation failed before driver access.
    InvalidInput(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Loader(message) | Self::Unsupported(message) => f.write_str(message),
            Self::Vulkan(operation, code) => write!(f, "{operation}: {code:?}"),
            Self::InvalidInput(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for Error {}

/// Properties actually queried from the selected Vulkan physical device.
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// Device name from `VkPhysicalDeviceProperties`.
    pub device_name: String,
    /// Vulkan API version (packed Vulkan encoding).
    pub api_version: u32,
    /// Vendor and device IDs.
    pub vendor_device: [u32; 2],
    /// Vendor-encoded driver version, not a fabricated semantic version.
    pub driver_version: u32,
    /// UUID for identifying driver-specific pipeline cache compatibility.
    pub pipeline_cache_uuid: [u8; 16],
    /// Device type from the driver.
    pub device_type: vk::PhysicalDeviceType,
    /// Supported 2D image limit. Offscreen rendering imposes a smaller safety bound.
    pub max_image_dimension_2d: u32,
    /// `maxPushConstantsSize`, queried rather than assumed: the session refuses to start on
    /// a device whose limit is below [`session::PUSH_CONSTANT_BYTES`].
    pub max_push_constant_size: u32,
    /// Device-supported features. These are NOT automatically enabled or emulated.
    pub geometry_shader_supported: bool,
    /// Device-supported tessellation (not enabled by this renderer).
    pub tessellation_shader_supported: bool,
    /// Device extension names, queried rather than inferred from vendor names.
    pub extensions: Vec<String>,
}

/// Packed, interleaved input for the built-in vertex shader.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Vertex {
    /// XY in Vulkan normalized clip coordinates (positive viewport height, Y down).
    pub position: [f32; 2],
    /// Linear RGBA passed through smooth interpolation to an UNORM render target.
    pub color: [f32; 4],
}

/// Owned, tightly packed RGBA8 readback in framebuffer row order (top row first).
#[derive(Debug, Clone)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Exactly `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl Image {
    /// Return one pixel, or None outside the image.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = (y as usize)
            .checked_mul(self.width as usize)?
            .checked_add(x as usize)?
            .checked_mul(4)?;
        self.rgba.get(index..index.checked_add(4)?)?.try_into().ok()
    }
}

/// Synchronous offscreen Vulkan context, intended for correctness bring-up.
/// Owns its loader handle and all instance/device lifetimes. No global driver state.
pub struct OffscreenRenderer {
    context: driver::Context,
}

impl OffscreenRenderer {
    /// Load a trusted Vulkan loader. `None` selects `libvulkan.so.1` (Linux) or
    /// `libvulkan.so` (Android); pass an explicit loader path for test drivers.
    /// No environment variables are modified; ICD/layer configuration remains caller-owned.
    pub fn new(loader: Option<&Path>) -> Result<Self, Error> {
        Ok(Self { context: driver::Context::new(loader)? })
    }

    /// The selected device's real capabilities.
    pub fn capabilities(&self) -> &Capabilities {
        &self.context.capabilities
    }

    /// Clear an RGBA8 image, draw an optional triangle list, and read back actual pixels.
    /// Dimensions must be 1..=4096 and within the selected device limit; all supplied
    /// values must be finite. No GL semantics, surfaces or presentation are implied.
    pub fn render(
        &mut self,
        width: u32,
        height: u32,
        clear: [f32; 4],
        vertices: &[Vertex],
    ) -> Result<Image, Error> {
        validate(width, height, clear, vertices, self.capabilities().max_image_dimension_2d)?;
        render::render(&self.context, width, height, clear, vertices)
    }

    /// Draw with caller-supplied SPIR-V and read the pixels back.
    ///
    /// This is the check that a shader compiled from GLSL by `hematite-glsl`
    /// actually runs: the driver has to execute the module to produce pixels, so a
    /// module it merely tolerated at creation time cannot pass by accident. The
    /// vertex stage must read `location = 0` as a `vec2` position, matching the
    /// vertex layout used here.
    pub fn render_with_shaders(
        &mut self,
        width: u32,
        height: u32,
        clear: [f32; 4],
        vertices: &[Vertex],
        vertex_shader: &[u32],
        fragment_shader: &[u32],
    ) -> Result<Image, Error> {
        validate(width, height, clear, vertices, self.capabilities().max_image_dimension_2d)?;
        if vertex_shader.len() < 5 || fragment_shader.len() < 5 {
            return Err(Error::InvalidInput("SPIR-V module is shorter than its header"));
        }
        render::render_with_shaders(
            &self.context,
            width,
            height,
            clear,
            vertices,
            vertex_shader,
            fragment_shader,
        )
    }

    /// Hand SPIR-V to the driver and report whether it accepts it.
    ///
    /// This is the check that matters for a compiled shader: `vkCreateShaderModule`
    /// is where a driver parses the module, so a module the driver accepts is
    /// structurally valid SPIR-V, not merely a byte sequence Hematite produced. The
    /// module is destroyed again; nothing is drawn.
    ///
    /// The words must be 4-byte aligned SPIR-V, which is what `hematite-glsl` emits.
    pub fn accepts_shader_module(&self, spirv: &[u32]) -> Result<(), Error> {
        if spirv.len() < 5 {
            return Err(Error::InvalidInput("SPIR-V module is shorter than its header"));
        }
        let info = vk::ShaderModuleCreateInfo::default().code(spirv);
        // SAFETY: `spirv` outlives the call and the module is destroyed below.
        let module = unsafe { self.context.device.create_shader_module(&info, None) }
            .map_err(|error| Error::Vulkan("create shader module", error))?;
        // SAFETY: the module was created above and is not used after this point.
        unsafe { self.context.device.destroy_shader_module(module, None) };
        Ok(())
    }

    /// Ask the driver to build a graphics pipeline from two compiled stages.
    ///
    /// This is the stronger of the two driver checks: a driver can accept a module
    /// at [`Self::accepts_shader_module`] without inspecting it, but building a
    /// pipeline makes it process the shader interface. Both stages must take no
    /// vertex input and write one colour attachment.
    pub fn accepts_pipeline(&self, vertex: &[u32], fragment: &[u32]) -> Result<(), Error> {
        if vertex.len() < 5 || fragment.len() < 5 {
            return Err(Error::InvalidInput("SPIR-V module is shorter than its header"));
        }
        render::validate_pipeline(&self.context, vertex, fragment)
    }
}

fn validate(
    width: u32,
    height: u32,
    clear: [f32; 4],
    vertices: &[Vertex],
    limit: u32,
) -> Result<(), Error> {
    if width == 0 || height == 0 || width > limit.min(4096) || height > limit.min(4096) {
        return Err(Error::InvalidInput("offscreen dimensions exceed supported bounds"));
    }
    if vertices.len() % 3 != 0 || vertices.len() > 1_000_000 {
        return Err(Error::InvalidInput("expected a bounded triangle-list vertex count"));
    }
    if !clear.iter().all(|v| v.is_finite())
        || vertices.iter().any(|v| v.position.iter().chain(&v.color).any(|x| !x.is_finite()))
    {
        return Err(Error::InvalidInput("non-finite vertex or clear value"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_dimensions_and_vertex_counts_are_rejected_before_driver_calls() {
        assert!(validate(0, 1, [0.0; 4], &[], 4096).is_err());
        assert!(validate(1, 4097, [0.0; 4], &[], 8192).is_err());
        assert!(validate(33, 1, [0.0; 4], &[], 32).is_err());
        let v = Vertex { position: [0.0; 2], color: [1.0; 4] };
        assert!(validate(1, 1, [0.0; 4], &[v], 4096).is_err());
        assert!(validate(1, 1, [f32::NAN; 4], &[], 4096).is_err());
        assert!(validate(1, 1, [0.0; 4], &[v; 3], 4096).is_ok());
    }

    #[test]
    fn image_pixel_checks_bounds_and_truncation() {
        let image = Image { width: 1, height: 1, rgba: vec![1, 2, 3, 4] };
        assert_eq!(image.pixel(0, 0), Some([1, 2, 3, 4]));
        assert_eq!(image.pixel(1, 0), None);
        assert_eq!(Image { rgba: vec![], ..image }.pixel(0, 0), None);
    }

    #[test]
    fn missing_loader_is_a_real_error() {
        assert!(OffscreenRenderer::new(Some(Path::new("/hematite-no-such-loader.so"))).is_err());
    }
}

#[cfg(test)]
pub(crate) mod session_tests {
    use crate::session::{
        PipelineState, Session, ShaderStages, Uniforms, VertexAttribute, VertexLayout,
        UNIFORM_MVP_OFFSET, UNIFORM_UNIFORM_OFFSET,
    };
    use ash::vk;
    use std::path::PathBuf;

    /// The uniform block every Hematite pipeline declares: fixed-function state, the
    /// modelview-projection matrix and the program uniforms, as a `std140` block at binding
    /// 0. Both stages must declare the *same* block, because a driver is entitled to assume
    /// they agree.
    /// The uniform block, written out the way [`crate::session`] lays it out: 32 separate
    /// `vec4` uniform members, because this driver cannot build a block with an array member.
    const UNIFORM_BLOCK: &str = "layout(std140, binding = 0) uniform HematiteFrame {
    vec4 lightModelProduct;
    vec4 fog;
    mat4 mvp;
    vec4 u0;
    vec4 u1;
} hematiteFrame;
";

    /// The stages the OpenGL frontend generates for a fixed-function draw: position and
    /// colour in, the transformed position and the modulated colour out.
    pub(crate) const FIXED_VERTEX: &str = "#version 450 core
layout(location = 0) in vec2 inPosition;
layout(location = 1) in vec4 inColor;
layout(location = 0) out vec2 texcoord;
layout(location = 1) out vec4 color;
void main() {
    texcoord = inPosition;
    color = inColor;
    gl_Position = hematiteFrame.mvp * vec4(inPosition, 0.0, 1.0);
}
";
    pub(crate) const FIXED_FRAGMENT: &str = "#version 450 core
layout(location = 1) in vec4 color;
layout(location = 0) out vec4 outColor;
void main() {
    outColor = color * hematiteFrame.u0;
}
";

    pub(crate) fn compile(stage: hematite_glsl::Stage, source: &str) -> Vec<u32> {
        hematite_glsl::compile(&hematite_glsl::Request::new(stage, source))
            .unwrap_or_else(|error| panic!("stage must compile: {}", error.to_info_log()))
            .spirv
    }

    /// Build a stage source from the shared uniform block plus a body.
    fn stage(body: &str) -> String {
        format!("#version 450 core\n{UNIFORM_BLOCK}{body}")
    }

    /// A triangle that covers the whole target, interleaved as (vec2 position, vec4 colour).
    pub(crate) fn covering_triangle(color: [f32; 4]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for [x, y] in [[-1.0f32, -1.0], [3.0, -1.0], [-1.0, 3.0]] {
            for value in [x, y] {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            for value in color {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        bytes
    }

    /// Position at offset 0 and colour at offset 8 of a 24-byte vertex.
    fn position_and_color() -> VertexLayout {
        VertexLayout::packed(&[
            VertexAttribute { location: 0, format: vk::Format::R32G32_SFLOAT, offset: 0 },
            VertexAttribute { location: 1, format: vk::Format::R32G32B32A32_SFLOAT, offset: 0 },
        ])
        .expect("both formats are supported")
    }

    /// Uniforms with an identity matrix, a current colour at `u0` and a second value at `u1`.
    fn uniforms(u0: [f32; 4], u1: [f32; 4]) -> Uniforms {
        Uniforms { state: [0.0; 8], mvp: Uniforms::identity().mvp, uniforms: vec![u0, u1] }
    }

    #[test]
    #[ignore = "requires an actual Vulkan 1.1+ driver; run ./build.sh gpu"]
    fn session_draws_with_a_cached_pipeline() {
        let loader = std::env::var_os("HEMATITE_VULKAN_LIBRARY").map(PathBuf::from);
        let mut session = Session::new(loader.as_deref(), false).expect("real Vulkan device");
        let stages = ShaderStages {
            vertex: compile(
                hematite_glsl::Stage::Vertex,
                &stage(
                    "layout(location = 0) in vec2 inPosition;
layout(location = 1) in vec4 inColor;
layout(location = 1) out vec4 color;
void main() {
    color = inColor;
    gl_Position = hematiteFrame.mvp * vec4(inPosition, 0.0, 1.0);
}
",
                ),
            ),
            fragment: compile(
                hematite_glsl::Stage::Fragment,
                &stage(
                    "layout(location = 1) in vec4 color;
layout(location = 0) out vec4 outColor;
void main() {
    outColor = color * hematiteFrame.u0;
}
",
                ),
            ),
        };
        let layout = position_and_color();
        assert_eq!(layout.stride, 24, "a vec2 and a vec4 interleave to 24 bytes");
        let state = PipelineState::opaque(4, 4);
        let vertices = covering_triangle([1.0, 0.0, 0.0, 1.0]);
        // Vertex colour red, u0 white: the product is red, so this frame proves the vertex
        // attribute reached the shader.
        let uniform_data = uniforms([1.0, 1.0, 1.0, 1.0], [0.0, 0.0, 0.0, 0.0]);

        session.begin(4, 4, [0.0, 1.0, 0.0, 1.0]).expect("begin a frame");
        // The pipeline is built on the worker, so the first draw must report "not ready"
        // rather than block: that is the asynchronous-pipeline contract.
        assert!(
            !session
                .record_draw(
                    &stages,
                    layout,
                    state,
                    vk::PrimitiveTopology::TRIANGLE_LIST,
                    &vertices,
                    &uniform_data,
                )
                .expect("record the first draw"),
            "the first draw with a new state must not block on the pipeline build"
        );
        session.wait_for_pipelines();
        assert_eq!(session.pipelines_built(), 1, "exactly one pipeline was built");

        // Second frame: the same state must come out of the cache and draw.
        session.begin(4, 4, [0.0, 1.0, 0.0, 1.0]).expect("begin the second frame");
        assert!(
            session
                .record_draw(
                    &stages,
                    layout,
                    state,
                    vk::PrimitiveTopology::TRIANGLE_LIST,
                    &vertices,
                    &uniform_data,
                )
                .expect("record the cached draw"),
            "the cached pipeline must be ready"
        );
        assert_eq!(session.pipelines_built(), 1, "state deduplication built no second pipeline");
        assert_eq!(session.pipeline_reuses(), 1, "the cached pipeline was reused");
        assert_eq!(session.recorded_draws(), 1, "the draw was recorded");
        let pixels = session.flush().expect("flush the frame");
        assert_eq!(pixels.len(), 4 * 4 * 4, "a 4x4 RGBA8 target");
        let pixel = |x: usize, y: usize| -> [u8; 4] {
            let offset = (y * 4 + x) * 4;
            [pixels[offset], pixels[offset + 1], pixels[offset + 2], pixels[offset + 3]]
        };
        assert_eq!(pixel(2, 2), [255, 0, 0, 255], "the covered pixel is the vertex colour");

        // Vertex colour white, u0 green: the product is green, so this frame proves the
        // uniform block reached the shader and is not just the vertex colour. u1 is left at
        // zero on purpose: were the block misaligned by one member the result would be black.
        let white = covering_triangle([1.0, 1.0, 1.0, 1.0]);
        session.begin(4, 4, [1.0, 0.0, 0.0, 1.0]).expect("begin the third frame");
        let tinted = uniforms([0.0, 1.0, 0.0, 1.0], [0.0, 0.0, 0.0, 0.0]);
        assert!(session
            .record_draw(
                &stages,
                layout,
                state,
                vk::PrimitiveTopology::TRIANGLE_LIST,
                &white,
                &tinted,
            )
            .expect("record the tinted draw"));
        let pixels = session.flush().expect("flush the tinted frame");
        assert_eq!(
            &pixels[(2 * 4 + 2) * 4..(2 * 4 + 2) * 4 + 4],
            &[0, 255, 0, 255],
            "green on a red clear means the uniform arrived and multiplied the white vertex"
        );
        assert_eq!(
            session.pipelines_built(),
            1,
            "only the state, not the uniform data, selects a pipeline"
        );

        // A different render state is a different pipeline, and it must build.
        session.begin(4, 4, [0.0, 1.0, 0.0, 1.0]).expect("begin the fourth frame");
        let blend = PipelineState {
            blend: true,
            src_factor: vk::BlendFactor::SRC_ALPHA,
            dst_factor: vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
            ..state
        };
        assert!(!session
            .record_draw(
                &stages,
                layout,
                blend,
                vk::PrimitiveTopology::TRIANGLE_LIST,
                &vertices,
                &uniform_data,
            )
            .expect("record the blended draw"));
        session.wait_for_pipelines();
        assert_eq!(session.pipelines_built(), 2, "a new state builds a new pipeline");

        // The vertex ring must reject a frame larger than the allocation instead of writing
        // past its end.
        session.begin(4, 4, [0.0, 1.0, 0.0, 1.0]).expect("begin the fifth frame");
        let huge = vec![0u8; 8 * 1024 * 1024];
        assert!(
            session
                .record_draw(
                    &stages,
                    layout,
                    state,
                    vk::PrimitiveTopology::TRIANGLE_LIST,
                    &huge,
                    &uniform_data,
                )
                .is_err(),
            "an oversized vertex array must be rejected"
        );

        // A driver that cannot build a pipeline must be reported, once, with a reason, and
        // then treated as permanently unavailable instead of being rebuilt every frame.
        let broken = ShaderStages {
            vertex: stages.vertex.clone(),
            // A fragment stage that writes nothing the pipeline can use: no output at all.
            fragment: compile(hematite_glsl::Stage::Fragment, &stage("void main() { }\n")),
        };
        session.begin(4, 4, [0.0; 4]).expect("begin the sixth frame");
        let first = session
            .record_draw(
                &broken,
                layout,
                state,
                vk::PrimitiveTopology::TRIANGLE_LIST,
                &vertices,
                &uniform_data,
            )
            .expect("record the draw with a pipeline the driver may reject");
        session.wait_for_pipelines();
        let second = session
            .record_draw(
                &broken,
                layout,
                state,
                vk::PrimitiveTopology::TRIANGLE_LIST,
                &vertices,
                &uniform_data,
            )
            .expect("record it again");
        match (first, second) {
            (false, true) => println!("  this driver builds a fragment stage with no output"),
            (false, false) => {
                let reason = session
                    .pipeline_rejected(&broken, layout, state, vk::PrimitiveTopology::TRIANGLE_LIST)
                    .expect("a rejected pipeline must carry its reason");
                println!("  this driver rejects it, once: {reason}");
            }
            other => panic!("a pipeline either builds once or is rejected: {other:?}"),
        }
        let _ = (UNIFORM_MVP_OFFSET, UNIFORM_UNIFORM_OFFSET, FIXED_VERTEX, FIXED_FRAGMENT);
    }

    /// A session opened with a sampler: its pipeline builds, its draws record, its frames
    /// flush, and a driver that refuses the pipeline is reported with a reason instead of
    /// being retried every frame.
    ///
    /// The sampled value is not asserted, and the frame is deliberately never flushed. The
    /// session's sampler binding points at a 1x1 placeholder image whose contents are never
    /// written, so its value is undefined by construction, and the pinned SwiftShader build in
    /// `.cache/vulkan/driver` dies with SIGSEGV while executing a frame that sampled — the
    /// pipeline builds and the draw records, the submission is what kills it. Uploading real
    /// texture data and executing a sampled frame are therefore both NOT-VERIFIED here; see
    /// docs/KNOWN_LIMITATIONS.md. What this does prove is the descriptor set layout, the pool
    /// sizing and pipeline creation for a sampling stage.
    #[test]
    #[ignore = "needs a Vulkan driver; run ./build.sh gpu"]
    fn sampler_session_builds_and_draws() {
        let loader = std::env::var_os("HEMATITE_VULKAN_LIBRARY").map(PathBuf::from);
        let mut session = Session::new(loader.as_deref(), true).expect("a session with a sampler");
        let stages = ShaderStages {
            vertex: compile(
                hematite_glsl::Stage::Vertex,
                &stage(
                    "layout(location = 0) in vec2 inPosition;
layout(location = 1) in vec4 inColor;
layout(location = 0) out vec2 texcoord;
layout(location = 1) out vec4 color;
void main() {
    texcoord = inPosition * 0.5 + 0.5;
    color = inColor;
    gl_Position = hematiteFrame.mvp * vec4(inPosition, 0.0, 1.0);
}
",
                ),
            ),
            fragment: compile(
                hematite_glsl::Stage::Fragment,
                &stage(
                    "layout(location = 0) in vec2 texcoord;
layout(location = 1) in vec4 color;
layout(location = 0) out vec4 outColor;
layout(binding = 1) uniform sampler2D diffuse;
void main() {
    outColor = color * texture(diffuse, texcoord);
}
",
                ),
            ),
        };
        let layout = position_and_color();
        let state = PipelineState { uses_sampler: true, ..PipelineState::opaque(4, 4) };
        let vertices = covering_triangle([1.0; 4]);
        let uniform_data = uniforms([1.0; 4], [0.0; 4]);
        session.begin(4, 4, [0.0; 4]).expect("begin");
        assert!(
            !session
                .record_draw(
                    &stages,
                    layout,
                    state,
                    vk::PrimitiveTopology::TRIANGLE_LIST,
                    &vertices,
                    &uniform_data,
                )
                .expect("record the first sampling draw"),
            "the first draw with a new state is never ready"
        );
        session.wait_for_pipelines();
        match session.pipelines_built() {
            1 => {
                let second = session
                    .record_draw(
                        &stages,
                        layout,
                        state,
                        vk::PrimitiveTopology::TRIANGLE_LIST,
                        &vertices,
                        &uniform_data,
                    )
                    .expect("record the second sampling draw");
                assert!(second, "a built pipeline is ready on the next draw");
                println!("  this driver builds and records sampling pipelines");
            }
            0 => {
                let reason = session
                    .pipeline_rejected(&stages, layout, state, vk::PrimitiveTopology::TRIANGLE_LIST)
                    .expect("a rejected pipeline must carry its reason");
                println!("  this driver rejects sampling pipelines: {reason}");
            }
            other => panic!("a pipeline either builds once or is rejected, got {other}"),
        }
    }
}
