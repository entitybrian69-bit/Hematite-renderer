//! Offscreen render-pass/pipeline/transfer implementation. All resources are per-call
//! owned and fence-completed before destruction. This correctness path is not optimized.

use crate::driver::Context;
use crate::{Error, Image, Vertex};
use ash::{vk, Device};
use std::io::Cursor;

fn result<T>(operation: &'static str, value: Result<T, vk::Result>) -> Result<T, Error> {
    value.map_err(|error| Error::Vulkan(operation, error))
}

struct Frame<'a> {
    device: &'a Device,
    image: vk::Image,
    image_memory: vk::DeviceMemory,
    view: vk::ImageView,
    readback: vk::Buffer,
    readback_memory: vk::DeviceMemory,
    readback_coherent: bool,
    vertices: vk::Buffer,
    vertex_memory: vk::DeviceMemory,
    render_pass: vk::RenderPass,
    framebuffer: vk::Framebuffer,
    vertex_shader: vk::ShaderModule,
    fragment_shader: vk::ShaderModule,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pool: vk::CommandPool,
    fence: vk::Fence,
    submitted: bool,
}

impl<'a> Frame<'a> {
    fn new(device: &'a Device) -> Self {
        Self {
            device,
            image: vk::Image::null(),
            image_memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
            readback: vk::Buffer::null(),
            readback_memory: vk::DeviceMemory::null(),
            readback_coherent: false,
            vertices: vk::Buffer::null(),
            vertex_memory: vk::DeviceMemory::null(),
            render_pass: vk::RenderPass::null(),
            framebuffer: vk::Framebuffer::null(),
            vertex_shader: vk::ShaderModule::null(),
            fragment_shader: vk::ShaderModule::null(),
            layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            pool: vk::CommandPool::null(),
            fence: vk::Fence::null(),
            submitted: false,
        }
    }

