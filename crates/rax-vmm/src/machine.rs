//! Assembles a guest and runs its vCPU.

use std::error::Error;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::Thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rax_host::virgl::Renderer;
use rax_host::vm::{self, Interrupter, Kicker, RamReleaser, Vm};
use rax_host::whp::{Api, ExitInfo, VmExit};
use rax_input::Touch;
use rax_net::host::Network;
use rax_net::nat::GUEST_MAC;
use serde::Serialize;

use crate::boot::{self, BOOT_PARAMS_GPA, BzImage, CMDLINE_GPA, GDT_GPA};
use crate::bus::{Bus, IoStats};
use crate::cpio;
use crate::framebuffer::{FRAMEBUFFER_GPA, Framebuffer};
use crate::ioapic::Interrupt;
use crate::mmio;
use crate::mptable;
use crate::ps2::GuestInput;
use crate::serial::Sink;
use crate::virtio_balloon::VirtioBalloon;
use crate::virtio_blk::{GuestMemory, OutOfRange, VirtioBlk};
use crate::virtio_gpu::{self, GpuStats, Scanout, VirtioGpu};
use crate::virtio_input::{ISR_OFFSET, NOTIFY_OFFSET, VirtioInput};
use crate::virtio_net::VirtioNet;

/// Printed by the guest once user space is running.
const READY_MARKER: &[u8] = b"RAX_GUEST_READY";
/// Printed by Linux as it powers off. With the hypervisor's local APIC, its
/// final `hlt` never reaches RAX, so this is how RAX learns the guest is done.
const POWER_OFF_MARKER: &[u8] = b": Power down";
/// The port Linux polls while timing its clock against the PIT.
const SPEAKER_PORT: u16 = 0x61;
/// Time between those polls. Linux rejects the whole measurement if it gets
/// under 1000 reads in 10 ms, or if one read takes ten times the fastest. A VM
/// exit can do that easily, and Android then ran with a clock that only moves
/// every few milliseconds. Holding each read to this length passes both tests.
const SPEAKER_POLL_NS: u64 = 8_000;
/// Pacing only applies this long after boot, when Linux takes that measurement.
const SPEAKER_PACING_ENDS_NS: u64 = 5_000_000_000;
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
/// The longest the guest waits for RAM to finish being released.
const RELEASE_WAIT: Duration = Duration::from_secs(5);
/// How long the render thread stays awake after finishing work.
const RENDER_SPIN: Duration = Duration::from_micros(200);
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
    /// Folder holding virglrenderer. The screen then comes from a virtio GPU
    /// drawing on the host's graphics card.
    pub gpu: Option<PathBuf>,
    /// Saves the screen to this BMP file when the guest stops.
    pub screenshot: Option<PathBuf>,
    /// Gives the guest a network card, with internet through this network.
    pub network: Option<Network>,
    /// Gives the guest a multi-touch screen.
    pub touchscreen: bool,
    /// Lets the guest give unused RAM back to the host.
    pub balloon: bool,
    /// Uses the hypervisor's local APIC and an I/O APIC, for guests that
    /// boot without `nolapic noapic`.
    pub apic: bool,
    /// Virtual CPUs. More than one needs `apic`.
    pub cpus: u8,
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
    /// Frames the guest has presented so far.
    pub frames: Arc<AtomicU64>,
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
            frames: Arc::new(AtomicU64::new(0)),
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
    /// GPU activity, if there is a GPU.
    pub gpu: Option<GpuStats>,
}

/// Totals across all vCPUs.
#[derive(Default)]
struct Counters {
    exits: AtomicU64,
    interrupts: AtomicU64,
    floating_pages: AtomicU64,
    window_exits: AtomicU64,
}

