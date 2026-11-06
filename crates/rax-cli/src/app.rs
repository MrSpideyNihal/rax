//! The desktop app: the launcher window and a running Android session.
//!
//! Everything lives in one folder. The app reads and writes only beside its
//! own executable, so deleting that folder removes RAX completely.

use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::apk::{self, InstallResult};
use rax_host::window::{self, Button, Event, Window};
use rax_net::host::Network;
use rax_ui::{Action, Launcher, MEMORY_CHOICES_MIB, SCREEN_CHOICES, Settings, UI_HEIGHT, UI_WIDTH};
use rax_vmm::framebuffer::Framebuffer;
use rax_vmm::machine::{self, Config, Display};
use rax_vmm::ps2::GuestInput;

const SETTINGS_FILE: &str = "rax.cfg";
const KERNEL: &str = "guest/kernel";
const INITRD: &str = "guest/initrd.img";
const SYSTEM: &str = "guest/android-x86_64-9.0-r2.iso";
/// Android's own data, which is what keeps apps between runs.
const DATA: &str = "data.img";
const DATA_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Spare disk that carries an APK into Android during a session.
const INBOX: &str = "inbox.tmp";
/// A blank data disk is formatted by Android itself, at its console.
const FORMAT_COMMAND: &str = "mke2fs -t ext4 -F /dev/block/vdb && echo RAX_FORMAT_OK; reboot -p\n";
const FORMAT_OK: &str = "RAX_FORMAT_OK";
const CONSOLE_PROMPT: &str = "console:/ #";
const FORMAT_TIMEOUT: Duration = Duration::from_secs(120);
const SETUP_RAM_MIB: usize = 1024;
/// A session has no time limit that a person would ever reach.
const SESSION_LIMIT: Duration = Duration::from_secs(60 * 60 * 24 * 365);
const FRAME_DELAY: Duration = Duration::from_millis(16);
const IDLE_DELAY: Duration = Duration::from_millis(30);
/// Right Ctrl, which releases the mouse.
const RELEASE_SCANCODE: u8 = 0x1d;
const HINT: &str = "RAX - click to capture the mouse, Right Ctrl releases, drop an APK to install";

const ANDROID_CMDLINE: &str = "console=ttyS0 nolapic noapic acpi=off nosmp reboot=k panic=-1 \
    tsc=reliable androidboot.hardware=android_x86_64 androidboot.console=ttyS0 \
    androidboot.selinux=permissive nomodeset HWACCEL=0 VIRT_WIFI=0 SRC= DEBUG=";

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
    // A crash can leave these behind, and neither holds anything of value.
    let _ = std::fs::remove_file(root.join(INBOX));
    let _ = std::fs::remove_file(root.join("data.tmp"));
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
                    if !root.join(DATA).is_file() {
                        launcher.set_status(Some("Preparing, first run only...".to_owned()));
                        if let Some(pixels) = launcher.draw() {
                            window.present(pixels, UI_WIDTH, UI_HEIGHT);
                        }
                    }
                    let prepared = if root.join(DATA).is_file() {
                        Ok(())
                    } else {
                        prepare_data_disk(&mut window, &root)
                    };
                    let outcome = prepared.and_then(|()| session(&mut window, &root, settings));
                    window.set_title("RAX");
                    launcher.set_status(outcome.err().map(|error| error.to_string()));
                }
            }
        }
        if let Some(pixels) = launcher.draw() {
            window.present(pixels, UI_WIDTH, UI_HEIGHT);
        }
        std::thread::sleep(IDLE_DELAY);
    }
}

fn android_config(
    root: &Path,
    data_arg: &str,
    disks: Vec<PathBuf>,
) -> Result<Config, Box<dyn Error>> {
    Ok(Config {
        kernel: std::fs::read(root.join(KERNEL))?,
        initrd: std::fs::read(root.join(INITRD))?,
        ram_mib: SETUP_RAM_MIB,
        timeout: SESSION_LIMIT,
        shell: false,
        disks,
        init_script: None,
        own_init_cmdline: Some(format!("{ANDROID_CMDLINE} DATA={data_arg}")),
        framebuffer: None,
        screenshot: None,
        network: None,
        display: None,
        input_script: Vec::new(),
    })
}