    fn attachment(&mut self, context: &Context, width: u32, height: u32) -> Result<(), Error> {
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
        // SAFETY: validated positive dimensions within device limits, live device,
        // no extensions or external storage; ownership is immediately recorded in Frame.
        self.image = result("create image", unsafe { self.device.create_image(&info, None) })?;
        // SAFETY: image was created on this device and is still alive.
        let requirements = unsafe { self.device.get_image_memory_requirements(self.image) };
        let (index, _) = context.memory_type(
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(index);
        // SAFETY: allocation size/type come from the image's queried requirements.
        self.image_memory =
            result("allocate image memory", unsafe { self.device.allocate_memory(&alloc, None) })?;
        // SAFETY: compatible, sufficiently large fresh allocation, aligned zero offset.
        result("bind image memory", unsafe {
            self.device.bind_image_memory(self.image, self.image_memory, 0)
        })?;
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .base_mip_level(0)
            .level_count(1)
            .base_array_layer(0)
            .layer_count(1);
        let view = vk::ImageViewCreateInfo::default()
            .image(self.image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .subresource_range(range);
        // SAFETY: view covers the single valid mip/layer of this bound image.
        self.view =
            result("create image view", unsafe { self.device.create_image_view(&view, None) })?;
        Ok(())
    }

    fn buffers(
        &mut self,
        context: &Context,
        pixels: u64,
        vertices: &[Vertex],
    ) -> Result<(), Error> {
        // Record each handle immediately so a later failure frees all earlier resources.
        let readback = vk::BufferCreateInfo::default()
            .size(pixels)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: pixels is a validated nonzero image byte count, supported core usage.
        self.readback = result("create readback buffer", unsafe {
            self.device.create_buffer(&readback, None)
        })?;
        // SAFETY: created buffer belongs to this live device.
        let req = unsafe { self.device.get_buffer_memory_requirements(self.readback) };
        let (index, coherent) = context.memory_type(
            req.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE,
            vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        self.readback_coherent = coherent;
        let alloc =
            vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(index);
        // SAFETY: allocation parameters are from the buffer requirements and queried memory types.
        self.readback_memory =
            result("allocate readback", unsafe { self.device.allocate_memory(&alloc, None) })?;
        // SAFETY: compatible fresh allocation covers the buffer, zero offset is aligned.
        result("bind readback memory", unsafe {
            self.device.bind_buffer_memory(self.readback, self.readback_memory, 0)
        })?;

        let mut bytes = Vec::with_capacity(vertices.len() * 24);
        for vertex in vertices {
            for value in vertex.position.iter().chain(&vertex.color) {
                bytes.extend_from_slice(&value.to_ne_bytes());
            }
        }
        let buffer = vk::BufferCreateInfo::default()
            .size((bytes.len() as u64).max(4))
            .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: bounded positive size, vertex buffer usage is core Vulkan.
        self.vertices =
            result("create vertex buffer", unsafe { self.device.create_buffer(&buffer, None) })?;
        // SAFETY: live vertex buffer was created on this device.
        let req = unsafe { self.device.get_buffer_memory_requirements(self.vertices) };
        let (index, coherent) = context.memory_type(
            req.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE,
            vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let alloc =
            vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(index);
        // SAFETY: queried type and size satisfy the vertex buffer's memory requirements.
        self.vertex_memory =
            result("allocate vertices", unsafe { self.device.allocate_memory(&alloc, None) })?;
        // SAFETY: bound once at aligned offset zero with a compatible allocation.
        result("bind vertex memory", unsafe {
            self.device.bind_buffer_memory(self.vertices, self.vertex_memory, 0)
        })?;
        if !bytes.is_empty() {
            // SAFETY: memory is HOST_VISIBLE, not mapped elsewhere, whole allocation mapped.
            let pointer = result("map vertices", unsafe {
                self.device.map_memory(
                    self.vertex_memory,
                    0,
                    vk::WHOLE_SIZE,
                    vk::MemoryMapFlags::empty(),
                )
            })?;
            // SAFETY: mapping has at least buffer size bytes, bytes is a separate owned Vec;
            // no GPU operation has started and ranges do not overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast(), bytes.len());
            }
            let flush = if coherent {
                Ok(())
            } else {
                let range = [vk::MappedMemoryRange::default()
                    .memory(self.vertex_memory)
                    .offset(0)
                    .size(vk::WHOLE_SIZE)];
                // SAFETY: range covers the mapped allocation. Whole-size at offset zero
                // satisfies nonCoherentAtomSize alignment without guessing its value.
                result("flush vertices", unsafe { self.device.flush_mapped_memory_ranges(&range) })
            };
            // SAFETY: exactly this allocation is currently mapped and GPU has not used it.
            unsafe {
                self.device.unmap_memory(self.vertex_memory);
            }
            flush?;
        }
        Ok(())
    }

    fn pass(&mut self, width: u32, height: u32) -> Result<(), Error> {
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
                .src_stage_mask(vk::PipelineStageFlags::TOP_OF_PIPE)
                .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
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
        // SAFETY: all attachment/subpass indices are valid, arrays live throughout call.
        self.render_pass =
            result("create render pass", unsafe { self.device.create_render_pass(&info, None) })?;
        let views = [self.view];
        let info = vk::FramebufferCreateInfo::default()
            .render_pass(self.render_pass)
            .attachments(&views)
            .width(width)
            .height(height)
            .layers(1);
        // SAFETY: live view matches render pass format/sample count and validated dimensions.
        self.framebuffer =
            result("create framebuffer", unsafe { self.device.create_framebuffer(&info, None) })?;
        Ok(())
    }

    fn pipeline(&mut self, width: u32, height: u32) -> Result<(), Error> {
        let vertex =
            ash::util::read_spv(&mut Cursor::new(include_bytes!("../shaders/offscreen.vert.spv")))
                .map_err(|_| Error::InvalidInput("invalid built-in vertex SPIR-V"))?;
        let fragment =
            ash::util::read_spv(&mut Cursor::new(include_bytes!("../shaders/offscreen.frag.spv")))
                .map_err(|_| Error::InvalidInput("invalid built-in fragment SPIR-V"))?;
        let vert_info = vk::ShaderModuleCreateInfo::default().code(&vertex);
        let frag_info = vk::ShaderModuleCreateInfo::default().code(&fragment);
        // SAFETY: aligned SPIR-V words generated from the checked-in original shader sources.
        self.vertex_shader = result("create vertex shader", unsafe {
            self.device.create_shader_module(&vert_info, None)
        })?;
        // SAFETY: same validated word representation for the fragment stage.
        self.fragment_shader = result("create fragment shader", unsafe {
            self.device.create_shader_module(&frag_info, None)
        })?;
        let layout = vk::PipelineLayoutCreateInfo::default();
        // SAFETY: no descriptors or push constants are accessed by these shaders.
        self.layout = result("create pipeline layout", unsafe {
            self.device.create_pipeline_layout(&layout, None)
        })?;
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(self.vertex_shader)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(self.fragment_shader)
                .name(c"main"),
        ];
        let bindings = [vk::VertexInputBindingDescription {
            binding: 0,
            stride: 24,
            input_rate: vk::VertexInputRate::VERTEX,
        }];
        let attributes = [
            vk::VertexInputAttributeDescription {
                location: 0,
                binding: 0,
                format: vk::Format::R32G32_SFLOAT,
                offset: 0,
            },
            vk::VertexInputAttributeDescription {
                location: 1,
                binding: 0,
                format: vk::Format::R32G32B32A32_SFLOAT,
                offset: 8,
            },
        ];
        let input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&bindings)
            .vertex_attribute_descriptions(&attributes);
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewports = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissors = [vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D { width, height },
        }];
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewports(&viewports)
            .scissors(&scissors);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
        let info = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .layout(self.layout)
            .render_pass(self.render_pass)
            .subpass(0)];
        // SAFETY: all state/entrypoint slices outlive call, shader interfaces and vertex
        // layout match, render pass is compatible, no unenabled feature is requested.
        match unsafe {
            self.device.create_graphics_pipelines(vk::PipelineCache::null(), &info, None)
        } {
            Ok(pipelines) => self.pipeline = pipelines[0],
            Err((pipelines, error)) => {
                for pipeline in pipelines {
                    // SAFETY: Vulkan may return partial success handles; none has been used.
                    unsafe {
                        self.device.destroy_pipeline(pipeline, None);
                    }
                }
                return Err(Error::Vulkan("create graphics pipeline", error));
            }
        }
        Ok(())
    }

    fn submit(
        &mut self,
        context: &Context,
        width: u32,
        height: u32,
        clear: [f32; 4],
        vertex_count: u32,
    ) -> Result<(), Error> {
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(context.queue_family)
            .flags(vk::CommandPoolCreateFlags::TRANSIENT);
        // SAFETY: queue family was negotiated during Context creation.
        self.pool = result("create command pool", unsafe {
            self.device.create_command_pool(&pool_info, None)
        })?;
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: pool is valid, buffers freed with pool after completion.
        let command = result("allocate command buffer", unsafe {
            self.device.allocate_command_buffers(&alloc)
        })?[0];
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: fresh primary buffer, not pending or recording elsewhere.
        result("begin command buffer", unsafe {
            self.device.begin_command_buffer(command, &begin)
        })?;
        let clear_values = [vk::ClearValue { color: vk::ClearColorValue { float32: clear } }];
        let pass = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(self.framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D { width, height },
            })
            .clear_values(&clear_values);
        // SAFETY: all handles belong to this device; command is recording. Framebuffer,
        // pipeline and single attachment agree. Vertex byte count was bounded and uploaded.
        // Final render-pass dependency makes attachment writes visible to transfer reads.
        // Copy uses tight RGBA rows into a buffer sized width*height*4. The following
        // transfer-to-host barrier and fence are required before mapped CPU access.
        unsafe {
            self.device.cmd_begin_render_pass(command, &pass, vk::SubpassContents::INLINE);
            if vertex_count != 0 {
                self.device.cmd_bind_pipeline(
                    command,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline,
                );
                self.device.cmd_bind_vertex_buffers(command, 0, &[self.vertices], &[0]);
                self.device.cmd_draw(command, vertex_count, 1, 0, 0);
            }
            self.device.cmd_end_render_pass(command);
            let regions = [vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .mip_level(0)
                        .base_array_layer(0)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D { width, height, depth: 1 })];
            self.device.cmd_copy_image_to_buffer(
                command,
                self.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.readback,
                &regions,
            );
            let barrier = [vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(self.readback)
                .offset(0)
                .size(vk::WHOLE_SIZE)];
            self.device.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[],
                &barrier,
                &[],
            );
        }
        // SAFETY: render pass ended and command recording is otherwise complete.
        result("end command buffer", unsafe { self.device.end_command_buffer(command) })?;
        // SAFETY: fence has no special flags or extension state.
        self.fence = result("create fence", unsafe {
            self.device.create_fence(&vk::FenceCreateInfo::default(), None)
        })?;
        let commands = [command];
        let submits = [vk::SubmitInfo::default().command_buffers(&commands)];
        // SAFETY: queue has graphics/transfer support, context is exclusively borrowed by
        // public render, command is executable, fence is fresh and unsignaled.
        result("queue submit", unsafe {
            self.device.queue_submit(context.queue, &submits, self.fence)
        })?;
        self.submitted = true;
        // SAFETY: fence belongs to this submission; Frame retains all referenced resources.
        result("wait for readback", unsafe {
            self.device.wait_for_fences(&[self.fence], true, 10_000_000_000)
        })?;
        self.submitted = false;
        Ok(())
    }

    fn read(&self, size: usize) -> Result<Vec<u8>, Error> {
        let mut out = vec![0; size];
        // SAFETY: readback allocation is HOST_VISIBLE, not previously mapped; submission
        // has completed before read is called. Whole allocation covers size bytes.
        let pointer = result("map readback", unsafe {
            self.device.map_memory(
                self.readback_memory,
                0,
                vk::WHOLE_SIZE,
                vk::MemoryMapFlags::empty(),
            )
        })?;
        let invalidate = if self.readback_coherent {
            Ok(())
        } else {
            let ranges = [vk::MappedMemoryRange::default()
                .memory(self.readback_memory)
                .offset(0)
                .size(vk::WHOLE_SIZE)];
            // SAFETY: mapped whole-allocation range is aligned; fence has completed GPU writes.
            result("invalidate readback", unsafe {
                self.device.invalidate_mapped_memory_ranges(&ranges)
            })
        };
        if invalidate.is_ok() {
            // SAFETY: mapping has size readable bytes; out is independently allocated,
            // valid writable storage. Host visibility is established by barrier and invalidate.
            unsafe {
                std::ptr::copy_nonoverlapping(pointer.cast(), out.as_mut_ptr(), size);
            }
        }
        // SAFETY: allocation is currently mapped and no thread is using the pointer.
        unsafe {
            self.device.unmap_memory(self.readback_memory);
        }
        invalidate?;
        Ok(out)
    }
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        // SAFETY: only owned handles are destroyed, all were created on device. Null
        // handles are legal for destroy/free operations. Outstanding work is waited on
        // even on a fence timeout/error, before any referenced object is reclaimed.
        unsafe {
            if self.submitted {
                let _ = self.device.device_wait_idle();
            }
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
            self.device.destroy_shader_module(self.vertex_shader, None);
            self.device.destroy_shader_module(self.fragment_shader, None);
            self.device.destroy_framebuffer(self.framebuffer, None);
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.image_memory, None);
            self.device.destroy_buffer(self.readback, None);
            self.device.free_memory(self.readback_memory, None);
            self.device.destroy_buffer(self.vertices, None);
            self.device.free_memory(self.vertex_memory, None);
        }
    }
}

pub(crate) fn render(
    context: &Context,
    width: u32,
    height: u32,
    clear: [f32; 4],
    vertices: &[Vertex],
) -> Result<Image, Error> {
    let size = width as usize * height as usize * 4; // public validation limits each dimension to 4096
    let mut frame = Frame::new(&context.device);
    frame.attachment(context, width, height)?;
    frame.buffers(context, size as u64, vertices)?;
    frame.pass(width, height)?;
    frame.pipeline(width, height)?;
    frame.submit(context, width, height, clear, vertices.len() as u32)?;
    Ok(Image { width, height, rgba: frame.read(size)? })
}