/// Writes the console to stdout and notes when the marker appears.
struct Console {
    out: std::io::Stdout,
    /// Shell prompts end without a newline, so they must not wait for one.
    flush_every_byte: bool,
    matched: usize,
    matched_power_off: usize,
    powered_off: Arc<AtomicBool>,
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
        self.matched_power_off = next_match(POWER_OFF_MARKER, self.matched_power_off, byte);
        if self.matched_power_off == POWER_OFF_MARKER.len() {
            self.matched_power_off = 0;
            self.powered_off.store(true, Ordering::Relaxed);
        }
        self.matched = next_match(READY_MARKER, self.matched, byte);
        if self.matched == READY_MARKER.len() {
            self.matched = 0;
            if let Ok(mut ready) = self.ready.lock() {
                ready.get_or_insert_with(Instant::now);
            }
        }
    }
}

/// Advances a search for `marker`. A marker's first byte must not repeat in
/// it, so a mismatch only ever needs to restart.
fn next_match(marker: &[u8], matched: usize, byte: u8) -> usize {
    if marker[matched] == byte {
        matched + 1
    } else {
        usize::from(marker[0] == byte)
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
    /// Rows in the display's frame.
    screen_height: usize,
    input_script: Vec<(Duration, GuestInput)>,
    touch_script: Vec<(Duration, Touch)>,
    /// Set when touches are waiting to reach guest memory.
    touch_ready: AtomicBool,
    /// Set when the network has frames waiting for the guest.
    network_ready: Option<Arc<AtomicBool>>,
    /// Hands guest RAM to the thread that gives it back to Windows.
    releases: Sender<Release>,
    /// Set when the guest's console says it has powered off.
    powered_off: Arc<AtomicBool>,
    /// When the configured run time ends.
    deadline: Instant,
    /// The GPU, if there is one.
    gpu: Option<GpuLink>,
    counters: Counters,
}

/// Work for the thread that gives RAM back. Windows takes up to milliseconds
/// per call, so doing this on the vCPU thread starved the guest: measured,
/// 11 seconds of guest time passed in 4 minutes.
enum Release {
    Discard {
        gpa: u64,
        length: u64,
    },
    /// Answered once everything sent earlier is done.
    Finish(Sender<()>),
}

fn release_loop(shared: &Shared, releaser: &RamReleaser<'_>, work: &Receiver<Release>) {
    while !shared.stop.load(Ordering::Relaxed) {
        match work.recv_timeout(HALT_POLL) {
            Ok(Release::Discard { gpa, length }) => {
                // A failure only means this memory stays with the guest.
                let _ = releaser.discard(gpa, length as usize);
            }
            Ok(Release::Finish(done)) => {
                let _ = done.send(());
            }
            Err(_) => {}
        }
    }
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

fn send_interrupts(interrupter: &Interrupter<'_>, interrupts: Vec<Interrupt>) {
    for interrupt in interrupts {
        // A failure here is not recoverable and would show as a stalled
        // guest, so it is printed rather than dropped.
        if let Err(error) = interrupter.send(
            interrupt.vector,
            interrupt.destination,
            interrupt.logical,
            interrupt.lowest_priority,
        ) {
            eprintln!("could not deliver interrupt {interrupt:?}: {error}");
        }
    }
}

fn timer_loop(
    shared: &Shared,
    kicker: &Kicker<'_>,
    interrupter: &Interrupter<'_>,
    input: Option<&Receiver<Vec<u8>>>,
) {
    let mut script_position = 0;
    let mut touch_position = 0;
    let mut last_screen_kick = Instant::now();
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
        let interrupts = bus.take_interrupts();
        drop(bus);
        send_interrupts(interrupter, interrupts);
        // A sleeping guest no longer exits to RAX by itself, so this thread
        // has to wake the vCPU for anything that should stop it.
        let stopping = shared.powered_off.load(Ordering::Relaxed)
            || Instant::now() >= shared.deadline
            || shared
                .display
                .as_ref()
                .is_some_and(|display| display.stop.load(Ordering::Relaxed));
        if stopping {
            kicker.kick_all();
        }
        // The vCPU also copies a software screen, and a sleeping guest no
        // longer exits by itself.
        if shared.gpu.is_none()
            && shared.display.is_some()
            && last_screen_kick.elapsed() >= FRAME_INTERVAL
        {
            last_screen_kick = Instant::now();
            kicker.kick();
        }
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
struct RamView<'a, 'b>(&'a Vm<'b>, &'a Shared);

impl GuestMemory for RamView<'_, '_> {
    fn read(&self, gpa: u64, buffer: &mut [u8]) -> Result<(), OutOfRange> {
        self.0.read_ram(gpa, buffer).map_err(|_| OutOfRange)
    }

    fn write(&self, gpa: u64, bytes: &[u8]) -> Result<(), OutOfRange> {
        self.0.write_ram(gpa, bytes).map_err(|_| OutOfRange)
    }

    fn discard(&self, gpa: u64, length: u64) {
        let _ = self.1.releases.send(Release::Discard { gpa, length });
    }

    fn finish_discards(&self) {
        let (done, finished) = mpsc::channel();
        if self.1.releases.send(Release::Finish(done)).is_ok() {
            // No answer only means RAX is shutting down.
            let _ = finished.recv_timeout(RELEASE_WAIT);
        }
    }
}

/// What the vCPUs share with the render thread.
struct GpuLink {
    device: Mutex<VirtioGpu>,
    /// The interrupt status, readable without the device lock.
    isr: Arc<AtomicU8>,
    /// Set when the guest has queued work.
    notified: AtomicBool,
    /// The render thread, once it is running.
    render_thread: OnceLock<Thread>,
    /// The last frame, for a screenshot once the guest has stopped.
    last_frame: Mutex<Option<(Scanout, Vec<u8>)>>,
}

impl GpuLink {
    fn new(device: VirtioGpu) -> Self {
        Self {
            isr: device.isr(),
            device: Mutex::new(device),
            notified: AtomicBool::new(false),
            render_thread: OnceLock::new(),
            last_frame: Mutex::new(None),
        }
    }

    fn notify(&self) {
        self.notified.store(true, Ordering::Release);
        if let Some(thread) = self.render_thread.get() {
            thread.unpark();
        }
    }
}

/// Reads what is on screen, as BGRX.
fn read_screen(
    link: &GpuLink,
    renderer: &mut Renderer<'_>,
    pixels: &mut Vec<u8>,
) -> Option<Scanout> {
    let started = Instant::now();
    let screen = link.device.lock().ok()?.scanout()?;
    let bytes = screen.width as usize * screen.height as usize * 4;
    pixels.resize(bytes, 0);
    renderer.prepare();
    let code = renderer.read_pixels(screen.resource, screen.width, screen.height, pixels);
    if code != 0 {
        return None;
    }
    if screen.swap_red_blue {
        virtio_gpu::swap_red_blue(pixels);
    }
    if let Ok(mut device) = link.device.lock() {
        device.record_present(started.elapsed());
    }
    Some(screen)
}

/// Runs the queues once, and tells the guest if anything finished.
fn render_once(
    vm: &Vm<'_>,
    shared: &Shared,
    link: &GpuLink,
    renderer: &mut Renderer<'_>,
    kicker: &Kicker<'_>,
    interrupter: &Interrupter<'_>,
) -> Result<(), &'static str> {
    let mut device = link.device.lock().map_err(|_| "GPU lock poisoned")?;
    device.service(&RamView(vm, shared), renderer);
    let finished = device.take_irq();
    let frames = device.stats().flushes;
    drop(device);
    if let Some(display) = &shared.display {
        display.frames.store(frames, Ordering::Relaxed);
    }
    if finished {
        let mut bus = shared.bus.lock().map_err(|_| "device lock poisoned")?;
        bus.raise_gpu();
        let pending = bus.pending_vector().is_some();
        let interrupts = bus.take_interrupts();
        drop(bus);
        send_interrupts(interrupter, interrupts);
        // Without an I/O APIC, the vCPU has to wake up to inject it.
        if pending {
            shared.wake.notify_all();
            kicker.kick();
        }
    }
    Ok(())
}

/// Owns the renderer, because an OpenGL context cannot change threads. The
/// guest keeps running while this works, so a long draw no longer stops it.
fn render_loop(
    vm: &Vm<'_>,
    shared: &Shared,
    link: &GpuLink,
    mut renderer: Renderer<'_>,
    kicker: &Kicker<'_>,
    interrupter: &Interrupter<'_>,
) {
    let mut pixels = Vec::new();
    let mut last_frame = Instant::now();
    let mut last_work = Instant::now();
    while !shared.stop.load(Ordering::Relaxed) {
        if link.notified.swap(false, Ordering::Acquire) {
            if render_once(vm, shared, link, &mut renderer, kicker, interrupter).is_err() {
                return;
            }
            last_work = Instant::now();
            continue;
        }
        if last_frame.elapsed() >= FRAME_INTERVAL {
            last_frame = Instant::now();
            renderer.poll();
            let waiting = link
                .device
                .lock()
                .is_ok_and(|mut device| device.take_frame_waiting());
            if waiting
                && let Some(display) = &shared.display
                && let Some(screen) = read_screen(link, &mut renderer, &mut pixels)
                && let Ok(mut frame) = display.frame.lock()
            {
                let rows = shared.screen_height.max(1);
                let columns = frame.len() / 4 / rows;
                virtio_gpu::copy_scaled(
                    &pixels,
                    (screen.width, screen.height),
                    &mut frame,
                    (columns as u32, rows as u32),
                );
            }
        }
        // Stay awake briefly: the guest's next request usually follows at
        // once, and it waits for this thread to wake.
        if last_work.elapsed() < RENDER_SPIN {
            std::hint::spin_loop();
        } else {
            std::thread::park_timeout(FRAME_INTERVAL);
        }
    }
    if let Some(screen) = read_screen(link, &mut renderer, &mut pixels)
        && let Ok(mut last) = link.last_frame.lock()
    {
        *last = Some((screen, pixels));
    }
}

/// The part of a register a store of `size` bytes writes.
fn stored_register(register: u64, size: u8) -> u32 {
    match size {
        1 => register as u32 & 0xff,
        2 => register as u32 & 0xffff,
        _ => register as u32,
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
    if let (Some(offset), Some(link)) = (bus.gpu_offset(gpa), shared.gpu.as_ref()) {
        drop(bus);
        if access.write {
            let value = match access.immediate {
                Some(value) => value,
                None => {
                    stored_register(vm.general_register(info.vp, access.register)?, access.size)
                }
            };
            // A notification must not wait for the draw in progress.
            if offset == NOTIFY_OFFSET {
                link.notify();
            } else {
                link.device
                    .lock()
                    .map_err(|_| "GPU lock poisoned")?
                    .write(offset, value);
            }
            vm.advance(info, access.length)?;
        } else {
            let value = if offset == ISR_OFFSET {
                u32::from(link.isr.swap(0, Ordering::Relaxed))
            } else {
                link.device
                    .lock()
                    .map_err(|_| "GPU lock poisoned")?
                    .read(offset, access.size)
            };
            vm.finish_load(
                info,
                access.register,
                access.size,
                value,
                access.zero_extend,
                access.length,
            )?;
        }
        return Ok(());
    }
    if access.write {
        let value = match access.immediate {
            Some(value) => value,
            None => stored_register(vm.general_register(info.vp, access.register)?, access.size),
        };
        bus.memory_write(gpa, value);
        bus.service_devices(&RamView(vm, shared));
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

/// Picks what to report from all the CPUs: any failure, then the first real
/// reason to stop. The rest only stopped because one CPU did.
fn choose_outcome(results: Vec<Result<Outcome, String>>) -> Result<Outcome, Box<dyn Error>> {
    let mut outcome = Outcome::Stopped;
    for result in results {
        let this = result?;
        if outcome == Outcome::Stopped {
            outcome = this;
        }
    }
    Ok(outcome)
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

fn vcpu_loop(
    vm: &Vm<'_>,
    shared: &Shared,
    vp: u32,
    interrupter: &Interrupter<'_>,
    deadline: Instant,
) -> Result<Outcome, Box<dyn Error>> {
    let counters = &shared.counters;
    let mut last_frame = Instant::now();
    let mut interruptible = false;
    let mut last_speaker_poll = 0;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return Ok(Outcome::Stopped);
        }
        let frame_due = vp == 0 && last_frame.elapsed() >= FRAME_INTERVAL;
        if frame_due {
            last_frame = Instant::now();
        }
        if let Some(display) = &shared.display {
            if display.stop.load(Ordering::Relaxed) {
                return Ok(Outcome::Stopped);
            }
            if frame_due
                && shared.gpu.is_none()
                && let Ok(mut frame) = display.frame.try_lock()
            {
                vm.read_framebuffer(&mut frame)?;
            }
        }
        if shared.powered_off.load(Ordering::Relaxed) {
            return Ok(Outcome::PoweredOff);
        }
        if Instant::now() >= deadline {
            return Ok(Outcome::TimedOut);
        }
        {
            let mut bus = shared.bus.lock().map_err(|_| "device lock poisoned")?;
            if frame_due {
                bus.poll_balloon(shared.now_ns(), &RamView(vm, shared));
            }
            let touched = shared.touch_ready.swap(false, Ordering::Relaxed);
            let frames = shared
                .network_ready
                .as_ref()
                .is_some_and(|ready| ready.swap(false, Ordering::Relaxed));
            if touched || frames {
                bus.service_devices(&RamView(vm, shared));
            }
            if bus.reset_requested() {
                return Ok(Outcome::Reset);
            }
            send_interrupts(interrupter, bus.take_interrupts());
            // The 8259 is wired to the first CPU only.
            if vp == 0 {
                match delivery(bus.pending_vector(), interruptible) {
                    Delivery::Inject(vector) => {
                        vm.inject_interrupt(vector)?;
                        bus.acknowledge_interrupt();
                        counters.interrupts.fetch_add(1, Ordering::Relaxed);
                    }
                    Delivery::AskForWindow => vm.request_interrupt_window()?,
                    Delivery::Nothing => {}
                }
            }
        }

        let info = vm.run(vp)?;
        counters.exits.fetch_add(1, Ordering::Relaxed);
        interruptible = info.interruptible;
        match info.exit {
            VmExit::PortRead { port, size } => {
                if port == SPEAKER_PORT && shared.now_ns() < SPEAKER_PACING_ENDS_NS {
                    while shared.now_ns() < last_speaker_poll + SPEAKER_POLL_NS {
                        std::hint::spin_loop();
                    }
                    last_speaker_poll = shared.now_ns();
                }
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
                bus.service_devices(&RamView(vm, shared));
                drop(bus);
                vm.skip_instruction(&info)?;
            }
            VmExit::Halted => {
                if vp != 0 {
                    continue;
                }
                if !info.interruptible {
                    return Ok(Outcome::PoweredOff);
                }
                wait_while_halted(shared, deadline)?;
            }
            VmExit::InterruptWindow => {
                counters.window_exits.fetch_add(1, Ordering::Relaxed);
            }
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
                counters.floating_pages.fetch_add(1, Ordering::Relaxed);
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

    if config.cpus > 1 && !config.apic {
        return Err("more than one CPU needs the APIC".into());
    }
    let mut vm = Vm::with_local_apic(
        &api,
        config.ram_mib * 1024 * 1024,
        u32::from(config.cpus.max(1)),
        config.apic,
    )?;
    if config.apic {
        vm.write_ram(mptable::MP_GPA, &mptable::build(config.cpus.max(1)))?;
    }
    // With a GPU, the guest finds its screen there and not in the boot data.
    let vesa_screen = config.framebuffer.filter(|_| config.gpu.is_none());
    if let Some(screen) = vesa_screen {
        vm.add_framebuffer(u64::from(FRAMEBUFFER_GPA), screen.size_bytes())?;
    }
    vm.write_ram(kernel.load_address(), kernel.payload())?;
    vm.write_ram(initrd_gpa, &initrd)?;
    vm.write_ram(CMDLINE_GPA, &boot::cmdline_bytes(&cmdline)?)?;
    let params = kernel.boot_params(ram_bytes, Some((initrd_gpa, initrd.len())), vesa_screen)?;
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
    let powered_off = Arc::new(AtomicBool::new(false));
    let console = Console {
        out: std::io::stdout(),
        flush_every_byte: config.shell,
        matched: 0,
        matched_power_off: 0,
        powered_off: Arc::clone(&powered_off),
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
    let (releases, release_work) = mpsc::channel();
    let shared = Shared {
        bus: Mutex::new(Bus::new(
            Box::new(console),
            unix_seconds,
            disks,
            card,
            config.touchscreen.then(VirtioInput::default),
            config.gpu.is_some(),
            config.balloon.then(VirtioBalloon::default),
        )),
        wake: Condvar::new(),
        stop: AtomicBool::new(false),
        started,
        display: config.display.clone(),
        screen_height: config
            .framebuffer
            .map_or(0, |screen| usize::from(screen.height)),
        input_script: config.input_script.clone(),
        touch_script: config.touch_script.clone(),
        touch_ready: AtomicBool::new(false),
        network_ready,
        releases,
        powered_off,
        deadline,
        counters: Counters::default(),
        gpu: config.gpu.as_ref().and(config.framebuffer).map(|screen| {
            GpuLink::new(VirtioGpu::new(
                u32::from(screen.width),
                u32::from(screen.height),
            ))
        }),
    };
    if config.gpu.is_some() && shared.gpu.is_none() {
        return Err("a GPU needs a screen size".into());
    }
    if config.apic {
        shared
            .bus
            .lock()
            .map_err(|_| "device lock poisoned")?
            .add_ioapic(config.cpus.max(1));
    }
    let kicker = vm.kicker();
    let interrupter = vm.interrupter();
    let releaser = vm.ram_releaser();
    let input = config.shell.then(spawn_stdin_reader);

    let finished = std::thread::scope(|scope| {
        let (timer_shared, timer_kicker, timer_interrupter) = (&shared, &kicker, &interrupter);
        scope.spawn(move || {
            timer_loop(
                timer_shared,
                timer_kicker,
                timer_interrupter,
                input.as_ref(),
            );
        });
        let (release_shared, release_handle) = (&shared, &releaser);
        scope.spawn(move || release_loop(release_shared, release_handle, &release_work));
        if let (Some(link), Some(folder)) = (&shared.gpu, &config.gpu) {
            let (vm, shared, kicker, interrupter) = (&vm, &shared, &kicker, &interrupter);
            let (started, outcome) = mpsc::channel();
            let thread = scope.spawn(move || match vm.start_renderer(folder) {
                Ok(renderer) => {
                    let _ = started.send(Ok(()));
                    render_loop(vm, shared, link, renderer, kicker, interrupter);
                }
                Err(error) => {
                    let _ = started.send(Err(error.to_string()));
                }
            });
            let _ = link.render_thread.set(thread.thread().clone());
            let start = outcome
                .recv()
                .map_err(|_| String::from("the render thread stopped"))
                .and_then(|result| result);
            if let Err(error) = start {
                shared.stop.store(true, Ordering::Relaxed);
                return Err(error.into());
            }
        }
        let run_cpu = |vp: u32| {
            let result = vcpu_loop(&vm, &shared, vp, &interrupter, deadline)
                .map_err(|error| error.to_string());
            // One CPU stopping means the whole guest has stopped.
            shared.stop.store(true, Ordering::Relaxed);
            kicker.kick_all();
            result
        };
        let others: Vec<_> = (1..u32::from(config.cpus))
            .map(|vp| scope.spawn(move || run_cpu(vp)))
            .collect();
        let first = run_cpu(0);
        // A CPU can be between its stop check and running the guest, where a
        // single kick would be lost.
        while !others
            .iter()
            .all(std::thread::ScopedJoinHandle::is_finished)
        {
            kicker.kick_all();
            std::thread::sleep(TIMER_IDLE);
        }
        let mut results = vec![first];
        results.extend(others.into_iter().map(|cpu| {
            cpu.join()
                .unwrap_or_else(|_| Err(String::from("a vCPU thread panicked")))
        }));
        choose_outcome(results)
    })
    .inspect(|_| {
        let _ = std::io::stdout().flush();
    })?;

    let total_ms = started.elapsed().as_secs_f64() * 1e3;
    if let (Some(screen), Some(path)) = (config.framebuffer, &config.screenshot) {
        let mut pixels = vec![0_u8; screen.size_bytes()];
        match &shared.gpu {
            Some(link) => {
                let last = link.last_frame.lock().map_err(|_| "GPU lock poisoned")?;
                if let Some((shown, source)) = last.as_ref() {
                    virtio_gpu::copy_scaled(
                        source,
                        (shown.width, shown.height),
                        &mut pixels,
                        (u32::from(screen.width), u32::from(screen.height)),
                    );
                }
            }
            None => vm.read_framebuffer(&mut pixels)?,
        }
        std::fs::write(path, screen.to_bmp(&pixels))?;
    }
    let gpu_stats = match &shared.gpu {
        Some(link) => Some(link.device.lock().map_err(|_| "GPU lock poisoned")?.stats()),
        None => None,
    };
    let boot_to_user_space_ms = ready
        .lock()
        .map_err(|_| "ready lock poisoned")?
        .map(|at| at.duration_since(started).as_secs_f64() * 1e3);
    let bus = shared.bus.lock().map_err(|_| "device lock poisoned")?;
    let (io, unclaimed_ports) = (bus.stats(), bus.busiest_unclaimed_ports(5));
    drop(bus);
    Ok(Report {
        outcome: finished,
        ram_mib: config.ram_mib,
        boot_to_user_space_ms,
        total_ms,
        vcpu_exits: shared.counters.exits.load(Ordering::Relaxed),
        interrupts: shared.counters.interrupts.load(Ordering::Relaxed),
        floating_pages: shared.counters.floating_pages.load(Ordering::Relaxed),
        interrupt_window_exits: shared.counters.window_exits.load(Ordering::Relaxed),
        io,
        unclaimed_ports,
        gpu: gpu_stats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(text: &[u8]) -> usize {
        scan_for(READY_MARKER, text)
    }

    fn scan_for(marker: &[u8], text: &[u8]) -> usize {
        let mut matched = 0;
        let mut found = 0;
        for &byte in text {
            matched = next_match(marker, matched, byte);
            if matched == marker.len() {
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
    fn power_off_is_found_in_the_console() {
        assert_eq!(
            scan_for(POWER_OFF_MARKER, b"[ 9.1] reboot: Power down\n"),
            1
        );
        assert_eq!(
            scan_for(POWER_OFF_MARKER, b"reboot: Restarting system\n"),
            0
        );
    }

    #[test]
    fn the_power_off_marker_can_restart_on_a_mismatch() {
        assert!(!POWER_OFF_MARKER[1..].contains(&POWER_OFF_MARKER[0]));
    }

    #[test]
    fn speaker_pacing_passes_the_kernels_two_tests() {
        // At least 1000 reads in 10 ms, and a 50 us host hiccup stays under
        // ten times the fastest read.
        const { assert!(10_000_000 / SPEAKER_POLL_NS >= 1000) };
        const { assert!(50_000 < 10 * SPEAKER_POLL_NS) };
    }

    #[test]
    fn a_real_outcome_beats_cpus_that_merely_stopped() {
        let results = vec![
            Ok(Outcome::Stopped),
            Ok(Outcome::Reset),
            Ok(Outcome::Stopped),
        ];
        assert_eq!(choose_outcome(results).unwrap(), Outcome::Reset);
    }

    #[test]
    fn the_first_cpu_to_stop_for_a_reason_wins() {
        let results = vec![Ok(Outcome::TimedOut), Ok(Outcome::PoweredOff)];
        assert_eq!(choose_outcome(results).unwrap(), Outcome::TimedOut);
    }

    #[test]
    fn a_failure_on_any_cpu_is_reported() {
        let results = vec![Ok(Outcome::Stopped), Err(String::from("bad"))];
        assert!(choose_outcome(results).is_err());
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
