//! Windows Hypervisor Platform access and measurements.
//!
//! Layouts and constants here mirror `WinHvPlatformDefs.h`. Compile-time
//! assertions pin the sizes, and `tests/whp_smoke.rs` proves them against the
//! real hypervisor by running a guest.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::Instant;

use serde::Serialize;

use crate::dylib::Library;
use crate::stats::{self, Summary};
use crate::{Error, Result};

type Handle = *mut c_void;
type Hresult = i32;

const CAP_HYPERVISOR_PRESENT: u32 = 0x0000;
const CAP_FEATURES: u32 = 0x0001;
const CAP_PROCESSOR_VENDOR: u32 = 0x1000;
const PROP_PROCESSOR_COUNT: u32 = 0x1fff;

const MAP_READ: u32 = 0x1;
const MAP_WRITE: u32 = 0x2;
const MAP_EXECUTE: u32 = 0x4;

const REG_RIP: u32 = 0x10;
const REG_RFLAGS: u32 = 0x11;
const REG_CS: u32 = 0x13;

const EXIT_IO_PORT: u32 = 0x0002;

const PAGE: usize = 0x1000;
const GUEST_CODE_GPA: u64 = 0x1000;
/// `out 0x10, al` followed by `jmp` back to it.
const GUEST_CODE: [u8; 4] = [0xE6, 0x10, 0xEB, 0xFC];
/// RFLAGS with only the architecturally reserved bit set.
const RFLAGS_RESET: u64 = 0x2;
/// Selector field of a `WHV_X64_SEGMENT_REGISTER`, within its second quadword.
const SEGMENT_SELECTOR_MASK: u64 = 0xffff << 32;

const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_RELEASE: u32 = 0x8000;
const PAGE_READWRITE: u32 = 0x04;

/// `WHV_REGISTER_VALUE`.
#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct RegisterValue([u64; 2]);

/// `WHV_RUN_VP_EXIT_CONTEXT`, with the exit-specific union left opaque.
#[repr(C, align(16))]
struct ExitContext {
    reason: u32,
    reserved: u32,
    execution_state: u16,
    /// Low nibble is the instruction length, high nibble is CR8.
    instruction_length_cr8: u8,
    reserved2: u8,
    reserved3: u32,
    cs: [u64; 2],
    rip: u64,
    rflags: u64,
    union_data: [u8; 176],
}

const _: () = assert!(size_of::<ExitContext>() == 224);
const _: () = assert!(size_of::<RegisterValue>() == 16);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn VirtualAlloc(address: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
    fn VirtualFree(address: *mut c_void, size: usize, kind: u32) -> i32;
    fn GetLastError() -> u32;
}

/// Loaded entry points of `WinHvPlatform.dll`.
pub struct Api {
    get_capability: unsafe extern "system" fn(u32, *mut c_void, u32, *mut u32) -> Hresult,
    create_partition: unsafe extern "system" fn(*mut Handle) -> Hresult,
    setup_partition: unsafe extern "system" fn(Handle) -> Hresult,
    delete_partition: unsafe extern "system" fn(Handle) -> Hresult,
    set_partition_property: unsafe extern "system" fn(Handle, u32, *const c_void, u32) -> Hresult,
    map_gpa_range: unsafe extern "system" fn(Handle, *mut c_void, u64, u64, u32) -> Hresult,
    unmap_gpa_range: unsafe extern "system" fn(Handle, u64, u64) -> Hresult,
    create_virtual_processor: unsafe extern "system" fn(Handle, u32, u32) -> Hresult,
    run_virtual_processor: unsafe extern "system" fn(Handle, u32, *mut c_void, u32) -> Hresult,
    get_registers:
        unsafe extern "system" fn(Handle, u32, *const u32, u32, *mut RegisterValue) -> Hresult,
    set_registers:
        unsafe extern "system" fn(Handle, u32, *const u32, u32, *const RegisterValue) -> Hresult,
    // Never moved out, so the pointers above stay valid for the lifetime of `Api`.
    _library: Library,
}

