//! Read-only Vulkan device enumeration through the system loader.

use std::ffi::{CStr, c_char, c_void};

use serde::Serialize;

use crate::dylib::Library;
use crate::{Error, Result};

type Instance = *mut c_void;
type PhysicalDevice = *mut c_void;

const STRUCTURE_TYPE_INSTANCE_CREATE_INFO: u32 = 1;
const DEVICE_TYPE_INTEGRATED: u32 = 1;
const DEVICE_TYPE_DISCRETE: u32 = 2;
const HEAP_DEVICE_LOCAL: u32 = 1;
const INCOMPLETE: i32 = 5;

/// Extensions that decide how far the host side of the graphics pipeline can
/// go in avoiding copies and in pacing frames.
const WATCHED_EXTENSIONS: [&str; 4] = [
    "VK_KHR_external_memory_win32",
    "VK_KHR_external_semaphore_win32",
    "VK_KHR_external_fence_win32",
    "VK_KHR_present_wait",
];

#[repr(C)]
struct InstanceCreateInfo {
    s_type: u32,
    p_next: *const c_void,
    flags: u32,
    application_info: *const c_void,
    enabled_layer_count: u32,
    enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    enabled_extension_names: *const *const c_char,
}

/// `VkPhysicalDeviceProperties`, with the tail left opaque.
#[repr(C, align(8))]
struct Properties {
    api_version: u32,
    driver_version: u32,
    vendor_id: u32,
    device_id: u32,
    device_type: u32,
    device_name: [c_char; 256],
    rest: [u8; 548],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MemoryHeap {
    size: u64,
    flags: u32,
}

/// `VkPhysicalDeviceMemoryProperties`.
#[repr(C)]
struct MemoryProperties {
    type_count: u32,
    types: [[u32; 2]; 32],
    heap_count: u32,
    heaps: [MemoryHeap; 16],
}

/// `VkExtensionProperties`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ExtensionProperties {
    name: [c_char; 256],
    spec_version: u32,
}

const _: () = assert!(size_of::<Properties>() == 824);
const _: () = assert!(size_of::<MemoryProperties>() == 520);
const _: () = assert!(size_of::<ExtensionProperties>() == 260);

type CreateInstance =
    unsafe extern "system" fn(*const InstanceCreateInfo, *const c_void, *mut Instance) -> i32;
type DestroyInstance = unsafe extern "system" fn(Instance, *const c_void);
type EnumerateDevices = unsafe extern "system" fn(Instance, *mut u32, *mut PhysicalDevice) -> i32;
type GetProperties = unsafe extern "system" fn(PhysicalDevice, *mut Properties);
type GetMemoryProperties = unsafe extern "system" fn(PhysicalDevice, *mut MemoryProperties);
type EnumerateExtensions = unsafe extern "system" fn(
    PhysicalDevice,
    *const c_char,
    *mut u32,
    *mut ExtensionProperties,
) -> i32;

/// Class of a Vulkan device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    /// Anything else, including software rasterizers.
    Other,
    /// A GPU sharing system memory.
    Integrated,
    /// A GPU with dedicated memory.
    Discrete,
}

impl DeviceKind {
    const fn from_raw(raw: u32) -> Self {
        match raw {
            DEVICE_TYPE_INTEGRATED => Self::Integrated,
            DEVICE_TYPE_DISCRETE => Self::Discrete,
            _ => Self::Other,
        }
    }
}

/// One Vulkan physical device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Device {
    /// Device name.
    pub name: String,
    /// Device class.
    pub kind: DeviceKind,
    /// Highest Vulkan version the device supports.
    pub api_version: String,
    /// PCI vendor ID.
    pub vendor_id: u32,
    /// Total device-local heap size, in MiB.
    pub device_local_mib: u64,
    /// Which of the watched extensions the device exposes.
    pub extensions: Vec<String>,
}

impl Device {
    /// Whether the device exposes `extension`.
    #[must_use]
    pub fn has_extension(&self, extension: &str) -> bool {
        self.extensions.iter().any(|name| name == extension)
    }
}

