//! The Intel MultiProcessor table.
//!
//! RAX boots Linux with ACPI off, so this is how the guest learns about its
//! CPUs and interrupt controller. Linux scans for it just below 1 MiB.

use crate::ioapic::{IOAPIC_GPA, PINS};

/// Where the table goes, inside the area the memory map keeps reserved.
pub const MP_GPA: u64 = 0xf_0000;

const LOCAL_APIC_GPA: u32 = 0xfee0_0000;
const SPEC_REVISION: u8 = 4;
const APIC_VERSION: u8 = 0x14;
const IOAPIC_VERSION: u8 = 0x11;
const FLOATING_BYTES: u32 = 16;
const CONFIG_HEADER_BYTES: usize = 44;

const ENTRY_PROCESSOR: u8 = 0;
const ENTRY_BUS: u8 = 1;
const ENTRY_IOAPIC: u8 = 2;
const ENTRY_INTERRUPT: u8 = 3;
const ENTRY_LOCAL_INTERRUPT: u8 = 4;

const CPU_ENABLED: u8 = 1;
const CPU_BOOT: u8 = 2;
const INTERRUPT_NORMAL: u8 = 0;
const INTERRUPT_NMI: u8 = 1;
const INTERRUPT_EXTERNAL: u8 = 3;
const ISA_BUS: u8 = 0;
/// The conventional routing of the timer, past the 8259's cascade line.
const TIMER_IRQ: u8 = 0;
const TIMER_PIN: u8 = 2;
const CASCADE_IRQ: u8 = 2;
const ISA_IRQS: u8 = 16;

fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0_u8, |sum, byte| sum.wrapping_sub(*byte))
}

fn interrupt(kind: u8, irq: u8, apic: u8, pin: u8) -> [u8; 8] {
    [ENTRY_INTERRUPT, kind, 0, 0, ISA_BUS, irq, apic, pin]
}

fn entries(cpus: u8) -> (Vec<u8>, u16) {
    let ioapic_id = cpus;
    let mut bytes = Vec::new();
    let mut count = 0_u16;
    for cpu in 0..cpus {
        let flags = CPU_ENABLED | if cpu == 0 { CPU_BOOT } else { 0 };
        bytes.extend_from_slice(&[ENTRY_PROCESSOR, cpu, APIC_VERSION, flags]);
        bytes.extend_from_slice(&[0; 16]);
        count += 1;
    }
    bytes.extend_from_slice(&[ENTRY_BUS, ISA_BUS]);
    bytes.extend_from_slice(b"ISA   ");
    count += 1;
    bytes.extend_from_slice(&[ENTRY_IOAPIC, ioapic_id, IOAPIC_VERSION, 1]);
    bytes.extend_from_slice(&(IOAPIC_GPA as u32).to_le_bytes());
    count += 1;
    for irq in (0..ISA_IRQS).filter(|irq| *irq != CASCADE_IRQ) {
        let pin = if irq == TIMER_IRQ { TIMER_PIN } else { irq };
        debug_assert!(usize::from(pin) < PINS);
        bytes.extend_from_slice(&interrupt(INTERRUPT_NORMAL, irq, ioapic_id, pin));
        count += 1;
    }
    let local = |kind, pin| [ENTRY_LOCAL_INTERRUPT, kind, 0, 0, ISA_BUS, 0, 0xff, pin];
    bytes.extend_from_slice(&local(INTERRUPT_EXTERNAL, 0));
    bytes.extend_from_slice(&local(INTERRUPT_NMI, 1));
    count += 2;
    (bytes, count)
}