impl Api {
    /// Loads `WinHvPlatform.dll`. This fails if the optional Windows feature
    /// "Windows Hypervisor Platform" has never been enabled.
    pub fn load() -> Result<Self> {
        let library = Library::load_system("WinHvPlatform.dll")?;
        // SAFETY: each field's type matches the signature documented for the
        // export it is resolved from, and `_library` keeps the module loaded
        // for as long as the pointers exist.
        unsafe {
            Ok(Self {
                get_capability: library.symbol(c"WHvGetCapability")?,
                create_partition: library.symbol(c"WHvCreatePartition")?,
                setup_partition: library.symbol(c"WHvSetupPartition")?,
                delete_partition: library.symbol(c"WHvDeletePartition")?,
                set_partition_property: library.symbol(c"WHvSetPartitionProperty")?,
                map_gpa_range: library.symbol(c"WHvMapGpaRange")?,
                unmap_gpa_range: library.symbol(c"WHvUnmapGpaRange")?,
                create_virtual_processor: library.symbol(c"WHvCreateVirtualProcessor")?,
                run_virtual_processor: library.symbol(c"WHvRunVirtualProcessor")?,
                get_registers: library.symbol(c"WHvGetVirtualProcessorRegisters")?,
                set_registers: library.symbol(c"WHvSetVirtualProcessorRegisters")?,
                _library: library,
            })
        }
    }

    fn capability(&self, code: u32) -> Result<u64> {
        let mut value = 0_u64;
        let mut written = 0_u32;
        // SAFETY: `value` is a writable 8-byte buffer and the size passed
        // matches it. Every capability read here fits in 8 bytes.
        let hr =
            unsafe { (self.get_capability)(code, (&raw mut value).cast(), 8, &raw mut written) };
        check("WHvGetCapability", hr)?;
        Ok(value)
    }

    /// Reads what the hypervisor reports about itself.
    pub fn capabilities(&self) -> Result<Capabilities> {
        let hypervisor_present = self.capability(CAP_HYPERVISOR_PRESENT)? & 0xffff_ffff != 0;
        if !hypervisor_present {
            return Ok(Capabilities {
                hypervisor_present,
                features: 0,
                processor_vendor: None,
            });
        }
        let vendor = match self.capability(CAP_PROCESSOR_VENDOR)? & 0xffff_ffff {
            0 => "AMD",
            1 => "Intel",
            2 => "Hygon",
            _ => "unknown",
        };
        Ok(Capabilities {
            hypervisor_present,
            features: self.capability(CAP_FEATURES)?,
            processor_vendor: Some(vendor.to_owned()),
        })
    }
}

/// Hypervisor facts from `WHvGetCapability`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// Whether the Windows hypervisor is running and usable through WHP.
    pub hypervisor_present: bool,
    /// Raw `WHV_CAPABILITY_FEATURES` bits.
    pub features: u64,
    /// Processor vendor as reported by the hypervisor.
    pub processor_vendor: Option<String>,
}

fn check(api: &'static str, hr: Hresult) -> Result<()> {
    if hr < 0 {
        Err(Error::Hresult { api, code: hr })
    } else {
        Ok(())
    }
}

/// A partition (virtual machine) deleted on drop.
struct Partition<'a> {
    api: &'a Api,
    handle: Handle,
}

impl<'a> Partition<'a> {
    fn new(api: &'a Api, processors: u32) -> Result<Self> {
        let mut handle: Handle = std::ptr::null_mut();
        // SAFETY: `handle` is a valid out pointer.
        check("WHvCreatePartition", unsafe {
            (api.create_partition)(&raw mut handle)
        })?;
        let partition = Self { api, handle };
        // SAFETY: the property value is a u32 and its size is passed with it.
        let hr = unsafe {
            (api.set_partition_property)(
                handle,
                PROP_PROCESSOR_COUNT,
                (&raw const processors).cast(),
                4,
            )
        };
        check("WHvSetPartitionProperty", hr)?;
        // SAFETY: `handle` is a live partition that has not been set up yet.
        check("WHvSetupPartition", unsafe {
            (api.setup_partition)(handle)
        })?;
        Ok(partition)
    }

    /// Maps `region` into the guest at `gpa`.
    ///
    /// The `'a` borrow ties the region to the partition, so a mapped region
    /// cannot be released while the guest can still reach it.
    fn map(&self, region: &'a GuestRegion, gpa: u64, flags: u32) -> Result<()> {
        // SAFETY: `region` is a live, page-aligned allocation of `size` bytes
        // and the borrow keeps it alive until the partition is deleted.
        let hr = unsafe {
            (self.api.map_gpa_range)(
                self.handle,
                region.base.as_ptr(),
                gpa,
                region.size as u64,
                flags,
            )
        };
        check("WHvMapGpaRange", hr)
    }

