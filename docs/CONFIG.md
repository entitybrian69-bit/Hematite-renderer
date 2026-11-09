# Configuration

**Scope:** configuration parsing/resolution is CPU-tested. Backend selection, pacing and
other runtime effects described below are planned semantics, not implemented renderer
behaviour. No GPU backend or launcher plugin exists yet.

Two sources, one precedence order. For any key:

1. `HEMATITE_<KEY>` in the environment — highest priority. Launchers can set these.
2. `<key> = <value>` in the config file.
3. The built-in default.

Environment wins so that a launcher can override a user's file without editing it. A key
that fails to parse keeps its default and is reported; **a bad value never stops the
renderer from starting**, because refusing to launch over a typo is worse than logging it.

Unknown keys are ignored, not rejected: a newer launcher may set a key this build does not
know yet.

The authoritative key list lives in code as `hematite_config::KEYS`, and a unit test fails
if a key is added there but not handled. This document mirrors that list.

## Keys

| Key | Env var | Type | Default | Meaning |
| --- | --- | --- | --- | --- |
| `backend` | `HEMATITE_BACKEND` | `auto` \| `vulkan` \| `gles` | `auto` | `auto` chooses from the capability profile and the quirks database. `vulkan`/`gles` force one backend and fail loudly instead of falling back. Takes effect on next launch. |
| `present_mode` | `HEMATITE_PRESENT_MODE` | 0–3 | `0` | 0 = choose, 1 = FIFO (vsync), 2 = mailbox, 3 = immediate. Unsupported modes fall back to FIFO. |
| `fps_limit` | `HEMATITE_FPS_LIMIT` | integer | `0` | Frame cap; 0 = unlimited. Applied by the frame pacer, not by sleeping in `eglSwapBuffers`. |
| `render_scale` | `HEMATITE_RENDER_SCALE` | 25–400 | `100` | Percentage of native resolution. Below 100 renders offscreen and upscales. |
| `msaa` | `HEMATITE_MSAA` | 1, 2, 4, 8 | `1` | Sample count for the default framebuffer. 1 disables MSAA. Clamped to `maxColorAttachments`/format support. |
| `anisotropy` | `HEMATITE_ANISOTROPY` | integer | `0` | Requested anisotropic filtering level; 0 disables. Clamped to `maxSamplerAnisotropy`. |
| `pipeline_policy` | `HEMATITE_PIPELINE_POLICY` | `skip` \| `block` \| `wait` | `skip` | What a draw does when its pipeline is still compiling. `skip` is the default because a brief missing draw is less visible than a stall. |
| `pipeline_skip_frames` | `HEMATITE_PIPELINE_SKIP_FRAMES` | integer | `3` | How many frames a draw may be skipped before the pipeline is created synchronously, so a draw can never disappear forever. |
| `shader_cache` | `HEMATITE_SHADER_CACHE` | on/off | `on` | Persistent SPIR-V and pipeline caches. |
| `shader_cache_mb` | `HEMATITE_SHADER_CACHE_MB` | integer | `256` | Cache size cap in MiB; LRU eviction above it. |
| `fast` | `HEMATITE_FAST` | on/off | `on` | Fast mode skips validation that can only produce wrong pixels. `off` selects strict mode: every mistake becomes a real GL error. Anything that could crash is always checked. |
| `log_level` | `HEMATITE_LOG_LEVEL` | `off` \| `error` \| `warn` \| `info` \| `debug` \| `trace` | `warn` | `trace` costs real time and is for diagnosis only. |
| `shader_dump` | `HEMATITE_SHADER_DUMP` | on/off | `off` | Writes original, rewritten, SPIR-V and reflection per shader into a per-session directory. |
| `profiler` | `HEMATITE_PROFILER` | on/off | `off` | Per-frame counters: draw calls, state changes, barriers, passes, upload bytes, cache hits. Near-zero cost when off. |
| `worker_threads` | `HEMATITE_WORKER_THREADS` | integer | `0` | Translation/compile worker count; 0 = automatic (half the cores, minimum 1). |
| `memory_budget_mb` | `HEMATITE_MEMORY_BUDGET_MB` | integer | `0` | GPU memory cap in MiB; 0 = unlimited. Also drives eviction under `VK_EXT_memory_budget`. |
| `gl_version` | `HEMATITE_GL_VERSION` | string | *(computed)* | Overrides the advertised `GL_VERSION`. Needed to pass mods' version gates. Example: `4.6.0 Hematite`. |
| `gl_renderer` | `HEMATITE_GL_RENDERER` | string | *(computed)* | Overrides `GL_RENDERER`. Some packs branch on the renderer string. |
| `gl_vendor` | `HEMATITE_GL_VENDOR` | string | *(computed)* | Overrides `GL_VENDOR`. |
| `disable_quirks` | `HEMATITE_DISABLE_QUIRKS` | comma list | *(empty)* | Quirks entry ids to switch off, for isolating a suspected workaround. |

Booleans accept `1/0`, `true/false`, `on/off`, `yes/no`, `enabled/disabled`, case-insensitive.

## File format

```ini
# <app files>/hematite/config.txt
backend = auto
render_scale = 85
msaa = 4
pipeline_policy = skip
log_level = info
gl_renderer = Hematite (Adreno 740)
disable_quirks = adreno-disable-eds2, example-force-gles
```

`#` starts a comment only at the beginning of a line. Trailing `# ...` after a value is
**part of the value**, deliberately — a `GL_RENDERER` string may legitimately contain one.

## Variables Hematite reads but does not own

Pojav-derived launchers set these. Hematite records them and honours the ones that describe
what the host expects; it does not let them override `HEMATITE_*`.

| Variable | Hematite's use |
| --- | --- |
| `POJAV_RENDERER` | Logged. Values are `opengles2`, `opengles3`, `vulkan_zink`, … |
| `POJAVEXEC_EGL` | Logged; confirms which path the host loaded. |
| `LIBGL_GL` | **Honoured as a default for `gl_version`.** gl4es-style encoding: `31` = 3.1, `21` = 2.1, `0` = unset. |
| `LIBGL_ES` | Used as the ES major version to request from the underlying driver. |
| `LIBGL_EGL` | The underlying EGL to load (`libEGL.so`, `libEGL_angle.so`). |
| `LIBGL_GLES` | The underlying GLES to load (`libGLESv2.so`, `libGLESv2_angle.so`). |
| `LIBGL_USE_MC_COLOR`, `LIBGL_NORMALIZE`, `LIBGL_NOINTOVLHACK`, `LIBGL_NOERROR` | **Ignored.** These change gl4es's behaviour, not Hematite's. Logged once at `info` so a user is not left wondering. |

## How the config is resolved in code

```rust
let (cfg, problems) = hematite_config::Config::from_env(file_body);
for p in problems {
    // p.key, p.value, p.source ("env" | "file"), p.reason
}
```

`problems` is never fatal. See `crates/hematite-config/src/lib.rs` and its tests.
