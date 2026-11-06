//! Assembles a guest and runs its vCPU.

use std::error::Error;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rax_host::vm::{self, Kicker, Vm};
use rax_host::whp::{Api, ExitInfo, VmExit};
use rax_input::Touch;
use rax_net::host::Network;
use rax_net::nat::GUEST_MAC;
use serde::Serialize;

use crate::boot::{self, BOOT_PARAMS_GPA, BzImage, CMDLINE_GPA, GDT_GPA};
use crate::bus::{Bus, IoStats};
use crate::cpio;
use crate::framebuffer::{FRAMEBUFFER_GPA, Framebuffer};
use crate::mmio;
use crate::ps2::GuestInput;
use crate::serial::Sink;
use crate::virtio_blk::{GuestMemory, OutOfRange, VirtioBlk};
use crate::virtio_input::VirtioInput;
use crate::virtio_net::VirtioNet;

/// Printed by the guest once user space is running.
const READY_MARKER: &[u8] = b"RAX_GUEST_READY";
const INIT_PATH: &str = "rax-init";
const BASE_CMDLINE: &str = "console=ttyS0 earlyprintk=serial,ttyS0,115200 nolapic noapic \
    acpi=off nosmp reboot=k panic=-1 i8042.noaux tsc=reliable rdinit=/rax-init";
/// Skips the bus scan when there is nothing on it.
const NO_PCI: &str = " pci=off";
/// How long the timer thread sleeps when nothing is due sooner.
const TIMER_IDLE: Duration = Duration::from_millis(1);
/// How often the screen is copied for a live window.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// How often a sleeping guest checks for a request to stop.
const HALT_POLL: Duration = Duration::from_millis(50);
/// Console text kept for a running app, in bytes.
const CONSOLE_KEEP: usize = 16 * 1024;

const SMOKE_INIT: &str = "#!/bin/busybox sh
/bin/busybox mount -t proc proc /proc
echo RAX_GUEST_READY
/bin/busybox uname -a
/bin/busybox grep MemTotal /proc/meminfo
/bin/busybox poweroff -f
";

const SHELL_INIT: &str = "#!/bin/busybox sh
/bin/busybox mkdir -p /usr/bin /usr/sbin /sbin
/bin/busybox --install -s
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
modprobe virtio_pci
modprobe virtio_blk
echo RAX_GUEST_READY
setsid -c sh
poweroff -f
";

/// What to boot.
#[derive(Debug, Clone)]
pub struct Config {
    /// A `bzImage`.
    pub kernel: Vec<u8>,
    /// An initramfs. RAX adds its init script to it.
    pub initrd: Vec<u8>,
    /// Guest RAM in MiB.
    pub ram_mib: usize,
    /// Stop the guest after this long.
    pub timeout: Duration,
    /// Give the host's stdin to a guest shell, instead of a smoke test.
    pub shell: bool,
    /// Raw disk images, shown as `/dev/vda`, `/dev/vdb` and so on. Writes
    /// are kept.
    pub disks: Vec<PathBuf>,
    /// Replaces the built-in init script.
    pub init_script: Option<String>,
    /// Boots the guest's own init with this command line, leaving its
    /// initramfs untouched. Used for full operating systems such as Android.
    pub own_init_cmdline: Option<String>,
    /// Gives the guest a screen of this size.
    pub framebuffer: Option<Framebuffer>,
    /// Saves the screen to this BMP file when the guest stops.
    pub screenshot: Option<PathBuf>,
    /// Gives the guest a network card, with internet through this network.
    pub network: Option<Network>,
    /// Gives the guest a multi-touch screen.
    pub touchscreen: bool,
    /// Connects a live window. `None` runs headless.
    pub display: Option<Display>,
    /// Timed input, for repeatable tests with no person at the keyboard.
    pub input_script: Vec<(Duration, GuestInput)>,
    /// Timed touches, for repeatable tests.
    pub touch_script: Vec<(Duration, Touch)>,
}

/// The link between the VM thread and a window on another thread.
#[derive(Debug, Clone)]
pub struct Display {
    /// The latest frame, copied while the vCPU is stopped.
    pub frame: Arc<Mutex<Vec<u8>>>,
    /// Input from the window.
    pub input: Arc<Mutex<Vec<GuestInput>>>,
    /// Set to shut the guest down.
    pub stop: Arc<AtomicBool>,
    /// Finger changes from the key mapper.
    pub touches: Arc<Mutex<Vec<Touch>>>,
    /// Text to type into the guest's console.
    pub console_input: Arc<Mutex<Vec<u8>>>,
    /// What the console has printed, newest last.
    pub console_output: Arc<Mutex<String>>,
}

