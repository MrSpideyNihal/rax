//! Command-line parsing.

const DEFAULT_ITERATIONS: usize = 200_000;
const DEFAULT_MAP_MIB: usize = 1024;

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
    /// Print usage.
    Help,
    /// Print the version.
    Version,
}

/// Parses arguments, excluding the program name.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] | ["help" | "-h" | "--help"] => Ok(Command::Help),
        ["version" | "-V" | "--version"] => Ok(Command::Version),
        ["doctor"] => Ok(Command::Doctor { json: false }),
        ["doctor", "--json"] => Ok(Command::Doctor { json: true }),
        ["bench", "whp", options @ ..] => parse_bench(options),
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
    fn no_arguments_prints_help() {
        assert_eq!(run(&[]), Ok(Command::Help));
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
    fn rejects_bad_input() {
        assert!(run(&["bogus"]).is_err());
        assert!(run(&["doctor", "--nope"]).is_err());
        assert!(run(&["bench", "whp", "--iterations"]).is_err());
        assert!(run(&["bench", "whp", "--iterations", "0"]).is_err());
        assert!(run(&["bench", "whp", "--iterations", "-1"]).is_err());
        assert!(run(&["bench", "whp", "--unknown", "1"]).is_err());
    }
}
