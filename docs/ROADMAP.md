# Hematite Renderer — Roadmap

Ordered so that each step is *verifiable before the next one is attempted*. The ordering is
the point: the previous phase produced a design and an ABI table but could not compile
anything, and the fastest way to stop repeating that is to make the first milestone "a
build that runs and fails a test", not "more code".

Nothing here has a date. A step is done when its exit criterion is demonstrated, and every
step's status must be updated in `STATUS_REPORT.md` when it is.

---

## Phase 0 — Make the project buildable (blocking)

Native CPU verification is now working. `./build.sh check` passes with Rust 1.85.0;
82 unit tests also pass independently under WASI. Android cross-compilation and GPU
execution now work locally (`./build.sh android`, `./build.sh gpu`). A remote green CI run
and any device execution are still pending.

| Step | Work | Exit criterion |
| --- | --- | --- |
| 0.1 | Get a Rust toolchain with `cargo` into the build environment (or run on a machine that has one) | `cargo --version` succeeds |
| 0.2 | `cargo check --workspace` on the eight existing members | compiles clean |
| 0.3 | `cargo test --workspace` | all 82 written tests pass |
| 0.4 | `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| 0.5 | `cargo fmt --check` | clean |
| 0.6 | `cargo run -p xtask -- check-abi` and `gen-abi` | generates `gen/abi.rs`, `gen/decls.rs`, `abi/expected_symbols.txt` |
| 0.7 | CI workflow running 0.2–0.6 on push | first green run |

The first compile caught and fixed invalid derives and missing imports; native Clippy
caught additional lints. See `ENVIRONMENT.md`. Native checks now pass.

---

## Phase 1 — An ABI that loads

| Step | Work | Exit criterion |
| --- | --- | --- |
| 1.1 | `hematite-gl-frontend` skeleton: real `#[no_mangle] extern "C"` implementations checked against `gen/decls.rs`; no panic stubs | `cdylib` builds for `aarch64-linux-android` |
| 1.2 | `nm -D` diff against `abi/expected_symbols.txt` in CI | every expected symbol present |
| 1.3 | `eglGetProcAddress` returning known entry points by name | unit test: 100 sampled names resolve |
| 1.4 | EGL lifecycle: display, config, context, pbuffer surface, `MakeCurrent`, `GetProcAddress` | a host-app test on an emulator logs a successful init |
| 1.5 | Error machinery: latched `GL_INVALID_OPERATION` for unsupported entries, logged once | unit test |

Milestone: **the library loads in a launcher and does not crash.** No pixels yet.

---

## Phase 2 — First pixels (Vulkan primary)

The CPU foundation is tested. The next graphics milestone follows the brief's priority:
real Vulkan execution before the GLES fallback, with synthetic shaders rather than shader
pack redistribution. This supersedes the earlier GLES-first draft.

| Step | Work | Exit criterion |
| --- | --- | --- |
| 2.1 | **DONE.** Real instance/device capability selection, offscreen render pass, pipeline, vertex upload and readback (`hematite-vk-backend`) | `./build.sh gpu`: capability profile plus five analytic pixel checks on SwiftShader |
| 2.1b | Depth/stencil, textures and samplers, blending, indexed draws, pipeline reuse, async submission | textured quad with checked readback; no per-draw reallocation |
| 2.2 | GL frontend buffer/program/VAO operations feeding that executor | a triangle from real GL calls, not a fabricated screenshot |
| 2.3 | Texture upload, sampler and indexed draws | textured quad with checked readback |
| 2.4 | Android `ANativeWindow*`, surface, swapchain and EGL ownership | same scene presented on a device/emulator |
| 2.5 | Validation-layer and lifetime/error tests | no validation errors in the synthetic scene |

Milestone: **a triangle from a real GL call sequence through Vulkan**. Only then can GPU
rendering, rather than CPU helper behaviour, be marked VERIFIED. The GLES fallback follows
with real driver dispatch, shader translation and independent visual tests.

---

## Phase 3 — Vulkan backend

