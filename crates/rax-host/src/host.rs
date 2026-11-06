//! Basic facts about the host machine.

use std::ffi::c_void;

use serde::Serialize;

use crate::dylib::Library;
use crate::{Error, Result};

#[repr(C)]
struct MemoryStatusEx {
    length: u32,
    memory_load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page_file: u64,
    avail_page_file: u64,
    total_virtual: u64,
    avail_virtual: u64,
    avail_extended_virtual: u64,
}

#[repr(C)]
struct OsVersionInfoW {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    service_pack: [u16; 128],
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    fn GetLastError() -> u32;
}

/// Static facts about the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostInfo {
    /// CPU brand string.
    pub cpu: String,
    /// Logical processors available to this process.
    pub logical_cpus: usize,
    /// Installed memory visible to Windows, in MiB.
    pub total_ram_mib: u64,
    /// Memory available right now, in MiB.
    pub available_ram_mib: u64,
    /// Windows version as `major.minor.build`.
    pub windows_version: String,
}

/// Collects [`HostInfo`].
pub fn probe() -> Result<HostInfo> {
    let (total_ram_mib, available_ram_mib) = memory_mib()?;
    Ok(HostInfo {
        cpu: cpu_brand(),
        logical_cpus: std::thread::available_parallelism().map_or(1, usize::from),
        total_ram_mib,
        available_ram_mib,
        windows_version: windows_version()?,
    })
}

fn memory_mib() -> Result<(u64, u64)> {
    const MIB: u64 = 1024 * 1024;
    // SAFETY: MemoryStatusEx is plain integers, so all-zero is a valid value.
    let mut status: MemoryStatusEx = unsafe { std::mem::zeroed() };
    status.length = size_of::<MemoryStatusEx>() as u32;
    // SAFETY: `status` is a valid MEMORYSTATUSEX with its length field set.
    if unsafe { GlobalMemoryStatusEx(&raw mut status) } == 0 {
        // SAFETY: GetLastError has no preconditions.
        let code = unsafe { GetLastError() };
        return Err(Error::Win32 {
            api: "GlobalMemoryStatusEx",
            code,
        });
    }
    Ok((status.total_phys / MIB, status.avail_phys / MIB))
}

fn windows_version() -> Result<String> {
    type RtlGetVersion = unsafe extern "system" fn(*mut c_void) -> i32;
    let ntdll = Library::load_system("ntdll.dll")?;
    // SAFETY: the signature matches RtlGetVersion and `ntdll` outlives the call.
    let get_version = unsafe { ntdll.symbol::<RtlGetVersion>(c"RtlGetVersion")? };
    // SAFETY: OsVersionInfoW is plain integers, so all-zero is a valid value.
    let mut info: OsVersionInfoW = unsafe { std::mem::zeroed() };
    info.size = size_of::<OsVersionInfoW>() as u32;
    // SAFETY: `info` is a valid RTL_OSVERSIONINFOW with its size field set.
    let status = unsafe { get_version((&raw mut info).cast()) };
    if status != 0 {
        return Err(Error::Hresult {
            api: "RtlGetVersion",
            code: status,
        });
    }
    Ok(format!("{}.{}.{}", info.major, info.minor, info.build))
}

fn cpu_brand() -> String {
    use std::arch::x86_64::__cpuid;
    let bytes: Vec<u8> = (0x8000_0002_u32..=0x8000_0004)
        .flat_map(|leaf| {
            let r = __cpuid(leaf);
            [r.eax, r.ebx, r.ecx, r.edx]
                .into_iter()
                .flat_map(u32::to_le_bytes)
        })
        .take_while(|&byte| byte != 0)
        .collect();
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_windows_abi() {
        assert_eq!(size_of::<MemoryStatusEx>(), 64);
        assert_eq!(size_of::<OsVersionInfoW>(), 276);
    }

    #[test]
    fn probe_reports_plausible_values() {
        let info = probe().unwrap();
        assert!(info.logical_cpus >= 1);
        assert!(info.total_ram_mib >= info.available_ram_mib);
        assert!(info.total_ram_mib > 0);
        assert!(!info.cpu.is_empty());
        assert!(info.windows_version.starts_with("10.0."));
    }
}
