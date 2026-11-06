//! Assembles a guest and runs its vCPU.

use std::error::Error;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rax_host::vm::{self, Kicker, Vm};
use rax_host::whp::{Api, VmExit};
use serde::Serialize;

use crate::boot::{self, BOOT_PARAMS_GPA, BzImage, CMDLINE_GPA};
use crate::bus::{Bus, IoStats};
use crate::cpio;
use crate::serial::Sink;

/// Printed by the guest once user space is running.
const READY_MARKER: &[u8] = b"RAX_GUEST_READY";
const INIT_PATH: &str = "rax-init";
const BASE_CMDLINE: &str = "console=ttyS0 earlyprintk=serial,ttyS0,115200 nolapic noapic \
    acpi=off pci=off nosmp reboot=k panic=-1 i8042.noaux tsc=reliable rdinit=/rax-init";
/// How long the timer thread sleeps when nothing is due sooner.
const TIMER_IDLE: Duration = Duration::from_millis(1);

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
}

impl Sink for Console {
    fn put(&mut self, byte: u8) {
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
}

impl Shared {
    fn now_ns(&self) -> u64 {
        self.started.elapsed().as_nanos() as u64
    }
}

fn timer_loop(shared: &Shared, kicker: &Kicker<'_>, input: Option<&Receiver<Vec<u8>>>) {
    while !shared.stop.load(Ordering::Relaxed) {
        let now = shared.now_ns();
        let Ok(mut bus) = shared.bus.lock() else {
            return;
        };
        while let Some(bytes) = input.and_then(|input| input.try_recv().ok()) {
            bus.console_input(&bytes);
        }
        let pending = bus.tick(now);
        let next = bus.next_timer_ns();
        drop(bus);
        if pending {
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
    let mut interruptible = false;
    loop {
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
            VmExit::PortWrite { port, value, .. } => {
                let now = shared.now_ns();
                shared
                    .bus
                    .lock()
                    .map_err(|_| "device lock poisoned")?
                    .write(port, value, now);
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
                let bus = shared.bus.lock().map_err(|_| "device lock poisoned")?;
                let wait = deadline.saturating_duration_since(Instant::now());
                let (_bus, _) = shared
                    .wake
                    .wait_timeout_while(bus, wait, |bus| {
                        bus.pending_vector().is_none() && !bus.reset_requested()
                    })
                    .map_err(|_| "device lock poisoned")?;
            }
            VmExit::InterruptWindow => window_exits += 1,
            VmExit::Canceled => {}
            VmExit::Mmio { gpa, write: false } => {
                // Linux probes for chipset registers that this machine does
                // not have, and expects all ones when nothing answers.
                vm.map_floating_page(gpa)?;
                floating_pages += 1;
            }
            VmExit::Mmio { gpa, write } => {
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

fn command_line(api: &Api) -> String {
    match vm::tsc_hz(api) {
        Some(hz) => format!("{BASE_CMDLINE} tsc_early_khz={}", hz / 1000),
        None => BASE_CMDLINE.to_owned(),
    }
}

/// Boots the guest and runs it until it stops.
pub fn run(config: Config) -> Result<Report, Box<dyn Error>> {
    const MIB: u64 = 1024 * 1024;
    let api = Api::load()?;
    let ram_bytes = config.ram_mib as u64 * MIB;
    let kernel = BzImage::parse(&config.kernel)?;

    let mut initrd = config.initrd;
    let script = if config.shell { SHELL_INIT } else { SMOKE_INIT };
    cpio::concatenate(
        &mut initrd,
        &cpio::single_executable(INIT_PATH, script.as_bytes()),
    );
    let initrd_gpa = kernel.initrd_address(initrd.len(), ram_bytes)?;

    let mut vm = Vm::new(&api, config.ram_mib * 1024 * 1024)?;
    vm.write_ram(kernel.load_address(), kernel.payload())?;
    vm.write_ram(initrd_gpa, &initrd)?;
    vm.write_ram(CMDLINE_GPA, &boot::cmdline_bytes(&command_line(&api))?)?;
    let params = kernel.boot_params(ram_bytes, Some((initrd_gpa, initrd.len())))?;
    vm.write_ram(BOOT_PARAMS_GPA, &params)?;
    vm.enter_protected_mode(kernel.load_address(), BOOT_PARAMS_GPA)?;

    let ready = Arc::new(Mutex::new(None));
    let console = Console {
        out: std::io::stdout(),
        flush_every_byte: config.shell,
        matched: 0,
        ready: Arc::clone(&ready),
    };
    let started = Instant::now();
    let deadline = started + config.timeout;
    let shared = Shared {
        bus: Mutex::new(Bus::new(Box::new(console), unix_seconds)),
        wake: Condvar::new(),
        stop: AtomicBool::new(false),
        started,
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
