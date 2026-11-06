# Architecture

This describes the target design. **Only the parts marked "exists" are built.** Rationale and evidence are in [FEASIBILITY.md](FEASIBILITY.md) and the [ADRs](ADR/).

## Overview

```mermaid
flowchart TB
    subgraph host [Windows host]
        rax["rax.exe: orchestrator, Rust<br/>lifecycle, config, metrics, Raw Input"]
        vmm["rax-vmm: RAX's own VMM, Rust"]
        gfx["gfxstream host renderer"]
        driver["Vulkan driver, RTX 3050"]
        whp["WinHvPlatform.dll"]
        rax -->|supervises, IPC| vmm
        vmm --> gfx --> driver
        vmm --> whp
    end
    subgraph guest [Guest VM]
        game["Android app"]
        art["ART and framework"]
        gles["GLES / Vulkan guest libraries"]
        linux["Linux kernel, virtio drivers"]
        game --> art --> linux
        game --> gles --> linux
    end
    whp -.vCPU execution.- guest
    linux -.virtio rings, shared memory.- vmm
```

## Principles

1. **Measure before designing further.** Each layer lands with a benchmark.
2. **The repository is enough.** RAX's own VMM needs no outside emulator ([ADR-0006](ADR/0006-own-rust-vmm.md)). The guest OS is still downloaded, with verified hashes.
3. **Reuse before writing.** RAX owns a small monitor, orchestration, input, tuning and measurement. It does not own Android, Linux or a GPU translator.
4. **No hidden software rendering.** If hardware acceleration is not active, RAX says so and refuses to report gaming benchmarks.
5. **Nothing runs when idle.** No background service. Quitting RAX stops the VM.

## Components

| Component | State | Responsibility |
|---|---|---|
| `crates/rax-host` | **exists** | OS integration and all current `unsafe`: WHP, Vulkan probing, host facts, readiness rules, statistics |
| `crates/rax-vmm` | **exists** | Linux boot protocol, initramfs writer, serial, PIC, PIT, RTC, PCI and virtio-blk devices, one vCPU. Forbids `unsafe` |
| `crates/rax-cli` | **exists** | `rax doctor`, `rax bench whp`, `rax vm run`. Forbids `unsafe` |
| Multiple vCPUs, more virtio devices | planned, Phase 1b | Required before Android is usable |
| Input bridge | planned, Phase 3 | Raw Input to virtio-input |
| Launcher UI | planned, after the GPU pipeline | Small native window |
| Guest image recipe | test guest **exists**, Android planned | `scripts/fetch-guest.ps1` fetches a pinned Alpine Linux guest. AOSP comes in Phase 1b |

Crates are added when real code needs them. The layout proposed in the original brief (`rax-vm`, `rax-memory`, `rax-graphics` and so on) is the expected direction, not a set of empty placeholders.

## Key flows

### Graphics

Not built yet. Guest GLES or Vulkan calls are serialized by gfxstream guest libraries into shared-memory ring buffers and decoded by the host renderer. This is **API forwarding, not GPU passthrough**. The guest never touches the GPU directly.

The ring design matters because it limits VM exits. Exit cost is low (about 3 µs, measured), so the larger concerns are decode cost, synchronization, and the final copy path to the display on hybrid laptops.

### Input

Planned design, to be validated:

```
Raw Input (WM_INPUT) --> rax.exe --> virtio-input --> guest evdev
```

The hypothesis is that Android apps should see a **real mouse and keyboard**, not synthesized touch, because that removes a translation layer. Android supports pointer capture, and Minecraft on ChromeOS suggests keyboard and mouse handling exists in the game. This is unverified for Bedrock on Android. Touch mapping is the fallback only.

### Memory

Guest RAM is mapped from host-committed regions through `WHvMapGpaRange`. Reclaim is planned through virtio-balloon free page reporting, handing freed pages back to Windows. WHP behaviour under that churn has **no public data we could find** and is a measured item for Phase 5. Current numbers cover untouched mappings only.

### Lifecycle

A single process with no installed service. The vCPU runs on the main thread, one thread drives time and console input, and the VM ends with the process. Suspend and resume are investigated later.

### Storage

Disk requests run on the vCPU thread during the notifying `out` exit, so a slow host disk stalls the guest. Everything the guest supplies is untrusted: descriptor indexes, chain lengths, addresses, and sizes are all checked, and a malformed ring stops the queue rather than panicking. Moving I/O off the vCPU thread is future work, to be justified by measurement.

### Boot

RAX boots Linux directly, with no BIOS. It loads the `bzImage` payload, writes `boot_params` with an E820 memory map, adds an init script to the initramfs as a concatenated cpio archive, and enters the kernel's 32-bit entry point. The kernel switches itself to long mode.

Devices are a 16550A serial port, master 8259A PIC, 8254 PIT, CMOS RTC, and a PCI bus holding one legacy virtio block device. Legacy virtio was chosen because its registers are port I/O, which avoids needing an MMIO instruction decoder. Interrupts are injected through the hypervisor's pending-interruption register, and an interrupt-window exit is requested when the guest cannot accept one yet. Reads of absent hardware map a read-only page of `0xFF`, matching what an unclaimed bus returns, so no instruction decoder is needed so far.

## Hypervisor

WHP is the only supported path. It works with VBS, which is on by default on many Windows 11 machines, and it works on Home editions, confirmed on the target laptop. Google is ending the alternative AEHD driver on 31 December 2026, and HAXM is gone from the emulator. See [ADR-0001](ADR/0001-use-windows-hypervisor-platform.md).

## Security

- System DLLs load from `System32` only, closing DLL search-order hijacking.
- `unsafe` lives in `rax-host`, with a `SAFETY` comment required on every block by lint.
- The guest is untrusted. Graphics decode in the VMM is the largest attack surface, and sandboxing it is a Phase 6 requirement.
- See [SECURITY.md](../SECURITY.md).
