# Hematite Renderer — Research

Status of this document: **evidence-based, with sources named.** Every claim about the
launcher contract below was read out of source code in repositories reachable from this
environment, and the file path is given so it can be re-checked. Claims about GPU drivers,
other renderers and Minecraft internals that could **not** be checked against source or a
device in this environment are marked **[UNVERIFIED]**, with the specific reason.

The environment this research was performed in could not run Android, a GPU, a Vulkan
driver, or a Rust compiler (see `docs/ENVIRONMENT.md`). Nothing in this document is
presented as a measurement.

---

## 1. What Hematite is

Hematite Renderer is a GL→(Vulkan | GLES3) translation layer shipped as a renderer plugin
for PojavLauncher-derived Android launchers. Inside the game process it *is* the desktop
OpenGL implementation: LWJGL binds every `gl*` entry point to Hematite's shared object,
and EGL context/buffer-swap calls land in the same object. Hematite translates those calls
to Vulkan 1.1+ (primary) or GLES 3.x (fallback).

This is the same architectural slot as GL4ES, MobileGlues, Zink-in-Mesa, and ANGLE. It is
not a mod, not a Minecraft client, and not a launcher.

---

## 2. Launcher plugin contract

### 2.1 The three-launcher family and what actually differs

| Launcher | Base | Renderer selection mechanism |
| --- | --- | --- |
| PojavLauncher | original | `RenderSpec` implementations compiled into the app; `RendererCache` holds them |
| ZalithLauncher / ZalithLauncher 2 | Pojav fork | external renderer plugins as **separate APKs**, discovered by manifest metadata |
| FCL (Fold Craft Launcher) | forks Pojav's native layer | same APK-plugin idea, `fclPlugin` / `fclPlugin_V2` metadata |
| Aerix Launcher (this account) | Pojav + ZL2 plugin bridge | both: built-in `RenderSpec`s *and* `PojavPluginBridge` discovery |

Source: `entitybrian69-bit/Aerix-launcher`, `app_pojavlauncher/src/main/java/net/kdt/pojavlaunch/`
`aerix/bridge/PojavPluginBridge.kt` — its own header comment records that it was ported from
`MiraiLauncher/src/main/java/com/movtery/zalithlauncher/game/plugin/renderer/` (Zalith
Launcher 2) and lists the accepted host metadata markers verbatim:

```
fclPlugin, fclPlugin_V2 (Fold Craft Launcher), zalithRendererPlugin (Zalith Launcher 2),
pojavRendererPlugin, aerixPlugin, emesisPlugin (the com.emesis package space counts implicitly)
```

**Consequence for Hematite: a plugin APK declares at least `pojavRendererPlugin`,
`zalithRendererPlugin` and `fclPlugin_V2`, so one APK works on ZL2, Aerix, and FCL hosts.**

### 2.2 The binary contract (the part that matters)

The host prepares the renderer through `MojoExec.prepareEgl(path, useBypass, useGles, glesVersion)`
and then hands the *same* handle to LWJGL as its OpenGL library. Read from source:

`MojoLauncher/mojoexec`, `mojoexec.c`:

```c
Java_git_artdeell_mojoexec_MojoExec_prepareEgl(JNIEnv*, jclass, jstring egl_path,
                                              jboolean use_bypass, jboolean use_gles,
                                              jint gles_version) {
    save_jvm_string(env, &native_egl_path, egl_path);
    ...
    void* preload_handle = mojoexec_acq_egl_handle();   // dlopen(egl_path) or nsbypass dlopen
    if(!preload_handle) return false;
    mojoexec_renderspec.force_gles_context  = use_gles;
    mojoexec_renderspec.override_major_version = gles_version;
    return true;
}

void* mojoexec_acq_egl_handle() {
    int flags = RTLD_LOCAL | RTLD_NOW;
    if (egl_use_bypass) return linker_ns_dlopen(get_primary_namespace(), native_egl_path, flags);
    else                return dlopen(native_egl_path, flags);
}
```

`include/mojoexec.h`:

