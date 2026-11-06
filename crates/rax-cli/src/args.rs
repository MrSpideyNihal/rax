//! Command-line parsing.

const DEFAULT_ITERATIONS: usize = 200_000;
const DEFAULT_MAP_MIB: usize = 1024;
const DEFAULT_VM_RAM_MIB: usize = 512;
const DEFAULT_VM_TIMEOUT_SECS: usize = 60;

/// A parsed invocation.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Print the readiness report.
    Doctor {
        /// Emit JSON instead of text.
        json: bool,
    },
    /// Run the hypervisor benchmark.
    BenchWhp {
        /// Guest exits to measure.
        iterations: usize,
        /// Guest RAM to map, in MiB.
        map_mib: usize,
        /// File to write the JSON result to.
        out: Option<String>,
    },
    /// Boot a Linux guest.
    VmRun(Box<VmRun>),
    /// Check a key-mapping profile.
    InputCheck {
        /// Path to the profile.
        profile: String,
    },
    /// Start virglrenderer and report what it can do.
    GpuProbe {
        /// Folder holding the renderer DLLs.
        folder: String,
    },
    /// Open the launcher.
    Launch,
    /// Save the launcher screen as a BMP.
    UiPreview {
        /// Output file.
        out: String,
    },
    /// Print usage.
    Help,
    /// Print the version.
    Version,
}

/// Options for `rax vm run`.
#[derive(Debug, PartialEq, Eq)]
pub struct VmRun {
    /// Path to a `bzImage`.
    pub kernel: String,
    /// Path to an initramfs.
    pub initrd: String,
    /// Guest RAM in MiB.
    pub ram_mib: usize,
    /// Stop the guest after this many seconds.
    pub timeout_secs: usize,
    /// Attach stdin to a guest shell.
    pub shell: bool,
    /// File to write the JSON report to.
    pub report: Option<String>,
    /// Raw disk images for the guest. The option may repeat.
    pub disks: Vec<String>,
    /// Boot the guest's own init with this command line.
    pub cmdline: Option<String>,
    /// Screen size, as `(width, height)`.
    pub framebuffer: Option<(u16, u16)>,
    /// Folder holding virglrenderer, which turns on the GPU.
    pub gpu: Option<String>,
    /// Lets the guest give unused RAM back.
    pub balloon: bool,
    /// Uses the hypervisor's local APIC.
    pub apic: bool,
    /// File to save the screen to.
    pub screenshot: Option<String>,
    /// File of timed input for repeatable tests.
    pub input_script: Option<String>,
    /// Give the guest a network card.
    pub network: bool,
    /// File of timed touches. Also adds the touchscreen.
    pub touch_script: Option<String>,
}

/// Parses arguments, excluding the program name.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] => Ok(Command::Launch),
        ["help" | "-h" | "--help"] => Ok(Command::Help),
        ["gpu", "probe", folder] => Ok(Command::GpuProbe {
            folder: (*folder).to_owned(),
        }),
        ["ui", "preview", out] => Ok(Command::UiPreview {
            out: (*out).to_owned(),
        }),
        ["version" | "-V" | "--version"] => Ok(Command::Version),
        ["doctor"] => Ok(Command::Doctor { json: false }),
        ["doctor", "--json"] => Ok(Command::Doctor { json: true }),
        ["bench", "whp", options @ ..] => parse_bench(options),
        ["vm", "run", options @ ..] => parse_vm_run(options),
        ["input", "check", profile] => Ok(Command::InputCheck {
            profile: (*profile).to_owned(),
        }),
        [other, ..] => Err(format!("unknown command or option: {other}")),
    }
}