    fn unmap(&self, gpa: u64, size: u64) -> Result<()> {
        // SAFETY: the handle is live. The hypervisor validates the range.
        check("WHvUnmapGpaRange", unsafe {
            (self.api.unmap_gpa_range)(self.handle, gpa, size)
        })
    }

    fn create_processor(&self, index: u32) -> Result<()> {
        // SAFETY: the handle is live and the index is below the configured count.
        let hr = unsafe { (self.api.create_virtual_processor)(self.handle, index, 0) };
        check("WHvCreateVirtualProcessor", hr)
    }

    fn register(&self, name: u32) -> Result<RegisterValue> {
        let mut value = RegisterValue::default();
        // SAFETY: both pointers refer to exactly one element, matching the count.
        let hr =
            unsafe { (self.api.get_registers)(self.handle, 0, &raw const name, 1, &raw mut value) };
        check("WHvGetVirtualProcessorRegisters", hr)?;
        Ok(value)
    }

    fn set_registers(&self, names: &[u32], values: &[RegisterValue]) -> Result<()> {
        debug_assert_eq!(names.len(), values.len());
        let count = names.len().min(values.len()) as u32;
        // SAFETY: both slices hold at least `count` elements.
        let hr = unsafe {
            (self.api.set_registers)(self.handle, 0, names.as_ptr(), count, values.as_ptr())
        };
        check("WHvSetVirtualProcessorRegisters", hr)
    }

    fn run(&self, exit: &mut ExitContext) -> Result<()> {
        // SAFETY: `exit` is a writable WHV_RUN_VP_EXIT_CONTEXT of the size passed.
        let hr = unsafe {
            (self.api.run_virtual_processor)(
                self.handle,
                0,
                std::ptr::from_mut(exit).cast(),
                size_of::<ExitContext>() as u32,
            )
        };
        check("WHvRunVirtualProcessor", hr)
    }
}

impl Drop for Partition<'_> {
    fn drop(&mut self) {
        // SAFETY: `handle` is a live partition and is deleted only here.
        unsafe {
            (self.api.delete_partition)(self.handle);
        }
    }
}

/// Page-aligned, zero-initialised host memory for a guest.
struct GuestRegion {
    base: NonNull<c_void>,
    size: usize,
}

impl GuestRegion {
    fn new(size: usize) -> Result<Self> {
        // SAFETY: a null address asks the OS to choose one, and no existing
        // memory is affected.
        let raw = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                size,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        match NonNull::new(raw) {
            Some(base) => Ok(Self { base, size }),
            None => {
                // SAFETY: GetLastError has no preconditions.
                let code = unsafe { GetLastError() };
                Err(Error::Win32 {
                    api: "VirtualAlloc",
                    code,
                })
            }
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        assert!(bytes.len() <= self.size);
        // SAFETY: the bounds were checked above, `self` is exclusively
        // borrowed, and the source cannot overlap a private allocation.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.as_ptr().cast(), bytes.len());
        }
    }
}

impl Drop for GuestRegion {
    fn drop(&mut self) {
        // SAFETY: `base` came from VirtualAlloc and size 0 with MEM_RELEASE
        // is the documented way to free the whole allocation.
        unsafe {
            VirtualFree(self.base.as_ptr(), 0, MEM_RELEASE);
        }
    }
}

/// Result of [`measure_exit_latency`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExitLatency {
    /// Time from entering `WHvRunVirtualProcessor` until the I/O exit returns.
    pub run_to_exit: Summary,
    /// Whole-loop time per exit, including advancing the guest past the
    /// instruction. This is the closest figure to a trivial device emulation.
    pub mean_full_exit_cycle_ns: f64,
}

