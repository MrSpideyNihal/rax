//! The desktop app: the launcher window and a running Android session.
//!
//! Everything lives in one folder. The app reads and writes only beside its
//! own executable, so deleting that folder removes RAX completely.

use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::apk::{self, InstallResult};
use rax_host::window::{self, Button, Event, Window};
use rax_input::{Input, Key, Mapper, MouseButton, Profile};
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
/// F12, which switches between a normal mouse and game controls.
const GAME_MODE_SCANCODE: u8 = 0x58;
const PROFILES: &str = "profiles";
const BUILT_IN_PROFILE: &str = include_str!("../../../assets/profiles/example-shooter.json");
const HINT: &str = "RAX - click to capture the mouse, Right Ctrl releases, F9 Home, F10 Back, F12 game controls, drop an APK to install";
/// F9 and F10, which stand in for a phone's Home and Back buttons.
const F9_SCANCODE: u8 = 0x43;
const F10_SCANCODE: u8 = 0x44;
/// The keyboard codes Linux reads as `KEY_HOMEPAGE` and `KEY_BACK`, which
/// Android treats as its Home and Back buttons.
const ANDROID_HOME: (u8, bool) = (0x32, true);
const ANDROID_BACK: (u8, bool) = (0x6a, true);

/// Turns the keys RAX reserves into the Android buttons they stand for.
fn android_key(scancode: u8, extended: bool) -> (u8, bool) {
    match (scancode, extended) {
        (F9_SCANCODE, false) => ANDROID_HOME,
        (F10_SCANCODE, false) => ANDROID_BACK,
        other => other,
    }
}

const ANDROID_CMDLINE: &str = "console=ttyS0 acpi=off reboot=k panic=-1 \
    tsc=reliable androidboot.hardware=android_x86_64 androidboot.console=ttyS0 \
    androidboot.selinux=permissive VIRT_WIFI=0 SRC= DEBUG=";
/// Keeps Android on the plain framebuffer.
const SOFTWARE_GRAPHICS: &str = "nomodeset HWACCEL=0";
/// The 8259 interrupt controller, for the short run that formats the disk.
const LEGACY_INTERRUPTS: &str = "nolapic noapic nosmp";
/// The hypervisor's local APIC and RAX's I/O APIC, which cost far fewer exits.
const APIC_INTERRUPTS: &str = "no_timer_check nr_cpus=4";
/// CPUs for a session. Must match `nr_cpus` above.
const SESSION_CPUS: u8 = 4;
/// The folder `scripts/fetch-gpu.ps1` fills.
const GPU_FOLDER: &str = "gpu";
const GPU_LIBRARY: &str = "libvirglrenderer-1.dll";
/// The refresh rate Android is told its screen has. It paces frames to this,
/// so at 60 the game stopped there with most of the machine idle.
const REFRESH_HZ: u32 = 240;
/// Typed into Android's console once it is up. Its 4.19 GPU driver copies each
/// command batch into one continuous block, up to 256 KiB. Once memory
/// fragments, that copy fails, the batch is lost, and a game waits forever for
/// a query inside it. So this keeps memory free and regularly compacted. The
/// marker file means typing it again does nothing, which is what lets RAX
/// repeat it until the shell has really started.
const MEMORY_TUNING: &[u8] = b"[ -e /dev/.rax ] || { : > /dev/.rax; \
    echo 196608 > /proc/sys/vm/min_free_kbytes; \
    (while true; do echo 1 > /proc/sys/vm/compact_memory; sleep 30; done) \
    > /dev/null 2>&1 & }\n";
/// How often that is typed, and how many times.
const TUNING_INTERVAL: Duration = Duration::from_secs(15);
const TUNING_TRIES: u32 = 6;
/// How often the frame rate in the title bar updates.
const TITLE_INTERVAL: Duration = Duration::from_secs(1);

