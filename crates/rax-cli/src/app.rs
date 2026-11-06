//! The desktop app: the launcher window and a running Android session.
//!
//! Everything lives in one folder. The app reads and writes only beside its
//! own executable, so deleting that folder removes RAX completely.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use rax_host::window::{self, Button, Event, Window};
use rax_ui::{Action, Launcher, MEMORY_CHOICES_MIB, SCREEN_CHOICES, Settings, UI_HEIGHT, UI_WIDTH};
use rax_vmm::framebuffer::Framebuffer;
use rax_vmm::machine::{self, Config, Display};
use rax_vmm::ps2::GuestInput;

const SETTINGS_FILE: &str = "rax.cfg";
const KERNEL: &str = "guest/kernel";
const INITRD: &str = "guest/initrd.img";
const SYSTEM: &str = "guest/android-x86_64-9.0-r2.iso";
/// A session has no time limit that a person would ever reach.
const SESSION_LIMIT: Duration = Duration::from_secs(60 * 60 * 24 * 365);
const FRAME_DELAY: Duration = Duration::from_millis(16);
const IDLE_DELAY: Duration = Duration::from_millis(30);
/// Right Ctrl, which releases the mouse.
const RELEASE_SCANCODE: u8 = 0x1d;

const ANDROID_CMDLINE: &str = "console=ttyS0 nolapic noapic acpi=off nosmp reboot=k panic=-1 \
    tsc=reliable androidboot.hardware=android_x86_64 androidboot.console=ttyS0 \
    androidboot.selinux=permissive nomodeset HWACCEL=0 SRC= DATA= DEBUG=";

fn has_android(root: &Path) -> bool {
    [KERNEL, INITRD, SYSTEM]
        .iter()
        .all(|file| root.join(file).is_file())
}

/// Finds the RAX folder: next to the executable, or above it during
/// development, where the build output sits inside the repository.
fn find_root() -> PathBuf {
    let executable = std::env::current_exe().unwrap_or_default();
    let start = executable.parent().unwrap_or(Path::new("."));
    start
        .ancestors()
        .take(4)
        .find(|folder| has_android(folder))
        .unwrap_or(start)
        .to_path_buf()
}

/// Maps a window position to launcher coordinates.
fn to_ui(x: i32, y: i32, client: (i32, i32)) -> (i32, i32) {
    (
        x * UI_WIDTH / client.0.max(1),
        y * UI_HEIGHT / client.1.max(1),
    )
}

/// Runs the desktop app.
pub fn run() -> Result<(), Box<dyn Error>> {
    window::hide_own_console();
    let root = find_root();
    let settings_path = root.join(SETTINGS_FILE);
    let settings = std::fs::read_to_string(&settings_path)
        .map(|text| Settings::parse(&text))
        .unwrap_or_default();

    let mut window = Window::new("RAX", UI_WIDTH, UI_HEIGHT)?;
    let mut launcher = Launcher::new(settings, has_android(&root));
    loop {
        let mut pending = Vec::new();
        for event in window.poll() {
            match event {
                Event::Close => return Ok(()),
                Event::Repaint => launcher.invalidate(),
                Event::Pointer { x, y } => {
                    let (x, y) = to_ui(x, y, window.client_size());
                    launcher.pointer(x, y);
                }
                Event::Button {
                    button: Button::Left,
                    down: true,
                    x,
                    y,
                } => {
                    let (x, y) = to_ui(x, y, window.client_size());
                    pending.extend(launcher.click(x, y));
                }
                _ => {}
            }
        }
        for action in pending {
            match action {
                Action::Save(settings) => std::fs::write(&settings_path, settings.serialize())?,
                Action::Start(settings) => {
                    session(&mut window, &root, settings)?;
                    window.set_title("RAX");
                    launcher.invalidate();
                }
            }
        }
        if let Some(pixels) = launcher.draw() {
            window.present(pixels, UI_WIDTH, UI_HEIGHT);
        }
        std::thread::sleep(IDLE_DELAY);
    }
}

/// Turns window events into guest input while the mouse is captured.
#[derive(Default)]
struct InputState {
    buttons: u8,
}

