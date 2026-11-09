//! Persistent device session used by the OpenGL frontend.
//!
//! [`crate::render`] creates and destroys every resource per call, which is right for a
//! correctness harness and wrong for a renderer. This module keeps the device, the render
//! target, the command recording, the descriptor sets and the pipelines alive across draws,
//! and implements the three properties the frontend's frame loop depends on:
//!
//! * **state deduplication** — a pipeline is built once per (stages, vertex layout, render
//!   state, topology) key and reused. [`Session::pipelines_built`] counts the pipelines that
//!   were actually created, so a frame that repeats a state provably creates nothing,
//! * **ring buffers** — vertex bytes and per-draw uniform blocks are carved out of two
//!   mapped host-visible allocations instead of one `vkAllocateMemory` per draw,
//! * **asynchronous pipelines** — a new pipeline is handed to a worker thread and the draw
//!   that requested it reports "not ready" instead of blocking; the next draw with the same
//!   state finds it in the cache.
//!
//! ## Why uniforms travel in a buffer and not in push constants
//!
//! Push constants would be the obvious choice for a matrix and a colour. They are also a
//! dead end here, for two reasons that were both measured rather than assumed:
//!
//! * the Vulkan-guaranteed `maxPushConstantsSize` is 128 bytes, which is one matrix plus one
//!   colour and no room for program uniforms, and the SwiftShader build in
//!   `.cache/vulkan/driver` reports exactly that;
//! * that same driver segfaults while executing a pipeline that combines push constants with
//!   vertex input. Each half works on its own — a pipeline with vertex input and no push
//!   constants draws correctly, and a pipeline with push constants and no vertex input draws
//!   correctly — but together the process dies inside the driver.
//!
//! So every draw gets a `Uniforms` block in a uniform buffer, described by a descriptor set
//! taken from a pool. That also removes the ceiling: [`UNIFORM_UNIFORM_OFFSET`] onwards there
//! is room for 64 `vec4` program uniforms on any device.
//!
//! ## What a session does and does not do
//!
//! A session renders into an offscreen colour attachment that it clears at the start of each
//! frame and reads back at the end, because the frontend composites the result into its own
//! framebuffer (which also owns the scissor, colour mask and blend state). Depth is resolved
//! inside the render pass and is not read back, so callers that need depth results must not
//! use this path.

use crate::driver::{Context, Shared};
use crate::{Capabilities, Error};
use ash::{vk, Device};
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

fn result<T>(operation: &'static str, value: Result<T, vk::Result>) -> Result<T, Error> {
    value.map_err(|error| Error::Vulkan(operation, error))
}

/// Byte size of one draw's uniform block.
///
/// The layout is std140 and is what the shaders the frontend generates declare as
/// `HematiteFrame`:
///
/// | Offset | Size | Contents |
/// | --- | --- | --- |
/// | [`UNIFORM_STATE_OFFSET`] | 32 | fixed-function state: light model product, fog colour |
/// | [`UNIFORM_MVP_OFFSET`] | 64 | modelview-projection matrix, column major |
/// | [`UNIFORM_UNIFORM_OFFSET`] | 512 | 32 `vec4` program uniforms, one member each |
///
/// The program uniforms are 32 separate `vec4` members rather than one `vec4 uniforms[32]`
/// array. That is not a style choice: the SwiftShader build in `.cache/vulkan/driver`
/// segfaults while creating a pipeline whose uniform block has an array member. It accepts
/// the module (`vkCreateShaderModule` succeeds), logs `UNSUPPORTED: Unsupported Descriptor
/// Type` when it reflects the block, and then dereferences the failed layout. Scalar members
/// of the same total size build and run. See docs/KNOWN_LIMITATIONS.md.
pub const UNIFORM_BLOCK_BYTES: u32 = 608;
/// Offset of the fixed-function state inside a uniform block.
pub const UNIFORM_STATE_OFFSET: u32 = 0;
/// Offset of the modelview-projection matrix inside a uniform block.
pub const UNIFORM_MVP_OFFSET: u32 = 32;
/// Offset of the first program uniform inside a uniform block.
pub const UNIFORM_UNIFORM_OFFSET: u32 = 96;
/// How many `vec4` program uniforms a block carries.
pub const UNIFORM_SLOTS: usize = 32;
/// Descriptor set binding of the uniform block.
pub const UNIFORM_BINDING: u32 = 0;
/// Descriptor set binding of the sampler, in sessions that have one.
pub const SAMPLER_BINDING: u32 = 1;

/// Bytes of uniform data per frame before a session refuses the frame.
const UNIFORM_RING_BYTES: u64 = 4 * 1024 * 1024;
/// Bytes of vertex data per frame before a session refuses the frame.
const VERTEX_RING_BYTES: u64 = 4 * 1024 * 1024;
/// Descriptor sets allocated up front. One is consumed per draw, so this is also the number
/// of draws a single frame may make; a frame that needs more is refused rather than
/// silently allocating while a submission is pending.
const DESCRIPTOR_SETS: u32 = 2048;

/// The vertex layout of a draw with no vertex input at all.
pub const EMPTY_LAYOUT: VertexLayout = VertexLayout {
    attributes: [VertexAttribute { location: 0, format: vk::Format::UNDEFINED, offset: 0 }; 8],
    count: 0,
    stride: 0,
};

/// One vertex attribute of a pipeline's input layout.
///
/// All attributes of a draw come from one interleaved array, which is what an OpenGL vertex
/// array looks like once the frontend has packed it: `offset` is the attribute's byte offset
/// inside one vertex and [`VertexLayout::stride`] the distance between two vertices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VertexAttribute {
    /// Shader input location.
    pub location: u32,
    /// Component format.
    pub format: vk::Format,
    /// Byte offset of this attribute inside one vertex.
    pub offset: u32,
}

/// The vertex layout of a draw: one interleaved array.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VertexLayout {
    /// Attributes; only the first `count` are used.
    pub attributes: [VertexAttribute; 8],
    /// How many of `attributes` are used.
    pub count: usize,
    /// Byte distance between two vertices.
    pub stride: u32,
}

impl VertexLayout {
    /// A layout of tightly packed attributes, in the order given.
    pub fn packed(attributes: &[VertexAttribute]) -> Option<Self> {
        if attributes.len() > 8 {
            return None;
        }
        let mut slots =
            [VertexAttribute { location: 0, format: vk::Format::UNDEFINED, offset: 0 }; 8];
        let mut offset = 0u32;
        for (index, attribute) in attributes.iter().enumerate() {
            let size = component_bytes(attribute.format);
            if size == 0 {
                return None;
            }
            slots[index] = VertexAttribute { offset, ..*attribute };
            offset += size;
        }
        Some(Self { attributes: slots, count: attributes.len(), stride: offset })
    }

    fn used(&self) -> &[VertexAttribute] {
        &self.attributes[..self.count]
    }
}

