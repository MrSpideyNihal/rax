//! The Linux x86 boot protocol.
//!
//! See `Documentation/arch/x86/boot.rst` in the kernel source. RAX uses the
//! 32-bit entry point: the kernel's own stub then switches to long mode.

use std::fmt;

/// Where `boot_params` (the "zero page") is placed.
pub const BOOT_PARAMS_GPA: u64 = 0x7000;
/// Where the boot GDT is placed.
pub const GDT_GPA: u64 = 0x6000;
/// The GDT the boot protocol requires, with flat 4 GiB segments at the
/// selectors it names: `__BOOT_CS` at 0x10 and `__BOOT_DS` at 0x18. Older
/// kernels load these before setting up their own table.
pub const BOOT_GDT: [u64; 4] = [0, 0, 0x00cf_9b00_0000_ffff, 0x00cf_9300_0000_ffff];
/// Where the command line is placed.
pub const CMDLINE_GPA: u64 = 0x2_0000;
/// Largest command line RAX will pass.
pub const CMDLINE_MAX: usize = 0x1000;

const SECTOR: usize = 512;
const DEFAULT_SETUP_SECTS: usize = 4;
const MIN_PROTOCOL: u16 = 0x020a;
const HEADER_START: usize = 0x1f1;

const OFF_E820_ENTRIES: usize = 0x1e8;
const OFF_SETUP_SECTS: usize = 0x1f1;
const OFF_BOOT_FLAG: usize = 0x1fe;
const OFF_JUMP: usize = 0x200;
const OFF_MAGIC: usize = 0x202;
const OFF_VERSION: usize = 0x206;
const OFF_TYPE_OF_LOADER: usize = 0x210;
const OFF_LOADFLAGS: usize = 0x211;
const OFF_CODE32_START: usize = 0x214;
const OFF_RAMDISK_IMAGE: usize = 0x218;
const OFF_RAMDISK_SIZE: usize = 0x21c;
const OFF_HEAP_END: usize = 0x224;
const OFF_CMD_LINE_PTR: usize = 0x228;
const OFF_INITRD_MAX: usize = 0x22c;
const OFF_RELOCATABLE: usize = 0x234;
const OFF_PREF_ADDRESS: usize = 0x258;
const OFF_INIT_SIZE: usize = 0x260;
const OFF_E820_TABLE: usize = 0x2d0;

const LOADER_UNDEFINED: u8 = 0xff;
const LOADED_HIGH: u8 = 0x01;
const CAN_USE_HEAP: u8 = 0x80;
const HEAP_END: u16 = 0xfe00;

const E820_RAM: u32 = 1;
const E820_RESERVED: u32 = 2;
const LOW_RAM_END: u64 = 0x9_fc00;
const HIGH_RAM_START: u64 = 0x10_0000;

/// Why a kernel image was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootError {
    /// The file is too short to hold what its header promises.
    Truncated,
    /// The `0xAA55` flag or `HdrS` magic is missing.
    NotBzImage,
    /// The boot protocol predates what RAX needs.
    ProtocolTooOld(u16),
    /// The kernel cannot be loaded at the address it prefers.
    NotRelocatable,
    /// The kernel and initrd do not fit in guest RAM.
    RamTooSmall {
        /// Bytes required.
        needed: u64,
        /// Bytes available.
        available: u64,
    },
    /// The command line exceeds [`CMDLINE_MAX`].
    CmdlineTooLong,
}

impl fmt::Display for BootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "kernel image is truncated"),
            Self::NotBzImage => write!(f, "file is not a Linux bzImage"),
            Self::ProtocolTooOld(version) => {
                write!(f, "boot protocol {version:#06x} is too old")
            }
            Self::NotRelocatable => write!(f, "kernel is not relocatable"),
            Self::RamTooSmall { needed, available } => {
                write!(
                    f,
                    "needs {needed} bytes of RAM, only {available} configured"
                )
            }
            Self::CmdlineTooLong => write!(f, "kernel command line is too long"),
        }
    }
}

impl std::error::Error for BootError {}

