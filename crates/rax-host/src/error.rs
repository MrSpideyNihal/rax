//! Error type shared by the host probes.

use std::fmt;

/// Convenience alias for results in this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Failures from host probing and hypervisor measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A system DLL could not be loaded.
    LibraryMissing {
        /// DLL file name.
        library: String,
        /// Win32 error code from `GetLastError`.
        code: u32,
    },
    /// A DLL lacks an expected export.
    SymbolMissing {
        /// DLL file name.
        library: String,
        /// Export name.
        symbol: String,
    },
    /// A Windows Hypervisor Platform call returned a failing `HRESULT`.
    Hresult {
        /// API that failed.
        api: &'static str,
        /// The failing `HRESULT`.
        code: i32,
    },
    /// A Vulkan call returned a failing `VkResult`.
    Vulkan {
        /// API that failed.
        api: &'static str,
        /// The failing `VkResult`.
        code: i32,
    },
    /// A Win32 call failed.
    Win32 {
        /// API that failed.
        api: &'static str,
        /// Win32 error code from `GetLastError`.
        code: u32,
    },
    /// The measurement guest exited for a reason the harness did not expect.
    UnexpectedExit {
        /// `WHV_RUN_VP_EXIT_REASON` value.
        reason: u32,
        /// Guest instruction pointer at the exit.
        rip: u64,
    },
    /// A write to guest memory fell outside mapped RAM.
    GuestRange {
        /// Guest physical address.
        gpa: u64,
        /// Length of the access.
        len: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LibraryMissing { library, code } => {
                write!(f, "could not load {library} (Win32 error {code})")
            }
            Self::SymbolMissing { library, symbol } => {
                write!(f, "{library} does not export {symbol}")
            }
            Self::Hresult { api, code } => {
                // Printed as the unsigned form Microsoft documents, such as 0x80370102.
                write!(
                    f,
                    "{api} failed with HRESULT {:#010x}",
                    code.cast_unsigned()
                )
            }
            Self::Vulkan { api, code } => write!(f, "{api} failed with VkResult {code}"),
            Self::Win32 { api, code } => write!(f, "{api} failed with Win32 error {code}"),
            Self::UnexpectedExit { reason, rip } => {
                write!(
                    f,
                    "guest exited unexpectedly: reason {reason:#x} at rip {rip:#x}"
                )
            }
            Self::GuestRange { gpa, len } => {
                write!(f, "{len} bytes at {gpa:#x} is outside guest RAM")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hresult_prints_in_documented_unsigned_form() {
        let error = Error::Hresult {
            api: "WHvCreatePartition",
            code: 0x8037_0102_u32.cast_signed(),
        };
        assert_eq!(
            error.to_string(),
            "WHvCreatePartition failed with HRESULT 0x80370102"
        );
    }
}