/// Byte size of one vertex of `format`, or 0 for a format Hematite does not upload.
pub fn component_bytes(format: vk::Format) -> u32 {
    match format {
        vk::Format::R32G32B32A32_SFLOAT => 16,
        vk::Format::R32G32B32_SFLOAT => 12,
        vk::Format::R32G32_SFLOAT => 8,
        vk::Format::R32_SFLOAT
        | vk::Format::R8G8B8A8_UNORM
        | vk::Format::R8G8B8A8_SNORM
        | vk::Format::R16G16_SFLOAT
        | vk::Format::R32_UINT
        | vk::Format::R32_SINT => 4,
        vk::Format::R8G8_UNORM | vk::Format::R8G8_SNORM | vk::Format::R16_SFLOAT => 2,
        vk::Format::R8_UNORM | vk::Format::R8_SNORM => 1,
        _ => 0,
    }
}

/// Per-draw uniform data, laid out as [`UNIFORM_BLOCK_BYTES`] describes.
#[derive(Clone, Debug)]
pub struct Uniforms {
    /// Fixed-function state at [`UNIFORM_STATE_OFFSET`]: light model product, fog colour,
    /// point size, padding.
    pub state: [f32; 8],
    /// Modelview-projection matrix at [`UNIFORM_MVP_OFFSET`], column major.
    pub mvp: [f32; 16],
    /// Program uniforms at [`UNIFORM_UNIFORM_OFFSET`]; unused slots are zero.
    pub uniforms: Vec<[f32; 4]>,
}

impl Uniforms {
    /// An identity transform with no program uniforms.
    pub fn identity() -> Self {
        Self {
            state: [0.0; 8],
            mvp: [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0],
            uniforms: Vec::new(),
        }
    }

    fn write(&self, target: &mut [u8]) {
        for (index, value) in self.state.iter().enumerate() {
            target[UNIFORM_STATE_OFFSET as usize + index * 4..][..4]
                .copy_from_slice(&value.to_le_bytes());
        }
        for (index, value) in self.mvp.iter().enumerate() {
            target[UNIFORM_MVP_OFFSET as usize + index * 4..][..4]
                .copy_from_slice(&value.to_le_bytes());
        }
        for (index, value) in self.uniforms.iter().take(UNIFORM_SLOTS).enumerate() {
            let offset = UNIFORM_UNIFORM_OFFSET as usize + index * 16;
            for (component, value) in value.iter().enumerate() {
                target[offset + component * 4..][..4].copy_from_slice(&value.to_le_bytes());
            }
        }
    }
}

/// Render state that is baked into a pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PipelineState {
    /// Viewport in pixels.
    pub viewport: [u32; 2],
    /// Scissor rectangle, or None for the whole target.
    pub scissor: Option<[i32; 4]>,
    /// Enable the fixed-function blend stage.
    pub blend: bool,
    /// Source factor while blending.
    pub src_factor: vk::BlendFactor,
    /// Destination factor while blending.
    pub dst_factor: vk::BlendFactor,
    /// The pipeline's fragment stage samples the descriptor set's sampler binding.
    pub uses_sampler: bool,
}

impl PipelineState {
    /// An opaque triangle draw covering the whole target.
    pub fn opaque(width: u32, height: u32) -> Self {
        Self {
            viewport: [width, height],
            scissor: None,
            blend: false,
            src_factor: vk::BlendFactor::ONE,
            dst_factor: vk::BlendFactor::ZERO,
            uses_sampler: false,
        }
    }
}

/// The two stages of a graphics pipeline, as SPIR-V words.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ShaderStages {
    /// Vertex stage.
    pub vertex: Vec<u32>,
    /// Fragment stage.
    pub fragment: Vec<u32>,
}

/// Everything that identifies a built pipeline.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PipelineKey {
    stages: ShaderStages,
    layout: VertexLayout,
    state: PipelineState,
    topology: vk::PrimitiveTopology,
}

/// The per-pipeline layout a build needs: the session's set layouts plus nothing else, since
/// a pipeline layout with no push constant ranges is shared by every pipeline.
struct Layouts {
    handle: vk::PipelineLayout,
}

/// A pipeline build handed to the worker thread.
struct Job {
    id: u64,
    context: Arc<Shared>,
    render_pass: vk::RenderPass,
    layouts: Layouts,
    /// Set by the worker exactly once, before the job is published as finished.
    outcome: Mutex<Option<Result<vk::Pipeline, Error>>>,
    key: PipelineKey,
}

struct Worker {
    sender: Sender<Arc<Job>>,
    /// Handle, joined before any device handle is destroyed.
    thread: Option<std::thread::JoinHandle<()>>,
    /// Jobs the worker has finished, whether they succeeded or not.
    finished: Arc<Mutex<Vec<Arc<Job>>>>,
    next_id: u64,
}

/// A live session on one Vulkan device.
pub struct Session {
    context: Arc<Shared>,
    render_pass: vk::RenderPass,
    /// Set layout without a sampler, for pipelines that do not sample.
    plain_set_layout: vk::DescriptorSetLayout,
    /// Set layout with a sampler, or null in a session opened without one.
    sampler_set_layout: vk::DescriptorSetLayout,
    plain_layout: vk::PipelineLayout,
    sampler_layout: vk::PipelineLayout,
    descriptor_pool: vk::DescriptorPool,
    /// Sets of the plain layout, handed out in order; one per draw.
    plain_sets: Vec<vk::DescriptorSet>,
    /// Sets of the sampler layout, or empty in a session opened without one.
    sampler_sets: Vec<vk::DescriptorSet>,
    /// Sets claimed by the frame being recorded, as an index into each list.
    plain_claimed: usize,
    sampler_claimed: usize,
    /// Uniform ring: one block per draw.
    uniforms: Ring,
    /// Vertex ring: one range per draw.
    vertices: Ring,
    sampler: Option<Dummy>,
    target: Target,
    pipelines: HashMap<PipelineKey, vk::Pipeline>,
    /// Keys whose build the driver rejected. A rejected key is never retried: the caller
    /// keeps falling back for it, which turns a driver limitation into a permanent, visible
    /// fallback instead of a per-frame rebuild.
    rejected: HashMap<PipelineKey, String>,
    pending: HashMap<u64, PipelineKey>,
    pipelines_built: u64,
    pipeline_hits: u64,
    worker: Option<Worker>,
    pool: vk::CommandPool,
    command: vk::CommandBuffer,
    recording: bool,
    /// Command buffers handed back to the pool when a frame ends.
    spent: Vec<vk::CommandBuffer>,
    draws: u64,
    fence: vk::Fence,
    readback: vk::Buffer,
    readback_memory: vk::DeviceMemory,
    readback_coherent: bool,
    target_size: usize,
}

struct Target {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    width: u32,
    height: u32,
}

#[derive(Clone, Copy)]
struct Dummy {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    sampler: vk::Sampler,
}

/// One mapped host-visible allocation carved up per frame.
struct Ring {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    coherent: bool,
    capacity: u64,
    mapped: *mut u8,
    used: u64,
}

// SAFETY: the mapping is of the ring's own HOST_VISIBLE allocation, is only touched while
// the Session is exclusively borrowed, and is never handed to a caller.
unsafe impl Send for Ring {}

impl Ring {
    fn empty() -> Self {
        Self {
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            coherent: false,
            capacity: 0,
            mapped: std::ptr::null_mut(),
            used: 0,
        }
    }

