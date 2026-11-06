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
| Disk, network, multiple vCPUs, graphics | Not implemented |

## Android guest

| Item | Status |
|---|---|
| Android boot | Not started |
| ART | Not started |
| Hardware OpenGL ES | Not started |
| Hardware Vulkan | Not started |

## Minecraft Bedrock (Android)

| Item | Status | Evidence |
|---|---|---|
| Runs on RAX | **Not tested** | |
| x86_64 Android build | Listed version code, conflicting community report | [FEASIBILITY §6](FEASIBILITY.md#6-android-arm-compatibility-strategy) |
| Graphics API | OpenGL ES (RenderDragon) | Minecraft Wiki |
| Mouse and keyboard on Android | Unverified | |
| Installation from Google Play | **Not available.** RAX has no licensed Play environment | [ADR-0005](ADR/0005-google-play-policy.md) |

Owning Minecraft on Android does not provide a Windows Bedrock licence, and RAX does not change that.
