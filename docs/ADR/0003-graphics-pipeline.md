# ADR-0003: gfxstream on host Vulkan, with no passthrough claims

Status: Replaced by [ADR-0008](0008-gpu-through-virglrenderer.md), 2026-11-06. Its rule against claiming passthrough still stands.

## Problem

Minecraft Bedrock on Android renders with OpenGL ES. The guest needs hardware acceleration on a Windows host with an NVIDIA dGPU and an AMD iGPU.

## Options

| Option | Assessment |
|---|---|
| gfxstream | Apache-2.0, ring-buffer streaming, proven by the Android Emulator, has a Windows build path |
| Venus | Vulkan only, experimental on Windows through a fork |
| virGL | Older GL forwarding, not suited to high frame rates (estimate) |
| DRM native context | Linux hosts and open drivers only |
| GPU-PV via WSL | Only reachable through Microsoft's stack, `dzn` is non-conformant |
| Direct passthrough | No consumer WHP path. Would also remove the GPU from Windows |

## Decision

Use gfxstream, targeting host Vulkan on the discrete GPU. Benchmark two GLES routes: guest ANGLE on forwarded Vulkan, and forwarded GLES on host ANGLE. Keep whichever has better frame times.

## Rules

- Marketing and docs say "API forwarding". They never say "passthrough" or "native GPU".
- SwiftShader is allowed in CI and debugging. A gaming session on a software renderer is an error, never a silent fallback.

## Consequences

- Performance is limited by serialization and decode on the host CPU.
- Hybrid laptops may add a cross-adapter copy. Measure this rather than assume.
- gfxstream on Windows is built with Visual Studio and ClangCL, adding build weight.