fn android_cmdline(data_arg: &str, gpu: bool, apic: bool) -> String {
    let graphics = if gpu { "" } else { SOFTWARE_GRAPHICS };
    let interrupts = if apic {
        APIC_INTERRUPTS
    } else {
        LEGACY_INTERRUPTS
    };
    format!("{ANDROID_CMDLINE} {interrupts} {graphics} DATA={data_arg}")
}

/// Sets the screen's size and refresh rate on the kernel command line. The
/// virtio GPU offers no way to report a rate, and only the kernel's built-in
/// 60 Hz modes exist otherwise.
fn screen_mode(width: u16, height: u16) -> String {
    format!(" video=Virtual-1:{width}x{height}@{REFRESH_HZ}")
}

/// The renderer's folder, if this copy of RAX has one.
fn gpu_folder(root: &Path) -> Option<PathBuf> {
    let folder = root.join(GPU_FOLDER);
    folder.join(GPU_LIBRARY).is_file().then_some(folder)
}

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
        own_init_cmdline: Some(android_cmdline(data_arg, false, false)),
        framebuffer: None,
        gpu: None,
        balloon: false,
        apic: false,
        cpus: 1,
        screenshot: None,
        network: None,
        touchscreen: false,
        display: None,
        input_script: Vec::new(),
        touch_script: Vec::new(),
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

/// Loads the first key map in the profiles folder, or the built-in one.
fn load_profile(root: &Path) -> Result<Profile, Box<dyn Error>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(root.join(PROFILES))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let text = match files.first() {
        Some(path) => std::fs::read_to_string(path)?,
        None => BUILT_IN_PROFILE.to_owned(),
    };
    Ok(Profile::from_json(&text)?)
}

fn mouse_button(button: Button) -> MouseButton {
    match button {
        Button::Left => MouseButton::Left,
        Button::Right => MouseButton::Right,
        Button::Middle => MouseButton::Middle,
    }
}

/// Translates a window event for the key mapper.
fn to_mapper_input(event: &Event) -> Option<Input> {
    match *event {
        Event::Focus(false) => Some(Input::FocusLost),
        Event::Key {
            virtual_key, down, ..
        } => Some(if down {
            Input::KeyDown(Key(virtual_key))
        } else {
            Input::KeyUp(Key(virtual_key))
        }),
        Event::Button { button, down, .. } => {
            let button = mouse_button(button);
            Some(if down {
                Input::ButtonDown(button)
            } else {
                Input::ButtonUp(button)
            })
        }
        Event::RawMouse { dx, dy } => Some(Input::MouseMove {
            dx: dx as f32,
            dy: dy as f32,
        }),
        _ => None,
    }
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
    /// Keys Android believes are down, as `(scancode, extended)`.
    keys: Vec<(u8, bool)>,
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

    fn key(&mut self, scancode: u8, extended: bool, down: bool) -> GuestInput {
        let key = (scancode, extended);
        self.keys.retain(|&held| held != key);
        if down {
            self.keys.push(key);
        }
        GuestInput::Key {
            scancode,
            extended,
            down,
        }
    }

    /// Lifts everything held. Windows sends no key-up to a window that lost
    /// focus, so without this the Alt of an Alt+Tab stays down in Android,
    /// and every letter typed after coming back becomes an Alt character.
    fn release(&mut self) -> Vec<GuestInput> {
        let keys = std::mem::take(&mut self.keys);
        let mut lifted: Vec<GuestInput> = keys
            .into_iter()
            .map(|(scancode, extended)| GuestInput::Key {
                scancode,
                extended,
                down: false,
            })
            .collect();
        if std::mem::take(&mut self.buttons) != 0 {
            lifted.push(GuestInput::MouseButtons(0));
        }
        lifted
    }
}

/// A window into Android's console for diagnosing a live session. Set
/// `RAX_CONSOLE_DIR`, and RAX appends the console to `console.log` there and
/// types whatever appears in `console.in`, then deletes that file.
struct ConsoleTap {
    log: std::fs::File,
    commands: PathBuf,
    seen: usize,
}

