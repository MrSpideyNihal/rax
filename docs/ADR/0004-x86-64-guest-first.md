# ADR-0004: x86_64 guest with no ARM translation dependency

Status: Accepted, 2026-11-06

## Problem

Android games often ship ARM libraries. Host CPUs are x86-64. Translation is slow, and the good translators are proprietary.

## Options

| Option | Assessment |
|---|---|
| Native x86_64 libraries | Best performance. Bedrock lists an x86_64 Android version code |
| Houdini, ndk_translation | Proprietary, no right to redistribute |
| Berberis | Open, but riscv64 guests only |
| QEMU user mode | Legal under GPL, too slow for this purpose (estimate) |

## Decision

Build an x86_64 guest and rely on native x86_64 app libraries. Do not extract or redistribute proprietary translators.

## Consequences

- ARM-only games do not run. RAX is not a general-purpose emulator.
- The Minecraft plan depends on the x86_64 build being usable, which is unverified and contradicted by one community report.