```c
typedef struct {
    int   force_gles_context;
    int   override_major_version;
    int   disp_width;
    int   disp_height;
    float disp_hz;
} mojoexec_renderspec_t;
extern mojoexec_renderspec_t mojoexec_renderspec;
```

`app_pojavlauncher/src/main/jni/jvm_hooks/lwjgl_dlopen_hook.c` in Aerix shows how LWJGL is
redirected. `org.lwjgl.system.linux.DynamicLinkLoader.ndlopen` is replaced with:

```c
if (strstr(filename, "libvulkan.so") == filename) return (jlong) mojoexec_acq_vulkan_handle();
if (strstr(filename, "libGLMojo.so")  == filename) return (jlong) mojoexec_acq_egl_handle();
return (jlong) dlopen(filename, mode);
```

**Therefore, three hard requirements on Hematite's `.so`:**

1. It must export the **EGL 1.4/1.5 entry points** (`eglGetDisplay`, `eglInitialize`,
   `eglChooseConfig`, `eglCreateContext`, `eglMakeCurrent`, `eglCreateWindowSurface`
   (surface arrives as an `ANativeWindow*`), `eglSwapBuffers`, `eglSwapInterval`,
   `eglDestroySurface`, `eglQuerySurface`, `eglGetProcAddress`, …) — because it *is* the
   EGL implementation, not a consumer of the system one.
2. It must export the **desktop `gl*` entry points** — because the same `dlopen` handle is
   returned for `libGLMojo.so`, and LWJGL resolves `gl*` from it.
3. `eglGetProcAddress` must resolve the GL extensions Hematite supports; LWJGL uses it for
   everything above GL 1.1 and for all extensions.

### 2.3 Environment variables the host sets

From `EmesisRenderSpec.java` (the built-in renderer spec in Aerix, which mirrors what the
standalone plugin APK declares — its comments say so explicitly):

| Variable | Value | Meaning |
| --- | --- | --- |
| `POJAV_RENDERER` | `opengles3` | tells the host's native glue which renderer family |
| `POJAVEXEC_EGL` | `libemesis_renderer.so` | EGL path used by `pojavexec` |
| `LIBGL_GL` | `31` | desktop GL version the wrapper should expose |
| `LIBGL_ES` | `3` | ES major version to request from the underlying driver |
| `LIBGL_USE_MC_COLOR` | `1` | gl4es-lineage colour handling |
| `LIBGL_NORMALIZE` | `1` | gl4es-lineage normal normalization |
| `LIBGL_NOINTOVLHACK` | `1` | disables a gl4es overflow hack |
| `LIBGL_NOERROR` | `1` | suppresses wrapper-side `glGetError` bookkeeping |
| `SDL_EGL_LIBRARY`, `SDL_OPENGL_LIBRARY` | absolute `.so` path | for SDL-based hosts |

`LIBGL_EGL` / `LIBGL_GLES` (see `GLESConstants.java`) are the variables through which a
wrapper is told which *underlying* EGL/GLES driver to use; values are `libEGL.so` +
`libGLESv2.so` (system), or `libEGL_angle.so` + `libGLESv2_angle.so` (ANGLE).

**Consequence:** Hematite must accept these variables for compatibility but is free to
ignore the gl4es-specific ones (`LIBGL_USE_MC_COLOR`, `LIBGL_NORMALIZE`,
`LIBGL_NOINTOVLHACK`, `LIBGL_NOERROR`) because it is not gl4es. Documenting that is part of
`CONFIG.md`.

### 2.4 The V2 plugin metadata format

`aerix/bridge/RendererPluginV2.kt` parses a JSON block for external plugin APKs. Its
requirements are a precise, host-enforced spec:

```kotlin
rendererId        : String  matching [A-Za-z0-9_.-]{1,128}
displayName       : String  non-blank, <= 160 chars
rendererGLPath    : String  must match lib[A-Za-z0-9_.+-]+\.so and exist in the plugin's
                            native dir (canonicalised; parent must equal the native dir)
rendererEGLPath   : String  same, except the literal "libEGL.so" means "the system EGL"
dlopenLibPaths[]  : must be empty  ("This V2 plugin requires preloading, which this adapter
                                    does not yet support")
env[]             : <= 128 entries, keys matching [A-Za-z_][A-Za-z0-9_]*, no duplicates;
                    types NormalEnv | SelectableEnv | CustomizableEnv | ToggleableEnv
whole JSON        : <= 64 KiB
```

