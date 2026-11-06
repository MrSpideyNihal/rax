//! A safe virtual machine handle over the Windows Hypervisor Platform.
//!
//! The monitor in `rax-vmm` forbids `unsafe`, so everything it needs from the
//! hypervisor is exposed here as a small safe interface.

use std::marker::PhantomData;
use std::path::Path;

use crate::virgl::Renderer;
use crate::whp::{
    Api, ExitInfo, GuestRegion, Handle, PAGE, Partition, map_guest_memory, map_guest_ram,
    map_read_only, read_exit,
};
use crate::{Error, Result};

/// A one-vCPU virtual machine with RAM mapped from guest address zero.
pub struct Vm<'a> {
    // Declared first so the partition is deleted before its RAM is released.
    partition: Partition<'a>,
    ram: GuestRegion,
    /// One page of `0xFF`, mapped wherever the guest probes absent hardware.
    floating_page: GuestRegion,
    framebuffer: Option<GuestRegion>,
}

impl<'a> Vm<'a> {
    /// Creates the machine and maps `ram_bytes` of zeroed RAM.
    pub fn new(api: &'a Api, ram_bytes: usize) -> Result<Self> {
        let ram = GuestRegion::new(ram_bytes)?;
        let partition = Partition::new(api, 1)?;
        map_guest_ram(&partition, &ram)?;
        partition.create_processor(0)?;
        let floating_page = GuestRegion::new(PAGE)?;
        floating_page.write_at(0, &[0xff; PAGE])?;
        Ok(Self {
            partition,
            ram,
            floating_page,
            framebuffer: None,
        })
    }

    /// Copies `bytes` into guest RAM at `gpa`.
    pub fn write_ram(&self, gpa: u64, bytes: &[u8]) -> Result<()> {
        self.ram.write_at(gpa, bytes)
    }

    /// Maps zeroed memory at `gpa` for the guest to use as a screen.
    pub fn add_framebuffer(&mut self, gpa: u64, size: usize) -> Result<()> {
        let region = GuestRegion::new(size)?;
        map_guest_memory(&self.partition, &region, gpa)?;
        self.framebuffer = Some(region);
        Ok(())
    }

    /// Copies the screen into `buffer`. Does nothing without a framebuffer.
    pub fn read_framebuffer(&self, buffer: &mut [u8]) -> Result<()> {
        match &self.framebuffer {
            Some(region) => region.read_at(0, buffer),
            None => Ok(()),
        }
    }

    /// Fills `buffer` from guest RAM at `gpa`.
    pub fn read_ram(&self, gpa: u64, buffer: &mut [u8]) -> Result<()> {
        self.ram.read_at(gpa, buffer)
    }

