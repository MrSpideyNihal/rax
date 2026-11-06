//! The launcher screen: state, hit testing and drawing.

use crate::canvas::{Canvas, Color};
use crate::font::{self, GLYPH_HEIGHT};
use crate::scene;

/// Launcher width in pixels.
pub const UI_WIDTH: i32 = 960;
/// Launcher height in pixels.
pub const UI_HEIGHT: i32 = 600;

/// Memory choices offered, in MiB.
pub const MEMORY_CHOICES_MIB: [usize; 4] = [1024, 2048, 3072, 4096];
/// Screen choices offered, as width and height.
pub const SCREEN_CHOICES: [(u16, u16); 3] = [(1280, 720), (1600, 900), (1920, 1080)];

const INK: Color = Color::hex(0xf4ecff);
const MUTED: Color = Color::hex(0xcfc0ea);
const DIM: Color = Color::hex(0x8d7bb0);
const PRIMARY: Color = Color::hex(0x6a4dff);
const PRIMARY_HOVER: Color = Color::hex(0x8066ff);
const PANEL: Color = Color::hex(0x1b1036);
const SELECTED: Color = Color::hex(0x6a4dff);
const WARNING: Color = Color::hex(0xffc66b);

const LEFT: i32 = 52;
const TITLE_Y: i32 = 48;
const TITLE_SCALE: i32 = 11;
const START: Rect = Rect::new(LEFT, 214, 256, 60);
const CARD: Rect = Rect::new(600, 48, 308, 226);
const CHOICE_X: i32 = 620;
const CHOICE_GAP: i32 = 6;
const MEMORY_Y: i32 = 94;
const SCREEN_Y: i32 = 192;
const CHOICE_HEIGHT: i32 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Rect {
    const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn contains(self, x: i32, y: i32) -> bool {
        (self.x..self.x + self.width).contains(&x) && (self.y..self.y + self.height).contains(&y)
    }
}

/// The choices a user can make, saved between runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Index into [`MEMORY_CHOICES_MIB`].
    pub memory: usize,
    /// Index into [`SCREEN_CHOICES`].
    pub screen: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            memory: 1,
            screen: 0,
        }
    }
}

impl Settings {
    /// Parses the settings file, ignoring anything unknown or out of range.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        for line in text.lines() {
            match line
                .split_once('=')
                .map(|(key, value)| (key.trim(), value.trim()))
            {
                Some(("memory_mib", value)) => {
                    if let Some(index) = MEMORY_CHOICES_MIB
                        .iter()
                        .position(|mib| value.parse() == Ok(*mib))
                    {
                        settings.memory = index;
                    }
                }
                Some(("screen", value)) => {
                    if let Some(index) = SCREEN_CHOICES
                        .iter()
                        .position(|(width, height)| value == format!("{width}x{height}"))
                    {
                        settings.screen = index;
                    }
                }
                _ => {}
            }
        }
        settings
    }

    /// Writes the settings file.
    #[must_use]
    pub fn serialize(self) -> String {
        let (width, height) = SCREEN_CHOICES[self.screen];
        format!(
            "memory_mib={}\nscreen={width}x{height}\n",
            MEMORY_CHOICES_MIB[self.memory]
        )
    }
}

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Start Android with these settings.
    Start(Settings),
    /// A setting changed, so it should be saved.
    Save(Settings),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Start,
    Memory(usize),
    Screen(usize),
}

/// The launcher screen.
pub struct Launcher {
    background: Canvas,
    frame: Canvas,
    settings: Settings,
    ready: bool,
    hover: Option<Control>,
    dirty: bool,
}

fn choice_rect(y: i32, index: usize, count: usize) -> Rect {
    let count = count as i32;
    let width = (CARD.width - 40 - CHOICE_GAP * (count - 1)) / count;
    Rect::new(
        CHOICE_X + index as i32 * (width + CHOICE_GAP),
        y,
        width,
        CHOICE_HEIGHT,
    )
}

fn memory_label(mib: usize) -> String {
    format!("{} GB", mib / 1024)
}

fn screen_label(height: u16) -> String {
    format!("{height}P")
}

impl Launcher {
    /// Creates the screen. `ready` says whether the Android files are present.
    #[must_use]
    pub fn new(settings: Settings, ready: bool) -> Self {
        let background = scene::sunset(UI_WIDTH, UI_HEIGHT);
        let frame = background.clone();
        Self {
            background,
            frame,
            settings,
            ready,
            hover: None,
            dirty: true,
        }
    }

    /// The current settings.
    #[must_use]
    pub fn settings(&self) -> Settings {
        self.settings
    }

