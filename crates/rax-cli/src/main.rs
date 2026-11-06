//! `rax`: host readiness checks and measurements for the RAX runtime.

#![forbid(unsafe_code)]

mod args;

#[cfg(windows)]
mod commands;

use std::process::ExitCode;

const USAGE: &str = "\
rax - Android gaming runtime for Windows (early development)

USAGE:
    rax doctor [--json]
    rax bench whp [--iterations N] [--map-mib N] [--out FILE]
    rax vm run --kernel FILE --initrd FILE [--ram-mib N] [--timeout-secs N]
               [--shell] [--disk FILE] [--cmdline TEXT] [--report FILE]
               [--framebuffer WxH] [--screenshot FILE]
    rax input check FILE
    rax --version
    rax --help

COMMANDS:
    doctor     Check whether this PC can host RAX.
               Exit code: 0 ready, 1 error, 2 a blocker was found.
    bench whp  Run a tiny guest and measure hypervisor costs.
    vm run     Boot a Linux guest on RAX's own VM, with a serial console.
    input check  Validate a key-mapping profile.
";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let command = match args::parse(&arguments) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match command {
        args::Command::Help => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        args::Command::Version => {
            println!("rax {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        #[cfg(windows)]
        command => match commands::run(command) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        },
        #[cfg(not(windows))]
        _ => {
            eprintln!("error: RAX runs on Windows only");
            ExitCode::FAILURE
        }
    }
}
