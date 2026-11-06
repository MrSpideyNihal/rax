//! Windows host probes and hypervisor measurements for RAX.
//!
//! This crate holds every piece of `unsafe` OS integration in the project so
//! far. Optional system DLLs are resolved at run time, so a machine without
//! the Windows Hypervisor Platform or a Vulkan loader gets a diagnostic from
//! `rax doctor` rather than a failure to start.

#![cfg(windows)]

pub mod assess;
pub mod dylib;
pub mod error;
pub mod host;
pub mod stats;
pub mod virgl;
pub mod vm;
pub mod vulkan;
pub mod whp;
pub mod window;

pub use error::{Error, Result};