/// Creates and formats the data disk. Android does this at its own console,
/// so RAX needs no filesystem code. A new file only gets its final name once
/// the format worked, so an interrupted first run simply starts again.
fn prepare_data_disk(window: &mut Window, root: &Path) -> Result<(), Box<dyn Error>> {
    let partial = root.join("data.tmp");
    let file = std::fs::File::create(&partial)?;
    file.set_len(DATA_BYTES)?;
    drop(file);

    let display = Display::new(0);
    let mut config = android_config(root, "", vec![root.join(SYSTEM), partial.clone()])?;
    config.timeout = FORMAT_TIMEOUT;
    config.display = Some(display.clone());
    let guest = std::thread::spawn(move || machine::run(config).map_err(|error| error.to_string()));

    let mut sent = false;
    while !guest.is_finished() {
        // The window must keep answering Windows during the wait.
        for event in window.poll() {
            if event == Event::Close {
                display.stop.store(true, Ordering::Relaxed);
            }
        }
        let prompt = display
            .console_output
            .lock()
            .is_ok_and(|text| text.contains(CONSOLE_PROMPT));
        if prompt && !sent {
            sent = true;
            // Disks can appear a moment after the prompt does.
            std::thread::sleep(Duration::from_secs(2));
            if let Ok(mut input) = display.console_input.lock() {
                input.extend_from_slice(FORMAT_COMMAND.as_bytes());
            }
        }
        std::thread::sleep(IDLE_DELAY);
    }
    guest.join().map_err(|_| "the guest thread failed")??;
    let formatted = display
        .console_output
        .lock()
        .is_ok_and(|text| text.lines().any(|line| line.trim() == FORMAT_OK));
    if !formatted {
        let _ = std::fs::remove_file(&partial);
        return Err("could not prepare the data disk".into());
    }
    std::fs::rename(partial, root.join(DATA))?;
    Ok(())
}

/// One APK on its way into Android.
struct Install {
    name: String,
}

/// Starts installing `path`, writing it to the inbox disk.
fn begin_install(path: &Path, inbox: &Path, display: &Display) -> Result<Install, Box<dyn Error>> {
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    if !apk::is_apk(&name) {
        return Err("Only .apk files can be installed".into());
    }
    let bytes = std::fs::read(path)?;
    if bytes.len() as u64 > apk::INBOX_BYTES {
        return Err("That APK is larger than 1 GB".into());
    }
    let mut file = std::fs::OpenOptions::new().write(true).open(inbox)?;
    file.write_all(&bytes)?;
    file.sync_data()?;
    display
        .console_output
        .lock()
        .map_err(|_| "console lock failed")?
        .clear();
    display
        .console_input
        .lock()
        .map_err(|_| "console lock failed")?
        .extend_from_slice(apk::install_command(bytes.len() as u64).as_bytes());
    Ok(Install { name })
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
    let inbox = root.join(INBOX);
    std::fs::File::create(&inbox)?.set_len(apk::INBOX_BYTES)?;
    let disks = vec![root.join(SYSTEM), root.join(DATA), inbox.clone()];
    let mut config = android_config(root, "vdb", disks)?;
    config.ram_mib = MEMORY_CHOICES_MIB[settings.memory];
    config.framebuffer = Some(screen);
    config.network = Some(Network::start());
    config.display = Some(display.clone());

    window.set_title(HINT);
    let guest = std::thread::spawn(move || machine::run(config).map_err(|error| error.to_string()));
    let mut input = InputState::default();
    let mut install: Option<Install> = None;
    let (width, height) = (i32::from(width), i32::from(height));

    while !guest.is_finished() {
        let mut queued = Vec::new();
        for event in window.poll() {
            let captured = window.is_captured();
            match event {
                Event::Close => display.stop.store(true, Ordering::Relaxed),
                Event::Dropped(path) if install.is_none() => {
                    match begin_install(&path, &inbox, &display) {
                        Ok(started) => {
                            window.set_title(&format!("RAX - installing {}...", started.name));
                            install = Some(started);
                        }
                        Err(error) => window.set_title(&format!("RAX - {error}")),
                    }
                }
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
        let finished = install.as_ref().and_then(|_| {
            let text = display.console_output.lock().ok()?;
            apk::parse_result(&text)
        });
        if let (Some(result), Some(done)) = (finished, &install) {
            window.set_title(&match result {
                InstallResult::Installed => format!("RAX - installed {}", done.name),
                InstallResult::Failed(reason) => {
                    format!("RAX - could not install {}: {reason}", done.name)
                }
            });
            install = None;
        }
        std::thread::sleep(FRAME_DELAY);
    }

    window.set_capture(false);
    let _ = std::fs::remove_file(&inbox);
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
    fn android_uses_plain_ethernet() {
        // Android connects wired networks by itself. The Android-x86 default
        // wraps the card as a Wi-Fi network that someone would have to join.
        assert!(ANDROID_CMDLINE.contains("VIRT_WIFI=0"));
    }

    #[test]
    fn the_data_argument_comes_last_and_is_not_preset() {
        assert!(!ANDROID_CMDLINE.contains("DATA="));
    }

    #[test]
    fn the_android_command_line_keeps_the_mouse_port() {
        assert!(!ANDROID_CMDLINE.contains("i8042.noaux"));
    }
}
