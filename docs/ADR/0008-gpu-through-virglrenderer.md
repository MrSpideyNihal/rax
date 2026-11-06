# ADR-0008: GPU forwarding through virglrenderer

Status: Accepted, 2026-11-06. Replaces the gfxstream plan in [ADR-0003](0003-graphics-pipeline.md).

## Problem

Games need hardware rendering. ADR-0003 chose gfxstream, but its guest libraries would have to be built into Android, and building Android needs a Linux machine with far more disk than this project has. RAX's own VMM also rules out reusing an emulator's graphics stack as it stands.

## What was found

| Question | Finding |
|---|---|
| Guest support | Android-x86 9 already ships `virtio_gpu_dri.so` and `virtio-gpu.ko`, and its init script handles them |
| Windows build | `virglrenderer` 1.3.0 is packaged by MSYS2, so nothing needs compiling |
| Size and licence | About 2.6 MB for the renderer and libepoxy, both MIT |
| Windowing | It accepts OpenGL contexts from the caller, so RAX creates them with WGL |
| **Target GPU** | **Measured**: it starts on the RTX 3050 with NVIDIA's OpenGL 4.6 driver, and reports both capability sets |

## Decision

Use virglrenderer, loaded at run time from a `gpu` folder beside `rax.exe`. Its DLLs are downloaded by `scripts/fetch-gpu.ps1`, and they are not stored in Git.

On hybrid laptops the executable exports `NvOptimusEnablement` and `AmdPowerXpressRequestHighPerformance`, so the choice of GPU stays out of the registry. The test showed this matters: without the exports, the renderer landed on the Radeon 740M.

## Risks

- **Speed is unknown.** virgl translates guest OpenGL to host OpenGL, with commands going through the VM. It may be too slow for demanding games, and nothing here measures that.
- **The guest speaks OpenGL ES through virgl.** Android 9's Mesa version limits which features games get. Vulkan is not part of this path.
- **It is native third-party code** running inside RAX with the user's permissions, and it parses commands from the guest. Sandboxing it is a requirement before wide release.
- The `virtio-gpu` device itself is not built yet.

## Consequences

"The repository alone" stops being fully true for graphics: RAX depends on these DLLs. They still live inside the one folder, and deleting it removes them.
