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

That covers the hypervisor. For guest boot and idle cost:

```powershell
./scripts/fetch-guest.ps1
cargo build --release
./scripts/measure-boot.ps1 -Passes 10
```

Each script writes numbered JSON files to a dated folder under `benchmarks/results`. Record the state of the machine with the results: mains or battery, power plan, and other running applications.

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

### Linux guest boot and idle, 2026-11-06

| | |
|---|---|
| Raw data | [`benchmarks/results/2026-11-06-vm-boot`](../benchmarks/results/2026-11-06-vm-boot) |
| Command | `./scripts/measure-boot.ps1 -Passes 10` |
| Build | Release, `x86_64-pc-windows-gnu`, rustc 1.99.1 |
| Guest | Alpine 3.24 `linux-virt` 6.18.60, 512 MiB, one vCPU, serial console |
| Host | Same laptop and power plan as above, on mains |
| State | **Not isolated.** Another application held about 7 GB, and about 1.9 GB of RAM was available |

Boot, from the first vCPU instruction until user space printed its marker:

| | Result |
|---|---|
| Passes | 10, all reached user space and powered off |
| Mean | 774 ms |
| Range | 757 to 796 ms |
| vCPU exits per boot | About 36,200, of which 33,344 were serial port accesses |
| Interrupts per boot | 309 to 333 |

Idle, with a guest sitting at a shell prompt, sampled over 10 s:

| | Result |
|---|---|
| Host CPU | 2.18% of one core |
| Working set of `rax.exe` | 138 MiB |
| Private memory of `rax.exe` | 544.5 MiB, of which 512 MiB is guest RAM reservation |

Inside the guest, `free -m` reported 34 MiB used of 469 MiB, in one interactive check.

Interpretation:

- Nearly all exits are boot messages on the emulated serial port, so a `quiet` boot should need far fewer. That has not been measured.
- **Idle CPU misses the 1% target.** The likely cause is the timer thread waking every millisecond, but this has not been profiled.
- The working set is low because guest RAM is only touched on demand. It will grow with real workloads, so **138 MiB must not be read as RAX's memory cost**.

Limitations:

- This is a tiny BusyBox guest, not Android. **Android will be far slower and larger.**
- One sampling run for idle. Single vCPU. No disk, network, or graphics.

Boot time history, for the record. These figures came from builds with known bugs and their raw data was discarded:

| Change | Boot | Exits |
|---|---|---|
| First working boot | About 3.3 s | About 121,000 |
| Added RTC. Linux had spun about 80,000 times on a clock it could not read | About 0.8 s | About 36,000 |

### Interrupt delivery bug, found by measurement

The first results showed about 1 boot in 5 losing all user-space output. The first diagnosis, a stale flag, was only a symptom: removing the flag left 5 of 30 boots failing. The root cause was a wrong register number, `0x80000003` instead of `0x80000004`, written from memory, so interrupt-window notifications were never requested. Each blocked interrupt then waited up to 1 ms for the next timer kick.

Exits were not counted before the fix, but a trace showed the window request never clearing in any of 12 boots. After the fix, every boot took 125 to 153 window exits, and 30 of 30 boots kept their output. `crates/rax-vmm/tests/boot_linux.rs` boots real Linux ten times to guard against this. Left unfixed, the same delay could have added input latency in a game.

### Guest disk throughput, 2026-11-06

| | |
|---|---|
| Raw data | [`benchmarks/results/2026-11-06-disk`](../benchmarks/results/2026-11-06-disk), exit counts only. Speeds below were read from guest `dd` output |
| Command | `sh scripts/disk-speed.sh`, 3 runs |
| Guest | Same as above, 256 MiB raw image, legacy virtio-blk |
| State | Not isolated |

| Run | Write, 128 MiB with `fsync` | Cached read, 128 MiB |
|---|---|---|
| 1 | 296.9 MB/s | 1.1 GB/s |
| 2 | 279.0 MB/s | 1.1 GB/s |
| 3 | 299.5 MB/s | 1.2 GB/s |

**The read column is not disk speed.** The guest dropped its own cache, but Windows was still holding the data it had just written. It measures the virtio path against host memory.

The test writes zeros sequentially in 1 MiB blocks, so it says nothing about random I/O, or about how Android's storage workload will behave.

## Planned

| Measurement | Phase | Tool |
|---|---|---|
| Android boot time, idle RAM and CPU | 1b | `rax vm` |
| GLES frame time through gfxstream | 2 | Open test APK, Perfetto |
| Rendering overhead against the Android Emulator | 2 | Same APK |
| Input latency | 3 | Timestamped event path |
| First-touch page fault and reclaim cost | 5 | `rax bench` extension |
| Minecraft FPS, 1% low, frame time variance | Gated | Requires a licensed path |

Control baseline note: the Android SDK licence covers use "solely to develop applications", so running a retail game on Google's Play system images as a performance benchmark needs legal review first. The open test APK avoids the question.
