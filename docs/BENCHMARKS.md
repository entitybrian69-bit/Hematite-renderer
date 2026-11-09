# Benchmarks

## There are no results

No graphics benchmark has been run. CPU libraries are built and unit-tested, and the Vulkan
backend renders offscreen on the SwiftShader software driver — which measures nothing about
a phone: it is a CPU rasterizer, every render reallocates its resources, and there is no
frame loop, swapchain or texture path to time. There is no device or emulator
(`docs/ENVIRONMENT.md`). This document therefore contains **no numbers**, and specifically
contains no invented ones.

What follows is the harness that must exist before any number can be published, and the rules
for publishing one. It is a specification, not a report.

## Why a harness before an implementation

The project's entire premise is speed, and "it feels smoother" is not a measurement. The
failure mode to avoid is the one this document's absence would represent: a renderer shipped
with performance claims that turn out to be one device's warm-cache best case.

So the harness is a Phase 7 deliverable (see `ROADMAP.md`) with a hard rule attached: **a
number appears in this file only together with its methodology block** (below). No exceptions,
including for numbers that look obviously right.

## Required methodology block for every published number

```
Build:        <git commit>, cargo profile (release/profiling), target triple
Device:       <exact model>, <SoC>, <GPU>, <driver version from VkPhysicalDeviceProperties>
OS:           <Android version, vendor skin if it changes scheduling>
Minecraft:    <version>, <loader>, <mods with versions>
Render:       <backend>, <config keys that matter: render_scale, msaa, present_mode>
Scene:        <named, reproducible: seed, coordinates, camera path, duration>
Measurement:  <tool: own profiler / GPU timestamp queries / systrace>
Samples:      <N runs>, <how warm-up was excluded>
Reported:     median frame time, 1% low, 0.1% low, CPU frame time, GPU frame time
Caveat:       <what this does NOT show>
```

`0.1% low` and `1% low` are computed from the full frame-time distribution, not from
averaged FPS, because a stutter is what users feel.

## The measurement tool

Two layers, both inside Hematite (`docs/ARCHITECTURE.md` §7):

1. **Always available**: CPU frame time, draw calls, state changes, pipeline binds,
   descriptor updates, barrier count, render-pass count, upload bytes, shader
   translate/compile time, cache hit rate, peak GPU memory. Overhead must be near zero when
   the profiler is off — enforced by a compile-time feature *and* a runtime switch, and
   verified by measuring a disabled run against a build with the profiler compiled out.
2. **GPU timing**: `vkCmdWriteTimestamp` around passes, aggregated per frame. On Android the
   `timestampPeriod` and `timestampValidBits` must be checked per device; some drivers report
   unreliable values, and the quirks database exists partly to record that.

## Scenes

Each must be reproducible from a seed and a camera path, and each isolates a cost:

| Scene | What it isolates |
| --- | --- |
| Draw-call throughput | per-submission and per-draw CPU cost, with and without merging |
| State-change throughput | the frontend dedup path: 1M alternating `glBindTexture`/`glUseProgram` |
| Upload bandwidth | `glBufferSubData` and `glMapBufferRange` streaming, MB/s before a stall |
| Texture upload | `glTexImage2D` of 1024² RGBA, uncompressed and BC-transcoded |
| Shader compile stutter | number of frames exceeding 16.7 ms when a new pipeline appears, with each `pipeline_policy` |
| Display-list replay (legacy) | `glCallList` cost for a 1.12-style chunk list, baked vs verbatim |
| Chunk rendering | Sodium-style multi-draw-indirect with 5k–50k draws |
| Deferred pass chain | 8-MRT ping-pong, 4 compositing passes — the shader-pack shape |
| Compute | dispatch grid with SSBO atomics, as a shadow/voxel pass does |

## Comparisons

The brief asks for comparisons against GL4ES, MobileGlues and Zink.
**No comparison has been run**, and none may be published unless:

- the competitor build is named and versioned,
- the same device, Minecraft version, world and camera path are used,
- the same measurement tool produces both sets of numbers,
- the differences in features (a renderer that cannot run the shader pack is not "faster"
  at it) are stated next to the numbers.

A comparison that drops these is worse than no comparison, because it will be quoted.

## What will be reported, and what will not

**Will:** frame time distributions, CPU/GPU split, stutter counts, memory peaks, compile
latencies, cache hit rates, and the cost of every non-default setting.

**Will not:** FPS rounded to a single number, numbers without the methodology block, numbers
from a different scene described loosely as "the same", or any figure produced by a model
rather than a measurement.