    /// Starts the GPU renderer from `folder`. It may only use guest RAM while
    /// the vCPU is stopped, and must stay on this thread.
    pub fn start_renderer(&self, folder: &Path) -> Result<Renderer<'_>> {
        let (ram, size) = self.ram.raw_parts();
        Renderer::start(folder, ram, size)
    }

    /// Makes reads of the page holding `gpa` return all ones, as reads of an
    /// unclaimed address do on a physical bus. Writes there still exit.
    pub fn map_floating_page(&self, gpa: u64) -> Result<()> {
        map_read_only(
            &self.partition,
            &self.floating_page,
            gpa & !(PAGE as u64 - 1),
        )
    }

    /// Enters 32-bit protected mode with flat segments at `entry`, with `rsi`
    /// pointing at the boot parameters, as the Linux boot protocol requires.
    pub fn enter_protected_mode(
        &self,
        entry: u64,
        rsi: u64,
        gdt_gpa: u64,
        gdt_bytes: u16,
    ) -> Result<()> {
        self.partition
            .enter_protected_mode(entry, rsi, gdt_gpa, gdt_bytes)
    }

    /// Runs the vCPU until the next exit.
    pub fn run(&self) -> Result<ExitInfo> {
        read_exit(&self.partition)
    }

    /// Completes a port read by storing `value` in the low `size` bytes of
    /// RAX, then moves past the instruction.
    pub fn finish_port_read(&self, info: &ExitInfo, size: u8, value: u32) -> Result<()> {
        self.partition.finish_port_read(info, size, value)
    }

    /// Moves past the instruction that caused the exit.
    pub fn skip_instruction(&self, info: &ExitInfo) -> Result<()> {
        self.partition.skip_instruction(info)
    }

    /// Reads general register `index`, numbered as x86 encodes them.
    pub fn general_register(&self, index: u8) -> Result<u64> {
        self.partition.general_register(index)
    }

    /// Finishes a decoded device load.
    pub fn finish_load(
        &self,
        info: &ExitInfo,
        index: u8,
        size: u8,
        value: u32,
        zero_extend: bool,
        length: u8,
    ) -> Result<()> {
        self.partition
            .finish_load(info, index, size, value, zero_extend, length)
    }

    /// Moves past a decoded instruction of `length` bytes.
    pub fn advance(&self, info: &ExitInfo, length: u8) -> Result<()> {
        self.partition.advance(info, length)
    }

    /// Delivers an external interrupt. Only valid when `info.interruptible`.
    pub fn inject_interrupt(&self, vector: u8) -> Result<()> {
        self.partition.inject_interrupt(vector)
    }

    /// Asks for an exit as soon as the guest can take an interrupt.
    pub fn request_interrupt_window(&self) -> Result<()> {
        self.partition.request_interrupt_window()
    }

    /// A handle other threads use to stop the vCPU.
    #[must_use]
    pub fn kicker(&self) -> Kicker<'_> {
        let (cancel, handle) = self.partition.cancel_parts();
        Kicker {
            cancel,
            handle,
            _vm: PhantomData,
        }
    }
}

/// Forces [`Vm::run`] to return. Borrowing the `Vm` keeps the partition alive.
pub struct Kicker<'a> {
    cancel: unsafe extern "system" fn(Handle, u32, u32) -> i32,
    handle: Handle,
    _vm: PhantomData<&'a ()>,
}

// SAFETY: `handle` is only passed to WHvCancelRunVirtualProcessor, which
// exists to be called from a thread other than the one running the vCPU. The
// lifetime keeps the partition alive for as long as the Kicker exists.
unsafe impl Send for Kicker<'_> {}
// SAFETY: as above. The call needs no synchronisation on the caller's side.
unsafe impl Sync for Kicker<'_> {}

impl Kicker<'_> {
    /// Requests an exit. Failure is ignored: it only means the vCPU is not
    /// running, and the caller's pending work is seen on its next loop.
    pub fn kick(&self) {
        // SAFETY: the partition outlives `self`, and vCPU 0 always exists.
        let _ = unsafe { (self.cancel)(self.handle, 0, 0) };
    }
}

/// Reads the processor clock in hertz, when the hypervisor reports one.
pub fn tsc_hz(api: &Api) -> Option<u64> {
    api.processor_clock_hz().ok().filter(|&hz| hz != 0)
}

/// Turns a guest address range check into an error.
pub(crate) fn check_range(gpa: u64, len: usize, size: usize) -> Result<usize> {
    let start = usize::try_from(gpa).ok();
    match start.and_then(|s| s.checked_add(len).map(|end| (s, end))) {
        Some((s, end)) if end <= size => Ok(s),
        _ => Err(Error::GuestRange { gpa, len }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_check_accepts_exact_fit() {
        assert_eq!(check_range(0x10, 0x10, 0x20), Ok(0x10));
    }

    #[test]
    fn range_check_rejects_overrun_and_overflow() {
        assert!(check_range(0x11, 0x10, 0x20).is_err());
        assert!(check_range(u64::MAX, 2, 0x20).is_err());
    }
}
