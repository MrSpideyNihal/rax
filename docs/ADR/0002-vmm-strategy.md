# ADR-0002: crosvm spike with QEMU fallback, behind a process boundary

Status: Superseded by [ADR-0006](0006-own-rust-vmm.md), 2026-11-06. crosvm was never built.

## Problem

RAX needs a VMM providing virtio devices and a GPU path. Writing one is out of scope, and the lightest candidate is the least proven on Windows.

## Options

| Option | Strength | Weakness |
|---|---|---|
| crosvm | Rust, BSD-3-Clause, small, supports `gfxstream` and WHPX features | WHPX is "Tested upstream: no". Windows device coverage undocumented |
| QEMU + WHPX | Mature, broad device model | GPL-2.0, larger, gfxstream support on Windows is young |
| Android Emulator | Proven end to end | Not redistributable as a binary, heavy, phone-oriented |
| Custom Rust VMM | Smallest possible | Rebuilds virtio and years of quirks, against the project brief |

## Decision

Try crosvm first, in a time-boxed spike. If it fails, use QEMU. The Android Emulator is a measurement control only.

RAX talks to the VMM only as a child process, so either can be swapped. This also keeps GPL code out of the RAX binary.

## Consequences

- Building crosvm needs MSVC, which is much heavier than the current toolchain.
- We may carry patches against an upstream that does not test our platform.
- Process isolation adds supervision work, and shared memory must cross that boundary.
