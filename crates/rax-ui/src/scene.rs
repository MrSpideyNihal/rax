//! A pixel-art sunset, generated rather than stored.

use crate::canvas::{Canvas, Color};

/// Art is drawn on a coarse grid, then each cell is scaled up.
const CELL: i32 = 4;

/// Sky colours from the top of the scene to the horizon.
const SKY: [u32; 12] = [
    0x150d33, 0x1d1242, 0x281650, 0x38195c, 0x4c1d66, 0x64216e, 0x7f2a70, 0x9c356f, 0xbb4769,
    0xd65f5e, 0xe97f55, 0xf3a25a,
];

const SUN_CORE: Color = Color::hex(0xffe08a);
const SUN_EDGE: Color = Color::hex(0xffb85c);
const CLOUD: Color = Color::hex(0x6f3a86);
const CLOUD_LIT: Color = Color::hex(0xf0a35e);
const FAR_HILLS: Color = Color::hex(0x4b2563);
const NEAR_HILLS: Color = Color::hex(0x2c1645);
const FOREGROUND: Color = Color::hex(0x130b26);

/// Small repeatable hash, so the scene is identical on every run.
fn noise(value: i32) -> i32 {
    let mixed = (value as u32).wrapping_mul(2_654_435_761) ^ 0x9e37_79b9;
    (mixed.wrapping_mul(1_597_334_677) >> 16) as i32 & 0x7fff
}

fn hill(column: i32, seed: i32, step: i32, height: i32) -> i32 {
    // Blocky steps read as distant ridgelines.
    let block = column.div_euclid(step);
    let first = noise(block + seed) % height;
    let second = noise(block / 3 + seed * 7) % height;
    (first + second * 2) / 3
}

/// Draws the scene at `width` by `height`.
#[must_use]
pub fn sunset(width: i32, height: i32) -> Canvas {
    let columns = width / CELL + 1;
    let rows = height / CELL + 1;
    let horizon = rows * 76 / 100;
    let mut art = Grid::new(columns, rows);

    draw_sky(&mut art, horizon);
    draw_stars(&mut art, horizon);
    draw_sun(&mut art, columns * 56 / 100, horizon, rows / 5);
    draw_cloud(&mut art, columns * 6 / 100, horizon * 80 / 100, 42);
    draw_cloud(&mut art, columns * 72 / 100, horizon * 84 / 100, 36);
    draw_land(&mut art, horizon, rows);

    art.scaled(width, height)
}

struct Grid {
    columns: i32,
    rows: i32,
    cells: Vec<Color>,
}

impl Grid {
    fn new(columns: i32, rows: i32) -> Self {
        Self {
            columns,
            rows,
            cells: vec![FOREGROUND; (columns * rows) as usize],
        }
    }

    fn set(&mut self, column: i32, row: i32, color: Color) {
        if (0..self.columns).contains(&column) && (0..self.rows).contains(&row) {
            self.cells[(row * self.columns + column) as usize] = color;
        }
    }

    fn scaled(&self, width: i32, height: i32) -> Canvas {
        let mut canvas = Canvas::new(width, height);
        for row in 0..self.rows {
            for column in 0..self.columns {
                let color = self.cells[(row * self.columns + column) as usize];
                canvas.fill(column * CELL, row * CELL, CELL, CELL, color);
            }
        }
        canvas
    }
}

fn draw_sky(art: &mut Grid, horizon: i32) {
    let bands = SKY.len() as i32;
    for row in 0..horizon {
        let position = row * bands * 16 / horizon;
        let band = position / 16;
        let blend = position % 16;
        for column in 0..art.columns {
            // A checkerboard on the last rows of a band softens the step.
            let dithered = blend >= 12 && (column + row) & 1 == 0;
            let index = (band + i32::from(dithered)).clamp(0, bands - 1) as usize;
            art.set(column, row, Color::hex(SKY[index]));
        }
    }
}

fn draw_stars(art: &mut Grid, horizon: i32) {
    for star in 0..46 {
        let column = noise(star * 3 + 1) % art.columns;
        let row = noise(star * 5 + 2) % (horizon * 2 / 5);
        let bright = star % 5 == 0;
        art.set(
            column,
            row,
            Color::hex(if bright { 0xe8d9ff } else { 0x8f78c8 }),
        );
    }
}

fn draw_sun(art: &mut Grid, center: i32, horizon: i32, radius: i32) {
    for row in horizon - radius..horizon {
        let rise = horizon - row;
        let half = ((radius * radius - rise * rise) as f32).sqrt() as i32;
        // Slits widen towards the horizon, the classic retro sun.
        let slit = rise < radius / 2 && rise % 3 == 0;
        if slit {
            continue;
        }
        for column in center - half..center + half {
            let edge = (column - center).abs() + rise / 2 > half - 2;
            art.set(column, row, if edge { SUN_EDGE } else { SUN_CORE });
        }
    }
}

fn draw_cloud(art: &mut Grid, column: i32, row: i32, width: i32) {
    for (layer, inset) in [(0, width / 3), (1, width / 6), (2, 0)] {
        for offset in inset..width - inset {
            art.set(column + offset, row + layer, CLOUD);
        }
    }
    for offset in 0..width {
        art.set(column + offset, row + 3, CLOUD_LIT);
    }
}

fn draw_tree(art: &mut Grid, column: i32, base: i32, height: i32) {
    for level in 0..height {
        let half = (height - level) / 2;
        for offset in -half..=half {
            art.set(column + offset, base - level, FOREGROUND);
        }
    }
    art.set(column, base + 1, FOREGROUND);
}

fn draw_land(art: &mut Grid, horizon: i32, rows: i32) {
    for column in 0..art.columns {
        let far = horizon - hill(column, 11, 7, 7);
        let near = horizon + 4 - hill(column, 29, 5, 6);
        let front = horizon + 11 - hill(column, 53, 9, 4);
        for row in far..near.min(rows) {
            art.set(column, row, FAR_HILLS);
        }
        for row in near..front.min(rows) {
            art.set(column, row, NEAR_HILLS);
        }
        for row in front..rows {
            art.set(column, row, FOREGROUND);
        }
    }
    for tree in 0..22 {
        let column = noise(tree * 11 + 4) % art.columns;
        let base = horizon + 12 - hill(column, 53, 9, 4);
        draw_tree(art, column, base, 5 + noise(tree + 40) % 7);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scene_is_identical_every_time() {
        assert_eq!(sunset(200, 120), sunset(200, 120));
    }

    #[test]
    fn the_top_is_night_and_the_bottom_is_dark_land() {
        let scene = sunset(960, 600);
        assert_eq!(scene.get(2, 590), Some(FOREGROUND));
        let top = scene.get(480, 1).unwrap();
        assert!(top.2 > top.0, "the sky starts blue-violet");
    }

    #[test]
    fn the_scene_contains_the_sun() {
        let scene = sunset(960, 600);
        let found = (0..600).any(|y| (0..960).any(|x| scene.get(x, y) == Some(SUN_CORE)));
        assert!(found);
    }

    #[test]
    fn art_stays_on_the_pixel_grid() {
        let scene = sunset(160, 160);
        for y in (0..156).step_by(4) {
            for x in (0..156).step_by(4) {
                assert_eq!(scene.get(x, y), scene.get(x + 3, y + 3));
            }
        }
    }
}
