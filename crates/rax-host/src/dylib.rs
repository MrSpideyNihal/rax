//! Run-time loading of system DLLs.

use std::ffi::{CStr, c_void};
use std::ptr::NonNull;

use crate::{Error, Result};

const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn FreeLibrary(module: *mut c_void) -> i32;
    fn GetLastError() -> u32;
}

/// An owned handle to a DLL loaded from `System32`.
#[derive(Debug)]
pub struct Library {
    handle: NonNull<c_void>,
    name: String,
}

impl Library {
    /// Loads `name` from `System32` only.
    ///
    /// Restricting the search path means a planted DLL in the working
    /// directory or on `PATH` is never picked up.
    pub fn load_system(name: &str) -> Result<Self> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the
        // call, and a null file handle is the documented value for this API.
        let raw = unsafe {
            LoadLibraryExW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        NonNull::new(raw)
            .map(|handle| Self {
                handle,
                name: name.to_owned(),
            })
            .ok_or_else(|| {
                // SAFETY: GetLastError has no preconditions.
                let code = unsafe { GetLastError() };
                Error::LibraryMissing {
                    library: name.to_owned(),
                    code,
                }
            })
    }

    /// Resolves an export as the function pointer type `F`.
    ///
    /// # Safety
    ///
    /// `F` must be an `extern "system"` function pointer whose signature
    /// matches the export, and the returned pointer must not be called after
    /// this `Library` is dropped.
    pub unsafe fn symbol<F: Copy>(&self, symbol: &CStr) -> Result<F> {
        const {
            assert!(size_of::<F>() == size_of::<*mut c_void>());
        }
        // SAFETY: `handle` is a live module and `symbol` is NUL-terminated.
        let address = unsafe { GetProcAddress(self.handle.as_ptr(), symbol.as_ptr().cast()) };
        if address.is_null() {
            return Err(Error::SymbolMissing {
                library: self.name.clone(),
                symbol: symbol.to_string_lossy().into_owned(),
            });
        }
        // SAFETY: `F` is pointer-sized (asserted above) and the caller
        // guarantees it is the correct function pointer type for this export.
        Ok(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&address) })
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: `handle` came from LoadLibraryExW and is released only here.
        unsafe {
            FreeLibrary(self.handle.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type GetTickCount = unsafe extern "system" fn() -> u32;

    #[test]
    fn loads_kernel32_and_resolves_a_symbol() {
        let library = Library::load_system("kernel32.dll").expect("kernel32 is always present");
        // SAFETY: GetTickCount takes no arguments and returns a DWORD.
        let symbol = unsafe { library.symbol::<GetTickCount>(c"GetTickCount") };
        assert!(symbol.is_ok());
    }

    #[test]
    fn reports_a_missing_library() {
        let error = Library::load_system("rax-does-not-exist.dll").unwrap_err();
        assert!(matches!(error, Error::LibraryMissing { .. }));
    }

    #[test]
    fn reports_a_missing_symbol() {
        let library = Library::load_system("kernel32.dll").expect("kernel32 is always present");
        // SAFETY: the export does not exist, so nothing is ever called.
        let error = unsafe { library.symbol::<GetTickCount>(c"RaxNoSuchExport") }.unwrap_err();
        assert!(matches!(error, Error::SymbolMissing { .. }));
    }
}
