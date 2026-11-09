# Hematite Renderer — Architecture

This document describes the design. It contains **no claims that anything is built**; for
that, see `STATUS_REPORT.md`, which labels every component VERIFIED,
BUILT-BUT-UNTESTED or NOT-IMPLEMENTED.

Diagrams are ASCII both because they render everywhere and because a generated image would
be a claim of polish this project has not earned yet.

---

## 1. Module layout

```
                        game process (JVM, LWJGL 2 or 3)
                                    │  dlopen("libGLMojo.so") → same handle
                                    ▼
┌──────────────────────────────────────────────────────────────────────────┐
│ hematite-gl-frontend                                                     │
│  • the only crate that owns unsafe FFI entry points                      │
│  • dense export table: gl* (1.0–4.6) + egl* + eglGetProcAddress           │
│  • GL object name management, share groups, per-context current binding   │
│  • shadow state machine, error semantics (fast / strict), version strings │
│  • does NOT link Vulkan or GLES types                                    │
└───────────────┬──────────────────────────────────────────────────────────┘
                │ hematite-ir: backend-neutral commands & state blocks
                ▼
┌──────────────────────────────────────────────────────────────────────────┐
│ hematite-ir                                                              │
│  • Command enum (Bind, SetState, Draw, Dispatch, Copy, Barrier, Swap…)    │
│  • resource ids, formats, layouts; no API-specific handles               │
└───────────────┬────────────────────────────────────┬─────────────────────┘
                ▼                                    ▼
┌───────────────────────────────────┐  ┌───────────────────────────────────┐
│ hematite-vk-backend               │  │ hematite-gles-backend             │
│  capability profile, allocator,   │  │ GLES 3.0 baseline, 3.1/3.2 when    │
│  pipelines, descriptors, barriers, │  │ present; SPIRV-Cross → ESSL       │
│  render passes / dynamic rendering │  │                                   │
└───────────────┬───────────────────┘  └───────────────────────────────────┘
                │
   ┌────────────┴─────────────┬──────────────────┬───────────────────┐
   ▼                          ▼                  ▼                   ▼
hematite-fixed-function   hematite-glsl    hematite-shader-cache  hematite-quirks
(state-keyed shader       (preprocess,     (SPIR-V, pipeline      (vendor/driver →
 variants, matrix stacks,  rewrite to       cache, FF variants)     capability deltas)
 display lists)            SPIR-V)
                                   │
                                   ▼
                       hematite-config   hematite-diagnostics
                       (env + file)      (log, profiler, dumps, crash reports)
```

Dependency rule, enforced by `cargo-deny`-style review and by the workspace layout:
`hematite-gl-frontend` must not have `ash`/`vk-sys`/`khronos-egl` in its dependency graph.
The backend boundary is `hematite-ir` only.

**Implementation note (2026-11-09).** Of this diagram, `hematite-ir`, `hematite-config`,
`hematite-fixed-function`, `hematite-shader-cache`, `hematite-gl-frontend` (the 866-symbol
GL/EGL export surface, the GL state machine, the software rasterizer and a software EGL),
`hematite-vk-backend`, `hematite-testharness` and `hematite-plugin` (the C ABI compiled to
`libhematite.so`) exist
and build; the rest is design. The Vulkan backend does **not** yet consume `hematite-ir`:
it takes its own vertex struct, so the boundary above is still aspirational. The backend is
offscreen-only — no surface, swapchain or presentation.

---

## 2. Why an IR at all

The alternative is a trait with N methods, one per GL call, implemented by each backend
(`trait Backend { fn bind_texture(..); fn draw(..); }`). That was rejected for three reasons:

1. **Recording must be cheap and must not call into the backend on the render thread.**
   With an IR, the render thread pushes value-type `Command`s into a per-context buffer; the
   submission thread drains it. With a trait, every GL call would reach the backend directly.
   (D-004)
2. **Deduplication needs to see a sequence**, not single calls: skipping a draw whose pipeline
   is still compiling requires knowing the next few commands; merging draws requires the
   previous draw's state.
