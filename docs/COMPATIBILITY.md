# Compatibility

Status vocabulary: **Tested** means someone ran it and results are in this repository. Anything else is not a claim of support.

## Hosts

| Host | Status |
|---|---|
| Windows 11 Home 26300, Ryzen 7 7445HS, RTX 3050 Laptop + Radeon 740M, VBS on | **Tested**: `rax doctor`, `rax bench whp`, `rax vm run` |
| Any other Windows machine | Not tested |

`rax doctor` requires WHP and a hardware Vulkan device. Enable "Windows Hypervisor Platform" in Windows Features, and virtualization in firmware, if it reports a blocker.

## Linux guest

| Item | Status |
|---|---|
| Alpine Linux 3.24 `virt` kernel 6.18 to user space | **Tested**, 512 MiB, one vCPU |
| Interactive shell over serial | **Tested** |
| Disk as `/dev/vda`, legacy virtio-blk over PCI | **Tested**: read and write verified from the host |
| Network: DHCP, DNS, TCP, UDP | **Tested**: fetched a live web page |
| Multiple vCPUs, graphics | Not implemented |

## Android guest

| Item | Status |
|---|---|
| Android 9 (Android-x86 9.0-r2) full boot | **Tested**: 2 GB, one vCPU, `sys.boot_completed` set, setup screen drawn |
| ART and system apps | **Tested**: `system_server` and the setup wizard run |
| Mouse and keyboard in Android | **Tested**: a scripted click pressed the setup button |
| Multi-finger touch in Android | **Tested**: a scripted tap pressed a setup button, and Android logged two tracked fingers at once |
| Key mapping, window to mapper | Unit tested |
| Key mapping with a real keyboard in a real app | **Not tested** |
| Internet in Android | **Tested**: Android reported validated Ethernet with no setup, and its WebView loaded an HTTPS site |
| Download speed | **Not measured** |
| IPv6, ICMP beyond the router | Not implemented |
| Audio | Not implemented |
| Installing an APK by console | **Tested**: `pm install` returned `Success` |
| Dragging an APK onto the window | Built, **not tested end to end** |
| Apps and data surviving a shutdown | **Tested**: both survived a reboot on `data.img` |
| Software rendering | **Tested**, for booting only. Never a gaming path |
| Hardware OpenGL ES | Not started |
| Hardware Vulkan | Not started |

## Free Fire and BGMI

Goals, not support claims. See [ADR-0007](ADR/0007-game-goals-and-input.md).

| Item | Free Fire | BGMI |
|---|---|---|
| Runs on RAX | **Not tested** | **Not tested, and blocked** |
| Native x86 build | Not confirmed | None listed, `arm64-v8a` only |
| Emulator rules | Reported separate pool and leaderboard exclusion | Reported banned from competitive events |
| Key mapping | Connected, positions not set | Connected, positions not set |

Your account carries the risk of following or breaking each game's rules. RAX does not hide from anti-cheat, and has no macros.

## Minecraft Bedrock (Android)

| Item | Status | Evidence |
|---|---|---|
| Runs on RAX | **Not tested** | |
| x86_64 Android build | Listed version code, conflicting community report | [FEASIBILITY §6](FEASIBILITY.md#6-android-arm-compatibility-strategy) |
| Graphics API | OpenGL ES (RenderDragon) | Minecraft Wiki |
| Mouse and keyboard on Android | Unverified | |
| Installation from Google Play | **Not available.** RAX has no licensed Play environment | [ADR-0005](ADR/0005-google-play-policy.md) |

Owning Minecraft on Android does not provide a Windows Bedrock licence, and RAX does not change that.