/// Measures the round trip of a guest port I/O exit into user mode.
///
/// A one-vCPU guest repeatedly executes `out`. Every virtio notification in
/// a Windows-hosted VMM costs at least this, so it bounds how chatty the
/// device model can afford to be.
pub fn measure_exit_latency(api: &Api, iterations: usize) -> Result<ExitLatency> {
    let mut code = GuestRegion::new(PAGE)?;
    code.write(&GUEST_CODE);
    let partition = Partition::new(api, 1)?;
    partition.map(&code, GUEST_CODE_GPA, MAP_READ | MAP_WRITE | MAP_EXECUTE)?;
    partition.create_processor(0)?;

    // Keep the reset value of CS, including its attributes, and rebase it to
    // zero so that IP is the guest physical address in real mode.
    let mut cs = partition.register(REG_CS)?;
    cs.0[0] = 0;
    cs.0[1] &= !SEGMENT_SELECTOR_MASK;
    partition.set_registers(
        &[REG_CS, REG_RIP, REG_RFLAGS],
        &[
            cs,
            RegisterValue([GUEST_CODE_GPA, 0]),
            RegisterValue([RFLAGS_RESET, 0]),
        ],
    )?;

    // SAFETY: ExitContext is plain integers, so all-zero is a valid value.
    let mut exit: ExitContext = unsafe { std::mem::zeroed() };
    let mut samples = Vec::with_capacity(iterations);
    let started = Instant::now();
    for _ in 0..iterations {
        let before = Instant::now();
        partition.run(&mut exit)?;
        let elapsed = before.elapsed();
        if exit.reason != EXIT_IO_PORT {
            return Err(Error::UnexpectedExit {
                reason: exit.reason,
                rip: exit.rip,
            });
        }
        samples.push(u32::try_from(elapsed.as_nanos()).unwrap_or(u32::MAX));
        let length = u64::from(exit.instruction_length_cr8 & 0x0f);
        partition.set_registers(&[REG_RIP], &[RegisterValue([exit.rip + length, 0])])?;
    }
    let total = started.elapsed();
    let mean_full_exit_cycle_ns = total.as_nanos() as f64 / iterations.max(1) as f64;
    let run_to_exit = stats::summarize(&mut samples).ok_or(Error::UnexpectedExit {
        reason: 0,
        rip: GUEST_CODE_GPA,
    })?;
    Ok(ExitLatency {
        run_to_exit,
        mean_full_exit_cycle_ns,
    })
}

/// Result of [`measure_map_cost`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MapCost {
    /// Size of the mapped region.
    pub size_mib: usize,
    /// Time to create a one-vCPU partition.
    pub partition_create_us: f64,
    /// Time for `WHvMapGpaRange`.
    pub map_us: f64,
    /// Time for `WHvUnmapGpaRange`.
    pub unmap_us: f64,
}

/// Measures partition creation and mapping of untouched guest RAM.
///
/// The region is committed but never written, so this measures bookkeeping
/// and not the first-touch page faults a booting guest will take.
pub fn measure_map_cost(api: &Api, size_mib: usize) -> Result<MapCost> {
    const MIB: usize = 1024 * 1024;
    let size = size_mib * MIB;
    let region = GuestRegion::new(size)?;
    let started = Instant::now();
    let partition = Partition::new(api, 1)?;
    let partition_create_us = started.elapsed().as_secs_f64() * 1e6;
    let started = Instant::now();
    partition.map(&region, 0, MAP_READ | MAP_WRITE | MAP_EXECUTE)?;
    let map_us = started.elapsed().as_secs_f64() * 1e6;
    let started = Instant::now();
    partition.unmap(0, size as u64)?;
    let unmap_us = started.elapsed().as_secs_f64() * 1e6;
    Ok(MapCost {
        size_mib,
        partition_create_us,
        map_us,
        unmap_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_context_fields_sit_at_abi_offsets() {
        assert_eq!(std::mem::offset_of!(ExitContext, execution_state), 8);
        assert_eq!(
            std::mem::offset_of!(ExitContext, instruction_length_cr8),
            10
        );
        assert_eq!(std::mem::offset_of!(ExitContext, cs), 16);
        assert_eq!(std::mem::offset_of!(ExitContext, rip), 32);
        assert_eq!(std::mem::offset_of!(ExitContext, rflags), 40);
        assert_eq!(std::mem::offset_of!(ExitContext, union_data), 48);
    }

    #[test]
    fn guest_loop_jumps_back_to_the_out_instruction() {
        // The rel8 is relative to the end of the two-byte jmp at offset 2.
        assert_eq!(2 + 2 + i32::from(GUEST_CODE[3].cast_signed()), 0);
    }

    #[test]
    fn check_treats_positive_hresults_as_success() {
        assert!(check("x", 0).is_ok());
        assert!(check("x", 1).is_ok());
        assert!(check("x", -1).is_err());
    }
}
