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
