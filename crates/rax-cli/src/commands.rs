//! Command implementations.

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rax_host::assess::{self, Finding, Probes, Severity};
use rax_host::host::{self, HostInfo};
use rax_host::vulkan::{self, Device};
use rax_host::whp::{self, Capabilities, ExitLatency, MapCost};
use rax_vmm::framebuffer::Framebuffer;
use rax_vmm::machine::{self, Config, Outcome, Report};
use rax_vmm::ps2::GuestInput;
use serde::Serialize;

use crate::args::{Command, VmRun};

const EXIT_BLOCKED: u8 = 2;
/// Bump when a benchmark result field changes meaning.
const RESULT_SCHEMA_VERSION: u32 = 1;

/// Runs a command that touches the host.
pub fn run(command: Command) -> Result<ExitCode, Box<dyn Error>> {
    match command {
        Command::Doctor { json } => doctor(json),
        Command::BenchWhp {
            iterations,
            map_mib,
            out,
        } => bench_whp(iterations, map_mib, out.as_deref()),
        Command::VmRun(options) => vm_run(&options),
        Command::InputCheck { profile } => input_check(&profile),
        Command::Launch => {
            crate::app::run()?;
            Ok(ExitCode::SUCCESS)
        }
        Command::UiPreview { out } => ui_preview(&out),
        Command::Help | Command::Version => unreachable!("handled in main"),
    }
}

#[derive(Serialize)]
struct DoctorReport<'a> {
    verdict: Severity,
    host: &'a HostInfo,
    hypervisor: Option<&'a Capabilities>,
    gpus: &'a [Device],
    findings: &'a [Finding],
}

fn doctor(json: bool) -> Result<ExitCode, Box<dyn Error>> {
    let host = host::probe()?;
    let capabilities = whp::Api::load()
        .and_then(|api| api.capabilities())
        .map_err(|error| error.to_string());
    let devices = vulkan::enumerate().map_err(|error| error.to_string());

    let findings = assess::assess(Probes {
        host: &host,
        hypervisor: capabilities.as_ref().map_err(String::as_str),
        vulkan: devices.as_ref().map(Vec::as_slice).map_err(String::as_str),
    });
    let verdict = assess::overall(&findings);

    if json {
        let report = DoctorReport {
            verdict,
            host: &host,
            hypervisor: capabilities.as_ref().ok(),
            gpus: devices.as_ref().map_or(&[], Vec::as_slice),
            findings: &findings,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_doctor(&host, &findings, verdict);
    }
    Ok(if verdict == Severity::Blocker {
        ExitCode::from(EXIT_BLOCKED)
    } else {
        ExitCode::SUCCESS
    })
}

fn print_doctor(host: &HostInfo, findings: &[Finding], verdict: Severity) {
    println!("Host: {} / Windows {}", host.cpu, host.windows_version);
    println!(
        "RAM:  {} MiB total, {} MiB available\n",
        host.total_ram_mib, host.available_ram_mib
    );
    for finding in findings {
        let label = match finding.severity {
            Severity::Ok => "[ ok ]",
            Severity::Warning => "[warn]",
            Severity::Blocker => "[FAIL]",
        };
        println!("{label} {}", finding.message);
    }
    println!(
        "\nVerdict: {}",
        match verdict {
            Severity::Ok => "ready",
            Severity::Warning => "ready, with warnings",
            Severity::Blocker => "blocked",
        }
    );
}

#[derive(Serialize)]
struct BenchResult {
    schema_version: u32,
    benchmark: &'static str,
    rax_version: &'static str,
    profile: &'static str,
    captured_at_unix: u64,
    host: HostInfo,
    hypervisor: Capabilities,
    iterations: usize,
    exit_latency: ExitLatency,
    memory_map: MapCost,
}

fn bench_whp(
    iterations: usize,
    map_mib: usize,
    out: Option<&str>,
) -> Result<ExitCode, Box<dyn Error>> {
    let api = whp::Api::load()?;
    let hypervisor = api.capabilities()?;
    if !hypervisor.hypervisor_present {
        return Err("no hypervisor is running; see `rax doctor`".into());
    }
    let result = BenchResult {
        schema_version: RESULT_SCHEMA_VERSION,
        benchmark: "whp-exit-and-map",
        rax_version: env!("CARGO_PKG_VERSION"),
        profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        captured_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        host: host::probe()?,
        hypervisor,
        iterations,
        exit_latency: whp::measure_exit_latency(&api, iterations)?,
        memory_map: whp::measure_map_cost(&api, map_mib)?,
    };
    let json = serde_json::to_string_pretty(&result)?;
    match out {
        Some(path) => {
            std::fs::write(path, format!("{json}\n"))?;
            eprintln!("wrote {path}");
        }
        None => println!("{json}"),
    }
    if cfg!(debug_assertions) {
        eprintln!("warning: debug build, numbers are not representative");
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Serialize)]
struct VmRunResult {
    schema_version: u32,
    benchmark: &'static str,
    rax_version: &'static str,
    profile: &'static str,
    captured_at_unix: u64,
    host: HostInfo,
    run: Report,
}

fn ui_preview(path: &str) -> Result<ExitCode, Box<dyn Error>> {
    let mut launcher = rax_ui::Launcher::new(rax_ui::Settings::default(), true);
    let pixels = launcher.draw().ok_or("nothing was drawn")?;
    let screen = Framebuffer {
        width: rax_ui::UI_WIDTH as u16,
        height: rax_ui::UI_HEIGHT as u16,
    };
    std::fs::write(path, screen.to_bmp(pixels))?;
    Ok(ExitCode::SUCCESS)
}

fn input_check(path: &str) -> Result<ExitCode, Box<dyn Error>> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("could not read {path}: {error}"))?;
    let profile = rax_input::Profile::from_json(&text)?;
    println!(
        "{}: {} bindings, valid",
        profile.name,
        profile.bindings.len()
    );
    Ok(ExitCode::SUCCESS)
}

