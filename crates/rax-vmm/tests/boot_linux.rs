//! Boots real Linux. Ignored by default because it needs the hypervisor and
//! the guest files from `scripts/fetch-guest.ps1`. Run with:
//!
//! ```text
//! cargo test --release -p rax-vmm --test boot_linux -- --ignored
//! ```

#![cfg(windows)]
#![allow(missing_docs)]

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rax_vmm::machine::{self, Config, Outcome};

/// Two partitions mapping memory at once in one process fail with a
/// hypervisor "operation denied" status, so these tests take turns.
static ONE_VM_AT_A_TIME: Mutex<()> = Mutex::new(());

fn take_turn() -> MutexGuard<'static, ()> {
    ONE_VM_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
    let _turn = take_turn();
    let kernel = guest_file("vmlinuz-virt");
    let initrd = guest_file("initramfs-virt");
    for attempt in 1..=10 {
        let report = machine::run(Config {
            kernel: kernel.clone(),
            initrd: initrd.clone(),
            ram_mib: 512,
            timeout: Duration::from_secs(30),
            shell: false,
            disks: Vec::new(),
            init_script: None,
            own_init_cmdline: None,
            framebuffer: None,
            gpu: None,
            balloon: false,
            apic: false,
            cpus: 1,
            screenshot: None,
            network: None,
            touchscreen: false,
            display: None,
            input_script: Vec::new(),
            touch_script: Vec::new(),
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

const COPY_SECTOR_SCRIPT: &str = "#!/bin/busybox sh
/bin/busybox --install -s
export PATH=/bin:/sbin
mount -t proc proc /proc
mount -t devtmpfs dev /dev
modprobe virtio_pci
modprobe virtio_blk
dd if=/dev/vda of=/dev/vda bs=512 count=1 seek=1 conv=notrunc,fsync
poweroff -f
";

/// Proves the whole path: PCI discovery, the virtio ring, interrupts, and
/// both directions of data. The guest copies sector 0 over sector 1, so the
/// host only sees the pattern there if a read and a write both worked.
#[test]
#[ignore = "requires Windows Hypervisor Platform and guest files"]
fn guest_reads_and_writes_the_disk() {
    let _turn = take_turn();
    let path = std::env::temp_dir().join(format!("rax-disk-test-{}.img", std::process::id()));
    let mut image = vec![0_u8; 1024 * 1024];
    image[..512].fill(0x5a);
    std::fs::write(&path, &image).expect("disk image");

    let report = machine::run(Config {
        kernel: guest_file("vmlinuz-virt"),
        initrd: guest_file("initramfs-virt"),
        ram_mib: 512,
        timeout: Duration::from_secs(30),
        shell: false,
        disks: vec![path.clone()],
        init_script: Some(COPY_SECTOR_SCRIPT.to_owned()),
        own_init_cmdline: None,
        framebuffer: None,
        gpu: None,
        balloon: false,
        apic: false,
        cpus: 1,
        screenshot: None,
        network: None,
        touchscreen: false,
        display: None,
        input_script: Vec::new(),
        touch_script: Vec::new(),
    })
    .expect("guest runs");

    let after = std::fs::read(&path).expect("disk image");
    std::fs::remove_file(&path).expect("cleanup");
    assert_eq!(report.outcome, Outcome::PoweredOff);
    assert!(report.io.disk_requests > 0, "guest never used the disk");
    assert!(after[512..1024].iter().all(|&byte| byte == 0x5a));
    assert!(after[1024..].iter().all(|&byte| byte == 0));
}