impl ConsoleTap {
    fn from_environment() -> Option<Self> {
        let folder = PathBuf::from(std::env::var_os("RAX_CONSOLE_DIR")?);
        std::fs::create_dir_all(&folder).ok()?;
        let log = std::fs::File::create(folder.join("console.log")).ok()?;
        Some(Self {
            log,
            commands: folder.join("console.in"),
            seen: 0,
        })
    }

    fn pump(&mut self, display: &Display) {
        if let Ok(text) = display.console_output.lock() {
            // The buffer drops old text when full, which restarts the count.
            if text.len() < self.seen {
                self.seen = 0;
            }
            let _ = self.log.write_all(&text.as_bytes()[self.seen..]);
            self.seen = text.len();
        }
        if let Ok(commands) = std::fs::read(&self.commands) {
            let _ = std::fs::remove_file(&self.commands);
            if let Ok(mut typed) = display.console_input.lock() {
                typed.extend_from_slice(&commands);
            }
        }
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
    config.gpu = gpu_folder(root);
    config.apic = true;
    config.cpus = SESSION_CPUS;
    let mut cmdline = android_cmdline("vdb", config.gpu.is_some(), true);
    if config.gpu.is_some() {
        cmdline.push_str(&screen_mode(width, height));
    }
    config.own_init_cmdline = Some(cmdline);
    config.network = Some(Network::start());
    config.touchscreen = true;
    config.display = Some(display.clone());

    let mut title = HINT.to_owned();
    let guest = std::thread::spawn(move || machine::run(config).map_err(|error| error.to_string()));
    let mut last_title = Instant::now();
    let mut last_frames = 0;
    let mut tap = ConsoleTap::from_environment();
    let session_started = Instant::now();
    let mut tuning_sent = 0;
    let mut input = InputState::default();
    let mut install: Option<Install> = None;
    let profile = load_profile(root)?;
    let profile_name = profile.name.clone();
    let mut mapper = Mapper::new(profile);
    let mut game_mode = false;
    let (width, height) = (i32::from(width), i32::from(height));

    while !guest.is_finished() {
        let mut queued = Vec::new();
        let mut touches = Vec::new();
        for event in window.poll() {
            let captured = window.is_captured();
            if let Event::Key {
                scancode: GAME_MODE_SCANCODE,
                down: true,
                ..
            } = event
            {
                game_mode = !game_mode;
                if game_mode {
                    queued.extend(input.release());
                    window.set_capture(true);
                    title.clone_from(&format!(
                        "RAX - game controls: {profile_name}. F12 to leave"
                    ));
                } else {
                    touches.extend(mapper.handle(Input::FocusLost));
                    title = HINT.to_owned();
                }
                continue;
            }
            if game_mode {
                match event {
                    Event::Close => display.stop.store(true, Ordering::Relaxed),
                    Event::Focus(false) => window.set_capture(false),
                    Event::Focus(true) => window.set_capture(true),
                    _ => {}
                }
                let mapped = to_mapper_input(&event);
                if let Some(mapped) = mapped {
                    touches.extend(mapper.handle(mapped));
                }
                continue;
            }
            match event {
                Event::Close => display.stop.store(true, Ordering::Relaxed),
                Event::Dropped(path) if install.is_none() => {
                    match begin_install(&path, &inbox, &display) {
                        Ok(started) => {
                            title.clone_from(&format!("RAX - installing {}...", started.name));
                            install = Some(started);
                        }
                        Err(error) => title.clone_from(&format!("RAX - {error}")),
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
                    ..
                } => {
                    let (scancode, extended) = android_key(scancode, extended);
                    queued.push(input.key(scancode, extended, down));
                }
                _ => {}
            }
        }
        if !queued.is_empty()
            && let Ok(mut shared) = display.input.lock()
        {
            shared.append(&mut queued);
        }
        if !touches.is_empty()
            && let Ok(mut shared) = display.touches.lock()
        {
            shared.append(&mut touches);
        }
        if let Ok(frame) = display.frame.lock() {
            window.present(&frame, width, height);
        }
        if last_title.elapsed() >= TITLE_INTERVAL {
            let frames = display.frames.load(Ordering::Relaxed);
            let rate =
                frames.saturating_sub(last_frames) as f64 / last_title.elapsed().as_secs_f64();
            window.set_title(&format!("{rate:.0} FPS - {title}"));
            (last_frames, last_title) = (frames, Instant::now());
        }
        if let Some(tap) = tap.as_mut() {
            tap.pump(&display);
        }
        if tuning_sent < TUNING_TRIES
            && session_started.elapsed() >= TUNING_INTERVAL * (tuning_sent + 1)
            && let Ok(mut typed) = display.console_input.lock()
        {
            typed.extend_from_slice(MEMORY_TUNING);
            tuning_sent += 1;
        }
        let finished = install.as_ref().and_then(|_| {
            let text = display.console_output.lock().ok()?;
            apk::parse_result(&text)
        });
        if let (Some(result), Some(done)) = (finished, &install) {
            title.clone_from(&match result {
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
        assert_eq!(input.release(), [GuestInput::MouseButtons(0)]);
        assert_eq!(input.release(), []);
    }

    /// A press on a key bound at (0.5, 0.5), through the path the window uses.
    fn mapped_touches(event: &Event) -> Vec<rax_input::Touch> {
        let profile = Profile::from_json(
            r#"{"name":"t","pointer_toggle":"Alt","bindings":[
              {"type":"key","key":"R","at":{"x":0.5,"y":0.5}},
              {"type":"button","button":"Left","at":{"x":0.9,"y":0.9}},
              {"type":"look","origin":{"x":0.6,"y":0.4},"sensitivity":0.01,"range":0.2}
            ]}"#,
        )
        .unwrap();
        let mut mapper = Mapper::new(profile);
        to_mapper_input(event)
            .map(|input| mapper.handle(input))
            .unwrap_or_default()
    }

    fn key_event(virtual_key: u16, down: bool) -> Event {
        Event::Key {
            scancode: 0x13,
            extended: false,
            down,
            virtual_key,
        }
    }

    #[test]
    fn a_bound_key_from_the_window_becomes_a_finger() {
        let touches = mapped_touches(&key_event(0x52, true));
        assert_eq!(touches.len(), 1);
        assert_eq!(touches[0].phase, rax_input::Phase::Down);
        assert_eq!(touches[0].at, rax_input::Point { x: 0.5, y: 0.5 });
    }

    #[test]
    fn an_unbound_key_does_nothing() {
        assert!(mapped_touches(&key_event(0x51, true)).is_empty());
    }

    #[test]
    fn a_mouse_button_from_the_window_becomes_a_finger() {
        let touches = mapped_touches(&Event::Button {
            button: Button::Left,
            down: true,
            x: 0,
            y: 0,
        });
        assert_eq!(touches[0].at, rax_input::Point { x: 0.9, y: 0.9 });
    }

    #[test]
    fn raw_mouse_motion_aims() {
        let touches = mapped_touches(&Event::RawMouse { dx: 5, dy: 0 });
        assert_eq!(touches.len(), 2, "the finger lands, then drags");
    }

    #[test]
    fn losing_focus_reaches_the_mapper() {
        assert_eq!(
            to_mapper_input(&Event::Focus(false)),
            Some(Input::FocusLost)
        );
        assert_eq!(to_mapper_input(&Event::Repaint), None);
    }

    #[test]
    fn the_built_in_profile_is_valid() {
        assert!(Profile::from_json(BUILT_IN_PROFILE).is_ok());
    }

    #[test]
    fn a_missing_profiles_folder_uses_the_built_in_map() {
        let root = Path::new("this-folder-does-not-exist");
        assert!(load_profile(root).is_ok());
    }

    #[test]
    fn android_uses_plain_ethernet() {
        // Android connects wired networks by itself. The Android-x86 default
        // wraps the card as a Wi-Fi network that someone would have to join.
        assert!(ANDROID_CMDLINE.contains("VIRT_WIFI=0"));
    }

    const ALT: u8 = 0x38;

    fn key_up(scancode: u8, extended: bool) -> GuestInput {
        GuestInput::Key {
            scancode,
            extended,
            down: false,
        }
    }

    #[test]
    fn alt_held_while_focus_is_lost_is_lifted() {
        let mut input = InputState::default();
        input.key(ALT, false, true);
        assert_eq!(input.release(), [key_up(ALT, false)]);
        assert_eq!(input.release(), [], "lifted only once");
    }

    #[test]
    fn a_key_already_released_is_not_lifted_again() {
        let mut input = InputState::default();
        input.key(ALT, false, true);
        input.key(ALT, false, false);
        assert_eq!(input.release(), []);
    }

    #[test]
    fn left_and_right_alt_are_different_keys() {
        let mut input = InputState::default();
        input.key(ALT, false, true);
        input.key(ALT, true, true);
        input.key(ALT, false, false);
        assert_eq!(input.release(), [key_up(ALT, true)]);
    }

    #[test]
    fn auto_repeat_cannot_grow_the_held_list() {
        let mut input = InputState::default();
        input.key(ALT, false, true);
        input.key(ALT, false, true);
        assert_eq!(input.release().len(), 1);
    }

    #[test]
    fn keys_and_buttons_are_lifted_together() {
        let mut input = InputState::default();
        input.key(ALT, false, true);
        input.button(Button::Left, true);
        assert_eq!(
            input.release(),
            [key_up(ALT, false), GuestInput::MouseButtons(0)]
        );
    }

    #[test]
    fn f9_and_f10_become_home_and_back() {
        assert_eq!(android_key(F9_SCANCODE, false), ANDROID_HOME);
        assert_eq!(android_key(F10_SCANCODE, false), ANDROID_BACK);
    }

    #[test]
    fn other_keys_pass_through() {
        assert_eq!(android_key(0x1e, false), (0x1e, false));
        assert_eq!(android_key(F9_SCANCODE, true), (F9_SCANCODE, true));
    }

    #[test]
    fn only_the_gpu_needs_kernel_mode_setting() {
        assert!(android_cmdline("vdb", false, true).contains("nomodeset HWACCEL=0"));
        let gpu = android_cmdline("vdb", true, true);
        assert!(!gpu.contains("nomodeset") && !gpu.contains("HWACCEL"));
    }

    #[test]
    fn the_screen_mode_names_size_and_rate() {
        assert_eq!(screen_mode(1280, 720), " video=Virtual-1:1280x720@240");
    }

    #[test]
    fn memory_tuning_is_one_line_and_safe_to_repeat() {
        let text = std::str::from_utf8(MEMORY_TUNING).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
        assert!(text.ends_with('\n'));
        assert!(text.starts_with("[ -e /dev/.rax ] ||"));
    }

    #[test]
    fn the_command_line_and_the_machine_agree_on_cpus() {
        assert!(APIC_INTERRUPTS.contains(&format!("nr_cpus={SESSION_CPUS}")));
    }

    #[test]
    fn the_apic_path_must_not_be_disabled_on_the_command_line() {
        // Regression test: `nosmp` silently turned the I/O APIC off.
        let line = android_cmdline("vdb", true, true);
        assert!(!line.contains("nosmp") && !line.contains("noapic") && !line.contains("nolapic"));
        assert!(android_cmdline("vdb", false, false).contains(LEGACY_INTERRUPTS));
    }

    #[test]
    fn a_folder_without_the_renderer_stays_on_software() {
        assert_eq!(gpu_folder(Path::new("no-such-folder")), None);
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