impl Display {
    /// Creates a link for a screen of `size_bytes`.
    #[must_use]
    pub fn new(size_bytes: usize) -> Self {
        Self {
            frame: Arc::new(Mutex::new(vec![0; size_bytes])),
            input: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(false)),
            touches: Arc::new(Mutex::new(Vec::new())),
            console_input: Arc::new(Mutex::new(Vec::new())),
            console_output: Arc::new(Mutex::new(String::new())),
        }
    }
}

/// How the guest stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The guest halted with interrupts off, which is a power off.
    PoweredOff,
    /// The guest reset through the keyboard controller.
    Reset,
    /// The configured time ran out.
    TimedOut,
    /// The host asked the guest to stop.
    Stopped,
}

/// What happened during a run.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// How the guest stopped.
    pub outcome: Outcome,
    /// Guest RAM in MiB.
    pub ram_mib: usize,
    /// From the first vCPU instruction to the ready marker, if it was seen.
    pub boot_to_user_space_ms: Option<f64>,
    /// Whole run.
    pub total_ms: f64,
    /// Number of times the vCPU exited to the monitor.
    pub vcpu_exits: u64,
    /// Interrupts injected.
    pub interrupts: u64,
    /// Pages of absent hardware the guest probed.
    pub floating_pages: u64,
    /// Exits taken because a blocked guest became able to accept an interrupt.
    pub interrupt_window_exits: u64,
    /// Device activity.
    pub io: IoStats,
    /// Busiest ports with no device, as `(port, accesses)`.
    pub unclaimed_ports: Vec<(u16, u64)>,
}

struct Finished {
    outcome: Outcome,
    exits: u64,
    interrupts: u64,
    floating_pages: u64,
    window_exits: u64,
}

/// Writes the console to stdout and notes when the marker appears.
struct Console {
    out: std::io::Stdout,
    /// Shell prompts end without a newline, so they must not wait for one.
    flush_every_byte: bool,
    matched: usize,
    ready: Arc<Mutex<Option<Instant>>>,
    /// Also keeps recent output here, for a running app to read.
    capture: Option<Arc<Mutex<String>>>,
}

impl Sink for Console {
    fn put(&mut self, byte: u8) {
        if let Some(capture) = &self.capture
            && let Ok(mut text) = capture.lock()
        {
            text.push(char::from(byte));
            if text.len() > CONSOLE_KEEP * 2 {
                let cut = text.len() - CONSOLE_KEEP;
                text.drain(..cut);
            }
        }
        // A closed stdout must not stop the guest.
        let _ = self.out.write_all(&[byte]);
        if self.flush_every_byte || byte == b'\n' {
            let _ = self.out.flush();
        }
        self.matched = next_match(self.matched, byte);
        if self.matched == READY_MARKER.len() {
            self.matched = 0;
            if let Ok(mut ready) = self.ready.lock() {
                ready.get_or_insert_with(Instant::now);
            }
        }
    }
}