    fn control_at(&self, x: i32, y: i32) -> Option<Control> {
        if self.ready && START.contains(x, y) {
            return Some(Control::Start);
        }
        let memory = (0..MEMORY_CHOICES_MIB.len())
            .find(|&i| choice_rect(MEMORY_Y, i, MEMORY_CHOICES_MIB.len()).contains(x, y));
        if let Some(index) = memory {
            return Some(Control::Memory(index));
        }
        (0..SCREEN_CHOICES.len())
            .find(|&i| choice_rect(SCREEN_Y, i, SCREEN_CHOICES.len()).contains(x, y))
            .map(Control::Screen)
    }

    /// Tracks the pointer, in launcher coordinates.
    pub fn pointer(&mut self, x: i32, y: i32) {
        let hover = self.control_at(x, y);
        if hover != self.hover {
            self.hover = hover;
            self.dirty = true;
        }
    }

    /// Handles a click, in launcher coordinates.
    pub fn click(&mut self, x: i32, y: i32) -> Option<Action> {
        match self.control_at(x, y)? {
            Control::Start => Some(Action::Start(self.settings)),
            Control::Memory(index) => self.change(|settings| settings.memory = index),
            Control::Screen(index) => self.change(|settings| settings.screen = index),
        }
    }

    fn change(&mut self, apply: impl FnOnce(&mut Settings)) -> Option<Action> {
        let before = self.settings;
        apply(&mut self.settings);
        if self.settings == before {
            return None;
        }
        self.dirty = true;
        Some(Action::Save(self.settings))
    }

    /// Forces the next draw, such as after returning from Android.
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Redraws if anything changed, returning the pixels when it did.
    pub fn draw(&mut self) -> Option<&[u8]> {
        if !std::mem::take(&mut self.dirty) {
            return None;
        }
        self.frame.copy_from(&self.background);
        self.draw_title();
        self.draw_start();
        self.draw_card();
        Some(self.frame.pixels())
    }

    fn draw_title(&mut self) {
        let frame = &mut self.frame;
        frame.text(LEFT + 4, TITLE_Y + 4, "RAX", TITLE_SCALE, PANEL);
        frame.text(LEFT, TITLE_Y, "RAX", TITLE_SCALE, INK);
        let tagline_y = TITLE_Y + GLYPH_HEIGHT * TITLE_SCALE + 26;
        let tagline = "Android, without the weight.";
        frame.text(LEFT + 2, tagline_y + 2, tagline, 2, PANEL);
        frame.text(LEFT, tagline_y, tagline, 2, INK);
    }

    fn draw_start(&mut self) {
        let frame = &mut self.frame;
        if !self.ready {
            frame.panel(START.x, START.y, 480, START.height, 4, PANEL, 215);
            frame.text(
                START.x + 22,
                START.y + 12,
                "Android files not found",
                2,
                WARNING,
            );
            frame.text(
                START.x + 22,
                START.y + 36,
                "Run scripts/fetch-android.ps1",
                2,
                MUTED,
            );
            return;
        }
        let color = if self.hover == Some(Control::Start) {
            PRIMARY_HOVER
        } else {
            PRIMARY
        };
        frame.panel(START.x, START.y, START.width, START.height, 4, color, 255);
        let label = "Start Android";
        let width = font::text_width(label, 3);
        frame.text(
            START.x + (START.width - width) / 2,
            START.y + (START.height - GLYPH_HEIGHT * 3) / 2,
            label,
            3,
            INK,
        );
    }

    fn draw_choice(&mut self, rect: Rect, label: &str, selected: bool, hovered: bool) {
        let frame = &mut self.frame;
        let (color, alpha) = match (selected, hovered) {
            (true, _) => (SELECTED, 255),
            (false, true) => (Color::hex(0x4a3a78), 255),
            (false, false) => (Color::hex(0x2c2150), 255),
        };
        frame.panel(rect.x, rect.y, rect.width, rect.height, 2, color, alpha);
        let width = font::text_width(label, 2);
        frame.text(
            rect.x + (rect.width - width) / 2,
            rect.y + (rect.height - GLYPH_HEIGHT * 2) / 2,
            label,
            2,
            if selected { INK } else { MUTED },
        );
    }

