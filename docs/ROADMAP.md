# Roadmap

Each phase ends at a gate. A failed gate stops the plan until the recorded alternative is chosen. No dates are promised.

| Phase | Deliverable | Gate | Status |
|---|---|---|---|
| **0 Research** | Feasibility report, ADRs, host probe, WHP baseline | WHP guest runs on target hardware | **Done** |
| **1a Own VMM** | `rax vm run`, Linux on RAX's own VMM | Interactive Linux shell under WHP, boot and idle measured | **Done** |
| **1b Android guest** | virtio-blk, framebuffer, Android 9 boot (all **done**), multiple vCPUs, a current Android version | Android boots under WHP, idle measured | **Gate passed** on Android 9. Multiple vCPUs and a newer Android remain |
| **2 Graphics** | GPU forwarding on the RTX 3050, GLES test APK built with the local NDK | Hardware rendering confirmed, overhead measured against the Android Emulator | Blocked on 1 |
| **3 Input** | Key-mapping engine (**done**), Raw Input, virtual touch device, capture | Latency measured, mouse look usable in a test app | In progress, connection blocked on 1 |
| **Decision** | Choose the Play route | Written position from Google, or an explicit decision to remain research | Open |
| **4 Minecraft** | Compatibility testing | Requires a licensed path from the decision above | Gated |
| **5 Optimization** | Reclaim, pacing, shader cache, service reduction | Each change has before and after data | Blocked on 2 |
| **6 Distribution** | Installer, updates, sandboxing, signed releases | Legal review complete | Gated |

## Decisions and fallbacks

| Gate failure | Alternative |
|---|---|
| RAX's VMM cannot run Android | Upstream QEMU on WHPX as a child process, same `rax vm` interface |
| gfxstream will not run under the chosen VMM | Evaluate the Android Emulator's own build of it, then revisit WSL2 GPU-PV |
| x86_64 Minecraft unusable | Stop Minecraft work. ARM translation has no redistributable option |
| BGMI is `arm64` only, as currently listed | Blocked until a legal ARM translation path exists |
| A game rejects RAX through anti-cheat | Accept and document it. RAX will not hide from anti-cheat |
| No Google licence | RAX continues as an open runtime without Google apps |
| Overhead no better than the control | Publish the data honestly and reassess the project |

## Work that needs a person

- Contact Google through the GMS partner path. Only the project owner can do this.
- Read the Minecraft EULA and Google's uncertified-device registration page. Neither could be read automatically.
- Obtain legal advice before any public binary that goes beyond developer tooling.
