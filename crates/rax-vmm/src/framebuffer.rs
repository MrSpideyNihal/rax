//! A linear framebuffer: guest memory that acts as a screen.
//!
//! Linux finds it through `screen_info` in `boot_params`, and its VESA
//! driver needs no device behind it. Android draws here in software, so this
//! is enough to boot and to see the screen, but it is not a gaming path.

/// Where the framebuffer sits, above any RAM RAX configures.
pub const FRAMEBUFFER_GPA: u32 = 0xe000_0000;

const BYTES_PER_PIXEL: usize = 4;
const SIZE_UNIT: usize = 64 * 1024;

const VIDEO_TYPE_VLFB: u8 = 0x23;
const OFF_IS_VGA: usize = 0x0f;
const OFF_WIDTH: usize = 0x12;
const OFF_HEIGHT: usize = 0x14;
const OFF_DEPTH: usize = 0x16;
const OFF_BASE: usize = 0x18;
const OFF_SIZE: usize = 0x1c;
const OFF_LINE_LENGTH: usize = 0x24;
const OFF_RED_SIZE: usize = 0x26;

/// Screen dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Framebuffer {
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
}

impl Framebuffer {
    /// Bytes of guest memory the screen needs.
    #[must_use]
    pub fn size_bytes(self) -> usize {
        usize::from(self.width) * usize::from(self.height) * BYTES_PER_PIXEL
    }

    /// Describes the screen in `screen_info`, the first bytes of
    /// `boot_params`, as 32-bit BGRX.
    pub fn write_screen_info(self, page: &mut [u8]) {
        let line_length =
            u16::try_from(usize::from(self.width) * BYTES_PER_PIXEL).unwrap_or(u16::MAX);
        // The VESA driver reads this field in units of 64 KiB.
        let size_units = self.size_bytes().div_ceil(SIZE_UNIT) as u32;
        page[OFF_IS_VGA] = VIDEO_TYPE_VLFB;
        page[OFF_WIDTH..OFF_WIDTH + 2].copy_from_slice(&self.width.to_le_bytes());
        page[OFF_HEIGHT..OFF_HEIGHT + 2].copy_from_slice(&self.height.to_le_bytes());
        page[OFF_DEPTH..OFF_DEPTH + 2].copy_from_slice(&32_u16.to_le_bytes());
        page[OFF_BASE..OFF_BASE + 4].copy_from_slice(&FRAMEBUFFER_GPA.to_le_bytes());
        page[OFF_SIZE..OFF_SIZE + 4].copy_from_slice(&size_units.to_le_bytes());
        page[OFF_LINE_LENGTH..OFF_LINE_LENGTH + 2].copy_from_slice(&line_length.to_le_bytes());
        // Size then position, for red, green, blue and reserved.
        page[OFF_RED_SIZE..OFF_RED_SIZE + 8].copy_from_slice(&[8, 16, 8, 8, 8, 0, 8, 24]);
    }

    /// Wraps BGRX `pixels` in a top-down 32-bit BMP file.
    #[must_use]
    pub fn to_bmp(self, pixels: &[u8]) -> Vec<u8> {
        const HEADERS: u32 = 54;
        let mut file = Vec::with_capacity(pixels.len() + HEADERS as usize);
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&(HEADERS + pixels.len() as u32).to_le_bytes());
        file.extend_from_slice(&[0; 4]);
        file.extend_from_slice(&HEADERS.to_le_bytes());
        file.extend_from_slice(&40_u32.to_le_bytes());
        file.extend_from_slice(&i32::from(self.width).to_le_bytes());
        // A negative height means the first row is the top of the image.
        file.extend_from_slice(&(-i32::from(self.height)).to_le_bytes());
        file.extend_from_slice(&1_u16.to_le_bytes());
        file.extend_from_slice(&32_u16.to_le_bytes());
        file.extend_from_slice(&[0; 24]);
        file.extend_from_slice(pixels);
        file
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Framebuffer = Framebuffer {
        width: 1280,
        height: 720,
    };

    fn u16_at(data: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([data[at], data[at + 1]])
    }

    fn u32_at(data: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
    }

    #[test]
    fn size_is_four_bytes_per_pixel() {
        assert_eq!(SCREEN.size_bytes(), 3_686_400);
    }

    #[test]
    fn screen_info_describes_a_linear_framebuffer() {
        let mut page = [0_u8; 4096];
        SCREEN.write_screen_info(&mut page);
        assert_eq!(page[OFF_IS_VGA], VIDEO_TYPE_VLFB);
        assert_eq!(u16_at(&page, OFF_WIDTH), 1280);
        assert_eq!(u16_at(&page, OFF_HEIGHT), 720);
        assert_eq!(u16_at(&page, OFF_DEPTH), 32);
        assert_eq!(u32_at(&page, OFF_BASE), FRAMEBUFFER_GPA);
        assert_eq!(u16_at(&page, OFF_LINE_LENGTH), 5120);
    }

    #[test]
    fn size_rounds_up_to_whole_units() {
        let mut page = [0_u8; 4096];
        SCREEN.write_screen_info(&mut page);
        assert_eq!(
            u32_at(&page, OFF_SIZE),
            57,
            "3,686,400 bytes in 64 KiB units"
        );
    }

    #[test]
    fn pixel_layout_is_bgrx() {
        let mut page = [0_u8; 4096];
        SCREEN.write_screen_info(&mut page);
        assert_eq!(page[0x26..0x2e], [8, 16, 8, 8, 8, 0, 8, 24]);
    }

    #[test]
    fn bmp_has_a_valid_top_down_header() {
        let tiny = Framebuffer {
            width: 2,
            height: 1,
        };
        let file = tiny.to_bmp(&[1, 2, 3, 0, 4, 5, 6, 0]);
        assert_eq!(&file[..2], b"BM");
        assert_eq!(u32_at(&file, 2), 62, "file size");
        assert_eq!(u32_at(&file, 10), 54, "pixel offset");
        assert_eq!(u32_at(&file, 18), 2, "width");
        assert_eq!(u32_at(&file, 22) as i32, -1, "negative means top-down");
        assert_eq!(u16_at(&file, 28), 32, "bits per pixel");
        assert_eq!(&file[54..], &[1, 2, 3, 0, 4, 5, 6, 0]);
    }
}