    /// Claim `size` aligned bytes and copy into them.
    fn claim(&mut self, device: &Device, bytes: &[u8], alignment: u64) -> Result<u64, Error> {
        let start = self.used.div_ceil(alignment) * alignment;
        let size = bytes.len() as u64;
        if start + size > self.capacity {
            return Err(Error::InvalidInput(
                "one frame uploaded more data than the ring buffer holds",
            ));
        }
        // SAFETY: the ring is HOST_VISIBLE and mapped for the session's lifetime, the range
        // is inside the allocation and nothing else claimed it this frame.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.mapped.add(start as usize),
                bytes.len(),
            );
        }
        if !self.coherent {
            // SAFETY: the written range is inside this allocation.
            result("flush ring", unsafe {
                device.flush_mapped_memory_ranges(&[vk::MappedMemoryRange::default()
                    .memory(self.memory)
                    .offset(start)
                    .size(size)])
            })?;
        }
        self.used = start + size;
        Ok(start)
    }
}

impl Session {
    /// Open a session.
    ///
    /// `loader` selects the Vulkan loader, or None for the default. `uses_sampler` decides
    /// whether pipelines may sample: a session with it gets a second set layout carrying a
    /// combined image sampler bound to a 1x1 texture, and pipelines whose state asks for a
    /// sampler use that layout. A session without it refuses those pipelines, because some
    /// drivers reject the standalone sampler type outright — the SwiftShader build in
    /// `.cache/vulkan/driver` logs `UNSUPPORTED: Unsupported Descriptor Type` for it and then
    /// dereferences the failed layout, taking the process with it.
    pub fn new(loader: Option<&std::path::Path>, uses_sampler: bool) -> Result<Self, Error> {
        let context = Arc::new(Shared(Context::new(loader)?));
        let device = context.device.clone();
        let render_pass = create_render_pass(&device)?;
        let plain_set_layout = create_set_layout(&device, false)?;
        let sampler_set_layout = if uses_sampler {
            create_set_layout(&device, true)?
        } else {
            vk::DescriptorSetLayout::null()
        };
        let plain_layout = create_pipeline_layout(&device, plain_set_layout)?;
        let sampler_layout = if uses_sampler {
            create_pipeline_layout(&device, sampler_set_layout)?
        } else {
            vk::PipelineLayout::null()
        };
        let descriptor_pool = create_descriptor_pool(&device)?;
        // SAFETY: the pool was created with room for exactly this many sets of each layout.
        let plain_sets = result("allocate descriptor sets", unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(&vec![plain_set_layout; DESCRIPTOR_SETS as usize]),
            )
        })?;
        let sampler_sets = if uses_sampler {
            result("allocate sampling descriptor sets", unsafe {
                device.allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(descriptor_pool)
                        .set_layouts(&vec![sampler_set_layout; DESCRIPTOR_SETS as usize]),
                )
            })?
        } else {
            Vec::new()
        };
        let sampler = if uses_sampler { Some(create_dummy(&device, &context)?) } else { None };
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(context.queue_family)
            .flags(vk::CommandPoolCreateFlags::TRANSIENT);
        // SAFETY: the queue family was negotiated while the context was created.
        let pool =
            result("create command pool", unsafe { device.create_command_pool(&pool_info, None) })?;
        // SAFETY: no flags or extension state.
        let fence = result("create fence", unsafe {
            device.create_fence(&vk::FenceCreateInfo::default(), None)
        })?;
        let mut session = Self {
            context,
            render_pass,
            plain_set_layout,
            sampler_set_layout,
            plain_layout,
            sampler_layout,
            descriptor_pool,
            plain_sets,
            sampler_sets,
            plain_claimed: 0,
            sampler_claimed: 0,
            uniforms: Ring::empty(),
            vertices: Ring::empty(),
            sampler,
            target: Target::empty(),
            pipelines: HashMap::new(),
            rejected: HashMap::new(),
            pending: HashMap::new(),
            pipelines_built: 0,
            pipeline_hits: 0,
            worker: None,
            pool,
            command: vk::CommandBuffer::null(),
            recording: false,
            spent: Vec::new(),
            draws: 0,
            fence,
            readback: vk::Buffer::null(),
            readback_memory: vk::DeviceMemory::null(),
            readback_coherent: false,
            target_size: 0,
        };
        session.uniforms = create_ring(&device, &session.context, UNIFORM_RING_BYTES, false)?;
        session.vertices = create_ring(&device, &session.context, VERTEX_RING_BYTES, true)?;
        if session.sampler.is_some() {
            session.transition_sampler();
        }
        Ok(session)
    }

    /// Device properties of the session's device.
    pub fn capabilities(&self) -> &Capabilities {
        &self.context.capabilities
    }

    /// How many pipelines this session actually built.
    ///
    /// A frame that repeats a state must not raise this number; that is the
    /// state-deduplication guarantee and the tests assert on it.
    pub fn pipelines_built(&self) -> u64 {
        self.pipelines_built
    }

    /// How many draws found an already-built pipeline and built nothing.
    pub fn pipeline_reuses(&self) -> u64 {
        self.pipeline_hits
    }

    /// How many draws were recorded into the frame that has not been flushed yet.
    pub fn recorded_draws(&self) -> u64 {
        self.draws
    }

    /// Read back `len` bytes of the uniform ring at `offset`.
    ///
    /// Only for tests and diagnostics: it reads the host mapping directly, so it shows what
    /// the session wrote, not what the device saw.
    pub fn uniform_bytes(&self, offset: u64, len: usize) -> Vec<u8> {
        let start = offset as usize;
        let end = (start + len).min(self.uniforms.capacity as usize);
        if start >= end {
            return Vec::new();
        }
        // SAFETY: the ring is HOST_VISIBLE, mapped for the session's lifetime, and this only
        // reads a range inside the allocation.
        unsafe { std::slice::from_raw_parts(self.uniforms.mapped.add(start), end - start) }.to_vec()
    }

    /// Why the driver rejected this exact pipeline, if it did.
    ///
    /// Lets a caller report the real reason it fell back instead of guessing.
    pub fn pipeline_rejected(
        &self,
        stages: &ShaderStages,
        layout: VertexLayout,
        state: PipelineState,
        topology: vk::PrimitiveTopology,
    ) -> Option<&str> {
        let key = PipelineKey { stages: stages.clone(), layout, state, topology };
        self.rejected.get(&key).map(String::as_str)
    }

    /// Start a frame: begin recording, and clear the target to `clear`.
    ///
    /// Anything recorded since the previous [`Self::flush`] is discarded, including the
    /// descriptor sets and ring space it claimed.
    pub fn begin(&mut self, width: u32, height: u32, clear: [f32; 4]) -> Result<(), Error> {
        if width == 0 || height == 0 {
            return Err(Error::InvalidInput("render target must be at least 1x1"));
        }
        let limit = self.context.capabilities.max_image_dimension_2d;
        if width > limit || height > limit {
            return Err(Error::InvalidInput("render target exceeds the device image limit"));
        }
        if self.target.width != width || self.target.height != height {
            self.retarget(width, height)?;
        }
        self.draws = 0;
        self.plain_claimed = 0;
        self.sampler_claimed = 0;
        self.uniforms.used = 0;
        self.vertices.used = 0;
        self.recording = false;
        // Every frame records into a freshly allocated command buffer. Allocating from an
        // existing pool is cheap, and it means a frame never resets a buffer the driver may
        // still hold internal state for.
        if !self.spent.is_empty() {
            // SAFETY: these buffers were submitted and their fence was waited on.
            unsafe { self.context.device.free_command_buffers(self.pool, &self.spent) };
            self.spent.clear();
        }
        let allocate = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: the pool is valid and outlives the buffer.
        let buffers = result("allocate frame command buffer", unsafe {
            self.context.device.allocate_command_buffers(&allocate)
        })?;
        self.command = *buffers
            .first()
            .ok_or(Error::InvalidInput("the driver allocated no command buffer"))?;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: the buffer is fresh and nothing else is recording into it.
        result("begin command buffer", unsafe {
            self.context.device.begin_command_buffer(self.command, &begin)
        })?;
        self.recording = true;
        let clear_values = [vk::ClearValue { color: vk::ClearColorValue { float32: clear } }];
        let pass = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(self.target.framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D { width, height },
            })
            .clear_values(&clear_values);
        // SAFETY: the framebuffer was built for this render pass and these dimensions, the
        // command buffer is recording, and the clear value matches the single attachment.
        unsafe {
            self.context.device.cmd_begin_render_pass(
                self.command,
                &pass,
                vk::SubpassContents::INLINE,
            );
        }
        Ok(())
    }

    /// Record a draw with the pipeline described by these stages, layout and state.
    ///
    /// Returns false when that pipeline is not built yet: the first draw with a new state
    /// always does, because the build runs on a worker thread instead of blocking here. The
    /// caller decides what to do — the frontend falls back to its software rasterizer for
    /// that draw, so nothing is dropped. Vertex bytes and the uniform block are copied into
    /// the session's rings and are not referenced again afterwards.
    pub fn record_draw(
        &mut self,
        stages: &ShaderStages,
        layout: VertexLayout,
        state: PipelineState,
        topology: vk::PrimitiveTopology,
        vertices: &[u8],
        uniforms: &Uniforms,
    ) -> Result<bool, Error> {
        self.validate_layout(layout)?;
        if layout.count == 0 {
            return Err(Error::InvalidInput(
                "a draw with no vertex input must say how many vertices to draw",
            ));
        }
        if vertices.len() as u32 % layout.stride != 0 {
            return Err(Error::InvalidInput("vertex data is not a whole number of vertices"));
        }
        let vertex_count = vertices.len() as u32 / layout.stride;
        if vertex_count == 0 {
            return Ok(false);
        }
        let Some(pipeline) = self.pipeline(stages, layout, state, topology)? else {
            return Ok(false);
        };
        let uniform_offset = self.claim_uniform(uniforms)?;
        let set = self.claim_set(state, uniform_offset)?;
        let vertex_offset = self.vertices.claim(
            &self.context.device,
            vertices,
            // Vulkan requires a vertex buffer offset that is a multiple of 4.
            4,
        )?;
        let pipeline_layout = self.pipeline_layout(state);
        // SAFETY: the command buffer is recording inside the render pass begun by begin(),
        // the vertex range and uniform block were uploaded to the rings, the descriptor set
        // was written before this call, and the pipeline was built from this same key.
        unsafe {
            let device = &self.context.device;
            device.cmd_bind_pipeline(self.command, vk::PipelineBindPoint::GRAPHICS, pipeline);
            device.cmd_bind_descriptor_sets(
                self.command,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline_layout,
                0,
                std::slice::from_ref(&set),
                &[],
            );
            if layout.count != 0 {
                device.cmd_bind_vertex_buffers(
                    self.command,
                    0,
                    std::slice::from_ref(&self.vertices.buffer),
                    std::slice::from_ref(&vertex_offset),
                );
            }
            device.cmd_draw(self.command, vertex_count, 1, 0, 0);
        }
        self.draws += 1;
        Ok(true)
    }

    /// Record a draw whose vertex stage generates its own vertices.
    ///
    /// [`Self::record_draw`] needs a vertex count from the vertex data, which a shader with
    /// no vertex input does not provide, so it is given explicitly here.
    pub fn record_generated_draw(
        &mut self,
        stages: &ShaderStages,
        state: PipelineState,
        topology: vk::PrimitiveTopology,
        vertex_count: u32,
        uniforms: &Uniforms,
    ) -> Result<bool, Error> {
        if vertex_count == 0 {
            return Ok(false);
        }
        let Some(pipeline) = self.pipeline(stages, EMPTY_LAYOUT, state, topology)? else {
            return Ok(false);
        };
        let uniform_offset = self.claim_uniform(uniforms)?;
        let set = self.claim_set(state, uniform_offset)?;
        let pipeline_layout = self.pipeline_layout(state);
        // SAFETY: as in `record_draw`, minus the vertex buffer, which this pipeline's empty
        // vertex input state means it never reads.
        unsafe {
            let device = &self.context.device;
            device.cmd_bind_pipeline(self.command, vk::PipelineBindPoint::GRAPHICS, pipeline);
            device.cmd_bind_descriptor_sets(
                self.command,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline_layout,
                0,
                std::slice::from_ref(&set),
                &[],
            );
            device.cmd_draw(self.command, vertex_count, 1, 0, 0);
        }
        self.draws += 1;
        Ok(true)
    }

    /// Whether every pipeline requested so far has finished building.
    pub fn pipelines_ready(&mut self) -> bool {
        self.collect_finished();
        self.pending.is_empty()
    }

    /// Block until every requested pipeline exists.
    ///
    /// For tests and shutdown; a frame loop should poll [`Self::pipelines_ready`] instead.
    pub fn wait_for_pipelines(&mut self) {
        while !self.pipelines_ready() {
            std::thread::yield_now();
        }
    }

    /// Submit the frame and read the target back as tightly packed RGBA8 rows.
    pub fn flush(&mut self) -> Result<Vec<u8>, Error> {
        let device = &self.context.device;
        if self.recording {
            // SAFETY: the render pass begun by begin() is still open.
            unsafe { device.cmd_end_render_pass(self.command) };
            let (width, height) = (self.target.width, self.target.height);
            let region = [vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .mip_level(0)
                        .base_array_layer(0)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D { width, height, depth: 1 })];
            // SAFETY: the render pass left the attachment in TRANSFER_SRC_OPTIMAL, the
            // readback buffer is sized for the whole target, and rows are tightly packed.
            unsafe {
                device.cmd_copy_image_to_buffer(
                    self.command,
                    self.target.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    self.readback,
                    &region,
                );
            }
            let barrier = [vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.readback)
                .offset(0)
                .size(vk::WHOLE_SIZE)];
            // SAFETY: the barrier is the required transfer-to-host synchronisation.
            unsafe {
                device.cmd_pipeline_barrier(
                    self.command,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::HOST,
                    vk::DependencyFlags::empty(),
                    &[],
                    &barrier,
                    &[],
                );
            }
            // SAFETY: recording is otherwise complete.
            result("end command buffer", unsafe { device.end_command_buffer(self.command) })?;
            self.recording = false;
        }
        let commands = [self.command];
        let submits = [vk::SubmitInfo::default().command_buffers(&commands)];
        // SAFETY: the queue takes graphics work, the buffer was recorded to completion and
        // every resource it names is owned by this session.
        result("queue submit", unsafe {
            device.queue_submit(self.context.queue, &submits, self.fence)
        })?;
        // SAFETY: the fence belongs to this submission and the session outlives the wait.
        result("wait for frame", unsafe {
            device.wait_for_fences(std::slice::from_ref(&self.fence), true, 10_000_000_000)
        })?;
        // SAFETY: the fence is signalled, so resetting it is legal.
        result("reset fence", unsafe { device.reset_fences(std::slice::from_ref(&self.fence)) })?;
        // The buffer is spent; begin() hands it back to the pool next frame.
        if self.command != vk::CommandBuffer::null() {
            self.spent.push(self.command);
            self.command = vk::CommandBuffer::null();
        }
        self.draws = 0;
        self.plain_claimed = 0;
        self.sampler_claimed = 0;
        self.uniforms.used = 0;
        self.vertices.used = 0;
        self.collect_finished();
        self.read_pixels()
    }

    fn validate_layout(&self, layout: VertexLayout) -> Result<(), Error> {
        for attribute in layout.used() {
            let size = component_bytes(attribute.format);
            if size == 0 {
                return Err(Error::InvalidInput("unsupported vertex attribute format"));
            }
            if attribute.offset + size > layout.stride {
                return Err(Error::InvalidInput("a vertex attribute reaches past the stride"));
            }
        }
        Ok(())
    }

    fn pipeline_layout(&self, state: PipelineState) -> vk::PipelineLayout {
        if state.uses_sampler {
            self.sampler_layout
        } else {
            self.plain_layout
        }
    }

    /// Write this draw's uniform block into the ring and return its offset.
    fn claim_uniform(&mut self, uniforms: &Uniforms) -> Result<u64, Error> {
        if uniforms.uniforms.len() > UNIFORM_SLOTS {
            return Err(Error::InvalidInput(
                "a draw carries more program uniforms than one block holds",
            ));
        }
        let mut block = vec![0u8; UNIFORM_BLOCK_BYTES as usize];
        uniforms.write(&mut block);
        // The uniform buffer offset must be a multiple of minUniformBufferOffsetAlignment;
        // the block size is a multiple of every alignment a driver reports in practice, and
        // 256 covers the largest value Vulkan allows a driver to require.
        self.uniforms.claim(&self.context.device, &block, 256)
    }

    /// Take the next descriptor set and point it at this draw's uniform block.
    ///
    /// A set is reused on the next frame rather than allocated per draw, which is why the
    /// writes happen every time instead of once.
    fn claim_set(&mut self, state: PipelineState, offset: u64) -> Result<vk::DescriptorSet, Error> {
        let buffer_info = [vk::DescriptorBufferInfo::default()
            .buffer(self.uniforms.buffer)
            .offset(offset)
            .range(u64::from(UNIFORM_BLOCK_BYTES))];
        let (set, sampler_write) = if state.uses_sampler {
            let index = self.sampler_claimed;
            if index >= self.sampler_sets.len() {
                return Err(Error::InvalidInput(
                    "one frame recorded more sampling draws than the session has sets for",
                ));
            }
            self.sampler_claimed += 1;
            let image_info = self.sampler.map(|dummy| {
                vk::DescriptorImageInfo::default()
                    .sampler(dummy.sampler)
                    .image_view(dummy.view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            });
            (self.sampler_sets[index], image_info)
        } else {
            let index = self.plain_claimed;
            if index >= self.plain_sets.len() {
                return Err(Error::InvalidInput(
                    "one frame recorded more draws than the session has descriptor sets for",
                ));
            }
            self.plain_claimed += 1;
            (self.plain_sets[index], None)
        };
        let image_info = sampler_write.map(|info| [info]);
        let uniform_write = vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(UNIFORM_BINDING)
            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
            .buffer_info(&buffer_info);
        let sampler_write = image_info.as_ref().map(|info| {
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(SAMPLER_BINDING)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(info)
        });
        let writes: Vec<vk::WriteDescriptorSet> = sampler_write
            .map(|write| vec![uniform_write, write])
            .unwrap_or_else(|| vec![uniform_write]);
        // SAFETY: the set came from this session's pool with the layout these bindings
        // belong to, and the buffer, view and sampler live as long as the session.
        unsafe { self.context.device.update_descriptor_sets(&writes, &[]) };
        Ok(set)
    }

    fn read_pixels(&self) -> Result<Vec<u8>, Error> {
        let device = &self.context.device;
        if !self.readback_coherent {
            // SAFETY: the submission has completed, and invalidating is required before the
            // CPU reads a non-coherent mapping.
            result("invalidate readback", unsafe {
                device.invalidate_mapped_memory_ranges(&[vk::MappedMemoryRange::default()
                    .memory(self.readback_memory)
                    .offset(0)
                    .size(vk::WHOLE_SIZE)])
            })?;
        }
        let mut out = vec![0u8; self.target_size];
        // SAFETY: the allocation is HOST_VISIBLE, is not mapped elsewhere and covers the
        // whole target.
        let pointer = result("map readback", unsafe {
            device.map_memory(
                self.readback_memory,
                0,
                self.target_size as u64,
                vk::MemoryMapFlags::empty(),
            )
        })?;
        // SAFETY: the pointer is a valid mapping of target_size bytes and the device is no
        // longer writing it, because the fence this frame waited on is signalled.
        unsafe {
            std::ptr::copy_nonoverlapping(pointer.cast::<u8>(), out.as_mut_ptr(), self.target_size);
            device.unmap_memory(self.readback_memory);
        }
        Ok(out)
    }

    /// Return the cached pipeline for this key, or start building it and report None.
    fn pipeline(
        &mut self,
        stages: &ShaderStages,
        layout: VertexLayout,
        state: PipelineState,
        topology: vk::PrimitiveTopology,
    ) -> Result<Option<vk::Pipeline>, Error> {
        self.collect_finished();
        if state.uses_sampler && self.sampler.is_none() {
            return Err(Error::Unsupported(
                "this session was opened without a sampler, so a sampling pipeline has no layout"
                    .to_string(),
            ));
        }
        let key = PipelineKey { stages: stages.clone(), layout, state, topology };
        if let Some(pipeline) = self.pipelines.get(&key) {
            self.pipeline_hits += 1;
            return Ok(Some(*pipeline));
        }
        if self.pending.values().any(|pending| *pending == key) {
            // Already building; the caller falls back for this draw.
            return Ok(None);
        }
        if self.rejected.contains_key(&key) {
            // The driver refused this pipeline once; it will refuse it again.
            return Ok(None);
        }
        if stages.vertex.len() < 5 || stages.fragment.len() < 5 {
            return Err(Error::InvalidInput("SPIR-V module is shorter than its header"));
        }
        let layout_handle = self.pipeline_layout(state);
        let worker = self.worker.get_or_insert_with(|| {
            let (sender, receiver) = channel::<Arc<Job>>();
            let finished: Arc<Mutex<Vec<Arc<Job>>>> = Arc::new(Mutex::new(Vec::new()));
            let thread = spawn_worker(receiver, Arc::clone(&finished));
            Worker { sender, thread, finished, next_id: 0 }
        });
        let id = worker.next_id;
        worker.next_id += 1;
        let job = Arc::new(Job {
            id,
            context: Arc::clone(&self.context),
            render_pass: self.render_pass,
            layouts: Layouts { handle: layout_handle },
            outcome: Mutex::new(None),
            key: key.clone(),
        });
        worker
            .sender
            .send(job)
            .map_err(|_| Error::InvalidInput("the pipeline worker thread has stopped"))?;
        self.pending.insert(id, key);
        Ok(None)
    }

    /// Move finished builds from the worker's queue into the pipeline cache.
    fn collect_finished(&mut self) {
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        let finished = {
            let Ok(mut guard) = worker.finished.lock() else {
                return;
            };
            std::mem::take(&mut *guard)
        };
        for job in finished {
            let id = job.id;
            let published = job.outcome.lock().ok().and_then(|mut guard| guard.take());
            let outcome =
                published.unwrap_or(Err(Error::InvalidInput("the worker published no result")));
            match (self.pending.remove(&id), outcome) {
                (Some(key), Ok(pipeline)) => {
                    self.pipelines.insert(key, pipeline);
                    self.pipelines_built += 1;
                }
                (Some(key), Err(error)) => {
                    self.rejected.insert(key, error.to_string());
                }
                (None, Ok(pipeline)) => {
                    // The session dropped this build; nobody can reference the pipeline.
                    // SAFETY: it was created by the worker and never recorded into a frame.
                    unsafe { self.context.device.destroy_pipeline(pipeline, None) };
                }
                (None, Err(_)) => {}
            }
        }
    }

    fn retarget(&mut self, width: u32, height: u32) -> Result<(), Error> {
        let device = &self.context.device;
        destroy_target(device, &mut self.target);
        self.target = create_target(device, &self.context, width, height, self.render_pass)?;
        if self.readback != vk::Buffer::null() {
            // SAFETY: retarget only runs between frames, so no submission is pending.
            unsafe {
                device.destroy_buffer(self.readback, None);
                device.free_memory(self.readback_memory, None);
            }
            self.readback = vk::Buffer::null();
        }
        let size = usize::try_from(u64::from(width) * u64::from(height) * 4)
            .map_err(|_| Error::InvalidInput("render target is too large to read back"))?;
        let info = vk::BufferCreateInfo::default()
            .size(size as u64)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: the size is positive and the usage is core.
        self.readback =
            result("create readback buffer", unsafe { device.create_buffer(&info, None) })?;
        // SAFETY: the buffer belongs to this device.
        let requirements = unsafe { device.get_buffer_memory_requirements(self.readback) };
        let (index, coherent) = self.context.memory_type(
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE,
            vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        self.readback_coherent = coherent;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(index);
        // SAFETY: size and type come from the buffer's own requirements.
        self.readback_memory =
            result("allocate readback memory", unsafe { device.allocate_memory(&allocate, None) })?;
        // SAFETY: the allocation covers the buffer and offset 0 is aligned.
        result("bind readback memory", unsafe {
            device.bind_buffer_memory(self.readback, self.readback_memory, 0)
        })?;
        self.target_size = size;
        Ok(())
    }

    /// Move the sampler image into the layout its descriptors advertise.
    ///
    /// Without this the first sample would read an image that is still in `UNDEFINED`, which
    /// is undefined behaviour rather than a readable value.
    fn transition_sampler(&self) {
        let Some(dummy) = self.sampler else {
            return;
        };
        let device = &self.context.device;
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(self.context.queue_family)
            .flags(vk::CommandPoolCreateFlags::TRANSIENT);
        // SAFETY: the queue family was negotiated during context creation.
        let Ok(pool) = result("create transition pool", unsafe {
            device.create_command_pool(&pool_info, None)
        }) else {
            return;
        };
        let allocate = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: the pool is valid and outlives this buffer.
        let buffers = unsafe { device.allocate_command_buffers(&allocate) };
        let Ok(buffers) = buffers else {
            // SAFETY: the pool was created above and is destroyed here.
            unsafe { device.destroy_command_pool(pool, None) };
            return;
        };
        let Some(command) = buffers.first().copied() else {
            // SAFETY: as above.
            unsafe { device.destroy_command_pool(pool, None) };
            return;
        };
        let barrier = [vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(dummy.image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            )];
        let outcome = (|| -> Result<(), Error> {
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            // SAFETY: fresh primary buffer, not recording or pending.
            result("begin transition buffer", unsafe {
                device.begin_command_buffer(command, &begin)
            })?;
            // SAFETY: the barrier describes the whole image and the layout change is legal.
            unsafe {
                device.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &barrier,
                );
            }
            // SAFETY: recording is complete.
            result("end transition buffer", unsafe { device.end_command_buffer(command) })?;
            let commands = [command];
            let submits = [vk::SubmitInfo::default().command_buffers(&commands)];
            // SAFETY: the queue accepts graphics work and the buffer is executable.
            result("submit transition", unsafe {
                device.queue_submit(self.context.queue, &submits, vk::Fence::null())
            })?;
            // SAFETY: no other submission is in flight on this queue: the session has not
            // drawn yet.
            result("wait for transition", unsafe { device.queue_wait_idle(self.context.queue) })
        })();
        // SAFETY: the queue is idle, so the buffer and pool can be freed.
        unsafe {
            device.free_command_buffers(pool, std::slice::from_ref(&command));
            device.destroy_command_pool(pool, None);
        }
        if let Err(error) = outcome {
            eprintln!("hematite: sampler layout transition failed: {error}");
        }
    }
}