/// Advances a search for [`READY_MARKER`]. Its first byte does not repeat, so
/// a mismatch only ever needs to restart.
fn next_match(matched: usize, byte: u8) -> usize {
    if READY_MARKER[matched] == byte {
        matched + 1
    } else {
        usize::from(READY_MARKER[0] == byte)
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

struct Shared {
    bus: Mutex<Bus>,
    wake: Condvar,
    stop: AtomicBool,
    started: Instant,
    display: Option<Display>,
    input_script: Vec<(Duration, GuestInput)>,
    touch_script: Vec<(Duration, Touch)>,
    /// Set when touches are waiting to reach guest memory.
    touch_ready: AtomicBool,
    /// Set when the network has frames waiting for the guest.
    network_ready: Option<Arc<AtomicBool>>,
}

impl Shared {
    fn network_pending(&self) -> bool {
        self.touch_ready.load(Ordering::Relaxed)
            || self
                .network_ready
                .as_ref()
                .is_some_and(|ready| ready.load(Ordering::Relaxed))
    }
}

impl Shared {
    fn now_ns(&self) -> u64 {
        self.started.elapsed().as_nanos() as u64
    }
}

fn timer_loop(shared: &Shared, kicker: &Kicker<'_>, input: Option<&Receiver<Vec<u8>>>) {
    let mut script_position = 0;
    let mut touch_position = 0;
    while !shared.stop.load(Ordering::Relaxed) {
        let now = shared.now_ns();
        let window_input: Vec<GuestInput> = shared
            .display
            .as_ref()
            .and_then(|display| display.input.lock().ok())
            .map_or_else(Vec::new, |mut queue| std::mem::take(&mut *queue));
        let Ok(mut bus) = shared.bus.lock() else {
            return;
        };
        for event in window_input {
            bus.guest_input(event);
        }
        let mut touched = false;
        if let Some(display) = &shared.display
            && let Ok(mut touches) = display.touches.lock()
        {
            touched = !touches.is_empty();
            for touch in touches.drain(..) {
                bus.touch_input(touch);
            }
        }
        while let Some((at, touch)) = shared.touch_script.get(touch_position) {
            if u128::from(now) < at.as_nanos() {
                break;
            }
            bus.touch_input(*touch);
            touch_position += 1;
            touched = true;
        }
        if touched {
            shared.touch_ready.store(true, Ordering::Relaxed);
        }
        if let Some(display) = &shared.display
            && let Ok(mut typed) = display.console_input.lock()
            && !typed.is_empty()
        {
            bus.console_input(&typed);
            typed.clear();
        }
        while let Some((at, event)) = shared.input_script.get(script_position) {
            if u128::from(now) < at.as_nanos() {
                break;
            }
            bus.guest_input(*event);
            script_position += 1;
        }
        while let Some(bytes) = input.and_then(|input| input.try_recv().ok()) {
            bus.console_input(&bytes);
        }
        let pending = bus.tick(now);
        let next = bus.next_timer_ns();
        drop(bus);
        if pending || shared.network_pending() {
            shared.wake.notify_all();
            kicker.kick();
        }
        let until_next = next.map_or(TIMER_IDLE, |at| {
            Duration::from_nanos(at.saturating_sub(shared.now_ns())).min(TIMER_IDLE)
        });
        std::thread::sleep(until_next);
    }
}

/// Reads the host's stdin on a detached thread. It cannot be joined, because
/// a read of stdin has no timeout, so it talks to the machine over a channel.
fn spawn_stdin_reader() -> Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buffer = [0_u8; 256];
        while let Ok(count @ 1..) = stdin.read(&mut buffer) {
            // Windows consoles send CRLF, and the guest wants LF.
            let bytes = buffer[..count]
                .iter()
                .copied()
                .filter(|&byte| byte != b'\r')
                .collect();
            if sender.send(bytes).is_err() {
                return;
            }
        }
    });
    receiver
}

/// Waits for an interrupt while the guest sleeps. The wait is short and
/// repeated, so a request to stop is seen even if no interrupt ever comes.
fn wait_while_halted(shared: &Shared, deadline: Instant) -> Result<(), &'static str> {
    let mut bus = shared.bus.lock().map_err(|_| "device lock poisoned")?;
    loop {
        let stopping = shared
            .display
            .as_ref()
            .is_some_and(|display| display.stop.load(Ordering::Relaxed));
        let now = Instant::now();
        let wake = bus.pending_vector().is_some() || bus.reset_requested();
        if stopping || now >= deadline || wake || shared.network_pending() {
            return Ok(());
        }
        let wait = deadline.saturating_duration_since(now).min(HALT_POLL);
        bus = shared
            .wake
            .wait_timeout(bus, wait)
            .map_err(|_| "device lock poisoned")?
            .0;
    }
}

/// Guest RAM for devices. Only used while the vCPU is stopped at an exit.
struct RamView<'a, 'b>(&'a Vm<'b>);

impl GuestMemory for RamView<'_, '_> {
    fn read(&self, gpa: u64, buffer: &mut [u8]) -> Result<(), OutOfRange> {
        self.0.read_ram(gpa, buffer).map_err(|_| OutOfRange)
    }

    fn write(&self, gpa: u64, bytes: &[u8]) -> Result<(), OutOfRange> {
        self.0.write_ram(gpa, bytes).map_err(|_| OutOfRange)
    }
}

