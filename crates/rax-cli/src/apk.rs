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

const END_RECORD: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
const CENTRAL_RECORD: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
const END_BYTES: usize = 22;
const CENTRAL_BYTES: usize = 46;

fn word16(bytes: &[u8], at: usize) -> Option<usize> {
    let raw = bytes.get(at..at + 2)?;
    Some(usize::from(u16::from_le_bytes([raw[0], raw[1]])))
}

fn word32(bytes: &[u8], at: usize) -> Option<usize> {
    let raw = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize)
}

/// Lists the file names in an APK, which is a zip file. `None` if it isn't
/// one that can be read. A file from the user is not trusted to be well
/// formed, so every offset is checked.
fn zip_names(bytes: &[u8]) -> Option<Vec<&str>> {
    let last = bytes.len().checked_sub(END_BYTES)?;
    // A zip comment can follow the end record, by at most 64 KiB.
    let end = (last.saturating_sub(0xffff)..=last)
        .rev()
        .find(|&at| bytes[at..].starts_with(&END_RECORD))?;
    let count = word16(bytes, end + 10)?;
    let mut at = word32(bytes, end + 16)?;
    let mut names = Vec::with_capacity(count.min(bytes.len() / CENTRAL_BYTES));
    for _ in 0..count {
        if !bytes.get(at..)?.starts_with(&CENTRAL_RECORD) {
            return None;
        }
        let name_bytes = word16(bytes, at + 28)?;
        let extra = word16(bytes, at + 30)?;
        let comment = word16(bytes, at + 32)?;
        let start = at + CENTRAL_BYTES;
        let name = bytes.get(start..start + name_bytes)?;
        names.push(std::str::from_utf8(name).ok()?);
        at = start + name_bytes + extra + comment;
    }
    Some(names)
}

/// Whether an APK has native code, none of it for x86. Android would install
/// such an app, and it would then crash as soon as it opens. An APK that can't
/// be read returns false, so that Android gets to judge it.
#[must_use]
pub fn is_arm_only(bytes: &[u8]) -> bool {
    let Some(names) = zip_names(bytes) else {
        return false;
    };
    let abis = || {
        names
            .iter()
            .filter(|name| name.ends_with(".so"))
            .filter_map(|name| name.strip_prefix("lib/")?.split('/').next())
    };
    abis().next().is_some() && !abis().any(|abi| abi.starts_with("x86"))
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

    /// Builds the central directory of a zip with these file names.
    fn zip(names: &[&str]) -> Vec<u8> {
        let mut bytes = b"local file data".to_vec();
        let start = bytes.len() as u32;
        for name in names {
            let mut record = vec![0_u8; CENTRAL_BYTES];
            record[..4].copy_from_slice(&CENTRAL_RECORD);
            record[28..30].copy_from_slice(&(name.len() as u16).to_le_bytes());
            bytes.extend_from_slice(&record);
            bytes.extend_from_slice(name.as_bytes());
        }
        let mut end = vec![0_u8; END_BYTES];
        end[..4].copy_from_slice(&END_RECORD);
        end[10..12].copy_from_slice(&(names.len() as u16).to_le_bytes());
        end[16..20].copy_from_slice(&start.to_le_bytes());
        bytes.extend_from_slice(&end);
        bytes
    }

    #[test]
    fn an_arm_only_game_is_recognised() {
        let apk = zip(&[
            "classes.dex",
            "lib/armeabi-v7a/libgame.so",
            "lib/arm64-v8a/libgame.so",
        ]);
        assert!(is_arm_only(&apk));
    }

    #[test]
    fn x86_code_beside_arm_code_is_fine() {
        let apk = zip(&["lib/arm64-v8a/libgame.so", "lib/x86_64/libgame.so"]);
        assert!(!is_arm_only(&apk));
        assert!(!is_arm_only(&zip(&["lib/x86/libgame.so"])));
    }

    #[test]
    fn an_app_with_no_native_code_is_fine() {
        assert!(!is_arm_only(&zip(&["classes.dex", "res/layout/main.xml"])));
    }

    #[test]
    fn files_that_are_not_libraries_do_not_count() {
        assert!(!is_arm_only(&zip(&[
            "lib/armeabi-v7a/readme.txt",
            "assets/lib/arm/x.so"
        ])));
    }

    #[test]
    fn a_zip_comment_does_not_hide_the_directory() {
        let mut apk = zip(&["lib/armeabi-v7a/libgame.so"]);
        apk.extend_from_slice(b"a comment");
        let at = apk.len() - 9 - END_BYTES + 20;
        apk[at..at + 2].copy_from_slice(&9_u16.to_le_bytes());
        assert!(is_arm_only(&apk));
    }

    #[test]
    fn broken_files_are_left_for_android_to_judge() {
        assert!(!is_arm_only(b""));
        assert!(!is_arm_only(b"not a zip file at all, just some words here"));
        let mut cut = zip(&["lib/armeabi-v7a/libgame.so"]);
        cut[15] = 0;
        assert!(!is_arm_only(&cut));
    }

    #[test]
    fn hostile_counts_and_offsets_do_not_panic() {
        let mut apk = zip(&["lib/armeabi-v7a/libgame.so"]);
        let end = apk.len() - END_BYTES;
        apk[end + 10..end + 12].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(!is_arm_only(&apk));
        apk[end + 16..end + 20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(!is_arm_only(&apk));
    }

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
