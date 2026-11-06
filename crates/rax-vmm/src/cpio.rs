//! Writes `newc` cpio archives.
//!
//! The kernel accepts several archives concatenated into one initramfs, and
//! later files replace earlier ones. RAX uses that to add its own init script
//! to a stock initramfs without unpacking it.

const MODE_EXECUTABLE: u32 = 0o100_755;
const TRAILER: &str = "TRAILER!!!";

fn pad_to_four(archive: &mut Vec<u8>) {
    archive.resize(archive.len().next_multiple_of(4), 0);
}

fn push_entry(archive: &mut Vec<u8>, inode: u32, name: &str, mode: u32, data: &[u8]) {
    let name_size = name.len() + 1;
    let header = format!(
        "070701{inode:08X}{mode:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{name_size:08X}{:08X}",
        0,
        0,
        1,
        0,
        data.len(),
        0,
        0,
        0,
        0,
        0
    );
    archive.extend_from_slice(header.as_bytes());
    archive.extend_from_slice(name.as_bytes());
    archive.push(0);
    pad_to_four(archive);
    archive.extend_from_slice(data);
    pad_to_four(archive);
}

/// Builds an archive with one executable file at `path`.
#[must_use]
pub fn single_executable(path: &str, data: &[u8]) -> Vec<u8> {
    let mut archive = Vec::with_capacity(data.len() + 512);
    push_entry(&mut archive, 1, path, MODE_EXECUTABLE, data);
    push_entry(&mut archive, 2, TRAILER, 0, &[]);
    archive
}

/// Appends `extra` to `initrd`, keeping the four-byte alignment the kernel
/// requires between concatenated archives.
pub fn concatenate(initrd: &mut Vec<u8>, extra: &[u8]) {
    pad_to_four(initrd);
    initrd.extend_from_slice(extra);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(entry: &[u8], index: usize) -> u32 {
        let start = 6 + index * 8;
        let text = std::str::from_utf8(&entry[start..start + 8]).unwrap();
        u32::from_str_radix(text, 16).unwrap()
    }

    #[test]
    fn entry_header_has_the_documented_layout() {
        let archive = single_executable("/init", b"hello");
        assert_eq!(&archive[..6], b"070701");
        assert_eq!(field(&archive, 1), MODE_EXECUTABLE);
        assert_eq!(field(&archive, 4), 1, "nlink");
        assert_eq!(field(&archive, 6), 5, "file size");
        assert_eq!(field(&archive, 11), 6, "name size including NUL");
    }

    #[test]
    fn name_and_data_are_padded_to_four_bytes() {
        let archive = single_executable("/init", b"hello");
        // 110 byte header + 6 byte name = 116, already aligned.
        assert_eq!(&archive[110..116], b"/init\0");
        assert_eq!(&archive[116..121], b"hello");
        assert_eq!(&archive[124..130], b"070701");
    }

    #[test]
    fn archive_ends_with_a_trailer_and_stays_aligned() {
        let archive = single_executable("/init", b"x");
        assert_eq!(archive.len() % 4, 0);
        assert!(
            archive
                .windows(TRAILER.len())
                .any(|window| window == TRAILER.as_bytes())
        );
    }

    #[test]
    fn concatenation_pads_the_first_archive() {
        let mut initrd = vec![1, 2, 3, 4, 5];
        concatenate(&mut initrd, b"abcd");
        assert_eq!(initrd.len(), 12);
        assert_eq!(&initrd[8..], b"abcd");
    }
}