| Step | Work | Exit criterion |
| --- | --- | --- |
| 3.1 | Instance/device selection, capability profile struct, feature probing | profile dumped for lavapipe and for one real device |
| 3.2 | Allocator + staging ring + frame-in-flight fences | allocation test with 10k buffers |
| 3.3 | Command recording, pipeline creation (sync first) | triangle via Vulkan |
| 3.4 | Render pass / dynamic rendering with deferred begin | pass-count assertion in a debug counter |
| 3.5 | Layout and barrier tracking per subresource | validation layers clean on a test scene |
| 3.6 | Descriptor strategy: pool + layout cache, then indexing where present | zero per-draw allocations, asserted |
| 3.7 | Async pipeline compile with skip policy + pre-warm manifest | stutter test shows bounded frames |
| 3.8 | Pipeline cache persistence, validated by device identity | second launch has warm cache |

Milestone: **the Phase 2 scene renders through Vulkan**, and validation-layer output is
clean in debug builds.

---

## Phase 4 — Modern Minecraft

| Step | Work | Exit criterion |
| --- | --- | --- |
| 4.1 | Core-profile context policy, correct `GL_VERSION`/`GL_EXTENSIONS` strings | MC 1.20+ reaches the main menu |
| 4.2 | Full GL 3.3 object model (FBOs, MRT, samplers, queries, sync) | MC 1.20+ renders a world |
| 4.3 | `buffer_storage`, persistent mapping, `multi_draw_indirect`, base-instance | Sodium-class chunk rendering path |
| 4.4 | UBO rings, SSBOs, compute dispatch, image load/store | Iris gbuffers + composite pass |

---

## Phase 5 — Legacy Minecraft (the fixed-function engine)

| Step | Work | Exit criterion |
| --- | --- | --- |
| 5.1 | Matrix stacks (CPU tests pass); `glPush/PopAttrib` still missing | unit tests + a lit scene |
| 5.2 | Immediate capture/conversion (CPU tests pass); GPU ring integration missing | MC Alpha-era geometry renders |
| 5.3 | Fixed-function variant shaders keyed by state word | variant count and cache-hit rate logged |
| 5.4 | Display-list baking (D-006) | MC 1.12 chunk rendering at parity |
| 5.5 | Client-array upload + content-identity cache (D-012) | legacy MC frame time measured |

---

## Phase 6 — Shader packs

| Step | Work | Exit criterion |
| --- | --- | --- |
| 6.1 | Full preprocessor: includes confined to pack root, guards, cycles, option defines | preprocessing corpus passes |
| 6.2 | Legacy-to-modern rewrite for the whole GLSL 1.10–4.60 surface | corpus of tricky snippets |
| 6.3 | `const` directives: `colortexNFormat`, `shadowMapResolution`, clear/mipmap flags | pack buffers created with the declared formats |
| 6.4 | `colortex` ping-pong, shadow map with depth-compare samplers | a pack's shadow pass matches |
| 6.5 | Program-stage ordering: `gbuffers_*`, `shadow`, `deferred`, `composite`, `final` | a full frame from a real pack |
| 6.6 | Compute, SSBO, image load/store, `texture_barrier` | Iris path at parity |

Milestone: **one real shader pack renders a correct frame.** Which pack, and how it differs,
gets recorded in `COMPATIBILITY.md` at that moment.

---

## Phase 7 — Performance, packaging and proof

| Step | Work | Exit criterion |
| --- | --- | --- |
| 7.1 | Profiler: frame time, draw calls, barriers, passes, upload bandwidth, cache hits | overhead < 1% when disabled, measured |
| 7.2 | Benchmark harness with reproducible scenes | `BENCHMARKS.md` has real numbers and hardware |
| 7.3 | Quirks database, every entry either reproduced or marked unverified | file exists with honest flags |
| 7.4 | Android project, `cargo-ndk`, APK, debug keystore | APK installs on a device |
| 7.5 | V2 plugin metadata + built-in `RenderSpec` compatibility | loads in Aerix, ZL2 and FCL |
| 7.6 | Fuzz targets with reported iteration counts | no crashes in a stated duration |
| 7.7 | `COMPATIBILITY.md` filled from actual runs | every row has a label and evidence |

---

## What to delete if scope must shrink

In this order, because these add the least per unit of risk:

1. **Test harness golden images** — keep the scenes, drop pixel comparison.
2. **GLES backend** — Vulkan-only is defensible on most 2020+ hardware; drop it and the
   fallback becomes "use the launcher's default renderer".
3. **Render scale and upscaling** — costs a full-screen pass and a tuning effort.
4. **Thermal adaptation** — already unimplemented.

Do **not** cut, in this order: display-list baking, the shader cache, async pipeline
compilation. Those three are the difference between "works" and "works fast enough to use",
which is the entire premise of the project.
