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

pub(crate) type Handle = *mut c_void;
type Hresult = i32;

const CAP_HYPERVISOR_PRESENT: u32 = 0x0000;
const CAP_FEATURES: u32 = 0x0001;
const CAP_PROCESSOR_VENDOR: u32 = 0x1000;
const CAP_PROCESSOR_CLOCK: u32 = 0x1004;
const PROP_PROCESSOR_COUNT: u32 = 0x1fff;

const MAP_READ: u32 = 0x1;
const MAP_WRITE: u32 = 0x2;
const MAP_EXECUTE: u32 = 0x4;

const REG_RAX: u32 = 0x00;
const REG_RSI: u32 = 0x06;
const REG_RIP: u32 = 0x10;
const REG_RFLAGS: u32 = 0x11;
const REG_ES: u32 = 0x12;
const REG_CS: u32 = 0x13;
const REG_SS: u32 = 0x14;
const REG_DS: u32 = 0x15;
const REG_FS: u32 = 0x16;
const REG_GS: u32 = 0x17;
const REG_CR0: u32 = 0x1c;
const REG_PENDING_INTERRUPTION: u32 = 0x8000_0000;
const REG_DELIVERABILITY: u32 = 0x8000_0004;

const EXIT_MEMORY: u32 = 0x0001;
const EXIT_IO_PORT: u32 = 0x0002;
const EXIT_INTERRUPT_WINDOW: u32 = 0x0007;
const EXIT_HALT: u32 = 0x0008;
const EXIT_CANCELED: u32 = 0x2001;

pub(crate) const PAGE: usize = 0x1000;
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
    cancel_run: unsafe extern "system" fn(Handle, u32, u32) -> Hresult,
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
                cancel_run: library.symbol(c"WHvCancelRunVirtualProcessor")?,
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
pub(crate) struct Partition<'a> {
    api: &'a Api,
    handle: Handle,
}