3. Two backends that share one IR keep the frontend honest: if a feature cannot be expressed
   in the IR, it belongs in the frontend's own state, not in the backend.

Cost: the IR is a hot-path data structure. It is a `Vec<Command>` per context with a
fixed-capacity side buffer for large inline payloads (vertex data goes into a ring buffer,
not into the command).

---

## 3. Threading model

```
┌──────────────────┐  record   ┌────────────────────┐  drain   ┌─────────────────────┐
│ Minecraft render │ ────────► │ per-context command │ ───────► │ submission thread   │
│ thread           │           │ ring (SPSC)         │          │ (one per device)    │
└──────────────────┘           └────────────────────┘          └──────────┬──────────┘
                                                                          │ vkQueueSubmit
                                     ┌────────────────────────────────────┘
                                     ▼
                       ┌──────────────────────────────┐
                       │ GPU                          │
                       └──────────────────────────────┘

  worker pool 1: GLSL → SPIR-V (N threads, default = cores/2, min 1)
  worker pool 2: pipeline compile + pipeline cache growth
  worker pool 3: texture transcode / mipmap generation
  worker pool 4: asset & readback staging
```

Rules:

- **The render thread never blocks on a lock held by a worker.** Cross-thread queues are
  SPSC ring buffers with a single atomic head/tail; the render thread is always the producer
  for its own ring.
- **Synchronous GL calls** are exactly: `glGet*`, `glReadPixels` (unless a PBO is bound),
  `glFinish`, `glClientWaitSync`/`glWaitSync` with a real wait, `glMapBuffer*` without
  `GL_MAP_UNSYNCHRONIZED_BIT`, `glGetQueryObject*` with `GL_QUERY_RESULT`
  (the `_AVAILABLE` variants are not), `glCopyTexImage*`/`glCopyTexSubImage*` (they read the
  destination framebuffer), and any `glGetError` in strict mode. Everything else records.
- **Synchronous calls are serviced by draining the current context's ring up to the call**,
  then either reading state (for `glGet*`, answered from the shadow state where legal) or
  submitting and waiting on a timeline semaphore value.
- **Per-frame fence**: at most `N` (default 3) frames in flight; a frame is reclaimed by
  timeline semaphore value, never by `vkQueueWaitIdle`.

### 3.1 Contexts and share groups

- `eglCreateContext` creates a *context object* holding: its own shadow state, its own
  command ring, and a reference to a *share group*.
- A share group owns the object name tables (textures, buffers, programs, …). Two contexts
  in the same share group see the same names.
- `eglMakeCurrent` binds a context to the calling thread (`thread_local` current-context
  slot). A call with no current context must return `GL_INVALID_OPERATION` rather than
  crash — mods do this by mistake and desktop drivers tolerate it inconsistently.
- Cross-context use of an object without a barrier is undefined in GL and stays undefined
  here; we do not add implicit synchronisation because that costs a barrier per bind.

---

## 4. Frame lifecycle

```
 swap 0                swap 1                 swap 2
  │                     │                      │
  ├─ fence(frame-2) ────┼──────────────────────┤   reclaim oldest frame's
  │                     │                      │   ring + descriptor pool
  ├─ begin frame        │                      │
  ├─ upload dirty UBO/  │                      │
  │  push-constant data │                      │
  ├─ record commands    │                      │
  ├─ vkQueueSubmit      │                      │
  ├─ vkQueuePresentKHR  │                      │
  └─ eglSwapBuffers ────┘                      │
```

Swapchain recreation is triggered by `VK_ERROR_OUT_OF_DATE_KHR` / `VK_SUBOPTIMAL_KHR` /
`VK_ERROR_SURFACE_LOST_KHR` from present or acquire, and by an explicit `eglSwapInterval` or
window-resize notification. Recreation:

1. Drain and wait for the frame currently being presented.
2. Destroy image views and the swapchain; **keep** all GL-visible objects, pipelines,
   descriptor layouts and the pipeline cache.
3. Recreate with the new extent and, where supported, pre-rotation
   (`VK_KHR_surface::currentTransform`) so the compositor does not rotate on its own.
4. Never invalidate GL object names — `glGenTextures` results must survive a rotation.

---

## 5. Shader translation pipeline

```
 GLSL source (game / pack / mod)
        │
        ▼
 1. include resolution + macro expansion  (pack-root confined, cycle-limited)
        │
        ▼
 2. injected defines (MC_VERSION, MC_GL_*, IS_IRIS, …) + option defines
        │
        ▼
 3. legacy/compat rewrite → single modern baseline (Vulkan GLSL 450 or ESSL 310)
        │      fixed-function globals → declared uniforms
        │      gl_FragColor/gl_FragData → out variables
        │      texture2D &co → texture
        │      attribute/varying → in/out
        ▼
 4. translate → SPIR-V            (primary translator + fallback chain, see D-007)
        │
        ▼
 5. reflect: uniforms, UBO/SSBO layout, samplers, attributes, outputs
        │
        ▼
 6. cache (source hash + options + translator version) → SPIR-V blob
        │
        ▼
 7. pipeline creation (async, on worker pool 2)
```

Line numbers in every emitted error are mapped back to the **original** source through a
span table produced in steps 1–3, so a pack author sees their own line numbers.

---

## 6. Cache layout

```
<app files>/hematite/
├── logs/2026-11-09/session-<pid>/          (rotating; capped)
├── shaders/                                SPIR-V, keyed by hash
│   ├── idx.bin                             open-addressed index, CRC-protected
│   └── <hh>/<hash>.spv
├── pipelines/
│   ├── <device-uuid>/pipeline.cache        VkPipelineCache blob (header validated)
│   └── <device-uuid>/manifest.toml         pipeline pre-warm manifest (D-008)
└── dumps/<session>/                        only when debug dump mode is on
```

Every blob carries a header: magic, format version, translator version, device/driver
identity, and a CRC32. A mismatch is a cache *miss*, never a crash. Writes are
`write-temp + rename`, so a killed process cannot leave a half-written index.

---

## 7. Backend trait vs. command buffer — the concrete boundary

`hematite-ir` defines, in plain data:

```text
Command ::=
  | BindPipeline { pipeline: PipelineId }
  | BindBuffer   { slot, buffer: BufferId, offset, size }
  | BindTexture  { unit, view: ImageViewId, sampler: SamplerId }
  | SetDynamicState { viewport, scissor, blend_constants, depth_bias, … }
  | Draw         { topology, vertex_count, instance_count, first_vertex, first_instance,
                   indexed: Option<IndexRef> }
  | DrawIndirect { buffer, offset, count: Option<u32> }
  | Dispatch     { x, y, z } | DispatchIndirect { buffer, offset }
  | CopyBuffer   { src, src_off, dst, dst_off, size }
  | CopyImage    { src, dst, regions }
  | ClearImage   { image, layout, value }
  | MemoryBarrier{ bits }
  | Present      { swapchain_image }
  | Readback     { image, dst_buffer, region, fence_value }
```

The backend implements a single `submit(&[Command])`. It is deliberately *not* one method
per GL function: the backend is allowed to fuse, reorder within a submission, and drop
no-ops. This is where draw merging and render-pass merging live.

---

## 8. Error, panic and crash policy

- `panic = "abort"` in release. A panic hook writes a crash report (backtrace, GPU
  capability profile, bounding GL state dump, last 64 recorded commands) before aborting.
- In debug builds, every FFI entry point is wrapped in `catch_unwind` so no unwind crosses
  the boundary; the panic still aborts after the report is written.
- **No GL error is ever raised from a callback on another thread.** Errors are latched per
  context and returned by the next `glGetError` on the owning thread, matching GL semantics.
- A failed shader translation is a program-link failure with a real info log — not an abort.