impl Target {
    fn empty() -> Self {
        Self {
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
            framebuffer: vk::Framebuffer::null(),
            width: 0,
            height: 0,
        }
    }
}

/// Run pipeline builds until the queue closes.
///
/// The worker only reads the shared context handles and publishes finished pipelines through
/// the mutex-protected list, so it shares no mutable state with the submitter.
fn spawn_worker(
    receiver: Receiver<Arc<Job>>,
    finished: Arc<Mutex<Vec<Arc<Job>>>>,
) -> Option<std::thread::JoinHandle<()>> {
    let started = std::thread::Builder::new().name("hematite-pipelines".into()).spawn(move || {
        while let Ok(job) = receiver.recv() {
            let outcome = build_pipeline(&job);
            if let Ok(mut guard) = job.outcome.lock() {
                *guard = Some(outcome);
            }
            if let Ok(mut queue) = finished.lock() {
                queue.push(Arc::clone(&job));
            }
        }
    });
    match started {
        Ok(handle) => Some(handle),
        Err(error) => {
            // A renderer that cannot spawn a thread still works: the send fails, every draw
            // reports "pipeline not ready", and the caller falls back to software.
            eprintln!("hematite: cannot spawn the pipeline worker: {error}");
            None
        }
    }
}

/// Build the pipeline this job describes.
fn build_pipeline(job: &Job) -> Result<vk::Pipeline, Error> {
    let device = &job.context.device;
    let vertex_info = vk::ShaderModuleCreateInfo::default().code(&job.key.stages.vertex);
    let fragment_info = vk::ShaderModuleCreateInfo::default().code(&job.key.stages.fragment);
    // SAFETY: the word vectors are owned by the job and outlive the calls; both modules are
    // destroyed before this function returns.
    let vertex_module = unsafe { device.create_shader_module(&vertex_info, None) }
        .map_err(|error| Error::Vulkan("create vertex shader module", error))?;
    let fragment_module = match unsafe { device.create_shader_module(&fragment_info, None) } {
        Ok(module) => module,
        Err(error) => {
            // SAFETY: the vertex module was created and is unused.
            unsafe { device.destroy_shader_module(vertex_module, None) };
            return Err(Error::Vulkan("create fragment shader module", error));
        }
    };
    let pipeline = create_graphics_pipeline(device, job, vertex_module, fragment_module);
    // SAFETY: both modules were created above and are not used again.
    unsafe {
        device.destroy_shader_module(vertex_module, None);
        device.destroy_shader_module(fragment_module, None);
    }
    pipeline
}

