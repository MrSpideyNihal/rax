//! Installing an APK dropped on the window.
//!
//! The file reaches Android through a spare virtual disk, and a command typed
//! into Android's console installs it, so no network or adb is involved.

/// Largest APK the inbox disk holds.
pub const INBOX_BYTES: u64 = 1024 * 1024 * 1024;
const BLOCK: u64 = 4096;
const TEMP_PATH: &str = "/data/local/tmp/rax-install.apk";

/// Printed by Android when the install command has finished.
pub const DONE_MARKER: &str = "RAX_INSTALL_DONE";

/// What the install produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallResult {
    /// `pm` reported success.
    Installed,
    /// `pm` reported a reason, such as a missing native library.
    Failed(String),
}

/// Whether `name` looks like an APK.
#[must_use]
pub fn is_apk(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".apk")
}

/// The console line that installs `size` bytes from the inbox disk.
///
/// The cache is dropped first, because the guest may hold stale blocks from
/// an earlier install. The file is then trimmed to its exact size.
#[must_use]
pub fn install_command(size: u64) -> String {
    let blocks = size.div_ceil(BLOCK);
    format!(
        "echo 3 > /proc/sys/vm/drop_caches; \
         dd if=/dev/block/vdc of={TEMP_PATH} bs={BLOCK} count={blocks} 2>/dev/null; \
         truncate -s {size} {TEMP_PATH}; \
         pm install -r {TEMP_PATH}; \
         rm {TEMP_PATH}; \
         echo {DONE_MARKER}_$?\n"
    )
}

/// Reads the result from console output, once the marker has printed.
///
/// The marker is matched with its trailing `_`, and the command line echoed
/// back by the shell is skipped, because it contains the same text.
#[must_use]
pub fn parse_result(console: &str) -> Option<InstallResult> {
    let finished = console
        .lines()
        .any(|line| line.trim().starts_with(&format!("{DONE_MARKER}_")));
    if !finished {
        return None;
    }
    let outcome = console.lines().rev().find_map(|line| {
        let line = line.trim();
        if line == "Success" {
            Some(InstallResult::Installed)
        } else {
            line.strip_prefix("Failure")
                .map(|reason| InstallResult::Failed(reason.trim().to_owned()))
        }
    });
    Some(outcome.unwrap_or_else(|| InstallResult::Failed("no result from Android".to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apk_names_match_in_any_case() {
        assert!(is_apk("Game.APK"));
        assert!(is_apk("a.apk"));
        assert!(!is_apk("a.apk.exe"));
        assert!(!is_apk("apk"));
    }

    #[test]
    fn the_command_rounds_blocks_up_and_trims_exactly() {
        let command = install_command(4097);
        assert!(command.contains("count=2"));
        assert!(command.contains("truncate -s 4097"));
        assert!(command.ends_with('\n'));
    }

    #[test]
    fn nothing_is_reported_before_the_marker() {
        assert_eq!(parse_result("Performing Streamed Install\n"), None);
    }

    #[test]
    fn the_echoed_command_does_not_count_as_finished() {
        let echoed = format!("console:/ # {}", install_command(10));
        assert_eq!(parse_result(&echoed), None);
    }

    #[test]
    fn success_is_recognised() {
        let output = format!("Success\n{DONE_MARKER}_0\n");
        assert_eq!(parse_result(&output), Some(InstallResult::Installed));
    }

    #[test]
    fn failure_keeps_the_reason() {
        let output = format!("Failure [INSTALL_FAILED_NO_MATCHING_ABIS]\n{DONE_MARKER}_1\n");
        assert_eq!(
            parse_result(&output),
            Some(InstallResult::Failed(
                "[INSTALL_FAILED_NO_MATCHING_ABIS]".to_owned()
            ))
        );
    }

    #[test]
    fn the_latest_result_wins() {
        let output = format!("Failure [OLD]\nSuccess\n{DONE_MARKER}_0\n");
        assert_eq!(parse_result(&output), Some(InstallResult::Installed));
    }

    #[test]
    fn a_silent_finish_is_a_failure() {
        let output = format!("{DONE_MARKER}_0\n");
        assert!(matches!(
            parse_result(&output),
            Some(InstallResult::Failed(_))
        ));
    }
}
