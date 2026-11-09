//! Recorded commands.
//!
//! Design constraints, in order:
//!
//! 1. `Command` must stay small enough that pushing one is cheaper than doing the GL call
//!    it represents. The largest variant is a draw; it is deliberately kept under 64 bytes
//!    by putting optional parts behind ids rather than inline vectors.
//! 2. Bulk data (vertex bytes, uniform bytes) is referred to by `(buffer, offset, len)` for
//!    future ring-buffer integration. `SetState` owns a box and may allocate.

use crate::state::StateBlock;

/// A GL object name, already resolved to the backend's resource slot.
///
/// `ResourceId::NONE` (0) is GL's conventional "no object" and is never a valid resource,
/// which is what lets the frontend keep GL's zero-means-unbound semantics without an
/// `Option` in the hot struct.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct ResourceId(pub u32);

impl ResourceId {
    /// The "no object" id, matching GL name 0.
    pub const NONE: Self = Self(0);

    /// Whether this id refers to a real object.
    #[inline]
    pub const fn is_some(self) -> bool {
        self.0 != 0
    }
}

/// A resolved pipeline (program + fixed state) to bind.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct PipelineId(pub u32);

/// Primitive topology, after any conversion from GL's quads/polygons.
///
/// GL's `GL_QUADS`, `GL_QUAD_STRIP`, `GL_POLYGON` and `GL_LINE_LOOP` never reach a backend:
/// the frontend converts them (see `hematite-glsl`/`hematite-fixed-function` for the
/// fixed-function side and the frontend's topology module for the general case), because no
/// modern API has them.
#[derive(Clone, Copy, PartialEq, Hash, Debug)]
#[repr(u8)]
pub enum Topology {
    /// Independent triangles.
    Triangles,
    /// Triangle strip.
    TriangleStrip,
    /// Triangle fan.
    TriangleFan,
    /// Independent lines.
    Lines,
    /// Line strip.
    LineStrip,
    /// Looped line strip, already closed by the converter.
    LineLoop,
    /// Independent points.
    Points,
    /// Patches for tessellation, with the vertex count per patch.
    Patches(u8),
}

/// How a draw gets its indices.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum IndexRef {
    /// Non-indexed: `glDrawArrays` and friends.
    None,
    /// Indices from a bound buffer.
    Buffer {
        /// Buffer holding the indices.
        buffer: ResourceId,
        /// Byte offset into that buffer.
        offset: u32,
        /// Index element width in bytes (1, 2 or 4).
        width: u8,
    },
    /// Indices from client memory, already copied into the streaming ring.
    Streamed {
        /// Byte offset into the context's index ring.
        offset: u32,
        /// Index element width in bytes.
        width: u8,
    },
}

/// One draw call, after state resolution.
///
/// This is intentionally *not* one-to-one with a GL call: a single recorded draw may cover a
/// merged run of compatible draws, and a GL call may produce zero recorded draws (when a
/// pipeline is not ready and the skip policy applies).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct DrawDesc {
    /// Primitive topology.
    pub topology: Topology,
    /// Indices, if any.
    pub index: IndexRef,
    /// Vertices (or indices) to draw.
    pub count: u32,
    /// Instances to draw; 1 for a non-instanced draw.
    pub instances: u32,
    /// First vertex, or first index into the index buffer.
    pub first: u32,
    /// `glDrawArraysInstancedBaseInstance` base instance.
    pub base_instance: u32,
    /// `glDrawElementsBaseVertex` base vertex.
    pub base_vertex: i32,
}

impl DrawDesc {
    /// A non-instanced, non-indexed draw.
    #[inline]
    pub const fn arrays(topology: Topology, first: u32, count: u32) -> Self {
        Self {
            topology,
            index: IndexRef::None,
            count,
            instances: 1,
            first,
            base_instance: 0,
            base_vertex: 0,
        }
    }

    /// True when the draw would produce no fragments and may be dropped.
    ///
    /// GL permits `count == 0` (it is a no-op, but still an error check point) and
    /// `instances == 0`. Dropping these here is one of the cheapest wins available,
    /// because Minecraft emits them in some UI paths every frame.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.count == 0 || self.instances == 0
    }
}

/// A recorded unit of work.
///
/// The size of this enum is asserted by a test; changing it is a deliberate decision
/// because it sits on the render thread's hottest path.
#[derive(Clone, PartialEq, Debug)]
pub enum Command {
    /// Bind the resolved pipeline for subsequent draws.
    BindPipeline(PipelineId),

    /// Replace the whole dynamic-state block.
    ///
    /// Sending the entire block rather than per-field commands keeps the backend simple and
    /// lets it diff once; the frontend has already established that the block differs.
    SetState(Box<StateBlock>),

    /// Bind a buffer to a shader storage / uniform / vertex slot.
    BindBuffer {
        /// Binding slot within the declared usage.
        slot: u16,
        /// The buffer.
        buffer: ResourceId,
        /// Byte offset of the bound range.
        offset: u32,
        /// Byte size of the bound range, or 0 for "to the end".
        size: u32,
    },

