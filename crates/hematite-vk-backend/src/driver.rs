//! Vulkan loader ownership and instance/device RAII. Unsafe driver calls live here
//! and in render.rs; no Vulkan handles cross the public backend interface.

use crate::{Capabilities, Error};
use ash::{vk, Entry};
use std::ffi::{CStr, CString};
use std::path::Path;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod dynamic {
    use super::*;
    use std::ffi::{c_char, c_int, c_void};
    use std::os::unix::ffi::OsStrExt;

    #[link(name = "dl")]
    extern "C" {
        fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> c_int;
        fn dlerror() -> *const c_char;
    }

    pub(super) struct Library(*mut c_void);
    impl Library {
        pub(super) fn load(path: &Path) -> Result<(Self, Entry), Error> {
            let path = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| Error::Loader("NUL in loader path".to_string()))?;
            // SAFETY: path is NUL-terminated and lives throughout dlopen. RTLD_NOW=2 is
            // defined by both Linux and Android. A successful handle is closed by Drop.
            let handle = unsafe { dlopen(path.as_ptr(), 2) };
            if handle.is_null() {
                // SAFETY: dlerror returns NULL or a thread-local NUL-terminated diagnostic.
                let reason = unsafe {
                    let error = dlerror();
                    if error.is_null() {
                        "unknown loader failure".to_string()
                    } else {
                        CStr::from_ptr(error).to_string_lossy().into_owned()
                    }
                };
                return Err(Error::Loader(reason));
            }
            let library = Self(handle);
            // SAFETY: live dlopen handle and static NUL-terminated symbol name.
            let symbol = unsafe { dlsym(handle, c"vkGetInstanceProcAddr".as_ptr()) };
            if symbol.is_null() {
                return Err(Error::Loader("loader has no vkGetInstanceProcAddr".to_string()));
            }
            // SAFETY: Vulkan's loader ABI defines this exact symbol with this signature.
            // library remains owned by Context until after all Vulkan handles are destroyed.
            let get_instance_proc_addr = unsafe {
                std::mem::transmute::<*mut c_void, vk::PFN_vkGetInstanceProcAddr>(symbol)
            };
            // SAFETY: the resolved loader function is valid and library lifetime is retained.
            let entry = unsafe { Entry::from_static_fn(ash::StaticFn { get_instance_proc_addr }) };
            Ok((library, entry))
        }
    }
    impl Drop for Library {
        fn drop(&mut self) {
            // SAFETY: handle was acquired once by dlopen, and no Vulkan object remains.
            unsafe {
                dlclose(self.0);
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod dynamic {
    use super::*;
    pub(super) struct Library;
    impl Library {
        pub(super) fn load(_: &Path) -> Result<(Self, Entry), Error> {
            Err(Error::Unsupported("loader currently supports Linux/Android only".into()))
        }
    }
}

pub(crate) struct Context {
    pub(crate) instance: ash::Instance,
    pub(crate) device: ash::Device,
    pub(crate) queue: vk::Queue,
    pub(crate) queue_family: u32,
    pub(crate) memory: vk::PhysicalDeviceMemoryProperties,
    pub(crate) capabilities: Capabilities,
    // Keep function table and shared library alive until after device/instance destruction.
    _entry: Entry,
    _library: dynamic::Library,
}

impl Context {
    pub(crate) fn new(loader: Option<&Path>) -> Result<Self, Error> {
        let default = if cfg!(target_os = "android") { "libvulkan.so" } else { "libvulkan.so.1" };
        let (library, entry) = dynamic::Library::load(loader.unwrap_or(Path::new(default)))?;
        // SAFETY: loaded Entry owns a valid loader dispatch table; no instance needed.
        let version = unsafe { entry.try_enumerate_instance_version() }
            .map_err(|e| Error::Vulkan("enumerate instance version", e))?
            .unwrap_or(vk::API_VERSION_1_0);
        if version < vk::API_VERSION_1_1 {
            return Err(Error::Unsupported("Vulkan 1.1 loader is required".into()));
        }
        let application = vk::ApplicationInfo::default()
            .application_name(c"Hematite offscreen harness")
            .engine_name(c"Hematite")
            .api_version(vk::API_VERSION_1_1);
        let info = vk::InstanceCreateInfo::default().application_info(&application);
        // SAFETY: create info and C strings live throughout the call, no dangling pNext.
        let instance = unsafe { entry.create_instance(&info, None) }
            .map_err(|e| Error::Vulkan("create instance", e))?;
        // From this point every early return explicitly destroys the instance.
        let result = Self::create_device(&instance);
        let (device, physical, queue_family, capabilities) = match result {
            Ok(value) => value,
            Err(error) => {
                // SAFETY: no device was returned and no child object remains alive.
                unsafe {
                    instance.destroy_instance(None);
                }
                return Err(error);
            }
        };
        // SAFETY: queue family was queried and one queue was requested in device creation.
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        // SAFETY: physical device belongs to this live instance.
        let memory = unsafe { instance.get_physical_device_memory_properties(physical) };
        Ok(Self {
            instance,
            device,
            queue,
            queue_family,
            memory,
            capabilities,
            _entry: entry,
            _library: library,
        })
    }

    fn create_device(
        instance: &ash::Instance,
    ) -> Result<(ash::Device, vk::PhysicalDevice, u32, Capabilities), Error> {
        // SAFETY: live instance, Vulkan owns the output list contents.
        let devices = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| Error::Vulkan("enumerate physical devices", e))?;
        let mut candidates = Vec::new();
        for physical in devices {
            // SAFETY: physical handle was enumerated from this live instance.
            let properties = unsafe { instance.get_physical_device_properties(physical) };
            if properties.api_version < vk::API_VERSION_1_1 {
                continue;
            }
            // SAFETY: physical device is live and owned by instance.
            let families =
                unsafe { instance.get_physical_device_queue_family_properties(physical) };
            let Some(index) = families.iter().position(|q| {
                q.queue_count > 0 && q.queue_flags.contains(vk::QueueFlags::GRAPHICS)
            }) else {
                continue;
            };
            // SAFETY: queried physical device and valid core format constant.
            let format = unsafe {
                instance.get_physical_device_format_properties(physical, vk::Format::R8G8B8A8_UNORM)
            };
            let required =
                vk::FormatFeatureFlags::COLOR_ATTACHMENT | vk::FormatFeatureFlags::TRANSFER_SRC;
            if !format.optimal_tiling_features.contains(required) {
                continue;
            }
            let score = match properties.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 1000,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 800,
                vk::PhysicalDeviceType::VIRTUAL_GPU => 600,
                vk::PhysicalDeviceType::CPU => 100,
                _ => 0,
            };
            candidates.push((score, physical, index as u32, properties));
        }
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
        for (_, physical, family, properties) in candidates {
            // SAFETY: device handle belongs to instance; only reads supported properties.
            let features = unsafe { instance.get_physical_device_features(physical) };
            // SAFETY: extension enumeration with a live physical handle and no layer name.
            let extensions = unsafe { instance.enumerate_device_extension_properties(physical) }
                .map_err(|e| Error::Vulkan("enumerate device extensions", e))?;
            let priorities = [1.0];
            let queues = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(family)
                .queue_priorities(&priorities)];
            let info = vk::DeviceCreateInfo::default().queue_create_infos(&queues);
            // SAFETY: family exists, priority slice lives through call, no optional features
            // or extensions are enabled without negotiation. Failure tries the next device.
            let Ok(device) = (unsafe { instance.create_device(physical, &info, None) }) else {
                continue;
            };
            // SAFETY: Vulkan fixed-size name fields are guaranteed NUL-terminated.
            let device_name = unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let extensions = extensions
                .iter()
                .map(|extension| {
                    // SAFETY: Vulkan extension names are fixed NUL-terminated arrays.
                    unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) }
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            return Ok((
                device,
                physical,
                family,
                Capabilities {
                    device_name,
                    api_version: properties.api_version,
                    vendor_device: [properties.vendor_id, properties.device_id],
                    driver_version: properties.driver_version,
                    pipeline_cache_uuid: properties.pipeline_cache_uuid,
                    device_type: properties.device_type,
                    max_image_dimension_2d: properties.limits.max_image_dimension2_d,
                    geometry_shader_supported: features.geometry_shader != 0,
                    tessellation_shader_supported: features.tessellation_shader != 0,
                    extensions,
                },
            ));
        }
        Err(Error::Unsupported(
            "no usable Vulkan 1.1 graphics device with RGBA8 attachments".into(),
        ))
    }

    pub(crate) fn memory_type(
        &self,
        bits: u32,
        required: vk::MemoryPropertyFlags,
        preferred: vk::MemoryPropertyFlags,
    ) -> Result<(u32, bool), Error> {
        let mut fallback = None;
        for index in 0..self.memory.memory_type_count {
            let flags = self.memory.memory_types[index as usize].property_flags;
            if bits & (1 << index) != 0 && flags.contains(required) {
                let value = (index, flags.contains(vk::MemoryPropertyFlags::HOST_COHERENT));
                if flags.contains(preferred) {
                    return Ok(value);
                }
                fallback = Some(value);
            }
        }
        fallback.ok_or_else(|| Error::Unsupported("no compatible Vulkan memory type".into()))
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: context exclusively owns this device and instance. All per-render child
        // objects have dropped; waiting covers any failed/timeout submission cleanup.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