/// The device RAX would render on: discrete first, then most local memory.
#[must_use]
pub fn preferred(devices: &[Device]) -> Option<&Device> {
    devices
        .iter()
        .max_by_key(|device| (device.kind, device.device_local_mib))
}

/// Destroys the instance when probing ends, on every return path.
struct InstanceGuard {
    instance: Instance,
    destroy: DestroyInstance,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: `instance` is live and all use of it has finished.
        unsafe {
            (self.destroy)(self.instance, std::ptr::null());
        }
    }
}

/// Lists Vulkan devices.
///
/// Errors if the loader is missing. An empty list means the loader works but
/// no driver exposes a device.
pub fn enumerate() -> Result<Vec<Device>> {
    let library = Library::load_system("vulkan-1.dll")?;
    // SAFETY: each type alias matches the Vulkan 1.0 signature of the export
    // it is resolved from. `library` outlives every call in this function.
    let (create, destroy, enumerate_devices, properties, memory, extensions) = unsafe {
        (
            library.symbol::<CreateInstance>(c"vkCreateInstance")?,
            library.symbol::<DestroyInstance>(c"vkDestroyInstance")?,
            library.symbol::<EnumerateDevices>(c"vkEnumeratePhysicalDevices")?,
            library.symbol::<GetProperties>(c"vkGetPhysicalDeviceProperties")?,
            library.symbol::<GetMemoryProperties>(c"vkGetPhysicalDeviceMemoryProperties")?,
            library.symbol::<EnumerateExtensions>(c"vkEnumerateDeviceExtensionProperties")?,
        )
    };

    let info = InstanceCreateInfo {
        s_type: STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: 0,
        application_info: std::ptr::null(),
        enabled_layer_count: 0,
        enabled_layer_names: std::ptr::null(),
        enabled_extension_count: 0,
        enabled_extension_names: std::ptr::null(),
    };
    let mut instance: Instance = std::ptr::null_mut();
    // SAFETY: `info` is fully initialised and `instance` is a valid out pointer.
    let code = unsafe { create(&raw const info, std::ptr::null(), &raw mut instance) };
    if code != 0 {
        return Err(Error::Vulkan {
            api: "vkCreateInstance",
            code,
        });
    }
    let guard = InstanceGuard { instance, destroy };

    let mut count = 0_u32;
    // SAFETY: a null array asks only for the count.
    let code = unsafe { enumerate_devices(guard.instance, &raw mut count, std::ptr::null_mut()) };
    if code != 0 {
        return Err(Error::Vulkan {
            api: "vkEnumeratePhysicalDevices",
            code,
        });
    }
    let mut handles = vec![std::ptr::null_mut(); count as usize];
    // SAFETY: `handles` has room for `count` elements.
    let code = unsafe { enumerate_devices(guard.instance, &raw mut count, handles.as_mut_ptr()) };
    if code != 0 && code != INCOMPLETE {
        return Err(Error::Vulkan {
            api: "vkEnumeratePhysicalDevices",
            code,
        });
    }
    handles.truncate(count as usize);

    handles
        .into_iter()
        .map(|handle| describe(handle, properties, memory, extensions))
        .collect()
}