fn parse_bench(options: &[&str]) -> Result<Command, String> {
    let mut iterations = DEFAULT_ITERATIONS;
    let mut map_mib = DEFAULT_MAP_MIB;
    let mut out = None;
    let mut rest = options.iter();
    while let Some(option) = rest.next() {
        let value = rest
            .next()
            .ok_or_else(|| format!("{option} needs a value"))?;
        match *option {
            "--iterations" => iterations = positive(option, value)?,
            "--map-mib" => map_mib = positive(option, value)?,
            "--out" => out = Some((*value).to_owned()),
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(Command::BenchWhp {
        iterations,
        map_mib,
        out,
    })
}

fn parse_vm_run(options: &[&str]) -> Result<Command, String> {
    let mut kernel = None;
    let mut initrd = None;
    let mut ram_mib = DEFAULT_VM_RAM_MIB;
    let mut timeout_secs = DEFAULT_VM_TIMEOUT_SECS;
    let mut shell = false;
    let mut report = None;
    let mut disks = Vec::new();
    let mut cmdline = None;
    let mut framebuffer = None;
    let mut gpu = None;
    let mut balloon = false;
    let mut apic = false;
    let mut screenshot = None;
    let mut input_script = None;
    let mut network = false;
    let mut touch_script = None;
    let mut rest = options.iter();
    while let Some(option) = rest.next() {
        if *option == "--shell" {
            shell = true;
            continue;
        }
        if *option == "--apic" {
            apic = true;
            continue;
        }
        if *option == "--balloon" {
            balloon = true;
            continue;
        }
        if *option == "--net" {
            network = true;
            continue;
        }
        let value = rest
            .next()
            .ok_or_else(|| format!("{option} needs a value"))?;
        match *option {
            "--kernel" => kernel = Some((*value).to_owned()),
            "--initrd" => initrd = Some((*value).to_owned()),
            "--ram-mib" => ram_mib = positive(option, value)?,
            "--timeout-secs" => timeout_secs = positive(option, value)?,
            "--report" => report = Some((*value).to_owned()),
            "--disk" => disks.push((*value).to_owned()),
            "--cmdline" => cmdline = Some((*value).to_owned()),
            "--framebuffer" => framebuffer = Some(screen_size(value)?),
            "--gpu" => gpu = Some((*value).to_owned()),
            "--screenshot" => screenshot = Some((*value).to_owned()),
            "--input-script" => input_script = Some((*value).to_owned()),
            "--touch-script" => touch_script = Some((*value).to_owned()),
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(Command::VmRun(Box::new(VmRun {
        kernel: kernel.ok_or("--kernel is required")?,
        initrd: initrd.ok_or("--initrd is required")?,
        ram_mib,
        timeout_secs,
        shell,
        report,
        disks,
        cmdline,
        framebuffer,
        gpu,
        balloon,
        apic,
        screenshot,
        input_script,
        network,
        touch_script,
    })))
}

fn screen_size(value: &str) -> Result<(u16, u16), String> {
    value
        .split_once('x')
        .and_then(|(width, height)| Some((width.parse().ok()?, height.parse().ok()?)))
        .filter(|&(width, height)| width > 0 && height > 0)
        .ok_or_else(|| format!("--framebuffer needs a size like 1280x720, got {value}"))
}

fn positive(option: &str, value: &str) -> Result<usize, String> {
    match value.parse() {
        Ok(0) | Err(_) => Err(format!("{option} needs a positive integer, got {value}")),
        Ok(number) => Ok(number),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Command, String> {
        let owned: Vec<String> = args.iter().map(ToString::to_string).collect();
        parse(&owned)
    }

    #[test]
    fn no_arguments_opens_the_launcher() {
        assert_eq!(run(&[]), Ok(Command::Launch));
        assert_eq!(run(&["--help"]), Ok(Command::Help));
    }

    #[test]
    fn parses_doctor_variants() {
        assert_eq!(run(&["doctor"]), Ok(Command::Doctor { json: false }));
        assert_eq!(
            run(&["doctor", "--json"]),
            Ok(Command::Doctor { json: true })
        );
    }

    #[test]
    fn bench_uses_defaults() {
        assert_eq!(
            run(&["bench", "whp"]),
            Ok(Command::BenchWhp {
                iterations: DEFAULT_ITERATIONS,
                map_mib: DEFAULT_MAP_MIB,
                out: None
            })
        );
    }

    #[test]
    fn bench_accepts_options_in_any_order() {
        assert_eq!(
            run(&[
                "bench",
                "whp",
                "--out",
                "r.json",
                "--iterations",
                "5",
                "--map-mib",
                "7"
            ]),
            Ok(Command::BenchWhp {
                iterations: 5,
                map_mib: 7,
                out: Some("r.json".to_owned())
            })
        );
    }

    #[test]
    fn vm_run_needs_a_kernel_and_initrd() {
        assert!(run(&["vm", "run"]).is_err());
        assert!(run(&["vm", "run", "--kernel", "k"]).is_err());
    }

    #[test]
    fn vm_run_parses_flags_and_values() {
        assert_eq!(
            run(&[
                "vm",
                "run",
                "--shell",
                "--kernel",
                "k",
                "--initrd",
                "i",
                "--ram-mib",
                "256"
            ]),
            Ok(Command::VmRun(Box::new(VmRun {
                kernel: "k".to_owned(),
                initrd: "i".to_owned(),
                ram_mib: 256,
                timeout_secs: DEFAULT_VM_TIMEOUT_SECS,
                shell: true,
                report: None,
                disks: Vec::new(),
                cmdline: None,
                framebuffer: None,
                gpu: None,
                balloon: false,
                apic: false,
                screenshot: None,
                input_script: None,
                network: false,
                touch_script: None,
            })))
        );
    }

    #[test]
    fn input_check_takes_a_path() {
        assert_eq!(
            run(&["input", "check", "p.json"]),
            Ok(Command::InputCheck {
                profile: "p.json".to_owned()
            })
        );
        assert!(run(&["input", "check"]).is_err());
    }

    #[test]
    fn screen_sizes_parse() {
        assert_eq!(screen_size("1280x720"), Ok((1280, 720)));
        for bad in ["1280", "0x720", "ax720", "70000x1", "1280x"] {
            assert!(screen_size(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_bad_input() {
        assert!(run(&["bogus"]).is_err());
        assert!(run(&["doctor", "--nope"]).is_err());
        assert!(run(&["bench", "whp", "--iterations"]).is_err());
        assert!(run(&["bench", "whp", "--iterations", "0"]).is_err());
        assert!(run(&["bench", "whp", "--iterations", "-1"]).is_err());
        assert!(run(&["bench", "whp", "--unknown", "1"]).is_err());
    }
}