/// Performs a guest access to device memory.
fn device_memory(
    vm: &Vm<'_>,
    shared: &Shared,
    info: &ExitInfo,
    gpa: u64,
    bytes: &[u8],
) -> Result<(), Box<dyn Error>> {
    let access = mmio::decode(bytes)
        .ok_or_else(|| format!("cannot decode device access {bytes:02x?} at {gpa:#x}"))?;
    let mut bus = shared.bus.lock().map_err(|_| "device lock poisoned")?;
    if access.write {
        let value = match access.immediate {
            Some(value) => value,
            None => {
                let register = vm.general_register(access.register)? as u32;
                match access.size {
                    1 => register & 0xff,
                    2 => register & 0xffff,
                    _ => register,
                }
            }
        };
        bus.memory_write(gpa, value);
        bus.service_devices(&RamView(vm));
        vm.advance(info, access.length)?;
    } else {
        let value = bus.memory_read(gpa, access.size);
        vm.finish_load(
            info,
            access.register,
            access.size,
            value,
            access.zero_extend,
            access.length,
        )?;
    }
    Ok(())
}

/// What to do about interrupts before the vCPU next runs.
#[derive(Debug, PartialEq, Eq)]
enum Delivery {
    Nothing,
    Inject(u8),
    AskForWindow,
}

/// Decides how to deliver a pending interrupt.
///
/// The window request is repeated on every pass and deliberately not
/// remembered: the hypervisor's state is the only true record. If a window
/// never arrives, each interrupt waits for the next timer kick, which loses
/// serial output at power off and would add input latency.
fn delivery(pending: Option<u8>, interruptible: bool) -> Delivery {
    match pending {
        None => Delivery::Nothing,
        Some(vector) if interruptible => Delivery::Inject(vector),
        Some(_) => Delivery::AskForWindow,
    }
}

fn vcpu_loop(vm: &Vm<'_>, shared: &Shared, deadline: Instant) -> Result<Finished, Box<dyn Error>> {
    let mut exits = 0_u64;
    let mut interrupts = 0_u64;
    let mut floating_pages = 0_u64;
    let mut window_exits = 0_u64;
    let mut last_frame = Instant::now();
    let mut interruptible = false;
    loop {
        if let Some(display) = &shared.display {
            if display.stop.load(Ordering::Relaxed) {
                return Ok(Finished {
                    outcome: Outcome::Stopped,
                    exits,
                    interrupts,
                    floating_pages,
                    window_exits,
                });
            }
            if last_frame.elapsed() >= FRAME_INTERVAL {
                last_frame = Instant::now();
                if let Ok(mut frame) = display.frame.try_lock() {
                    vm.read_framebuffer(&mut frame)?;
                }
            }
        }
        if Instant::now() >= deadline {
            return Ok(Finished {
                outcome: Outcome::TimedOut,
                exits,
                interrupts,
                floating_pages,
                window_exits,
            });
        }
        {
            let mut bus = shared.bus.lock().map_err(|_| "device lock poisoned")?;
            let touched = shared.touch_ready.swap(false, Ordering::Relaxed);
            let frames = shared
                .network_ready
                .as_ref()
                .is_some_and(|ready| ready.swap(false, Ordering::Relaxed));
            if touched || frames {
                bus.service_devices(&RamView(vm));
            }
            if bus.reset_requested() {
                return Ok(Finished {
                    outcome: Outcome::Reset,
                    exits,
                    interrupts,
                    floating_pages,
                    window_exits,
                });
            }
            match delivery(bus.pending_vector(), interruptible) {
                Delivery::Inject(vector) => {
                    vm.inject_interrupt(vector)?;
                    bus.acknowledge_interrupt();
                    interrupts += 1;
                }
                Delivery::AskForWindow => vm.request_interrupt_window()?,
                Delivery::Nothing => {}
            }
        }

        let info = vm.run()?;
        exits += 1;
        interruptible = info.interruptible;
        match info.exit {
            VmExit::PortRead { port, size } => {
                let now = shared.now_ns();
                let value = shared
                    .bus
                    .lock()
                    .map_err(|_| "device lock poisoned")?
                    .read(port, size, now);
                vm.finish_port_read(&info, size, value)?;
            }
            VmExit::PortWrite { port, size, value } => {
                let now = shared.now_ns();
                let mut bus = shared.bus.lock().map_err(|_| "device lock poisoned")?;
                bus.write(port, size, value, now);
                bus.service_devices(&RamView(vm));
                drop(bus);
                vm.skip_instruction(&info)?;
            }
            VmExit::Halted => {
                if !info.interruptible {
                    return Ok(Finished {
                        outcome: Outcome::PoweredOff,
                        exits,
                        interrupts,
                        floating_pages,
                        window_exits,
                    });
                }
                wait_while_halted(shared, deadline)?;
            }
            VmExit::InterruptWindow => window_exits += 1,
            VmExit::Canceled => {}
            VmExit::Mmio {
                gpa,
                instruction,
                instruction_bytes,
                ..
            } if shared
                .bus
                .lock()
                .map_err(|_| "device lock poisoned")?
                .owns_memory(gpa) =>
            {
                let bytes = &instruction[..usize::from(instruction_bytes)];
                device_memory(vm, shared, &info, gpa, bytes)?;
            }
            VmExit::Mmio {
                gpa, write: false, ..
            } => {
                // Linux probes for chipset registers that this machine does
                // not have, and expects all ones when nothing answers.
                vm.map_floating_page(gpa)?;
                floating_pages += 1;
            }
            VmExit::Mmio { gpa, write, .. } => {
                return Err(format!(
                    "guest {} unmapped address {gpa:#x} at {:?}",
                    if write { "wrote" } else { "read" },
                    info.cpu
                )
                .into());
            }
            VmExit::Other { reason } => {
                return Err(format!("unhandled exit {reason:#x} at {:?}", info.cpu).into());
            }
        }
    }
}