Notes for Hematite:

- `dlopenLibPaths` **must be empty** on any host using this adapter. A renderer that needs a
  preloaded companion library must therefore either (a) be a single `.so`, or (b) load its
  companions itself, or (c) be installed through the built-in `RenderSpec` path instead.
- `nativeLibrary()` rejects paths that escape the plugin's native directory, so the manifest
  must use a bare soname, not a path.
- ABI is validated by reading the ELF header (`matchesElfAbi`): class byte, little-endian,
  and `e_machine` (183 = AArch64, 40 = ARM, 62 = x86-64, 3 = x86).

**Decision taken from this:** Hematite ships as **one** `.so` per ABI that contains the EGL
implementation, the GL frontend and the chosen backend, and loads nothing else at startup
except optional system drivers. This keeps it valid under both the built-in `RenderSpec`
path and the V2 metadata path simultaneously. See `DECISIONS.md` D-002.

### 2.5 Assumptions I had to make

| # | Assumption | Why / risk |
| --- | --- | --- |
| A-1 | The host calls `prepareEgl` *before* LWJGL binds GL, and the handle stays valid for the process lifetime | `prepareEgl` returns a `dlopen` handle; the hook uses it later. If a host ever unloaded it, Hematite's globals would vanish. Hematite therefore never relies on `dlclose`. |
| A-2 | `mojoexec_renderspec.force_gles_context` / `override_major_version` describe the context the *host* wants; Hematite should honour `override_major_version` when creating the underlying driver context | Read from `mojoexec.c`; the field is set but its consumer (`pojavexec`) is a separate submodule not read here. **[UNVERIFIED]** |
| A-3 | The `ANativeWindow*` reaches Hematite through `eglCreateWindowSurface`'s `native_window` argument | Standard EGL on Android; Pojav glues pass the `Surface`. **[UNVERIFIED]** — needs a device. |
| A-4 | `disp_hz` may be 0 (`setDisplayParams` has a `TODO: implement` in upstream mojoexec) | Do not divide by it; treat 0 as "unknown". |
| A-5 | FCL's `fclPlugin_V2` schema is equivalent to the ZL2/Aerix one | The Aerix port documents it as the same format; FCL's own source was not consulted. **[UNVERIFIED]** |

### 2.6 Which symbols LWJGL 2 vs LWJGL 3 resolve

- **LWJGL 2** (MC ≤ 1.12.2) links directly against the process's GL: it resolves `gl*` from
  whatever `libGL` the host reports, and calls `glXGetProcAddress`-equivalents via its own
  `GLContext`. On Android the host replaces that with the renderer handle. Practically:
  everything GL 1.1 must be a **real exported symbol**, and everything above plus extensions
  comes from `eglGetProcAddress`.
- **LWJGL 3** (MC ≥ 1.13) does `dlopen("libGLMojo.so")` then `dlsym`s each function by name
  through `FunctionProvider`. It will call the function pointer even if it is a stub — so a
  *missing* symbol is fine at link time but a `NULL` `dlsym` result is fatal when the game
  calls it. Hematite's export table therefore must be dense: **every** `gl*` name that any
  Minecraft/mod/LWJGL version may ask for should resolve, with unimplemented ones returning
  a safe no-op that raises `GL_INVALID_OPERATION`.

---

## 3. Existing renderers — what to learn, what to beat

### 3.1 GL4ES / Holy-GL4ES

- **Architecture:** C, immediate-mode GL 1.x/2.x → GLES 2.0/3.x. Batches fixed-function
  state into generated GLSL 100 shaders; display lists compiled to cached arrays; client
  arrays copied into a ring VBO.
- **Strengths:** extremely mature for legacy MC; small; no Vulkan dependency; predictable.
- **Bottlenecks:** shader generation on state change; per-draw uniform uploads; no
  multithreading; GLES-only feature ceiling (no compute/SSBO path, so Iris packs mostly fail);
  string-keyed shader cache lookups on the hot path.
