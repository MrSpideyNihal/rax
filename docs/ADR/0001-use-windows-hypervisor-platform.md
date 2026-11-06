# ADR-0001: Use the Windows Hypervisor Platform

Status: Accepted, 2026-11-06

## Problem

RAX needs hardware virtualization on consumer Windows 11, where VBS is commonly enabled and owns the hypervisor.

## Options

| Option | Coexists with VBS | Status | Assessment |
|---|---|---|---|
| WHP | Yes | Microsoft API, recommended by Google for the Android Emulator | Works on target hardware |
| AEHD | No, Hyper-V features must be off | Sunset 31 December 2026 | Dead end |
| HAXM | No | Removed from emulator 36.2 onwards | Dead end |
| Custom kernel driver | No | Requires signing, disables VBS | Worse security and more work |

## Decision

Use WHP as the only hypervisor interface.

## Evidence

Measured on the target laptop, Windows 11 Home with VBS on: guest executes, exits return in about 3 µs at the median. See [BENCHMARKS.md](../BENCHMARKS.md).

## Consequences

- Users may have to enable the "Windows Hypervisor Platform" feature once, which `rax doctor` explains.
- Performance depends on Microsoft's hypervisor, which RAX cannot tune.
- RAX cannot run inside another VM without nested virtualization.