fn create_graphics_pipeline(
    device: &Device,
    job: &Job,
    vertex_module: vk::ShaderModule,
    fragment_module: vk::ShaderModule,
) -> Result<vk::Pipeline, Error> {
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vertex_module)
            .name(c"main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(fragment_module)
            .name(c"main"),
    ];
    let binding = [vk::VertexInputBindingDescription::default()
        .binding(0)
        .stride(job.key.layout.stride)
        .input_rate(vk::VertexInputRate::VERTEX)];
    let bindings: &[vk::VertexInputBindingDescription] =
        if job.key.layout.count == 0 { &[] } else { &binding };
    let attributes: Vec<vk::VertexInputAttributeDescription> = job
        .key
        .layout
        .used()
        .iter()
        .map(|attribute| {
            vk::VertexInputAttributeDescription::default()
                .location(attribute.location)
                .binding(0)
                .format(attribute.format)
                .offset(attribute.offset)
        })
        .collect();
    let input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(bindings)
        .vertex_attribute_descriptions(&attributes);
    let assembly = vk::PipelineInputAssemblyStateCreateInfo::default().topology(job.key.topology);
    let [width, height] = job.key.state.viewport;
    let viewports = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: width as f32,
        height: height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    let scissors = [match job.key.state.scissor {
        Some([x, y, w, h]) => vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D { width: w.max(0) as u32, height: h.max(0) as u32 },
        },
        None => vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D { width, height },
        },
    }];
    let viewport =
        vk::PipelineViewportStateCreateInfo::default().viewports(&viewports).scissors(&scissors);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let attachment = vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(job.key.state.blend)
        .src_color_blend_factor(job.key.state.src_factor)
        .dst_color_blend_factor(job.key.state.dst_factor)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(job.key.state.src_factor)
        .dst_alpha_blend_factor(job.key.state.dst_factor)
        .alpha_blend_op(vk::BlendOp::ADD)
        .color_write_mask(vk::ColorComponentFlags::RGBA);
    let blend_attachments = [attachment];
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
    let info = [vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&input)
        .input_assembly_state(&assembly)
        .viewport_state(&viewport)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .layout(job.layouts.handle)
        .render_pass(job.render_pass)
        .subpass(0)];
    // SAFETY: every handle is live, the arrays outlive the call, and the vertex input
    // description was built from the same key as the shader interface check.
    match unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &info, None) } {
        Ok(pipelines) => Ok(pipelines[0]),
        Err((_pipelines, error)) => Err(Error::Vulkan("create graphics pipeline", error)),
    }
}