fn command_line(api: &Api, has_disk: bool) -> String {
    let mut line = BASE_CMDLINE.to_owned();
    if !has_disk {
        line.push_str(NO_PCI);
    }
    if let Some(hz) = vm::tsc_hz(api) {
        line.push_str(&format!(" tsc_early_khz={}", hz / 1000));
    }
    line
}

/// Tells the kernel the clock rate, which avoids a slow calibration.
fn with_tsc_rate(api: &Api, mut line: String) -> String {
    if !line.contains("tsc_early_khz=")
        && let Some(hz) = vm::tsc_hz(api)
    {
        line.push_str(&format!(" tsc_early_khz={}", hz / 1000));
    }
    line
}

fn open_disk(path: &PathBuf) -> Result<VirtioBlk, Box<dyn Error>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("could not open disk {}: {error}", path.display()))?;
    let size = file.metadata()?.len();
    Ok(VirtioBlk::new(Box::new(file), size))
}

/// Boots the guest and runs it until it stops.
pub fn run(config: Config) -> Result<Report, Box<dyn Error>> {
    const MIB: u64 = 1024 * 1024;
    let api = Api::load()?;
    let ram_bytes = config.ram_mib as u64 * MIB;
    let kernel = BzImage::parse(&config.kernel)?;

    let mut initrd = config.initrd;
    let built_in = if config.shell { SHELL_INIT } else { SMOKE_INIT };
    let script = config.init_script.as_deref().unwrap_or(built_in);
    if config.own_init_cmdline.is_none() {
        cpio::concatenate(
            &mut initrd,
            &cpio::single_executable(INIT_PATH, script.as_bytes()),
        );
    }
    let initrd_gpa = kernel.initrd_address(initrd.len(), ram_bytes)?;

    let disks = config
        .disks
        .iter()
        .map(open_disk)
        .collect::<Result<Vec<_>, _>>()?;
    let cmdline = match &config.own_init_cmdline {
        Some(own) => with_tsc_rate(&api, own.clone()),
        None => command_line(&api, !disks.is_empty() || config.network.is_some()),
    };

    let mut vm = Vm::new(&api, config.ram_mib * 1024 * 1024)?;
    if let Some(screen) = config.framebuffer {
        vm.add_framebuffer(u64::from(FRAMEBUFFER_GPA), screen.size_bytes())?;
    }
    vm.write_ram(kernel.load_address(), kernel.payload())?;
    vm.write_ram(initrd_gpa, &initrd)?;
    vm.write_ram(CMDLINE_GPA, &boot::cmdline_bytes(&cmdline)?)?;
    let params = kernel.boot_params(
        ram_bytes,
        Some((initrd_gpa, initrd.len())),
        config.framebuffer,
    )?;
    vm.write_ram(BOOT_PARAMS_GPA, &params)?;
    let gdt = boot::gdt_bytes();
    vm.write_ram(GDT_GPA, &gdt)?;
    vm.enter_protected_mode(
        kernel.load_address(),
        BOOT_PARAMS_GPA,
        GDT_GPA,
        gdt.len() as u16,
    )?;

    let ready = Arc::new(Mutex::new(None));
    let console = Console {
        out: std::io::stdout(),
        flush_every_byte: config.shell,
        matched: 0,
        ready: Arc::clone(&ready),
        capture: config
            .display
            .as_ref()
            .map(|display| Arc::clone(&display.console_output)),
    };
    let network_ready = config
        .network
        .as_ref()
        .map(|network| Arc::clone(&network.ready));
    let card = config.network.clone().map(|network| {
        let incoming = Arc::clone(&network.incoming);
        VirtioNet::new(
            GUEST_MAC,
            Box::new(move |frame| network.send_frame(frame)),
            incoming,
        )
    });
    let started = Instant::now();
    let deadline = started + config.timeout;
    let shared = Shared {
        bus: Mutex::new(Bus::new(
            Box::new(console),
            unix_seconds,
            disks,
            card,
            config.touchscreen.then(VirtioInput::default),
        )),
        wake: Condvar::new(),
        stop: AtomicBool::new(false),
        started,
        display: config.display.clone(),
        input_script: config.input_script.clone(),
        touch_script: config.touch_script.clone(),
        touch_ready: AtomicBool::new(false),
        network_ready,
    };
    let kicker = vm.kicker();
    let input = config.shell.then(spawn_stdin_reader);

    let finished = std::thread::scope(|scope| {
        let (timer_shared, timer_kicker) = (&shared, &kicker);
        scope.spawn(move || timer_loop(timer_shared, timer_kicker, input.as_ref()));
        let result = vcpu_loop(&vm, &shared, deadline);
        shared.stop.store(true, Ordering::Relaxed);
        result
    })
    .inspect(|_| {
        let _ = std::io::stdout().flush();
    })?;

    let total_ms = started.elapsed().as_secs_f64() * 1e3;
    if let (Some(screen), Some(path)) = (config.framebuffer, &config.screenshot) {
        let mut pixels = vec![0_u8; screen.size_bytes()];
        vm.read_framebuffer(&mut pixels)?;
        std::fs::write(path, screen.to_bmp(&pixels))?;
    }
    let boot_to_user_space_ms = ready
        .lock()
        .map_err(|_| "ready lock poisoned")?
        .map(|at| at.duration_since(started).as_secs_f64() * 1e3);
    let bus = shared.bus.lock().map_err(|_| "device lock poisoned")?;
    let (io, unclaimed_ports) = (bus.stats(), bus.busiest_unclaimed_ports(5));
    drop(bus);
    Ok(Report {
        outcome: finished.outcome,
        ram_mib: config.ram_mib,
        boot_to_user_space_ms,
        total_ms,
        vcpu_exits: finished.exits,
        interrupts: finished.interrupts,
        floating_pages: finished.floating_pages,
        interrupt_window_exits: finished.window_exits,
        io,
        unclaimed_ports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(text: &[u8]) -> usize {
        let mut matched = 0;
        let mut found = 0;
        for &byte in text {
            matched = next_match(matched, byte);
            if matched == READY_MARKER.len() {
                matched = 0;
                found += 1;
            }
        }
        found
    }

    #[test]
    fn finds_the_marker_inside_other_output() {
        assert_eq!(scan(b"[ 1.0] booting\nRAX_GUEST_READY\n"), 1);
    }

    #[test]
    fn recovers_after_a_partial_match() {
        assert_eq!(scan(b"RAX_RAX_GUEST_READY"), 1);
    }

    #[test]
    fn ignores_text_without_the_marker() {
        assert_eq!(scan(b"RAX_GUEST_NOT_READY"), 0);
    }

    #[test]
    fn nothing_pending_needs_no_action() {
        assert_eq!(delivery(None, true), Delivery::Nothing);
        assert_eq!(delivery(None, false), Delivery::Nothing);
    }

    #[test]
    fn interruptible_guests_get_the_vector() {
        assert_eq!(delivery(Some(0x34), true), Delivery::Inject(0x34));
    }

    #[test]
    fn a_blocked_guest_asks_for_a_window_every_time() {
        // Regression test: repeating this request is what prevents stalls.
        for _ in 0..2 {
            assert_eq!(delivery(Some(0x34), false), Delivery::AskForWindow);
        }
    }

    #[test]
    fn scripts_use_an_absolute_interpreter() {
        assert!(SMOKE_INIT.starts_with("#!/bin/busybox sh\n"));
        assert!(SHELL_INIT.starts_with("#!/bin/busybox sh\n"));
    }

    #[test]
    fn scripts_print_the_marker() {
        let marker = std::str::from_utf8(READY_MARKER).unwrap();
        assert!(SMOKE_INIT.contains(marker));
        assert!(SHELL_INIT.contains(marker));
    }

    #[test]
    fn kernel_is_told_where_init_lives() {
        assert!(BASE_CMDLINE.contains(&format!("rdinit=/{INIT_PATH}")));
    }
}