- **Hematite takes:** the fixed-function→shader-variant idea, with the variant key hashed to
  a 64-bit integer and resolved through an open-addressed table, and with variants
  pre-warmed.
- **Hematite avoids:** string hashing per draw, and doing translation on the render thread.

### 3.2 MobileGlues

- **Architecture:** a GL-on-GLES wrapper (gl4es-lineage) tuned for Android, with its own
  launcher-side plugin plumbing (`mobileglues/` and `PluginConfigStore.kt` in Aerix), and a
  user-selectable GLES driver underneath.
- **Strengths:** broad real-device hardening, practical quirks handling, good defaults.
- **Bottlenecks:** still GLES-backed, so no Vulkan-only capabilities; feature parity depends
  on the driver below.
- **Hematite takes:** the plugin configuration model and the "let the user override the
  driver" escape hatch.
- **Hematite avoids:** depending on the system GLES driver for the primary path.

### 3.3 Zink (Mesa)

- **Architecture:** gallium "zink" driver translates desktop GL to Vulkan inside Mesa.
  Closest analogue to Hematite's primary path, and the most battle-tested.
- **Strengths:** spec conformance, huge shader corpus, mature barrier and descriptor
  handling, GL 4.6 coverage.
- **Bottlenecks on Android:** large binary, Mesa build/ABI friction, descriptor-set churn
  without `descriptor_indexing`, pipeline-compile stalls, and each release assumes artifacts
  that mobile drivers often lack.
- **Hematite takes:** the capability-profile-driven code path selection (dynamic rendering vs
  render passes, descriptor indexing vs cached sets), and the discipline of per-subresource
  layout tracking.
- **Hematite aims to beat it on:** Android-specific pipeline policy (async compile + skip,
  pre-warm manifests), tiler-aware render-pass merging, and binary size.

### 3.4 ANGLE

- **Architecture:** GL ES → Vulkan/D3D/Metal; excellent validation and shader translator.
- **Strengths:** correctness, translator robustness, shipped on billions of devices.
- **Limits here:** it targets GL**ES** front-end, not desktop GL 1.x–4.6; it does not give
  Minecraft's desktop-GL consumers what they need (fixed-function, display lists, GL 3.3+ core).
- **Hematite takes:** nothing structurally; it is a *sibling* the launcher may choose
  (`libEGL_angle.so`), not a competitor.

### 3.5 VirGL

- **Architecture:** virtual GL over a virtio transport, for VMs/containers.
- **Relevance:** only in emulator/VM setups. **[UNVERIFIED]** whether any of these launchers
  ship it usefully on real hardware.
- **Hematite takes:** nothing.

### 3.6 Where Hematite specifically aims to be faster

1. **No render-thread translation.** GLSL translation, SPIR-V generation and pipeline creation
   happen on worker threads; the render thread only records (see `ARCHITECTURE.md` §4).
2. **O(1) state dedup.** Frontend keeps a shadow state block per context and diffs against the
   backend's last-applied block; known-redundant Minecraft patterns (`glBindTexture` to the
   same unit/target, `glUseProgram` same program, `glEnable` already-enabled) exit early.
3. **Fewer render passes on tilers.** Deferred pass begin/lazy end, merging of compatible FBO
   bindings, `LOAD_OP_DONT_CARE`/`STORE_OP_DONT_CARE` derived from use tracking, transient
   lazily-allocated attachments, in-pass MSAA resolve.
4. **Pipeline stutter elimination.** Asynchronous compile with a skip-draw policy, plus a
   persisted per-version/per-pack pipeline manifest that pre-warms the next launch.
5. **Fixed-function fast path.** Variant shaders keyed by a hashed state word, decoded with
   specialization constants where that costs less than a new variant.

None of these is measured. `BENCHMARKS.md` defines the harness and states plainly that no
numbers exist yet.

---

## 4. Minecraft rendering behaviour to support

### 4.1 What each consumer needs

