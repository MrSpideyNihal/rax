# ADR-0006: Write RAX's own VMM in Rust

Status: Accepted, 2026-11-06. Supersedes [ADR-0002](0002-vmm-strategy.md).

## Problem

ADR-0002 planned to run crosvm as a separate process, and listed a custom VMM as rejected. The project owner then set a new requirement: the RAX repository alone should be enough, with no outside VMM to obtain, build, and supervise.

crosvm also carried a real cost on this hardware. It needs a pinned older Rust toolchain and its setup path is built around Visual Studio, while upstream does not test WHPX.

## Options

| Option | Self-contained | Windows risk | Cost |
|---|---|---|---|
| crosvm as a child process | No | Untested upstream | Heavy toolchain, external patches |
| QEMU as a child process | No | Moderate | GPL process to ship, larger footprint |
| Own Rust VMM on WHP | Yes | Ours to debug | Every device is ours to write |

## Decision

Write the VMM in this repository as `crates/rax-vmm`, on the WHP layer in `rax-host`. Reuse ideas and documented interfaces, not whole external processes.

## Evidence

Measured on the target laptop on the day of the decision: Linux 6.18 reaches user space in about 0.77 s and an interactive shell works, with one vCPU, using about 2,000 lines of device and boot code and the GNU Rust toolchain only. See [BENCHMARKS.md](../BENCHMARKS.md).

## What this does not solve

Being honest about the limits of "the repo alone":

- **The guest OS is still downloaded.** Linux and Android are far too large for a Git repository. `scripts/fetch-guest.ps1` fetches them and verifies pinned hashes.
- **It still lacks network, input, and multiple vCPUs.** Disk and a basic display were added later, and Android 9 boots.
- **Graphics remains the hard part.** A VMM alone cannot replace gfxstream or an equivalent GPU forwarding stack. Whether that can be linked into RAX, and with what toolchain, is open. Rewriting it is not realistic.
- **Bugs are ours.** The first boots exposed a wrong register number written from memory, which upstream projects would have spared us.

## Consequences

- No Visual Studio or crosvm build is needed so far.
- All `unsafe` remains in `rax-host`, and `rax-vmm` forbids it.
- The boot loader and device logic are testable without a hypervisor.
- The brief's instruction not to rewrite an entire emulator still stands. RAX writes the small monitor, and keeps reusing Android, Linux, and GPU forwarding.
