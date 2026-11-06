//! A BGRX pixel buffer with the few drawing operations the launcher needs.

use crate::font;

/// An opaque colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(pub u8, pub u8, pub u8);

impl Color {
    /// Builds a colour from `0xRRGGBB`.
    #[must_use]
    pub const fn hex(value: u32) -> Self {
        Self((value >> 16) as u8, (value >> 8) as u8, value as u8)
    }
}

/// A rectangular image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canvas {
    width: i32,
    height: i32,
    pixels: Vec<u8>,
}

impl Canvas {
    /// Creates a black canvas.
    #[must_use]
    pub fn new(width: i32, height: i32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; (width * height * 4) as usize],
        }
    }

    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> i32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> i32 {
        self.height
    }

    /// The BGRX bytes.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Replaces this canvas with `other`, which must be the same size.
    pub fn copy_from(&mut self, other: &Self) {
        self.pixels.copy_from_slice(&other.pixels);
    }

    fn offset(&self, x: i32, y: i32) -> Option<usize> {
        let inside = (0..self.width).contains(&x) && (0..self.height).contains(&y);
        inside.then(|| ((y * self.width + x) * 4) as usize)
    }

    /// Reads one pixel, or `None` outside the canvas.
    #[must_use]
    pub fn get(&self, x: i32, y: i32) -> Option<Color> {
        let at = self.offset(x, y)?;
        Some(Color(
            self.pixels[at + 2],
            self.pixels[at + 1],
            self.pixels[at],
        ))
    }

    /// Writes one pixel. Off-canvas writes are ignored.
    pub fn set(&mut self, x: i32, y: i32, color: Color) {
        if let Some(at) = self.offset(x, y) {
            self.pixels[at] = color.2;
            self.pixels[at + 1] = color.1;
            self.pixels[at + 2] = color.0;
        }
    }

    /// Fills a rectangle.
    pub fn fill(&mut self, x: i32, y: i32, width: i32, height: i32, color: Color) {
        for row in y..y + height {
            for column in x..x + width {
                self.set(column, row, color);
            }
        }
    }

    /// Mixes `color` over a rectangle, where `alpha` runs from 0 to 255.
    pub fn tint(&mut self, x: i32, y: i32, width: i32, height: i32, color: Color, alpha: u32) {
        let mix = |under: u8, over: u8| {
            ((u32::from(under) * (255 - alpha) + u32::from(over) * alpha) / 255) as u8
        };
        for row in y..y + height {
            for column in x..x + width {
                if let Some(under) = self.get(column, row) {
                    let mixed = Color(
                        mix(under.0, color.0),
                        mix(under.1, color.1),
                        mix(under.2, color.2),
                    );
                    self.set(column, row, mixed);
                }
            }
        }
    }

    /// Mixes a rectangle with stepped corners, which suit pixel art better
    /// than smooth ones.
    #[allow(clippy::too_many_arguments)]
    pub fn panel(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        step: i32,
        color: Color,
        alpha: u32,
    ) {
        self.tint(x + step * 2, y, width - step * 4, step, color, alpha);
        self.tint(x + step, y + step, width - step * 2, step, color, alpha);
        self.tint(x, y + step * 2, width, height - step * 4, color, alpha);
        self.tint(
            x + step,
            y + height - step * 2,
            width - step * 2,
            step,
            color,
            alpha,
        );
        self.tint(
            x + step * 2,
            y + height - step,
            width - step * 4,
            step,
            color,
            alpha,
        );
    }

    /// Draws text with its top-left corner at `(x, y)`.
    pub fn text(&mut self, x: i32, y: i32, text: &str, scale: i32, color: Color) {
        let mut pen = x;
        for character in text.chars() {
            let rows = font::glyph(character);
            for (row, bits) in rows.iter().enumerate() {
                for column in 0..font::GLYPH_WIDTH {
                    if bits & (1 << (font::GLYPH_WIDTH - 1 - column)) != 0 {
                        self.fill(
                            pen + column * scale,
                            y + row as i32 * scale,
                            scale,
                            scale,
                            color,
                        );
                    }
                }
            }
            pen += font::ADVANCE * scale;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colours_split_into_channels() {
        assert_eq!(Color::hex(0x12_34_56), Color(0x12, 0x34, 0x56));
    }

    #[test]
    fn pixels_are_stored_as_bgrx() {
        let mut canvas = Canvas::new(1, 1);
        canvas.set(0, 0, Color(1, 2, 3));
        assert_eq!(canvas.pixels(), &[3, 2, 1, 0]);
    }

    #[test]
    fn drawing_off_the_canvas_is_ignored() {
        let mut canvas = Canvas::new(4, 4);
        canvas.fill(-2, -2, 3, 3, Color(9, 9, 9));
        canvas.set(100, 100, Color(1, 1, 1));
        assert_eq!(canvas.get(0, 0), Some(Color(9, 9, 9)));
        assert_eq!(canvas.get(1, 1), Some(Color(0, 0, 0)));
        assert_eq!(canvas.get(100, 100), None);
    }

    #[test]
    fn tint_mixes_towards_the_colour() {
        let mut canvas = Canvas::new(1, 1);
        canvas.tint(0, 0, 1, 1, Color(255, 255, 255), 255);
        assert_eq!(canvas.get(0, 0), Some(Color(255, 255, 255)));
        canvas.tint(0, 0, 1, 1, Color(0, 0, 0), 0);
        assert_eq!(canvas.get(0, 0), Some(Color(255, 255, 255)));
    }

    #[test]
    fn panels_have_stepped_corners() {
        let mut canvas = Canvas::new(40, 40);
        canvas.panel(0, 0, 40, 40, 4, Color(255, 0, 0), 255);
        assert_eq!(canvas.get(0, 0), Some(Color(0, 0, 0)), "corner stays clear");
        assert_eq!(canvas.get(20, 20), Some(Color(255, 0, 0)));
    }

    #[test]
    fn text_draws_scaled_glyphs() {
        let mut canvas = Canvas::new(40, 40);
        canvas.text(0, 0, "I", 2, Color(255, 255, 255));
        // The top bar of the I spans columns 1 to 3, at double size.
        assert_eq!(canvas.get(2, 0), Some(Color(255, 255, 255)));
        assert_eq!(canvas.get(0, 0), Some(Color(0, 0, 0)));
    }

    #[test]
    fn copying_replaces_every_pixel() {
        let mut first = Canvas::new(2, 2);
        let mut second = Canvas::new(2, 2);
        second.set(1, 1, Color(5, 5, 5));
        first.copy_from(&second);
        assert_eq!(first, second);
    }
}
