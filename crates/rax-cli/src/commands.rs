//! Command implementations.

use std::error::Error;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use rax_host::assess::{self, Finding, Probes, Severity};
use rax_host::host::{self, HostInfo};
use rax_host::vulkan::{self, Device};
use rax_host::whp::{self, Capabilities, ExitLatency, MapCost};
use serde::Serialize;

use crate::args::Command;

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
