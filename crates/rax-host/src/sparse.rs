//! Sparse files, which only take up the disk space actually written to.

use std::ffi::c_void;
use std::fs::File;
use std::os::windows::io::AsRawHandle;

use crate::{Error, Result};

const FSCTL_SET_SPARSE: u32 = 0x0009_00c4;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn DeviceIoControl(
        device: *mut c_void,
        code: u32,
        input: *mut c_void,
        input_size: u32,
        output: *mut c_void,
        output_size: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn GetLastError() -> u32;
}

/// Marks `file` as sparse. Do this before giving it a length, because a plain
/// file reserves all its space at once. It fails on disks that don't support
/// sparse files, where the file simply stays a normal one.
pub fn mark_sparse(file: &File) -> Result<()> {
    let mut returned = 0_u32;
    // SAFETY: the handle is live for the call, this control code takes no
    // buffers, `returned` is a valid out pointer, and a null overlapped
    // pointer selects a plain synchronous call.
    let done = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if done == 0 {
        // SAFETY: GetLastError has no preconditions.
        let code = unsafe { GetLastError() };
        return Err(Error::Win32 {
            api: "DeviceIoControl",
            code,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCompressedFileSizeW(name: *const u16, high: *mut u32) -> u32;
    }

    fn bytes_on_disk(path: &std::path::Path) -> u64 {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut high = 0_u32;
        // SAFETY: `wide` is NUL-terminated and `high` is a valid out pointer.
        let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &raw mut high) };
        u64::from(high) << 32 | u64::from(low)
    }

    #[test]
    fn a_large_empty_file_takes_almost_no_space() {
        const LENGTH: u64 = 64 * 1024 * 1024;
        let path = std::env::temp_dir().join("rax-sparse-test.tmp");
        let mut file = File::create(&path).unwrap();
        mark_sparse(&file).unwrap();
        file.set_len(LENGTH).unwrap();
        file.seek(SeekFrom::Start(LENGTH / 2)).unwrap();
        file.write_all(b"x").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let used = bytes_on_disk(&path);
        let length = std::fs::metadata(&path).unwrap().len();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(length, LENGTH);
        assert!(used < LENGTH / 16, "took {used} bytes");
    }
}