    fn draw_card(&mut self) {
        self.frame
            .panel(CARD.x, CARD.y, CARD.width, CARD.height, 4, PANEL, 215);
        self.frame.text(CHOICE_X, MEMORY_Y - 24, "Memory", 2, MUTED);
        self.frame.text(CHOICE_X, SCREEN_Y - 24, "Screen", 2, MUTED);
        for (index, mib) in MEMORY_CHOICES_MIB.into_iter().enumerate() {
            let rect = choice_rect(MEMORY_Y, index, MEMORY_CHOICES_MIB.len());
            let hovered = self.hover == Some(Control::Memory(index));
            let selected = self.settings.memory == index;
            self.draw_choice(rect, &memory_label(mib), selected, hovered);
        }
        for (index, (_, height)) in SCREEN_CHOICES.into_iter().enumerate() {
            let rect = choice_rect(SCREEN_Y, index, SCREEN_CHOICES.len());
            let hovered = self.hover == Some(Control::Screen(index));
            let selected = self.settings.screen == index;
            self.draw_choice(rect, &screen_label(height), selected, hovered);
        }
        let note = "Software graphics. Games are not ready yet.";
        self.frame.text(LEFT, UI_HEIGHT - 28, note, 1, DIM);
        let hint = "Click to capture the mouse. Right Ctrl releases it.";
        self.frame.text(LEFT, UI_HEIGHT - 44, hint, 1, DIM);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn center(rect: Rect) -> (i32, i32) {
        (rect.x + rect.width / 2, rect.y + rect.height / 2)
    }

    #[test]
    fn clicking_start_passes_the_current_settings() {
        let mut launcher = Launcher::new(Settings::default(), true);
        let (x, y) = center(START);
        assert_eq!(
            launcher.click(x, y),
            Some(Action::Start(Settings::default()))
        );
    }

    #[test]
    fn start_is_inactive_without_android_files() {
        let mut launcher = Launcher::new(Settings::default(), false);
        let (x, y) = center(START);
        assert_eq!(launcher.click(x, y), None);
    }

    #[test]
    fn choosing_memory_changes_and_saves_the_setting() {
        let mut launcher = Launcher::new(Settings::default(), true);
        let (x, y) = center(choice_rect(MEMORY_Y, 3, 4));
        let expected = Settings {
            memory: 3,
            screen: 0,
        };
        assert_eq!(launcher.click(x, y), Some(Action::Save(expected)));
        assert_eq!(launcher.settings(), expected);
    }

    #[test]
    fn choosing_the_current_value_saves_nothing() {
        let mut launcher = Launcher::new(Settings::default(), true);
        let (x, y) = center(choice_rect(SCREEN_Y, 0, 3));
        assert_eq!(launcher.click(x, y), None);
    }

    #[test]
    fn clicks_on_empty_space_do_nothing() {
        let mut launcher = Launcher::new(Settings::default(), true);
        assert_eq!(launcher.click(1, 1), None);
    }

    #[test]
    fn drawing_only_happens_when_something_changed() {
        let mut launcher = Launcher::new(Settings::default(), true);
        assert!(launcher.draw().is_some());
        assert!(launcher.draw().is_none());
        let (x, y) = center(START);
        launcher.pointer(x, y);
        assert!(launcher.draw().is_some());
        launcher.pointer(x, y);
        assert!(launcher.draw().is_none());
    }

    #[test]
    fn controls_do_not_overlap_and_stay_on_screen() {
        let mut rects = vec![START];
        rects.extend((0..4).map(|i| choice_rect(MEMORY_Y, i, 4)));
        rects.extend((0..3).map(|i| choice_rect(SCREEN_Y, i, 3)));
        for (index, rect) in rects.iter().enumerate() {
            assert!(rect.x >= 0 && rect.x + rect.width <= UI_WIDTH);
            assert!(rect.y >= 0 && rect.y + rect.height <= UI_HEIGHT);
            for other in &rects[index + 1..] {
                let apart = rect.x + rect.width <= other.x
                    || other.x + other.width <= rect.x
                    || rect.y + rect.height <= other.y
                    || other.y + other.height <= rect.y;
                assert!(apart, "{rect:?} overlaps {other:?}");
            }
        }
    }

    #[test]
    fn labels_fit_inside_their_buttons() {
        let width = font::text_width("Start Android", 3);
        assert!(width < START.width - 16);
        let rect = choice_rect(MEMORY_Y, 0, 4);
        assert!(font::text_width("4 GB", 2) < rect.width - 4);
        let rect = choice_rect(SCREEN_Y, 0, 3);
        assert!(font::text_width("1080P", 2) < rect.width - 4);
    }

    #[test]
    fn settings_survive_a_round_trip() {
        let settings = Settings {
            memory: 2,
            screen: 1,
        };
        assert_eq!(Settings::parse(&settings.serialize()), settings);
    }

    #[test]
    fn bad_settings_fall_back_to_defaults() {
        let text = "memory_mib=7\nscreen=1x1\nother=1\nbroken line\n";
        assert_eq!(Settings::parse(text), Settings::default());
    }
}