| Consumer | GL surface used | Version / profile | Notes |
| --- | --- | --- | --- |
| MC Alpha–1.6 (LWJGL 2) | fixed-function only | GL 1.x | display lists, immediate mode, `glBegin/glEnd`, client arrays |
| MC 1.7–1.12.2 (LWJGL 2) | fixed-function + ARB multitexture/`EXT_texture_env_combine` | GL 2.1 compat | OptiFine 1.7–1.12 injects shader programs on top of fixed-function state |
| MC 1.13–1.16 (LWJGL 3) | GL 2.1 compat, some GL 3.2 | GL 3.2 core optional | UBOs appear; VAOs mandatory |
| MC 1.17–1.21.x (LWJGL 3) | core 3.2/3.3, `GL_ARB_*` set | GL 3.2 core | Sodium/Embeddium path |
| Sodium / Embeddium / Rubidium | `buffer_storage` (persistent map), `multi_draw_indirect`, `base_instance`, `draw_indirect`, instancing, UBO rings | GL 3.3+ / 4.3 features | **[UNVERIFIED]** exact minimum per Sodium version — must be re-checked against Sodium source before claiming support |
| Iris / Oculus | compute (`4.3`), SSBO, image load/store, `texture_barrier`, MRT up to 8, `glFramebufferBlit`, shadow samplers, `glDrawBuffers` with `colortex` flip semantics | GL 4.3+ features on a 3.3 core context | the hardest target in this project |
| OptiFine shaders | fixed-function interception + `glProgram*` on legacy, custom uniforms | varies by version | dual code paths by MC version |

### 4.2 Limits that must be advertised and honoured

| Limit | Typical desktop value | Mobile reality | Hematite plan |
| --- | --- | --- | --- |
| `GL_MAX_TEXTURE_SIZE` | 16384 | 8192–16384 | query `VkPhysicalDeviceLimits.maxImageDimension2D`, advertise exactly |
| `GL_MAX_DRAW_BUFFERS` | 8 | 4–8 | clamp to `maxColorAttachments`; log when a pack wants more |
| `GL_MAX_UNIFORM_BLOCK_SIZE` | 65536 | ≥16384 | clamp; spill oversized blocks to SSBO-emulation |
| `GL_MAX_SHADER_STORAGE_BLOCK_SIZE` | 2^27 | device-defined | reflect from `maxStorageBufferRange` |
| `GL_MAX_COMPUTE_WORK_GROUP_INVOCATIONS` | 1024 | 128–1024 | clamp and rewrite local size where legal, else disable effect |
| `GL_MAX_COLOR_ATTACHMENTS` | 8 | 4–8 | as above |
| `GL_MAX_TEXTURE_IMAGE_UNITS` | 32 | 16–32 | drives combined-sampler descriptor strategy |

Values above are the *desktop* reference points and are **not** device measurements.

### 4.3 Mapping GL features onto Vulkan

| GL feature | Vulkan mechanism | Fallback |
| --- | --- | --- |
| FBO + `glDrawBuffers` | dynamic rendering with multiple colour attachments, or `VkFramebuffer` | renderpass cache |
| `glClear` / `glInvalidateFramebuffer` | `LOAD_OP_CLEAR` / `DONT_CARE`, `STORE_OP_DONT_CARE` | explicit clear draws |
| UBOs | `VkBuffer` + `UNIFORM_BUFFER_DYNAMIC` ring | push constants where size allows |
| SSBOs | `STORAGE_BUFFER` with `VK_KHR_descriptor_indexing` | clamped array with size checks |
| image load/store | `STORAGE_IMAGE`, format qualifiers → `VkImageView` format | format reinterpretation, else disable effect + log |
| texture barrier | `vkCmdPipelineBarrier` with `SHADER_READ|WRITE` | full barrier |
| fences | timeline semaphores (`1.2`) | binary semaphores + `VkFence` |
| queries (timer/occlusion) | `vkCmdBeginQuery`, timestamp queries | CPU-side counters, or unsupported error |
| transform feedback | none in Vulkan | emulate via SSBO writes in the vertex stage |
| geometry/tessellation | optional features | emulate or disable with a logged reason |
| dual-source blending | `VK_EXT_dual_source_blend` / core 1.1 | disable effect |

---

## 5. Android GPU landscape