fn describe(
    device: PhysicalDevice,
    get_properties: GetProperties,
    get_memory: GetMemoryProperties,
    enumerate_extensions: EnumerateExtensions,
) -> Result<Device> {
    // SAFETY: Properties is plain data, so all-zero is a valid value.
    let mut properties: Properties = unsafe { std::mem::zeroed() };
    // SAFETY: `device` came from the enumeration and the output is correctly sized.
    unsafe { get_properties(device, &raw mut properties) };

    // SAFETY: MemoryProperties is plain data, so all-zero is a valid value.
    let mut memory: MemoryProperties = unsafe { std::mem::zeroed() };
    // SAFETY: `device` came from the enumeration and the output is correctly sized.
    unsafe { get_memory(device, &raw mut memory) };

    let heaps = &memory.heaps[..(memory.heap_count as usize).min(16)];
    let device_local_bytes: u64 = heaps
        .iter()
        .filter(|heap| heap.flags & HEAP_DEVICE_LOCAL != 0)
        .map(|heap| heap.size)
        .sum();

    let mut count = 0_u32;
    // SAFETY: null layer and null output ask only for the count.
    let code = unsafe {
        enumerate_extensions(
            device,
            std::ptr::null(),
            &raw mut count,
            std::ptr::null_mut(),
        )
    };
    if code != 0 {
        return Err(Error::Vulkan {
            api: "vkEnumerateDeviceExtensionProperties",
            code,
        });
    }
    let blank = ExtensionProperties {
        name: [0; 256],
        spec_version: 0,
    };
    let mut available = vec![blank; count as usize];
    // SAFETY: `available` has room for `count` elements.
    let code = unsafe {
        enumerate_extensions(
            device,
            std::ptr::null(),
            &raw mut count,
            available.as_mut_ptr(),
        )
    };
    if code != 0 && code != INCOMPLETE {
        return Err(Error::Vulkan {
            api: "vkEnumerateDeviceExtensionProperties",
            code,
        });
    }
    available.truncate(count as usize);

    let extensions = WATCHED_EXTENSIONS
        .iter()
        .filter(|wanted| available.iter().any(|ext| c_name(&ext.name) == **wanted))
        .map(|wanted| (*wanted).to_owned())
        .collect();

    Ok(Device {
        name: c_name(&properties.device_name),
        kind: DeviceKind::from_raw(properties.device_type),
        api_version: version_string(properties.api_version),
        vendor_id: properties.vendor_id,
        device_local_mib: device_local_bytes / (1024 * 1024),
        extensions,
    })
}

fn c_name(raw: &[c_char; 256]) -> String {
    let bytes: Vec<u8> = raw.iter().map(|&c| c.cast_unsigned()).collect();
    CStr::from_bytes_until_nul(&bytes).map_or_else(
        |_| String::new(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn version_string(raw: u32) -> String {
    format!(
        "{}.{}.{}",
        (raw >> 22) & 0x7f,
        (raw >> 12) & 0x3ff,
        raw & 0xfff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, kind: DeviceKind, device_local_mib: u64) -> Device {
        Device {
            name: name.to_owned(),
            kind,
            api_version: String::new(),
            vendor_id: 0,
            device_local_mib,
            extensions: Vec::new(),
        }
    }

    #[test]
    fn device_name_sits_at_its_abi_offset() {
        assert_eq!(std::mem::offset_of!(Properties, device_name), 20);
    }

    #[test]
    fn memory_heaps_sit_at_their_abi_offset() {
        assert_eq!(std::mem::offset_of!(MemoryProperties, heaps), 264);
    }

    #[test]
    fn prefers_discrete_even_with_less_local_memory() {
        let devices = [
            device("igpu", DeviceKind::Integrated, 8192),
            device("dgpu", DeviceKind::Discrete, 4096),
        ];
        assert_eq!(preferred(&devices).unwrap().name, "dgpu");
    }

    #[test]
    fn breaks_ties_on_local_memory() {
        let devices = [
            device("small", DeviceKind::Discrete, 4096),
            device("large", DeviceKind::Discrete, 8192),
        ];
        assert_eq!(preferred(&devices).unwrap().name, "large");
    }

    #[test]
    fn no_devices_means_no_preference() {
        assert!(preferred(&[]).is_none());
    }

    #[test]
    fn unknown_device_types_map_to_other() {
        assert_eq!(DeviceKind::from_raw(0), DeviceKind::Other);
        assert_eq!(DeviceKind::from_raw(4), DeviceKind::Other);
    }

    #[test]
    fn decodes_vulkan_versions() {
        assert_eq!(version_string((1 << 22) | (4 << 12) | 341), "1.4.341");
    }

    #[test]
    fn stops_names_at_the_nul() {
        let mut raw = [0 as c_char; 256];
        raw[0] = b'h' as c_char;
        raw[1] = b'i' as c_char;
        assert_eq!(c_name(&raw), "hi");
    }
}