impl InputState {
    fn button(&mut self, button: Button, down: bool) -> GuestInput {
        let bit = match button {
            Button::Left => 1,
            Button::Right => 2,
            Button::Middle => 4,
        };
        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        GuestInput::MouseButtons(self.buttons)
    }

    /// Lifts held buttons, so nothing stays stuck after losing the mouse.
    fn release(&mut self) -> Option<GuestInput> {
        (std::mem::take(&mut self.buttons) != 0).then_some(GuestInput::MouseButtons(0))
    }
}

/// Runs Android until the user closes the window.
fn session(window: &mut Window, root: &Path, settings: Settings) -> Result<(), Box<dyn Error>> {
    let (width, height) = SCREEN_CHOICES[settings.screen];
    let screen = Framebuffer { width, height };
    let display = Display::new(screen.size_bytes());
    let config = Config {
        kernel: std::fs::read(root.join(KERNEL))?,
        initrd: std::fs::read(root.join(INITRD))?,
        ram_mib: MEMORY_CHOICES_MIB[settings.memory],
        timeout: SESSION_LIMIT,
        shell: false,
        disk: Some(root.join(SYSTEM)),
        init_script: None,
        own_init_cmdline: Some(ANDROID_CMDLINE.to_owned()),
        framebuffer: Some(screen),
        screenshot: None,
        display: Some(display.clone()),
        input_script: Vec::new(),
    };

    window.set_title("RAX - click to capture the mouse, Right Ctrl releases it");
    let guest = std::thread::spawn(move || machine::run(config).map_err(|error| error.to_string()));
    let mut input = InputState::default();
    let (width, height) = (i32::from(width), i32::from(height));

    while !guest.is_finished() {
        let mut queued = Vec::new();
        for event in window.poll() {
            let captured = window.is_captured();
            match event {
                Event::Close => display.stop.store(true, Ordering::Relaxed),
                Event::Focus(false) => {
                    window.set_capture(false);
                    queued.extend(input.release());
                }
                Event::Button {
                    button: Button::Left,
                    down: true,
                    ..
                } if !captured => window.set_capture(true),
                Event::Button { button, down, .. } if captured => {
                    queued.push(input.button(button, down));
                }
                Event::RawMouse { dx, dy } if captured => {
                    queued.push(GuestInput::MouseMove { dx, dy });
                }
                Event::Key {
                    scancode: RELEASE_SCANCODE,
                    extended: true,
                    ..
                } => {
                    window.set_capture(false);
                    queued.extend(input.release());
                }
                Event::Key {
                    scancode,
                    extended,
                    down,
                } => queued.push(GuestInput::Key {
                    scancode,
                    extended,
                    down,
                }),
                _ => {}
            }
        }
        if !queued.is_empty()
            && let Ok(mut shared) = display.input.lock()
        {
            shared.append(&mut queued);
        }
        if let Ok(frame) = display.frame.lock() {
            window.present(&frame, width, height);
        }
        std::thread::sleep(FRAME_DELAY);
    }

    window.set_capture(false);
    guest.join().map_err(|_| "the guest thread failed")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_positions_scale_to_the_launcher() {
        assert_eq!(to_ui(960, 600, (1920, 1200)), (480, 300));
        assert_eq!(to_ui(10, 10, (0, 0)), (10 * UI_WIDTH, 10 * UI_HEIGHT));
    }

    #[test]
    fn buttons_accumulate_and_clear() {
        let mut input = InputState::default();
        input.button(Button::Left, true);
        assert_eq!(
            input.button(Button::Right, true),
            GuestInput::MouseButtons(3)
        );
        assert_eq!(
            input.button(Button::Left, false),
            GuestInput::MouseButtons(2)
        );
    }

    #[test]
    fn releasing_lifts_held_buttons_once() {
        let mut input = InputState::default();
        input.button(Button::Left, true);
        assert_eq!(input.release(), Some(GuestInput::MouseButtons(0)));
        assert_eq!(input.release(), None);
    }

    #[test]
    fn the_android_command_line_keeps_the_mouse_port() {
        assert!(!ANDROID_CMDLINE.contains("i8042.noaux"));
    }
}
