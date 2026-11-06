# RAX

[![CI](https://github.com/MrSpideyNihal/rax/actions/workflows/ci.yml/badge.svg)](https://github.com/MrSpideyNihal/rax/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**A research project to build the lightest practical Android gaming runtime for Windows, in Rust.**

> **Status: Linux boots to a shell on RAX's own VM. RAX does not run Android or Minecraft yet.** No FPS, game RAM, or thermal figure exists, and none will be claimed until real benchmarks do.

## What exists

| Command | What it does |
|---|---|
| `rax doctor` | Checks WHP, RAM, CPU, and GPUs, and explains blockers |
| `rax bench whp` | Runs a tiny guest under the Windows Hypervisor Platform and measures VM-exit and mapping costs |
| `rax vm run` | Boots Linux on RAX's own Rust VMM, with a serial console or shell, and optionally a virtual disk |

Results on the target laptop (Ryzen 7 7445HS, Windows 11 Home, VBS on):

- A guest exit costs a **median of 3.0 µs**, with a worst case of 340 µs.
- A tiny Linux 6.18 guest reaches user space in **774 ms on average**, over 10 boots.

A tiny BusyBox guest says little about Android. The baseline to beat is Android Studio's emulator running Android 14 with Google Play: **38 s** to boot, **3.6 GB** of host RAM, and **56% of one core** at idle. RAX cannot be compared with that until it runs Android. [Raw data, method and limits](docs/BENCHMARKS.md).

## What the research found

Read the [feasibility report](docs/FEASIBILITY.md). The short version:

- **The runtime looks buildable.** WHP works, and exit cost is not the bottleneck. Android and GPU forwarding are the hard parts still ahead.
- **Google Play is the real obstacle.** It needs a licence from Google, and engineering cannot replace that. RAX will not bundle, spoof or bypass it ([ADR-0005](docs/ADR/0005-google-play-policy.md)).
- **ARM translation may be avoidable for Minecraft,** which lists an x86_64 Android build, though that is untested here. **BGMI is listed as ARM-only, so it is blocked** until a legal translation path exists.
- **Free Fire and BGMI are goals, not promises.** RAX has no macros and will not hide from anti-cheat ([ADR-0007](docs/ADR/0007-game-goals-and-input.md)).
- **There is no GPU passthrough.** The plan is API forwarding with gfxstream, and we will call it that.

## Quick start

Requires Windows on x86-64 (only Windows 11 has been tested), [Rust](https://rustup.rs), and the "Windows Hypervisor Platform" feature for the benchmark.

```powershell
git clone https://github.com/MrSpideyNihal/rax
cd rax
cargo run --release -p rax-cli -- doctor
cargo run --release -p rax-cli -- bench whp
```

To boot Linux, download the verified test guest first. It is too large to keep in Git:

```powershell
./scripts/fetch-guest.ps1
cargo run --release -p rax-cli -- vm run --shell --kernel guest/vmlinuz-virt --initrd guest/initramfs-virt
```

## Documentation

| Document | Contents |
|---|---|
| [Feasibility report](docs/FEASIBILITY.md) | Phase 0 findings, risks, targets, next prototype |
| [Architecture](docs/ARCHITECTURE.md) | Target design |
| [Roadmap](docs/ROADMAP.md) | Phases, gates and fallbacks |
| [Benchmarks](docs/BENCHMARKS.md) | Measured data and how to reproduce it |
| [Compatibility](docs/COMPATIBILITY.md) | What has been tested |
| [Decisions](docs/ADR/README.md) | Architecture decision records |

## Project rules

- No benchmark without raw data and a reproduction command.
- No "supported" without a test.
- No fake GPU acceleration, and no silent software rendering.
- No DRM, licence or integrity bypasses. See [ADR-0005](docs/ADR/0005-google-play-policy.md).

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Licence and trademarks

RAX is licensed under [Apache-2.0](LICENSE).

RAX is an independent project, not affiliated with or endorsed by Google, Microsoft, Mojang, or NVIDIA. Android, Google Play, Minecraft, Windows, and other marks belong to their owners and are used only to describe compatibility goals.
