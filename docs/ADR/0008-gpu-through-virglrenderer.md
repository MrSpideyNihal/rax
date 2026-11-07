# ADR-0008: GPU forwarding through virglrenderer

Status: Accepted, 2026-11-06. Replaces the gfxstream plan in [ADR-0003](0003-graphics-pipeline.md).

## Problem

Games need hardware rendering. ADR-0003 chose gfxstream, but its guest libraries would have to be built into Android, which means a full Android build on Linux, and this PC has neither Linux nor that build set up. RAX's own VMM also rules out reusing an emulator's graphics stack as it stands.

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
- **Frames are read back.** The window gets each frame through a synchronous copy from the GPU, at most about 60 times a second, which costs time that showing the texture directly would not.
- **The 60 Hz cap is still there.** A `video=` mode at 240 Hz reached the kernel, but Android stayed on 60 Hz.

## Result

Built on 2026-11-06. Android 9 boots with `GLES: Red Hat, virgl, OpenGL ES 3.1 Mesa 19.3.5`, and draws in the live window. All OpenGL contexts must share one group, or guest contexts cannot see resources created in the renderer's first context. Setting `RAX_GL_DEBUG` makes the driver print its errors, which is how that was found.

Three later findings mattered most, all from Minecraft:

- **Every command buffer must be flushed.** Without `glFlush` in the guest's own context plus a fence, queries wait on work that never reached the GPU. That held Minecraft near 3 FPS.
- **Guest memory must stay unfragmented.** Android's 4.19 driver needs up to 256 KiB of continuous memory for each batch. When that copy fails the batch is lost, and the game waits forever on a query inside it. RAX keeps 192 MiB free and compacts memory every 30 seconds.
- **Rendering has its own thread**, with its own interrupt line, so the guest keeps running during a long draw.

## Consequences

"The repository alone" stops being fully true for graphics: RAX depends on these DLLs. They still live inside the one folder, and deleting it removes them.