This section is **[UNVERIFIED]** in the sense that nothing here was measured or tested on a
device in this environment. It is written to drive the quirks database's structure; every
entry must be re-validated before it is trusted. `quirks/*.toml` marks unverified entries.

### 5.1 Vendors

| Vendor | Renderer family | Tiler? | Practical notes for a translator |
| --- | --- | --- | --- |
| Qualcomm Adreno | proprietary | yes (binning) | render-pass breaks are expensive; prefers `LOAD_OP_DONT_CARE`; LRZ sensitive to `gl_FragDepth` writes; historically inconsistent with some `VK_EXT_*`; pipeline compile cost noticeable |
| Arm Mali | proprietary | yes (tile-based, AFBC) | very sensitive to attachment load/store ops and to `glReadPixels`; benefits most from merged passes and transient attachments; geometry shaders often absent |
| Imagination PowerVR | proprietary | yes (TBDR, deferred) | depth/stencil feedback hazards; lazily-allocated attachments strongly preferred; some drivers dislike large descriptor counts |
| Samsung Xclipse (RDNA) | AMD-derived | immediate | closest to desktop behaviour; less tiler-specific tuning needed |

### 5.2 Features the capability profile must probe

`VK_EXT_extended_dynamic_state` (1/2/3), `VK_EXT_graphics_pipeline_library`,
`VK_EXT_shader_object`, `VK_KHR_dynamic_rendering`, `VK_EXT_descriptor_indexing`,
`VK_KHR_push_descriptor`, `VK_EXT_descriptor_buffer`, `VK_KHR_buffer_device_address`,
`VK_KHR_timeline_semaphore`, `VK_KHR_synchronization2`, `VK_EXT_memory_budget`,
`VK_EXT_robustness2`, `VK_EXT_swapchain_maintenance1`, `VK_EXT_present_mode_fifo_latest_ready`,
storage-16bit/8bit, subgroup operations, `VK_EXT_astc_decode_mode`, `VK_EXT_texture_compression_astc_hdr`.

Each is used only when present, with a fallback that is exercised by a test.

### 5.3 What cannot be determined without a device

- Which of the above actually work on a given driver build.
- Real pipeline-compile latencies and whether async compile hides them.
- Which driver bugs exist. **No quirk entry may be described as confirmed without a
  reproduction on hardware.**

---

## 6. Textures and compression (a real correctness trap)

Minecraft's classic textures are **BC1/BC2/BC3 (DXT1/3/5)** in `.mcmeta`/atlas form only in
some resource packs; vanilla uses PNG decoded to RGBA8. Shader packs and mods do ship DXT
textures. Android GPUs **do not** support BC by default; most support ASTC and ETC2.

Options: (a) request `VK_EXT_texture_compression_astc`/BC decode extensions where present,
(b) transcode BC→ASTC or BC→RGBA8 on worker threads, (c) advertise
`GL_EXT_texture_compression_s3tc` anyway and transcode at upload.

Hematite plan: transcode on worker threads into ASTC when the device supports it, else
RGBA8; never claim S3TC support that is not actually backed. This is a **behavioural**
compatibility decision recorded as D-011.

---

## 7. What this environment could and could not do

| Activity | Possible here | Evidence |
| --- | --- | --- |
| Read launcher/plugin/mojoexec sources | yes | `gh api` against `entitybrian69-bit/*`, `MojoLauncher/mojoexec` |
| Compile Rust | **yes** (since 2026-11-09) | pinned npm `@rustbin` toolchain; `static.rust-lang.org` itself is still unreachable (`ENVIRONMENT.md`) |
| Cross-compile for Android | **yes**, without an NDK | Zig `ld.lld` + generated bionic stubs (`D-022`); structural verification only |
| Run Android / NDK / Gradle | **no** | no JDK, no NDK, no emulator, no device |
| Run a Vulkan driver | **yes**, software only | SwiftShader extracted from a pinned npm package; no hardware GPU, no validation layers |
| Execute GL call streams | **no** | no GL frontend exists, so nothing resolves a `gl*` call |

The consequences for deliverable status labelling are spelled out in `STATUS_REPORT.md` §1.
