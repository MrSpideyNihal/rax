# Architecture Decision Records

Each record states the problem, the options, the decision, and the consequences. Records are never rewritten after acceptance. A new record supersedes an old one.

| ADR | Decision | Status |
|---|---|---|
| [0001](0001-use-windows-hypervisor-platform.md) | Use the Windows Hypervisor Platform | Accepted |
| [0002](0002-vmm-strategy.md) | crosvm spike, QEMU fallback, behind a process boundary | Superseded by 0006 |
| [0003](0003-graphics-pipeline.md) | gfxstream on host Vulkan, no passthrough claims | Replaced by 0008 |
| [0004](0004-x86-64-guest-first.md) | x86_64 guest, no ARM translation dependency | Accepted |
| [0005](0005-google-play-policy.md) | Never bundle, fake or bypass Google Play | Accepted |
| [0006](0006-own-rust-vmm.md) | Write RAX's own VMM in Rust | Accepted |
| [0007](0007-game-goals-and-input.md) | Free Fire and BGMI goals, and key-mapping rules | Accepted |
| [0008](0008-gpu-through-virglrenderer.md) | GPU forwarding through virglrenderer | Accepted |