fn create_render_pass(device: &Device) -> Result<vk::RenderPass, Error> {
    let attachments = [vk::AttachmentDescription::default()
        .format(vk::Format::R8G8B8A8_UNORM)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)];
    let color = [vk::AttachmentReference {
        attachment: 0,
        layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
    }];
    let subpasses = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color)];
    let dependencies = [
        vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::TRANSFER)
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE),
        vk::SubpassDependency::default()
            .src_subpass(0)
            .dst_subpass(vk::SUBPASS_EXTERNAL)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::TRANSFER)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ),
    ];
    let info = vk::RenderPassCreateInfo::default()
        .attachments(&attachments)
        .subpasses(&subpasses)
        .dependencies(&dependencies);
    // SAFETY: attachment 0 exists and every array outlives the call.
    result("create session render pass", unsafe { device.create_render_pass(&info, None) })
}

fn create_set_layout(device: &Device, sampler: bool) -> Result<vk::DescriptorSetLayout, Error> {
    let uniform = vk::DescriptorSetLayoutBinding::default()
        .binding(UNIFORM_BINDING)
        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT);
    let image = vk::DescriptorSetLayoutBinding::default()
        .binding(SAMPLER_BINDING)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::FRAGMENT);
    let bindings: Vec<vk::DescriptorSetLayoutBinding> =
        if sampler { vec![uniform, image] } else { vec![uniform] };
    let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
    // SAFETY: the binding array outlives the call.
    result("create descriptor set layout", unsafe {
        device.create_descriptor_set_layout(&info, None)
    })
}