/// Parses lines of `SECONDS move DX DY`, `SECONDS buttons MASK`, or
/// `SECONDS key SCANCODE down|up`, so input can be tested with nobody present.
fn parse_input_script(text: &str) -> Result<Vec<(Duration, GuestInput)>, Box<dyn Error>> {
    let mut script = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let words: Vec<&str> = line.split_whitespace().collect();
        let event = match words.as_slice() {
            [_, "move", dx, dy] => GuestInput::MouseMove {
                dx: dx.parse()?,
                dy: dy.parse()?,
            },
            [_, "buttons", mask] => GuestInput::MouseButtons(mask.parse()?),
            [_, "key", code, state] => GuestInput::Key {
                scancode: u8::from_str_radix(code, 16)?,
                extended: false,
                down: *state == "down",
            },
            _ => return Err(format!("bad input script line: {line}").into()),
        };
        script.push((Duration::from_secs_f64(words[0].parse()?), event));
    }
    Ok(script)
}

fn vm_run(options: &VmRun) -> Result<ExitCode, Box<dyn Error>> {
    let read =
        |path: &str| std::fs::read(path).map_err(|error| format!("could not read {path}: {error}"));
    let report = machine::run(Config {
        kernel: read(&options.kernel)?,
        initrd: read(&options.initrd)?,
        ram_mib: options.ram_mib,
        timeout: Duration::from_secs(options.timeout_secs as u64),
        shell: options.shell,
        disks: options.disks.iter().map(PathBuf::from).collect(),
        init_script: None,
        own_init_cmdline: options.cmdline.clone(),
        framebuffer: options
            .framebuffer
            .map(|(width, height)| Framebuffer { width, height }),
        screenshot: options.screenshot.as_ref().map(PathBuf::from),
        display: None,
        input_script: match &options.input_script {
            Some(path) => parse_input_script(&std::fs::read_to_string(path)?)?,
            None => Vec::new(),
        },
    })?;
    eprintln!(
        "\nguest stopped: {:?}, user space after {} ms, {} exits, {} interrupts",
        report.outcome,
        report
            .boot_to_user_space_ms
            .map_or_else(|| "never".to_owned(), |ms| format!("{ms:.0}")),
        report.vcpu_exits,
        report.interrupts
    );
    if let Some(path) = &options.report {
        let result = VmRunResult {
            schema_version: RESULT_SCHEMA_VERSION,
            benchmark: "vm-boot",
            rax_version: env!("CARGO_PKG_VERSION"),
            profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            captured_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            host: host::probe()?,
            run: report.clone(),
        };
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&result)?),
        )?;
    }
    Ok(if report.outcome == Outcome::TimedOut {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
