# Phase 0: Technical Feasibility Report

Status: complete, as of 2026-11-06.

> **Update, same day.** This report's recommendation to use crosvm was reversed by [ADR-0006](ADR/0006-own-rust-vmm.md), and RAX now has its own VMM. Linux boots to an interactive shell on it. Sections below are kept as written so the reasoning stays visible. Current results are in [BENCHMARKS.md](BENCHMARKS.md).

Every claim below carries one of three labels:

| Label | Meaning |
|---|---|
| **Measured** | Produced by code in this repository, with raw data in [`benchmarks/results`](../benchmarks/results). |
| **Sourced** | Stated by a page listed in [Sources](#sources). |
| **Estimate** | Engineering judgement. Not evidence. Must be tested before anyone relies on it. |

## 1. Verdict

**The runtime is plausibly buildable. The product as specified is not shippable without Google's agreement.**

| Question | Answer | Confidence |
|---|---|---|
| Can a Rust host drive a guest through WHP on the target laptop? | **Yes.** Done, on Windows 11 Home with VBS on. | Measured |
| Will hypervisor exit cost limit frame rate? | **Unlikely.** About 3 µs per exit. | Measured |
| Can Android boot under crosvm on WHP? | **Unknown.** Upstream does not test WHP. | Sourced gap |
| Can RAX boot Linux on its own VMM? | **Yes.** Added after this report. | Measured |
| Can gfxstream deliver hardware GPU acceleration on Windows? | **Yes in the Android Emulator stack, unproven in crosvm.** | Sourced gap |
| Can Minecraft run without ARM translation? | **Probably, must be tested.** An x86_64 Android version code exists. | Sourced, with one conflicting report |
| Can RAX beat BlueStacks on resources? | **Unknown.** Reliable public numbers do not exist. | Open |
| Can RAX ship Google Play on its own? | **No.** That needs a Google licence and certification. | Sourced |
| Is a Windows Bedrock licence implied by an Android purchase? | **No.** | Sourced (secondary) |

### The uncomfortable part

Steps 3 and 4 of the product vision ("sign in to a properly licensed Google Play environment", "download Minecraft from Google Play") are not engineering tasks. They are a contract with Google.

- Google Play and GMS are licensed only to Android-compatible devices that pass the CDD, CTS and certification. Compatibility makes a device *eligible*, and does not grant the licence.
- `MEETS_VIRTUAL_INTEGRITY` is the Play Integrity label for a certified emulator. Google documents it in the context of Google Play Games for PC.
- An uncertified build gets no Play Protect certification, and Google warns that its apps may not work.
- Google's Play system images in the Android SDK cannot fill the gap. The SDK licence grants use "solely to develop applications" and forbids redistribution.

So there are three honest outcomes, and we should choose deliberately:

1. **Become a licensed partner.** Apply through Google's GMS contact path. Slow, uncertain, and realistically aimed at a company rather than a solo project.
2. **Ship an open runtime with no Google apps.** Legal and technically interesting, but it cannot deliver a paid Play-purchased Minecraft. It can run open-source and self-distributed Android apps.
3. **Treat RAX as research.** Publish the lightest measured Android-on-Windows graphics stack as an open engineering project, without promising Minecraft to end users.

If the only goal is to play Minecraft well on this laptop, buying the Windows edition beats any emulator. It runs natively on DirectX with no VM. That is not the project's goal, but pretending otherwise would not be honest.

**Recommendation:** continue with outcome 3 now, because it produces value whatever happens. Keep the architecture compatible with outcome 1, and contact Google before spending on Phase 6.

## 2. Target hardware, as found

The machine differs from the brief. Plans use the real figures. **Measured** by `rax doctor` and system queries:

| | Brief | Actual |
|---|---|---|
| CPU | Ryzen 7 7000-series HS | Ryzen 7 7445HS, **6 cores / 12 threads** |
| RAM | 24 GB | **15.3 GB visible to Windows** (6.3 GB available during tests) |
| dGPU | RTX 3050 4 GB | RTX 3050 Laptop, 3962 MiB local, Vulkan 1.4.341 |
| iGPU | not mentioned | **Radeon 740M present**: hybrid graphics |
| OS | Windows 11 | Windows 11 **Home** Single Language, build 26300 |
| Virtualization | not mentioned | Hypervisor running, **VBS on** |

Consequences:

- A 4 GiB guest is a third of RAM. Memory reclaim moves from "nice" to essential, and the 24 GB assumption must not reach any design.
- Hybrid graphics means the renderer must pick the dGPU explicitly, and frames may cross adapters to reach the display.
- VBS rules out any design that needs exclusive VT-x or SVM access. WHP is the only hypervisor path that coexists with it.

## 3. Recommended Windows architecture

Details are in [ARCHITECTURE.md](ARCHITECTURE.md). In short:

```
rax.exe (Rust orchestrator)         launcher, lifecycle, config, Raw Input, metrics
   | supervises
VMM process (crosvm first, QEMU fallback)
   | WHP vCPUs, virtio-blk/net/input/vsock/balloon
   | virtio-gpu --> gfxstream host renderer --> Vulkan --> RTX 3050
Guest: minimal AOSP x86_64, ART, Linux kernel
```

### VMM options compared

| Option | Android proven on Windows | GPU path | Licence | Footprint (estimate) | Verdict |
|---|---|---|---|---|---|
| Android Emulator (QEMU fork + gfxstream) | Yes | gfxstream, proven | GPLv2 parts, Apache parts; SDK binary is not redistributable | Heavy, carries phone and UI features | **Control baseline** |
| crosvm + WHPX + gfxstream | Unproven | gfxstream, undocumented on Windows | BSD-3-Clause, Apache-2.0 | Lightest candidate | **Primary spike** |
| Upstream QEMU + WHPX | Partly | gfxstream via virtio-gpu is young on Windows | GPLv2 | Medium | **Fallback** |
| WSL2 + Waydroid-style container | No | `/dev/dxg` plus Mesa d3d12/dzn | GPL-3.0 (Waydroid) | Shares a Linux VM | **Rejected for now** |
| Custom Rust VMM on WHP | No | Would still embed gfxstream | Ours | Smallest possible | **Rejected**: rewrites what the brief says not to rewrite |
| Waydroid directly | Linux only | n/a | GPL-3.0 | n/a | Not applicable on Windows |
| Android-x86 / Bliss | Bare metal focus | Needs a VMM anyway | Apache/GPL | n/a | Useful only as a source of x86 patches |
| Windows Subsystem for Android | Discontinued March 2025 | n/a | Proprietary | n/a | Dead end |

**Why WSL2 was rejected.** It has the only real paravirtualized Windows GPU path (GPU-PV), which is attractive. But the Vulkan layer on it, `dzn`, is labelled non-conformant and for testing use, open reports show `llvmpipe` fallback and D3D12 device loss, Android's graphics stack would need a port onto it, and I found no working Android example. It is worth revisiting only if gfxstream fails.

## 4. Open-source components to reuse

| Component | Use | Licence | How we consume it | Status |
|---|---|---|---|---|
| crosvm | VMM | BSD-3-Clause | Separate process | WHPX "Tested upstream: no" |
| gfxstream | GPU command streaming | Apache-2.0 | Via VMM | Has a Windows build path, no stated support tier. "Not an officially supported Google product" |
| rutabaga_gfx | virtio-gpu abstraction | BSD-3-Clause | Via VMM | README does not mention Windows |
| AEMU base | gfxstream dependency | Apache-2.0 | Via gfxstream | |
| ANGLE | GLES on Vulkan/D3D | BSD-3-Clause (Estimate: verify per version) | Guest or host | Decided by spike |
| AOSP | Guest OS | Apache-2.0 plus component licences | Built from source | Without GMS |
| Linux kernel | Guest kernel | GPL-2.0 | Built from source, source offered | |
| QEMU | Fallback VMM | GPL-2.0 | Separate process, source offered | |

Rules that keep this clean:

- GPL components run as **separate processes** and we publish corresponding source.
- Google's prebuilt emulator, Play system images and ARM translators are **never redistributed**.
- BlueStacks and LDPlayer are comparison targets only.

## 5. GPU acceleration strategy

**What the game needs.** Bedrock on Android renders through RenderDragon on **OpenGL ES**. No official Android Vulkan backend was found, and Mojang's Vulkan work concerns Java Edition. The pipeline must therefore be excellent at GLES, not only Vulkan.

**No native passthrough.** RAX must not claim it. WHP exposes no consumer path to assign this GPU to a guest, and GPU-PV is reachable only through Microsoft's own stacks. The honest term is *API forwarding*: guest GPU commands are serialized and replayed by the host driver.

| Approach | Windows host | NVIDIA | Verdict |
|---|---|---|---|
| gfxstream | Build path documented | Works in Android Emulator (older NVIDIA workarounds exist) | **Chosen** |
| Venus (Vulkan over virtio-gpu) | Experimental fork only | Unknown | Watch |
| virGL | GLES over GL, historically slow (Estimate) | n/a | Rejected |
| DRM native context | AMD, Intel, Freedreno, Linux hosts | No open driver path | Not available |
| virtio-nvgpu | No Windows support | KVM only | Not available |
| SwiftShader | Software | n/a | CI and debugging only, never for gaming |

Open question for the spike: **guest ANGLE on forwarded Vulkan, or forwarded GLES onto host ANGLE?** Measure both with the same test APK.

Measured host facts that help: the RTX 3050 exposes `VK_KHR_external_memory_win32` and `VK_KHR_external_semaphore_win32`, so `rax doctor` raises no zero-copy warning. These are preconditions for avoiding copies, not proof that any copy has been avoided.

Priorities, each benchmarked before and after: explicit dGPU selection, frame timing instrumentation via gfxstream's Perfetto tracing, shader cache persistence on the host, resolution scaling, then frame pacing using `VK_KHR_present_wait` where available.

Hybrid graphics is the main unknown. Whether frames take a cross-adapter copy to the panel, and whether this laptop's firmware can bypass the iGPU, has not been established.

## 6. Android ARM compatibility strategy

**Do not start with a translator.** Sourced evidence:

- Bedrock 26.12 lists Android version codes for `armeabi-v7a`, `x86`, `arm64-v8a` and **`x86_64`** (`982601202`).
- The same page lists build numbers only for the ARM variants, and one community report says x86_64 Android builds stopped working after 1.21.114. **These conflict with the optimistic reading and must be tested.**
- Minecraft launched on ChromeOS listing `x86_64` and `arm64-v8a` as supported architectures.
- Google's guidance for Play Games on PC is to ship x86-64 libraries to avoid translation layers.

Strategy:

1. Boot an x86_64 guest and declare `x86_64` as the primary ABI, so a licensed store would deliver the native split.
2. Test the assumption only with a legitimately obtained build, under a licensed environment.
3. Treat ARM translation as out of scope until that test fails.

If translation does become necessary, the options are poor:

| Translator | Direction | Availability |
|---|---|---|
| Berberis | riscv64 to x86_64 **only** | Open, but no arm64 guest support found |
| Intel Houdini / Bridge Technology | ARM to x86 | Proprietary, no redistribution right |
| Google ndk_translation | ARM to x86 | Proprietary, bundled in Google images |
| QEMU user mode | ARM to x86 | GPL, but far too slow for games (Estimate) |

Extracting Houdini from WSA or Google images, as community projects do, is not an option for RAX.

## 7. Google Play certification and licensing strategy

Facts:

- Android compatibility: pass the CDD and CTS. Compatibility is what makes a device *eligible* to seek a Play and GMS licence.
- Certification requires testing plus a licence with Google. Details are largely under NDA.
- Play Terms (14 October 2026) forbid modifying content from Play and circumventing its protections.
- Android SDK licence: "solely to develop applications", no redistribution.
- Developer verification began enforcement in four countries on 30 September 2026, on certified devices. ADB installs are reportedly unaffected. Whether a RAX device would be in scope is unresolved.
- Android and Minecraft marks are used here only descriptively.

Commitments, also in [ADR-0005](ADR/0005-google-play-policy.md):

- RAX never bundles, downloads or scripts the installation of GApps, Play Store or Play Services.
- RAX never spoofs device fingerprints, certification state or Integrity verdicts.
- RAX contains no root hiding, no entitlement bypass, and no APK patching.
- "Install APK" is a general developer feature, documented for software the user has the right to run.
- No distribution of a Play-enabled build without a written licence from Google.

Licence reminder: buying Minecraft on Android gives no Windows Bedrock licence. Legal review is required before any public release. This document is not legal advice.

Pages that could **not** be read, and so must be checked by a human: the Minecraft EULA (timed out), Google's uncertified-device registration page (requires sign-in).

## 8. Expected overhead and realistic targets

**These are targets, not results.** Nothing below has been achieved. Where public numbers exist they conflict and are mostly vendor or affiliate claims, so no competitor figures are quoted.

| Metric | Target (Estimate) | Reasoning |
|---|---|---|
| Host orchestrator RSS | under 50 MiB | `rax.exe` is 0.38 MB on disk today (Measured) |
| VMM plus renderer, idle | under 300 MiB | No UI, few devices |
| Guest RAM, idle after reclaim | under 1.5 GiB | Stripped AOSP still runs ART and system_server |
| Whole stack in game | under 4 GiB | Must fit this laptop's 15.3 GB |
| Idle CPU, guest suspended | under 1% of one core | vCPUs parked |
| Cold boot to launcher | under 15 s | SSD, minimal services |
| FPS | At least the control, then beat it | A VM cannot beat native. A ratio is honest; an absolute number is not |
| 1% low | At least 60% of average | Prioritise pacing over peak |
| Added input latency | Under 5 ms host to guest event | Exit cost is negligible; scheduling dominates |

**Measured overhead so far** (details in [BENCHMARKS.md](BENCHMARKS.md)): median exit 3.0 µs, full cycle about 3.2 µs, map 1 GiB about 10 ms. A frame with 200 virtio notifications would spend about 0.64 ms on exits, which is 8% of a 120 FPS frame budget. Batched ring-buffer submission should keep real counts lower, but that is an estimate. Tail exits reached 340 µs.

## 9. Major technical risks

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| 1 | Google will not license a small independent runtime | **High** | **Fatal to the consumer product** | Decide early; keep the research path valuable |
| 2 | crosvm on WHPX is untested upstream and may have rotted | High | High | Time-boxed spike, QEMU fallback behind a process boundary |
| 3 | gfxstream on Windows under crosvm has no precedent we could find | High | High | Same spike; the Android Emulator proves the renderer itself works |
| 4 | Hybrid graphics adds a copy or pacing jitter | Medium | Medium | Measure early; document display modes |
| 5 | Minecraft's x86_64 Android build is unusable | Medium | High | Test early. If it fails, the Play path becomes harder still |
| 6 | 15.3 GB leaves little room under real workloads | Medium | Medium | virtio-balloon free page reporting, tested first |
| 7 | WHP memory reclaim APIs behave poorly under churn | Medium | Medium | Measure first-touch and unmap cost next. No public data found |
| 8 | Dependence on upstream projects with no support promise | Medium | Medium | Pin versions, keep local patches small |
| 9 | Shader compilation stutter through double translation | Medium | Medium | Persistent host-side cache |
| 10 | Bedrock anti-tamper or platform checks reject a virtualized device | Unknown | High | Do not bypass. Accept the result |
| 11 | Scope is large for a small team | High | High | The gates below can stop work cheaply |

## 10. Prioritized MVP roadmap

The full gated plan is in [ROADMAP.md](ROADMAP.md). Order of work:

1. Host probe and WHP measurement: **done**.
2. Boot Linux under crosvm on WHP. **Gate**: otherwise switch VMM.
3. Boot minimal AOSP x86_64 to a shell, and measure idle.
4. Bring up gfxstream. Run an open GLES 3.x test APK on the RTX 3050. **Gate**: hardware rendering confirmed, compared with the Android Emulator using the same APK.
5. Input through virtio-input, tested for latency.
6. Memory reclaim and frame pacing work, measured.
7. **Decision point on Play**, before any Minecraft work that depends on it.
8. Minecraft compatibility testing, only with a licensed path.
9. Installer and updates, after the legal model is settled.

## 11. The exact first prototype

### Prototype 0: done

`rax doctor` and `rax bench whp` in this repository. It proved, on the target laptop, that safe-wrapped Rust can load WHP, create a partition, map memory, run a real guest and service its exits, and it produced the first baseline.

### Prototype 1: build this next

**Goal:** answer whether crosvm is viable on Windows before anything depends on it.

**Scope:**

1. Install the MSVC toolchain, which the crosvm book requires, and Clang tooling.
2. Build crosvm with `cargo build --features all-msvc64,whpx`, pinned to a recorded commit.
3. Build a minimal x86_64 Linux kernel and BusyBox initramfs with virtio drivers.
4. Boot it through a new `rax vm run` command that supervises crosvm, captures the serial console, and handles clean shutdown.
5. Record boot time, VMM RSS and exit counts in a `benchmarks/results` entry.

**Success:** a Linux shell on the serial console under WHP, with a repeatable script.

**Failure:** more than five working days without a boot, or a required crosvm fix that is too large to keep locally. Then repeat with upstream QEMU on WHPX, keeping the same `rax vm` interface.

**Explicitly excluded:** Android, graphics, UI, and any Google component.

## Sources

Hypervisor and VMM:
[Android Emulator acceleration](https://developer.android.com/studio/run/emulator-acceleration),
[Windows Hypervisor Platform](https://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/hypervisor-platform),
[crosvm](https://github.com/google/crosvm),
[crosvm Windows build](https://crosvm.dev/book/building_crosvm/windows.html),
[crosvm hypervisors](https://crosvm.dev/book/hypervisors.html),
[Android Emulator hypervisor driver](https://github.com/google/android-emulator-hypervisor-driver).

Graphics:
[gfxstream](https://github.com/google/gfxstream),
[rutabaga_gfx](https://github.com/magma-gpu/rutabaga_gfx),
[QEMU virtio-gpu](https://www.qemu.org/docs/master/system/devices/virtio/virtio-gpu.html),
[QEMU Venus for Windows fork](https://gitlab.com/rodnoycry/qemu-virtio-venus-win32),
[RenderDragon](https://minecraft.wiki/w/RenderDragon),
[DirectX and Linux](https://devblogs.microsoft.com/directx/directx-heart-linux/),
[WSL issue 41900](https://github.com/microsoft/WSL/issues/41900),
[WSL issue 42185](https://github.com/microsoft/WSL/issues/42185),
[wslg issue 1254](https://github.com/microsoft/wslg/issues/1254),
[gentoo bug 937851](https://bugs.gentoo.org/937851),
[virtio-nvgpu discussion](https://news.ycombinator.com/item?id=49824864).

Android and translation:
[Android compatibility overview](https://source.android.com/docs/compatibility/overview),
[CDD](https://source.android.com/docs/compatibility/cdd),
[Berberis](https://android.googlesource.com/platform/frameworks/libs/binary_translation/),
[WSA archive](https://learn.microsoft.com/en-us/windows/android/wsa/),
[WSA end of support](https://www.phoronix.com/news/Microsoft-Dropping-WSA),
[Waydroid](https://github.com/waydroid/waydroid),
[AEMU](https://android.googlesource.com/platform/hardware/google/aemu/+/refs/tags/v0.1.2-aemu-release).

Licensing and policy:
[Play Protect certified](https://www.android.com/certified/),
[Play Integrity verdicts](https://developer.android.com/google/play/integrity/verdicts),
[Google Play Terms](https://play.google.com/about/play-terms/),
[Android SDK licence](https://developer.android.com/studio/terms),
[AVD management](https://developer.android.com/studio/run/managing-avds),
[Play Games on PC compatibility](https://developer.android.com/games/playgames/pc-compatibility),
[developer verification timeline](https://www.androidauthority.com/android-sideloading-changes-timeline-3679204/),
[sideloading changes, 30 September](https://android.gadgethacks.com/news/google-android-sideloading-restrictions-sept-30-explained/).

Minecraft:
[Bedrock 26.12](https://minecraft.wiki/w/Bedrock_Edition_26.12),
[ChromeOS launch](https://techcrunch.com/2023/06/07/minecraft-is-releasing-a-new-version-for-chromebooks),
[x86 feedback thread](https://feedback.minecraft.net/hc/en-us/community/posts/5435806939021-Minecraft-Bedrock-for-Android-x86),
[Android Vulkan feedback](https://feedback.minecraft.net/hc/en-us/community/posts/44604890463885-Implement-Vulkan-API-Support-for-Minecraft-Bedrock-on-Android).