fn create_descriptor_pool(device: &Device) -> Result<vk::DescriptorPool, Error> {
    let sizes = [
        vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::UNIFORM_BUFFER)
            .descriptor_count(DESCRIPTOR_SETS),
        vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(DESCRIPTOR_SETS),
    ];
    // A session with a sampler allocates DESCRIPTOR_SETS of each layout, so the pool must
    // hold twice that many sets.
    let info =
        vk::DescriptorPoolCreateInfo::default().max_sets(DESCRIPTOR_SETS * 2).pool_sizes(&sizes);
    // SAFETY: the pool sizes cover every set the session allocates.
    result("create descriptor pool", unsafe { device.create_descriptor_pool(&info, None) })
}

fn create_pipeline_layout(
    device: &Device,
    set_layout: vk::DescriptorSetLayout,
) -> Result<vk::PipelineLayout, Error> {
    let layouts = [set_layout];
    // No push constant ranges: see the module documentation for the two measured reasons.
    let info = vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts);
    // SAFETY: the set layout is alive for the session.
    result("create pipeline layout", unsafe { device.create_pipeline_layout(&info, None) })
}

fn create_target(
    device: &Device,
    context: &Context,
    width: u32,
    height: u32,
    render_pass: vk::RenderPass,
) -> Result<Target, Error> {
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
        .extent(vk::Extent3D { width, height, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    // SAFETY: the caller validated the dimensions against the device limit.
    let image = result("create target image", unsafe { device.create_image(&info, None) })?;
    // SAFETY: the image belongs to this device.
    let requirements = unsafe { device.get_image_memory_requirements(image) };
    let (index, _) = context.memory_type(
        requirements.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )?;
    let allocate = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(index);
    // SAFETY: size and type come from the image's requirements.
    let memory =
        result("allocate target memory", unsafe { device.allocate_memory(&allocate, None) })?;
    // SAFETY: the allocation covers the image and offset 0 is aligned.
    result("bind target memory", unsafe { device.bind_image_memory(image, memory, 0) })?;
    let range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);
    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
        .subresource_range(range);
    // SAFETY: the view covers the single valid mip and layer of this bound image.
    let view = result("create target view", unsafe { device.create_image_view(&view_info, None) })?;
    let framebuffer_info = vk::FramebufferCreateInfo::default()
        .render_pass(render_pass)
        .attachments(std::slice::from_ref(&view))
        .width(width)
        .height(height)
        .layers(1);
    // SAFETY: the attachment matches the render pass's single attachment and dimensions.
    let framebuffer = result("create target framebuffer", unsafe {
        device.create_framebuffer(&framebuffer_info, None)
    })?;
    Ok(Target { image, memory, view, framebuffer, width, height })
}

fn destroy_target(device: &Device, target: &mut Target) {
    if target.framebuffer == vk::Framebuffer::null() {
        return;
    }
    // SAFETY: every handle came from create_target and no submission is pending, because
    // retarget and Drop only run between frames.
    unsafe {
        device.destroy_framebuffer(target.framebuffer, None);
        device.destroy_image_view(target.view, None);
        device.destroy_image(target.image, None);
        device.free_memory(target.memory, None);
    }
    *target = Target::empty();
}

fn create_dummy(device: &Device, context: &Context) -> Result<Dummy, Error> {
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
        .extent(vk::Extent3D { width: 1, height: 1, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    // SAFETY: a 1x1 RGBA8 image is supported by every Vulkan device.
    let image = result("create sampler image", unsafe { device.create_image(&info, None) })?;
    // SAFETY: the image belongs to this device.
    let requirements = unsafe { device.get_image_memory_requirements(image) };
    let (index, _) = context.memory_type(
        requirements.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )?;
    let allocate = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(index);
    // SAFETY: size and type come from the image's requirements.
    let memory =
        result("allocate sampler memory", unsafe { device.allocate_memory(&allocate, None) })?;
    // SAFETY: the allocation covers the image.
    result("bind sampler memory", unsafe { device.bind_image_memory(image, memory, 0) })?;
    let range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);
    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
        .subresource_range(range);
    // SAFETY: the view covers the single valid mip and layer.
    let view =
        result("create sampler view", unsafe { device.create_image_view(&view_info, None) })?;
    let sampler_info = vk::SamplerCreateInfo::default()
        .mag_filter(vk::Filter::NEAREST)
        .min_filter(vk::Filter::NEAREST)
        .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .max_lod(0.0);
    // SAFETY: every parameter is core and inside the device limits.
    let sampler = result("create sampler", unsafe { device.create_sampler(&sampler_info, None) })?;
    Ok(Dummy { image, memory, view, sampler })
}

fn create_ring(
    device: &Device,
    context: &Context,
    capacity: u64,
    vertex_usage: bool,
) -> Result<Ring, Error> {
    let usage = if vertex_usage {
        vk::BufferUsageFlags::VERTEX_BUFFER
    } else {
        vk::BufferUsageFlags::UNIFORM_BUFFER
    };
    let info = vk::BufferCreateInfo::default()
        .size(capacity)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: the size is positive and the usage is core.
    let buffer = result("create ring", unsafe { device.create_buffer(&info, None) })?;
    // SAFETY: the buffer belongs to this device.
    let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
    let (index, coherent) = context.memory_type(
        requirements.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE,
        vk::MemoryPropertyFlags::HOST_COHERENT,
    )?;
    let allocate = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(index);
    // SAFETY: size and type come from the buffer's requirements.
    let memory = result("allocate ring", unsafe { device.allocate_memory(&allocate, None) })?;
    // SAFETY: the allocation covers the buffer and offset 0 is aligned.
    result("bind ring", unsafe { device.bind_buffer_memory(buffer, memory, 0) })?;
    // SAFETY: the allocation is HOST_VISIBLE and stays mapped for the session's lifetime.
    let mapped = result("map ring", unsafe {
        device.map_memory(memory, 0, requirements.size, vk::MemoryMapFlags::empty())
    })?;
    Ok(Ring {
        buffer,
        memory,
        coherent,
        capacity: requirements.size,
        mapped: mapped.cast::<u8>(),
        used: 0,
    })
}

impl Drop for Session {
    fn drop(&mut self) {
        let device = &self.context.device;
        // The worker holds jobs that reference this context, so it is stopped and joined
        // before any device handle below is destroyed.
        if let Some(worker) = self.worker.take() {
            // Dropping the sender closes the channel, which ends the worker's recv loop;
            // joining then guarantees no job can touch the device after this point.
            let Worker { sender, thread, .. } = worker;
            drop(sender);
            if let Some(handle) = thread {
                let _ = handle.join();
            }
        }
        for pipeline in self.pipelines.values() {
            // SAFETY: built by this session's worker and not in use.
            unsafe { device.destroy_pipeline(*pipeline, None) };
        }
        for ring in [&mut self.uniforms, &mut self.vertices] {
            if !ring.mapped.is_null() {
                // SAFETY: mapped by create_ring and never handed out.
                unsafe { device.unmap_memory(ring.memory) };
            }
        }
        if self.command != vk::CommandBuffer::null() {
            self.spent.push(self.command);
            self.command = vk::CommandBuffer::null();
        }
        if !self.spent.is_empty() {
            // SAFETY: allocated from this session's pool, which is destroyed below.
            unsafe { device.free_command_buffers(self.pool, &self.spent) };
            self.spent.clear();
        }
        if self.readback != vk::Buffer::null() {
            // SAFETY: created by retarget and not in use.
            unsafe {
                device.destroy_buffer(self.readback, None);
                device.free_memory(self.readback_memory, None);
            }
        }
        destroy_target(device, &mut self.target);
        if let Some(dummy) = self.sampler.take() {
            // SAFETY: created by create_dummy and not in use.
            unsafe {
                device.destroy_sampler(dummy.sampler, None);
                device.destroy_image_view(dummy.view, None);
                device.destroy_image(dummy.image, None);
                device.free_memory(dummy.memory, None);
            }
        }
        for ring in [&self.uniforms, &self.vertices] {
            if ring.buffer != vk::Buffer::null() {
                // SAFETY: created by create_ring, unmapped above and not in use.
                unsafe {
                    device.destroy_buffer(ring.buffer, None);
                    device.free_memory(ring.memory, None);
                }
            }
        }
        if self.fence != vk::Fence::null() {
            // SAFETY: created by new() and not in use.
            unsafe { device.destroy_fence(self.fence, None) };
        }
        if self.pool != vk::CommandPool::null() {
            // SAFETY: created by new(); its buffers were freed above.
            unsafe { device.destroy_command_pool(self.pool, None) };
        }
        for layout in [self.sampler_layout, self.plain_layout] {
            if layout != vk::PipelineLayout::null() {
                // SAFETY: created by new(); no live pipeline references it any more.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
        }
        if self.descriptor_pool != vk::DescriptorPool::null() {
            // SAFETY: the sets allocated from it are not in use.
            unsafe { device.destroy_descriptor_pool(self.descriptor_pool, None) };
        }
        for layout in [self.sampler_set_layout, self.plain_set_layout] {
            if layout != vk::DescriptorSetLayout::null() {
                // SAFETY: created by new(); the pool that used it was destroyed above.
                unsafe { device.destroy_descriptor_set_layout(layout, None) };
            }
        }
        if self.render_pass != vk::RenderPass::null() {
            // SAFETY: created by new(); the target framebuffer was destroyed above.
            unsafe { device.destroy_render_pass(self.render_pass, None) };
        }
    }
}