/// Builds the floating pointer and configuration table for `cpus` CPUs, to be
/// written at [`MP_GPA`]. The I/O APIC takes the APIC ID after the last CPU.
#[must_use]
pub fn build(cpus: u8) -> Vec<u8> {
    let (body, count) = entries(cpus);
    let config_gpa = MP_GPA as u32 + FLOATING_BYTES;

    let mut floating = Vec::with_capacity(FLOATING_BYTES as usize);
    floating.extend_from_slice(b"_MP_");
    floating.extend_from_slice(&config_gpa.to_le_bytes());
    floating.extend_from_slice(&[1, SPEC_REVISION, 0, 0, 0, 0, 0, 0]);
    floating[10] = checksum(&floating);

    let mut config = Vec::with_capacity(CONFIG_HEADER_BYTES + body.len());
    config.extend_from_slice(b"PCMP");
    config.extend_from_slice(&((CONFIG_HEADER_BYTES + body.len()) as u16).to_le_bytes());
    config.extend_from_slice(&[SPEC_REVISION, 0]);
    config.extend_from_slice(b"RAX     ");
    config.extend_from_slice(b"RAX VM      ");
    config.extend_from_slice(&[0; 6]);
    config.extend_from_slice(&count.to_le_bytes());
    config.extend_from_slice(&LOCAL_APIC_GPA.to_le_bytes());
    config.extend_from_slice(&[0; 4]);
    config.extend_from_slice(&body);
    config[7] = checksum(&config);

    floating.extend_from_slice(&config);
    floating
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sums_to_zero(bytes: &[u8]) -> bool {
        bytes.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)) == 0
    }

    fn config(table: &[u8]) -> &[u8] {
        &table[FLOATING_BYTES as usize..]
    }

    /// Walks the entries, returning `(type, bytes)` for each.
    fn walk(table: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let config = config(table);
        let mut at = CONFIG_HEADER_BYTES;
        let mut found = Vec::new();
        while at < config.len() {
            let length = if config[at] == ENTRY_PROCESSOR { 20 } else { 8 };
            found.push((config[at], config[at..at + length].to_vec()));
            at += length;
        }
        found
    }

    #[test]
    fn the_floating_pointer_is_valid_and_finds_the_table() {
        let table = build(1);
        assert_eq!(&table[..4], b"_MP_");
        assert!(sums_to_zero(&table[..16]));
        let target = u32::from_le_bytes([table[4], table[5], table[6], table[7]]);
        assert_eq!(target, MP_GPA as u32 + 16);
        assert_eq!(table[8], 1, "length in 16-byte units");
    }

    #[test]
    fn the_config_table_is_valid() {
        let table = build(4);
        let config = config(&table);
        assert_eq!(&config[..4], b"PCMP");
        assert!(sums_to_zero(config));
        assert_eq!(
            u16::from_le_bytes([config[4], config[5]]) as usize,
            config.len()
        );
        assert_eq!(config.len(), table.len() - 16);
    }

    #[test]
    fn the_header_is_44_bytes_with_the_count_in_place() {
        let table = build(2);
        let config = config(&table);
        let count = u16::from_le_bytes([config[34], config[35]]) as usize;
        assert_eq!(walk(&table).len(), count);
        assert_eq!(
            u32::from_le_bytes([config[36], config[37], config[38], config[39]]),
            LOCAL_APIC_GPA
        );
    }

    #[test]
    fn only_the_first_cpu_is_the_boot_cpu() {
        let table = build(3);
        let cpus: Vec<_> = walk(&table)
            .into_iter()
            .filter(|entry| entry.0 == ENTRY_PROCESSOR)
            .collect();
        assert_eq!(cpus.len(), 3);
        assert_eq!(cpus[0].1[3], CPU_ENABLED | CPU_BOOT);
        assert_eq!(cpus[1].1[3], CPU_ENABLED);
        assert_eq!(cpus[2].1[1], 2, "APIC IDs count up");
    }

    #[test]
    fn the_ioapic_takes_the_id_after_the_cpus() {
        let table = build(3);
        let (_, entry) = walk(&table)
            .into_iter()
            .find(|entry| entry.0 == ENTRY_IOAPIC)
            .unwrap();
        assert_eq!(entry[1], 3);
        assert_eq!(
            u64::from(u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]])),
            IOAPIC_GPA
        );
    }

    #[test]
    fn the_timer_is_routed_past_the_cascade_line() {
        let table = build(1);
        let routes: Vec<(u8, u8)> = walk(&table)
            .into_iter()
            .filter(|entry| entry.0 == ENTRY_INTERRUPT)
            .map(|(_, entry)| (entry[5], entry[7]))
            .collect();
        assert!(routes.contains(&(0, 2)));
        assert!(routes.contains(&(5, 5)));
        assert!(routes.contains(&(12, 12)));
        assert!(!routes.iter().any(|route| route.0 == 2));
    }
}
