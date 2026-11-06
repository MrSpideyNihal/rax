//! A minimal virtual machine monitor.
//!
//! Boots a Linux `bzImage` directly, without a BIOS, on one vCPU with a
//! serial console. Only the code in [`machine`] touches the hypervisor. Every
//! other module is plain logic, tested without one.

#![forbid(unsafe_code)]

pub mod boot;
pub mod bus;
pub mod cpio;
pub mod framebuffer;
pub mod mmio;
pub mod pci;
pub mod pic;
pub mod pit;
pub mod ps2;
pub mod rtc;
pub mod serial;
pub mod virtio_blk;
pub mod virtio_input;
pub mod virtio_net;

#[cfg(windows)]
pub mod machine;