fn read_u16(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn read_u32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn read_u64(data: &[u8], at: usize) -> u64 {
    u64::from(read_u32(data, at)) | u64::from(read_u32(data, at + 4)) << 32
}

fn write_u16(data: &mut [u8], at: usize, value: u16) {
    data[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(data: &mut [u8], at: usize, value: u32) {
    data[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(data: &mut [u8], at: usize, value: u64) {
    data[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// A validated `bzImage`.
#[derive(Debug, Clone, Copy)]
pub struct BzImage<'a> {
    image: &'a [u8],
    payload_offset: usize,
}

impl<'a> BzImage<'a> {
    /// Validates the header of `image`.
    pub fn parse(image: &'a [u8]) -> Result<Self, BootError> {
        if image.len() < OFF_INIT_SIZE + 4 {
            return Err(BootError::Truncated);
        }
        if read_u16(image, OFF_BOOT_FLAG) != 0xAA55 || &image[OFF_MAGIC..OFF_MAGIC + 4] != b"HdrS" {
            return Err(BootError::NotBzImage);
        }
        let version = read_u16(image, OFF_VERSION);
        if version < MIN_PROTOCOL {
            return Err(BootError::ProtocolTooOld(version));
        }
        if image[OFF_RELOCATABLE] == 0 {
            return Err(BootError::NotRelocatable);
        }
        let setup_sects = match usize::from(image[OFF_SETUP_SECTS]) {
            0 => DEFAULT_SETUP_SECTS,
            sects => sects,
        };
        let payload_offset = (setup_sects + 1) * SECTOR;
        if payload_offset >= image.len() {
            return Err(BootError::Truncated);
        }
        Ok(Self {
            image,
            payload_offset,
        })
    }

    /// The protected-mode kernel.
    #[must_use]
    pub fn payload(&self) -> &'a [u8] {
        &self.image[self.payload_offset..]
    }

    /// Where the protected-mode kernel is loaded, and where execution starts.
    #[must_use]
    pub fn load_address(&self) -> u64 {
        read_u64(self.image, OFF_PREF_ADDRESS)
    }

    fn init_size(&self) -> u64 {
        u64::from(read_u32(self.image, OFF_INIT_SIZE))
    }

    /// Where an initrd of `len` bytes goes: page aligned, as high as allowed.
    pub fn initrd_address(&self, len: usize, ram_bytes: u64) -> Result<u64, BootError> {
        let limit = ram_bytes.min(u64::from(read_u32(self.image, OFF_INITRD_MAX)) + 1);
        let kernel_end = self.load_address() + self.init_size();
        let needed = kernel_end + len as u64;
        if needed > limit {
            return Err(BootError::RamTooSmall {
                needed,
                available: limit,
            });
        }
        Ok((limit - len as u64) & !0xfff)
    }

    /// Builds the 4 KiB `boot_params` page.
    pub fn boot_params(
        &self,
        ram_bytes: u64,
        initrd: Option<(u64, usize)>,
    ) -> Result<[u8; 4096], BootError> {
        let mut page = [0_u8; 4096];
        // The setup header's length is encoded in the jump at offset 0x200.
        let header_end = (OFF_JUMP + 2 + usize::from(self.image[OFF_JUMP + 1])).min(page.len());
        page[HEADER_START..header_end].copy_from_slice(&self.image[HEADER_START..header_end]);

        page[OFF_TYPE_OF_LOADER] = LOADER_UNDEFINED;
        page[OFF_LOADFLAGS] |= LOADED_HIGH | CAN_USE_HEAP;
        write_u16(&mut page, OFF_HEAP_END, HEAP_END);
        write_u32(&mut page, OFF_CMD_LINE_PTR, CMDLINE_GPA as u32);
        write_u32(&mut page, OFF_CODE32_START, self.load_address() as u32);
        if let Some((address, len)) = initrd {
            write_u32(&mut page, OFF_RAMDISK_IMAGE, address as u32);
            write_u32(&mut page, OFF_RAMDISK_SIZE, len as u32);
        }
        if ram_bytes <= HIGH_RAM_START {
            return Err(BootError::RamTooSmall {
                needed: HIGH_RAM_START,
                available: ram_bytes,
            });
        }
        let map = [
            (0, LOW_RAM_END, E820_RAM),
            (LOW_RAM_END, HIGH_RAM_START - LOW_RAM_END, E820_RESERVED),
            (HIGH_RAM_START, ram_bytes - HIGH_RAM_START, E820_RAM),
        ];
        page[OFF_E820_ENTRIES] = map.len() as u8;
        for (index, (start, size, kind)) in map.into_iter().enumerate() {
            let at = OFF_E820_TABLE + index * 20;
            write_u64(&mut page, at, start);
            write_u64(&mut page, at + 8, size);
            write_u32(&mut page, at + 16, kind);
        }
        Ok(page)
    }
}

/// The boot GDT as bytes for guest memory.
#[must_use]
pub fn gdt_bytes() -> Vec<u8> {
    BOOT_GDT
        .iter()
        .flat_map(|entry| entry.to_le_bytes())
        .collect()
}

/// Returns `cmdline` as the NUL-terminated bytes the kernel expects.
pub fn cmdline_bytes(cmdline: &str) -> Result<Vec<u8>, BootError> {
    if cmdline.len() >= CMDLINE_MAX {
        return Err(BootError::CmdlineTooLong);
    }
    let mut bytes = cmdline.as_bytes().to_vec();
    bytes.push(0);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAM: u64 = 512 * 1024 * 1024;

    /// A fake image with the header fields of a modern kernel.
    fn image() -> Vec<u8> {
        let mut data = vec![0_u8; 0x1000];
        data[OFF_SETUP_SECTS] = 1;
        write_u16(&mut data, OFF_BOOT_FLAG, 0xAA55);
        data[OFF_JUMP + 1] = 0x6a;
        data[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(b"HdrS");
        write_u16(&mut data, OFF_VERSION, 0x020f);
        data[OFF_RELOCATABLE] = 1;
        write_u32(&mut data, OFF_INITRD_MAX, 0x7fff_ffff);
        write_u64(&mut data, OFF_PREF_ADDRESS, 0x100_0000);
        write_u32(&mut data, OFF_INIT_SIZE, 0x200_0000);
        data
    }

    #[test]
    fn finds_the_payload_after_the_setup_sectors() {
        let data = image();
        assert_eq!(
            BzImage::parse(&data).unwrap().payload().len(),
            0x1000 - 1024
        );
    }

    #[test]
    fn zero_setup_sects_means_four() {
        let mut data = image();
        data[OFF_SETUP_SECTS] = 0;
        assert_eq!(
            BzImage::parse(&data).unwrap().payload().len(),
            0x1000 - 5 * SECTOR
        );
    }

    #[test]
    fn rejects_files_that_are_not_kernels() {
        assert_eq!(BzImage::parse(&[0; 16]).unwrap_err(), BootError::Truncated);
        assert_eq!(
            BzImage::parse(&[0; 0x1000]).unwrap_err(),
            BootError::NotBzImage
        );
    }

    #[test]
    fn rejects_old_protocols_and_fixed_kernels() {
        let mut data = image();
        write_u16(&mut data, OFF_VERSION, 0x0204);
        assert_eq!(
            BzImage::parse(&data).unwrap_err(),
            BootError::ProtocolTooOld(0x0204)
        );
        let mut data = image();
        data[OFF_RELOCATABLE] = 0;
        assert_eq!(
            BzImage::parse(&data).unwrap_err(),
            BootError::NotRelocatable
        );
    }

    #[test]
    fn boot_params_carry_the_loader_fields() {
        let data = image();
        let page = BzImage::parse(&data)
            .unwrap()
            .boot_params(RAM, Some((0x1000_0000, 0x1234)))
            .unwrap();
        assert_eq!(page[OFF_TYPE_OF_LOADER], LOADER_UNDEFINED);
        assert_eq!(page[OFF_LOADFLAGS] & CAN_USE_HEAP, CAN_USE_HEAP);
        assert_eq!(read_u32(&page, OFF_CMD_LINE_PTR), CMDLINE_GPA as u32);
        assert_eq!(read_u32(&page, OFF_RAMDISK_IMAGE), 0x1000_0000);
        assert_eq!(read_u32(&page, OFF_RAMDISK_SIZE), 0x1234);
        assert_eq!(read_u32(&page, OFF_CODE32_START), 0x100_0000);
        assert_eq!(read_u16(&page, OFF_BOOT_FLAG), 0xAA55);
    }

    #[test]
    fn memory_map_covers_ram_and_reserves_the_legacy_hole() {
        let data = image();
        let page = BzImage::parse(&data)
            .unwrap()
            .boot_params(RAM, None)
            .unwrap();
        assert_eq!(page[OFF_E820_ENTRIES], 3);
        let entry = |index: usize| {
            let at = OFF_E820_TABLE + index * 20;
            (
                read_u64(&page, at),
                read_u64(&page, at + 8),
                read_u32(&page, at + 16),
            )
        };
        assert_eq!(entry(0), (0, LOW_RAM_END, E820_RAM));
        assert_eq!(entry(1).2, E820_RESERVED);
        assert_eq!(entry(2), (HIGH_RAM_START, RAM - HIGH_RAM_START, E820_RAM));
    }

    #[test]
    fn places_the_initrd_high_and_page_aligned() {
        let data = image();
        let kernel = BzImage::parse(&data).unwrap();
        let address = kernel.initrd_address(0x1800, RAM).unwrap();
        assert_eq!(address, RAM - 0x2000);
        assert_eq!(address & 0xfff, 0);
    }

    #[test]
    fn refuses_an_initrd_that_would_overlap_the_kernel() {
        let data = image();
        let kernel = BzImage::parse(&data).unwrap();
        assert!(matches!(
            kernel.initrd_address(0x100_0000, 0x300_0000),
            Err(BootError::RamTooSmall { .. })
        ));
    }

    #[test]
    fn gdt_has_the_protocol_selectors() {
        let bytes = gdt_bytes();
        assert_eq!(bytes.len(), 32);
        assert_eq!(read_u64(&bytes, 0x10), 0x00cf_9b00_0000_ffff, "__BOOT_CS");
        assert_eq!(read_u64(&bytes, 0x18), 0x00cf_9300_0000_ffff, "__BOOT_DS");
    }

    #[test]
    fn command_lines_are_nul_terminated_and_bounded() {
        assert_eq!(cmdline_bytes("a").unwrap(), b"a\0");
        assert_eq!(
            cmdline_bytes(&"x".repeat(CMDLINE_MAX)).unwrap_err(),
            BootError::CmdlineTooLong
        );
    }
}
