# Benchmarks

Rules:

- **No number appears here without raw data in the repository and a command to reproduce it.**
- Competitor comparisons use identical settings, scenes, and hardware, on the same day.
- Debug builds are never reported.
- Targets belong in [FEASIBILITY.md](FEASIBILITY.md), never in this file.

## Reproduce

```powershell
./scripts/run-benchmarks.ps1
```

This builds a release binary and writes numbered JSON files to a dated folder under `benchmarks/results`. Record the state of the machine with the results: mains or battery, power plan, and other running applications.

## Results

### WHP exit and mapping cost, 2026-11-06

| | |
|---|---|
| Raw data | [`benchmarks/results/2026-11-06-whp`](../benchmarks/results/2026-11-06-whp) |
| Command | `rax bench whp --iterations 200000 --map-mib 1024`, 5 passes |
| Build | Release, `x86_64-pc-windows-gnu`, rustc 1.99.1 |
| CPU | Ryzen 7 7445HS, 12 logical CPUs |
| OS | Windows 11 Home Single Language, build 26300, VBS on |
| Power | Mains, "Turbo" power plan |
| Clock resolution | 100 ns (`QueryPerformanceCounter`) |
| State | Normal desktop session, about 6.3 GiB RAM available. Not isolated |

Exit latency, per guest `out` instruction returning to user mode:

| Pass | p50 | p99 | p99.9 | max | full cycle, mean |
|---|---|---|---|---|---|
| 1 | 3.1 µs | 4.5 µs | 22.7 µs | 339.7 µs | 3.21 µs |
| 2 | 3.0 µs | 3.9 µs | 9.6 µs | 259.6 µs | 3.18 µs |
| 3 | 3.0 µs | 4.6 µs | 9.7 µs | 324.4 µs | 3.19 µs |
| 4 | 3.0 µs | 3.4 µs | 9.6 µs | 310.9 µs | 3.16 µs |
| 5 | 3.0 µs | 3.6 µs | 8.6 µs | 122.6 µs | 3.14 µs |

"Full cycle" includes advancing the guest instruction pointer, so it is the closest figure to a do-nothing device.

Partition and memory operations:

| Pass | Create partition | Map 1 GiB | Unmap 1 GiB |
|---|---|---|---|
| 1 | 821 µs | 15.5 ms | 9.9 ms |
| 2 | 465 µs | 9.7 ms | 10.0 ms |
| 3 | 525 µs | 10.4 ms | 10.3 ms |
| 4 | 428 µs | 11.6 ms | 9.9 ms |
| 5 | 499 µs | 9.7 ms | 9.8 ms |

Interpretation:

- Exit cost is stable and small. It is unlikely to be the main graphics bottleneck.
- Tails are real: up to 340 µs, or 4% of a 120 FPS frame, on a machine doing nothing unusual.
- Pass 1 had the slowest tail and mapping time, which may be a warm-up effect. Five passes are not enough to state this firmly.

Limitations:

- One vCPU, idle device model, and no contention from a game. Real workloads will differ.
- Mapping figures cover **untouched** memory. First-touch fault cost is not measured yet, and it decides real boot and reclaim behaviour.
- This is not a measurement of Android, graphics, or FPS.

## Planned

| Measurement | Phase | Tool |
|---|---|---|
| Boot time, VMM RSS | 1 | `rax vm` |
| Idle RAM and CPU | 1 | Windows performance counters |
| GLES frame time through gfxstream | 2 | Open test APK, Perfetto |
| Rendering overhead against the Android Emulator | 2 | Same APK |
| Input latency | 3 | Timestamped event path |
| First-touch and reclaim cost | 5 | `rax bench` extension |
| Minecraft FPS, 1% low, frame time variance | Gated | Requires a licensed path |

Control baseline note: the Android SDK licence covers use "solely to develop applications", so running a retail game on Google's Play system images as a performance benchmark needs legal review first. The open test APK avoids the question.
