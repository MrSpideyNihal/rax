//! Runs real guests. Ignored by default because hosted CI runners do not
//! expose the hypervisor. Run with `cargo test -- --ignored` on a machine
//! that has "Windows Hypervisor Platform" enabled.

#![cfg(windows)]
#![allow(missing_docs)]

use rax_host::whp::{Api, measure_exit_latency, measure_map_cost};

#[test]
#[ignore = "requires Windows Hypervisor Platform"]
fn guest_executes_and_exits_to_the_host() {
    let api = Api::load().expect("WinHvPlatform.dll");
    assert!(api.capabilities().expect("capabilities").hypervisor_present);
    let result = measure_exit_latency(&api, 1_000).expect("guest runs");
    assert_eq!(result.run_to_exit.count, 1_000);
    assert!(result.run_to_exit.min_ns > 0);
}

#[test]
#[ignore = "requires Windows Hypervisor Platform"]
fn maps_and_unmaps_guest_memory() {
    let api = Api::load().expect("WinHvPlatform.dll");
    let result = measure_map_cost(&api, 16).expect("mapping works");
    assert!(result.map_us > 0.0);
}
