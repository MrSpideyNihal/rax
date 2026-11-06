//! A minimal virtual machine monitor.
//!
//! Boots a Linux `bzImage` directly, without a BIOS, on one vCPU with a
//! serial console. Only the code in [`machine`] touches the hypervisor. Every
//! other module is plain logic, tested without one.

#![forbid(unsafe_code)]

pub mod boot;
pub mod bus;
pub mod cpio;
pub mod pic;
pub mod pit;
pub mod rtc;
pub mod serial;

#[cfg(windows)]
pub mod machine;
