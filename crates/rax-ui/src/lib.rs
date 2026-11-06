//! The launcher, drawn pixel by pixel.
//!
//! There is no toolkit and there are no image files. The art is generated
//! from code and the font is a hand-written 5 by 7 grid, so the interface
//! adds almost nothing to the program. Everything here is plain logic that
//! fills a buffer, which is what makes it testable without a window.

#![forbid(unsafe_code)]

mod canvas;
mod font;
mod launcher;
mod scene;

pub use canvas::{Canvas, Color};
pub use launcher::{
    Action, Launcher, MEMORY_CHOICES_MIB, SCREEN_CHOICES, Settings, UI_HEIGHT, UI_WIDTH,
};
pub use scene::sunset;
