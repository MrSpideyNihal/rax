# Roadmap

Each phase ends at a gate. A failed gate stops the plan until the recorded alternative is chosen. No dates are promised.

| Phase | Deliverable | Gate | Status |
|---|---|---|---|
| **0 Research** | Feasibility report, ADRs, host probe, WHP baseline | WHP guest runs on target hardware | **Done** |
| **1a Own VMM** | `rax vm run`, Linux on RAX's own VMM | Interactive Linux shell under WHP, boot and idle measured | **Done** |
| **1b Android guest** | virtio-blk (**done**), multiple vCPUs, a prebuilt Android x86_64 image | Android shell under WHP, idle measured | **In progress** |
| **2 Graphics** | gfxstream on the RTX 3050, GLES test APK | Hardware rendering confirmed, overhead measured against the Android Emulator | Blocked on 1 |
| **3 Input** | Raw Input to virtio-input, capture, mapping config | Latency measured, mouse look usable in a test app | Blocked on 1 |
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
| No Google licence | RAX continues as an open runtime without Google apps |
| Overhead no better than the control | Publish the data honestly and reassess the project |

## Work that needs a person

- Contact Google through the GMS partner path. Only the project owner can do this.
- Read the Minecraft EULA and Google's uncertified-device registration page. Neither could be read automatically.
- Obtain legal advice before any public binary that goes beyond developer tooling.