impl<'a> Partition<'a> {
    pub(crate) fn new(api: &'a Api, processors: u32) -> Result<Self> {
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

    pub(crate) fn create_processor(&self, index: u32) -> Result<()> {
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
pub(crate) struct GuestRegion {
    base: NonNull<c_void>,
    size: usize,
}

impl GuestRegion {
    pub(crate) fn new(size: usize) -> Result<Self> {
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

impl GuestRegion {
    /// Copies `bytes` to offset `gpa`, checking bounds.
    pub(crate) fn write_at(&mut self, gpa: u64, bytes: &[u8]) -> Result<()> {
        let offset = crate::vm::check_range(gpa, bytes.len(), self.size)?;
        // SAFETY: `check_range` proved the destination lies inside this
        // allocation, `self` is exclusively borrowed so the vCPU API cannot
        // be running through `Vm`, and the source cannot overlap it.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.base.as_ptr().cast::<u8>().add(offset),
                bytes.len(),
            );
        }
        Ok(())
    }
}

/// Registers captured at a vCPU exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuState {
    /// Instruction pointer of the exiting instruction.
    pub rip: u64,
    /// Flags at the exit.
    pub rflags: u64,
}

/// Why the vCPU stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmExit {
    /// `in` from a port.
    PortRead {
        /// Port number.
        port: u16,
        /// Access size in bytes.
        size: u8,
    },
    /// `out` to a port.
    PortWrite {
        /// Port number.
        port: u16,
        /// Access size in bytes.
        size: u8,
        /// Low bytes of RAX.
        value: u32,
    },
    /// The guest executed `hlt`.
    Halted,
    /// The guest can now accept an interrupt.
    InterruptWindow,
    /// Another thread kicked the vCPU.
    Canceled,
    /// An access outside mapped RAM.
    Mmio {
        /// Guest physical address.
        gpa: u64,
        /// Whether it was a write.
        write: bool,
    },
    /// Anything else, including string I/O.
    Other {
        /// `WHV_RUN_VP_EXIT_REASON`.
        reason: u32,
    },
}

/// A decoded exit.
#[derive(Debug, Clone, Copy)]
pub struct ExitInfo {
    /// Why the vCPU stopped.
    pub exit: VmExit,
    /// Registers at the exit.
    pub cpu: CpuState,
    /// Whether an external interrupt can be injected now.
    pub interruptible: bool,
    instruction_length: u8,
    rax: u64,
}

const RFLAGS_IF: u64 = 1 << 9;
const EXEC_INTERRUPTION_PENDING: u16 = 1 << 6;
const EXEC_INTERRUPT_SHADOW: u16 = 1 << 12;
const IO_INFO_WRITE: u32 = 1;
const IO_INFO_STRING: u32 = 1 << 4;
const CR0_PE_ET: u64 = 0x11;
const SEGMENT_CODE_ATTRIBUTES: u64 = 0xC09B;
const SEGMENT_DATA_ATTRIBUTES: u64 = 0xC093;
const SELECTOR_CODE: u64 = 0x10;
const SELECTOR_DATA: u64 = 0x18;
const SEGMENT_LIMIT_4G: u64 = 0xffff_ffff;

fn flat_segment(selector: u64, attributes: u64) -> RegisterValue {
    RegisterValue([0, SEGMENT_LIMIT_4G | selector << 32 | attributes << 48])
}

fn little_u16(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn little_u32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn little_u64(data: &[u8], at: usize) -> u64 {
    let low = u64::from(little_u32(data, at));
    low | u64::from(little_u32(data, at + 4)) << 32
}

/// Decodes `WHV_X64_IO_PORT_ACCESS_CONTEXT`.
fn decode_io(data: &[u8]) -> (VmExit, u64) {
    let info = little_u32(data, 20);
    let port = little_u16(data, 24);
    let rax = little_u64(data, 32);
    let size = ((info >> 1) & 0x7) as u8;
    if info & IO_INFO_STRING != 0 {
        return (
            VmExit::Other {
                reason: EXIT_IO_PORT,
            },
            rax,
        );
    }
    let exit = if info & IO_INFO_WRITE != 0 {
        VmExit::PortWrite {
            port,
            size,
            value: rax as u32,
        }
    } else {
        VmExit::PortRead { port, size }
    };
    (exit, rax)
}

/// Stores `value` in the low `size` bytes of `rax`, as `in` does. A 32-bit
/// access clears the upper half, as every 32-bit register write does.
fn merge_rax(rax: u64, size: u8, value: u32) -> u64 {
    match size {
        1 => (rax & !0xff) | u64::from(value & 0xff),
        2 => (rax & !0xffff) | u64::from(value & 0xffff),
        _ => u64::from(value),
    }
}

/// Runs vCPU 0 until the next exit and decodes it.
pub(crate) fn read_exit(partition: &Partition<'_>) -> Result<ExitInfo> {
    // SAFETY: ExitContext is plain integers, so all-zero is a valid value.
    let mut exit: ExitContext = unsafe { std::mem::zeroed() };
    partition.run(&mut exit)?;
    let mut rax = 0;
    let decoded = match exit.reason {
        EXIT_IO_PORT => {
            let (decoded, value) = decode_io(&exit.union_data);
            rax = value;
            decoded
        }
        EXIT_HALT => VmExit::Halted,
        EXIT_INTERRUPT_WINDOW => VmExit::InterruptWindow,
        EXIT_CANCELED => VmExit::Canceled,
        EXIT_MEMORY => VmExit::Mmio {
            gpa: little_u64(&exit.union_data, 24),
            write: little_u32(&exit.union_data, 20) & 0x3 == 1,
        },
        reason => VmExit::Other { reason },
    };
    let blocked = exit.execution_state & (EXEC_INTERRUPTION_PENDING | EXEC_INTERRUPT_SHADOW) != 0;
    Ok(ExitInfo {
        exit: decoded,
        cpu: CpuState {
            rip: exit.rip,
            rflags: exit.rflags,
        },
        interruptible: exit.rflags & RFLAGS_IF != 0 && !blocked,
        instruction_length: exit.instruction_length_cr8 & 0x0f,
        rax,
    })
}

/// Maps `ram` at guest address zero.
///
/// `Vm` declares its partition before its RAM, which is what makes this
/// sound without the borrow that [`Partition::map`] normally demands.
pub(crate) fn map_guest_ram(partition: &Partition<'_>, ram: &GuestRegion) -> Result<()> {
    partition.map_unchecked(ram, 0, MAP_READ | MAP_WRITE | MAP_EXECUTE)
}

/// Maps `region` read-only at `gpa`, with the same lifetime rule as
/// [`map_guest_ram`].
pub(crate) fn map_read_only(
    partition: &Partition<'_>,
    region: &GuestRegion,
    gpa: u64,
) -> Result<()> {
    partition.map_unchecked(region, gpa, MAP_READ)
}

impl Partition<'_> {
    fn map_unchecked(&self, region: &GuestRegion, gpa: u64, flags: u32) -> Result<()> {
        // SAFETY: `region` is a live, page-aligned allocation of `size`
        // bytes. The caller keeps it alive until the partition is deleted.
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

    pub(crate) fn enter_protected_mode(&self, entry: u64, rsi: u64) -> Result<()> {
        let data = flat_segment(SELECTOR_DATA, SEGMENT_DATA_ATTRIBUTES);
        self.set_registers(
            &[
                REG_CS, REG_DS, REG_ES, REG_SS, REG_FS, REG_GS, REG_CR0, REG_RIP, REG_RFLAGS,
                REG_RSI,
            ],
            &[
                flat_segment(SELECTOR_CODE, SEGMENT_CODE_ATTRIBUTES),
                data,
                data,
                data,
                data,
                data,
                RegisterValue([CR0_PE_ET, 0]),
                RegisterValue([entry, 0]),
                RegisterValue([RFLAGS_RESET, 0]),
                RegisterValue([rsi, 0]),
            ],
        )
    }

    pub(crate) fn skip_instruction(&self, info: &ExitInfo) -> Result<()> {
        let rip = info.cpu.rip + u64::from(info.instruction_length);
        self.set_registers(&[REG_RIP], &[RegisterValue([rip, 0])])
    }

    pub(crate) fn finish_port_read(&self, info: &ExitInfo, size: u8, value: u32) -> Result<()> {
        let rip = info.cpu.rip + u64::from(info.instruction_length);
        self.set_registers(
            &[REG_RAX, REG_RIP],
            &[
                RegisterValue([merge_rax(info.rax, size, value), 0]),
                RegisterValue([rip, 0]),
            ],
        )
    }

    pub(crate) fn inject_interrupt(&self, vector: u8) -> Result<()> {
        // WHV_X64_PENDING_INTERRUPTION_REGISTER: pending bit, type 0
        // (external interrupt), vector at bit 16.
        let pending = 1 | u64::from(vector) << 16;
        self.set_registers(&[REG_PENDING_INTERRUPTION], &[RegisterValue([pending, 0])])
    }

    pub(crate) fn request_interrupt_window(&self) -> Result<()> {
        // WHV_X64_DELIVERABILITY_NOTIFICATIONS_REGISTER: InterruptNotification.
        self.set_registers(&[REG_DELIVERABILITY], &[RegisterValue([1 << 1, 0])])
    }

    pub(crate) fn cancel_parts(
        &self,
    ) -> (unsafe extern "system" fn(Handle, u32, u32) -> i32, Handle) {
        (self.api.cancel_run, self.handle)
    }
}

impl Api {
    pub(crate) fn processor_clock_hz(&self) -> Result<u64> {
        self.capability(CAP_PROCESSOR_CLOCK)
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

    fn io_context(info: u32, port: u16, rax: u64) -> [u8; 176] {
        let mut data = [0_u8; 176];
        data[20..24].copy_from_slice(&info.to_le_bytes());
        data[24..26].copy_from_slice(&port.to_le_bytes());
        data[32..40].copy_from_slice(&rax.to_le_bytes());
        data
    }

    #[test]
    fn decodes_a_byte_write() {
        let (exit, _) = decode_io(&io_context(0b0011, 0x3f8, 0x41));
        assert_eq!(
            exit,
            VmExit::PortWrite {
                port: 0x3f8,
                size: 1,
                value: 0x41
            }
        );
    }

    #[test]
    fn decodes_a_dword_read() {
        let (exit, _) = decode_io(&io_context(0b1000, 0xcf8, 0));
        assert_eq!(
            exit,
            VmExit::PortRead {
                port: 0xcf8,
                size: 4
            }
        );
    }

    #[test]
    fn string_io_is_not_treated_as_a_plain_access() {
        let (exit, _) = decode_io(&io_context(0b1_0011, 0x3f8, 0));
        assert!(matches!(exit, VmExit::Other { .. }));
    }

    #[test]
    fn port_reads_merge_into_rax_like_the_hardware() {
        assert_eq!(
            merge_rax(0x1122_3344_5566_7788, 1, 0xab),
            0x1122_3344_5566_77ab
        );
        assert_eq!(
            merge_rax(0x1122_3344_5566_7788, 2, 0xabcd),
            0x1122_3344_5566_abcd
        );
        assert_eq!(
            merge_rax(0x1122_3344_5566_7788, 4, 0xdead_beef),
            0xdead_beef
        );
    }

    #[test]
    fn flat_segments_encode_selector_and_attributes() {
        let RegisterValue([base, rest]) = flat_segment(0x10, 0xC09B);
        assert_eq!(base, 0);
        assert_eq!(rest as u32, 0xffff_ffff);
        assert_eq!((rest >> 32) as u16, 0x10);
        assert_eq!((rest >> 48) as u16, 0xC09B);
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
