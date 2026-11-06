//! Boots real Linux. Ignored by default because it needs the hypervisor and
//! the guest files from `scripts/fetch-guest.ps1`. Run with:
//!
//! ```text
//! cargo test --release -p rax-vmm --test boot_linux -- --ignored
//! ```

#![cfg(windows)]
#![allow(missing_docs)]

use std::path::Path;
use std::time::Duration;

use rax_vmm::machine::{self, Config, Outcome};

fn guest_file(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../guest")
        .join(name);
    std::fs::read(&path)
        .unwrap_or_else(|error| panic!("{}: {error}. Run scripts/fetch-guest.ps1", path.display()))
}

/// User-space output travels over the serial interrupt. If interrupt windows
/// stop working, output is lost at power off on about one boot in five, so a
/// single boot would not catch that regression.
#[test]
#[ignore = "requires Windows Hypervisor Platform and guest files"]
fn user_space_output_survives_repeated_boots() {
    let kernel = guest_file("vmlinuz-virt");
    let initrd = guest_file("initramfs-virt");
    for attempt in 1..=10 {
        let report = machine::run(Config {
            kernel: kernel.clone(),
            initrd: initrd.clone(),
            ram_mib: 512,
            timeout: Duration::from_secs(30),
            shell: false,
        })
        .expect("guest runs");
        assert_eq!(report.outcome, Outcome::PoweredOff, "attempt {attempt}");
        assert!(
            report.boot_to_user_space_ms.is_some(),
            "attempt {attempt} lost user-space output"
        );
        assert!(
            report.interrupt_window_exits > 0,
            "attempt {attempt} never saw an interrupt window"
        );
    }
}