    /// Bind a texture and sampler to a texture unit.
    BindTexture {
        /// Texture unit.
        unit: u8,
        /// The texture's view, 0 to unbind.
        view: ResourceId,
        /// Sampler object, or `ResourceId::NONE` to use the texture's own parameters.
        sampler: ResourceId,
    },

    /// Bind a vertex array object.
    BindVertexArray(ResourceId),

    /// Bind a framebuffer as the render target.
    BindFramebuffer(ResourceId),

    /// Draw.
    Draw(DrawDesc),

    /// Draw several ranges from a buffer of `VkDrawIndirect`-shaped commands.
    DrawIndirect {
        /// Buffer holding the indirect commands.
        buffer: ResourceId,
        /// Byte offset of the first command.
        offset: u32,
        /// Command count, or 0 to read the count from the buffer.
        count: u32,
        /// Stride between commands in bytes.
        stride: u32,
    },

    /// Dispatch a compute grid.
    Dispatch {
        /// Workgroup count in X.
        x: u32,
        /// Workgroup count in Y.
        y: u32,
        /// Workgroup count in Z.
        z: u32,
    },

    /// Dispatch with workgroup counts read from a buffer.
    DispatchIndirect {
        /// Buffer holding the counts.
        buffer: ResourceId,
        /// Byte offset.
        offset: u32,
    },

    /// Copy between buffers.
    CopyBuffer {
        /// Source buffer.
        src: ResourceId,
        /// Source byte offset.
        src_offset: u32,
        /// Destination buffer.
        dst: ResourceId,
        /// Destination byte offset.
        dst_offset: u32,
        /// Bytes to copy.
        size: u32,
    },

    /// Clear a whole image.
    ClearImage {
        /// Target image.
        image: ResourceId,
        /// Value to write, interpreted according to the format.
        value: ClearValue,
    },

    /// Emit a barrier for the given GL memory-barrier bits, translated by the backend.
    MemoryBarrier(u32),

    /// Begin a render pass on the currently bound framebuffer, if the backend has deferred
    /// it this far. Backends that track passes lazily treat this as a hint.
    BeginRenderPass,

    /// End the current render pass.
    EndRenderPass,

    /// Resolve a multisampled image into a single-sampled one.
    Resolve {
        /// Multisampled source.
        src: ResourceId,
        /// Destination.
        dst: ResourceId,
    },

    /// Read pixels back into a buffer; the frontend waits on `fence_value`.
    Readback {
        /// Source image.
        image: ResourceId,
        /// Destination buffer.
        buffer: ResourceId,
        /// Byte offset in the destination.
        offset: u32,
        /// Rectangle origin and extent: `x, y, width, height`.
        rect: [i32; 4],
        /// Value the backend bumps on the context's timeline semaphore when the copy retires.
        fence_value: u64,
    },

    /// Insert a GPU-side timestamp for the profiler.
    Timestamp(u32),

    /// Present the current swapchain image; only EGL emits this.
    Present,
}

/// A clear value, matching the destination image's format class.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ClearValue {
    /// Normalised or floating-point colour.
    Color([f32; 4]),
    /// Depth only; the stencil byte is preserved.
    Depth(f32),
    /// Depth and stencil together.
    DepthStencil(f32, u32),
    /// Stencil only; the depth float is preserved.
    Stencil(u32),
}

impl ClearValue {
    /// The all-zero value, which Vulkan treats as "no value" only when the aspect is absent.
    pub const ZERO: Self = Self::Color([0.0; 4]);
}

/// Returns the size of `Command` in bytes.
///
/// Exposed for tests and for the profiler's recording-overhead accounting.
pub const fn command_size() -> usize {
    core::mem::size_of::<Command>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_id_zero_is_none() {
        assert!(!ResourceId::NONE.is_some());
        assert!(ResourceId(1).is_some());
    }

    #[test]
    fn empty_draws_are_detected() {
        let mut d = DrawDesc::arrays(Topology::Triangles, 0, 3);
        assert!(!d.is_empty());
        d.count = 0;
        assert!(d.is_empty());
        d.count = 3;
        d.instances = 0;
        assert!(d.is_empty());
    }

    /// The render thread pushes these; if this grows, the decision to grow it must be
    /// explicit because it directly costs per-call memory bandwidth.
    #[test]
    fn command_stays_small() {
        assert!(
            command_size() <= 72,
            "Command grew to {} bytes; justify this in DECISIONS.md",
            command_size()
        );
    }

    #[test]
    fn state_block_is_comparable() {
        let a = StateBlock::default();
        let b = StateBlock::default();
        assert_eq!(a, b);
    }

    #[test]
    fn topology_patch_roundtrips() {
        assert_eq!(Topology::Patches(4), Topology::Patches(4));
        assert_ne!(Topology::Patches(3), Topology::Patches(4));
    }

    #[test]
    fn clear_values_compare() {
        assert_eq!(ClearValue::Color([0.0; 4]), ClearValue::ZERO);
        assert_ne!(ClearValue::Depth(1.0), ClearValue::Stencil(0));
    }
}
