# Hematite Renderer — Known Limitations

Two kinds of entry live here, and they are kept separate on purpose:

- **Environment limitations** — things this development session could not do, which are
  not properties of the design.
- **Design and scope limitations** — things Hematite will not do, or will do imperfectly,
  even once it is finished. These are the ones a user will feel.

Status labels follow `STATUS_REPORT.md`: VERIFIED, BUILT-BUT-UNTESTED, DESIGNED,
NOT-IMPLEMENTED.

---

## 1. Environment limitations (this session, not the design)

| # | Limitation | Consequence |
| --- | --- | --- |
| E-1 | Official Rust distribution server unreachable; bundled npm tools work | Native Rust 1.85.0 check/Clippy/fmt/test now pass; WASI checks also pass. Toolchain provenance is documented in ENVIRONMENT.md. |
| E-2 | No JDK, Android NDK or Gradle | An APK *is* produced, but by hand-written tooling: `tools/android_linker.py` links with Zig's `ld.lld` against generated system-library stubs, and `tools/build_apk.py` writes binary AXML, the ZIP and both signature schemes. No `aapt2`, `apksigner`, `nm -D` or Gradle verification exists. |
| E-3 | No Android device or emulator | Zero on-device behaviour has been observed. The APK has never been installed; the arm64 library has never been loaded by a dynamic loader. |
| E-4 | No hardware Vulkan driver and no validation layers | Frames *are* rendered and read back, but only on SwiftShader (CPU, Vulkan 1.3.0). A lenient software driver can hide synchronization, layout and lifetime mistakes that a real driver or the validation layers would reject. |
| E-5 | crates.io unreachable | `ash` 0.38.0 is vendored from its GitHub tag with a reduced, dependency-free Cargo manifest (`vendor/ash/UPSTREAM.md`). Every other dependency (`vulkano`, `naga`, `gpu-allocator`, `glslang` crate, …) is still *planned*, not proven. |
| E-6 | No Android runtime verification | The arm64 `.so` is verified structurally (ELF, `DT_NEEDED`, alignment, exports) and by the absence of glibc/musl spellings. Whether bionic actually resolves all 45 undefined symbols at `dlopen` is untested. |
| E-7 | Symbol-to-library assignment is a table | `tools/android_linker.py` maps referenced symbols to `libc/libm/libdl/liblog/libunwind`; anything unlisted is assumed to be in `libc.so`. The NDK would have proven this at link time. A wrong guess appears only as a load-time failure on a device. |

**The direct consequence: there are no benchmark numbers, no device results and no
compatibility claims anywhere in this repository.** `STATUS_REPORT.md` §1 says so; this
file repeats it so that a reader who opens only one document still sees it.

---

## 2. Design and scope limitations

### 2.1 Things Hematite will never do

| # | Limitation | Reason |
| --- | --- | --- |
| D-1 | **No native Vulkan Minecraft renderer.** | Minecraft Java speaks OpenGL. Hematite translates GL; it does not replace the game's renderer. A future Minecraft with a Vulkan-native renderer would need a different integration entirely, and that is out of scope. The brief asked for this to be documented; this is it. |
| D-2 | **No perfect transform feedback.** | Core Vulkan does not provide it; `VK_EXT_transform_feedback` is device-dependent. Emulation via SSBO writes in the vertex shader changes memory-visibility timing, breaks `GL_RASTERIZER_DISCARD` semantics in edge cases, and cannot preserve query results exactly. Packs that depend on exact `glBeginTransformFeedback` timing will differ. |
| D-3 | **No exact display-list timing.** | Display lists are baked into immutable buffers (D-006). `glIsList`, nested-list semantics and `GL_COMPILE_AND_EXECUTE` are planned, not implemented; but the *cost profile* is different from a desktop driver's, so a program using lists as a timing side channel would see different behaviour. |
| D-4 | **No OpenGL 4.6 conformance.** | The target is "what Minecraft, Sodium, Iris and OptiFine actually use". Features outside that set must be explicitly rejected and documented, not silently faked. In particular: no `GL_ARB_gpu_shader_int64` emulation beyond storage, no bindless `glMakeTextureHandleResidentARB`, no `GL_NV_*` vendor paths unless research shows a launcher needs them. |
| D-5 | **Geometry and tessellation shaders depend on the device.** | Many mobile drivers do not expose them. Where absent, the affected pack is disabled with a logged reason rather than emulated (D-005's sibling decision). Emulation is a possible later addition, not a promise. |
| D-6 | **Not bit-exact with the desktop driver.** | Floating-point differences (precision qualifiers, FMA contraction, different texture filtering implementations) mean a translated shader can differ by a pixel or two from the same shader on a desktop GPU. This is inherent to translation, not a bug to fix. |

### 2.2 Things that are harder than they look

| # | Limitation | Reason / mitigation |
| --- | --- | --- |
| H-1 | **Descriptor indexing is not universal.** | Without `VK_EXT_descriptor_indexing`, per-draw descriptor writes return. Mitigation: cached set layouts plus a per-frame bump pool (D-009). Cost measured? No. |
| H-2 | **Pipeline compile stutter may still be visible.** | The skip policy trades a missing draw for a stall (D-008). On a slow driver the 4th-frame synchronous create can still hitch once per pipeline. Mitigation: pre-warm manifest. Unproven without a device. |
| H-3 | **BC/DXT transcoding costs time and memory.** | Every DXT texture is decoded and re-encoded to ASTC or RGBA8 on upload (D-011). Large packs pay this once, but the cache must be persistent or it repeats every launch. |
| H-4 | **Render-pass merging is a heuristic.** | Merging compatible FBO binds is only sound when no read of the previous contents intervenes. A pack that reads a texture it is also writing through a different attachment can defeat the tracking and produce a wrong frame unless a `glTextureBarrier` is issued — which is the pack's responsibility per the GL spec, and Iris packs do issue it. |
| H-5 | **`glGetError` in fast mode is not a full validator.** | Fast mode skips checks that can only produce wrong pixels (D-013). A user debugging a broken pack must switch to strict mode; that switch is a relaunch. |
| H-6 | **Thermal adaptation is optional and unmeasured.** | Reading Android thermal status through JNI from a renderer `.so` is possible but invasive, and every tuning threshold would be a guess without hardware. Not implemented. |
| H-7 | **Pre-rotation requires driver cooperation.** | Where `currentTransform` is reported, pre-rotation avoids a compositor pass; where it is not, the compositor rotates and the cost is unavoidable. |

### Implemented CPU-module boundaries (NOT-IMPLEMENTED beyond these boundaries)

- **Fixed function:** matrices and vertex/index assembly are tested, but no matrix-mode
  frontend, attribute stacks, lighting, texenv combiners, texgen, fog/alpha-test shaders,
  flat-shading provoking-vertex handling, edge flags, display lists, client-array upload,
  GPU ring integration or draw submission exists. Topology conversion currently targets
  smooth-filled geometry only. Eight captured texture coordinate sets are a CPU data
  layout, not an advertised GL device limit. The assembler returns a capacity error
  rather than silently growing; a future frontend must flush/chunk safely or report it.
- **Cache:** record codec, SHA-256 keys, CRC32 and Unix-style atomic disk replacement are
  tested; no compiler/pipeline integration, disk quota/eviction, async IO, stale temp-file
  collection or pipeline-specific device-header validation exists. Caller-supplied key
  fields must include all compiler/options/device identity. Payloads are opaque, not
  validated SPIR-V. The directory must be app-private. CRC32 is not authentication;
  directory metadata is not fsynced, so power loss may lose an entry.
- **IR:** command/state data is an initial subset, not a complete GL object model. Separate
  RGB/alpha blending and full stencil masks/ops are not represented. Line width is quantized
  to eighths; this must not be advertised as arbitrary desktop GL precision. `SetState`
  allocates a boxed snapshot; no zero-allocation rendering claim is verified.
- **ABI:** all 866 entry points are exported with their declared signatures and resolve
  through `eglGetProcAddress`, but only 117 do real work; the other 749 record the call
  (`hematite_gl_unsupported_count`) and return zeros. The table in `abi/gl_export.def` is
  hand-curated, not audited against real GL headers, so a wrong parameter type in the table
  would be reproduced faithfully by the compiler check that is supposed to catch drift.
- **Android and backends:** the Vulkan backend renders offscreen only. There is no
  swapchain, surface, `ANativeWindow`, presentation, texture upload, depth/stencil,
  blending, MSAA, indexed draw, descriptor management, pipeline cache reuse, async
  submission or command-buffer recycling. Every render allocates a fresh image, buffers,
  render pass, pipeline, pool and fence, so the code path is a correctness harness and not
  a frame loop. The plugin library exports 12 `hematite_*` functions and all 866
  `gl*`/`egl*` symbols; the Vulkan path and the GL path do not talk to each other.
- **Plugin discovery:** the APK now satisfies the v1 contract
  (`docs/PLUGIN_CONTRACT.md`), but the launcher has never been run against it, and the
  declared `org.hematite.renderer.PluginInfo` activity has no implementation (the APK
  contains no dex), so it exists only so `queryIntentActivities(ACTION_MAIN)` finds the
  package. The v2 `fclPlugin_V2` JSON contract is not implemented because it needs a
  `resources.arsc` string resource this encoder does not write.
- **GL frontend:** the fixed-function subset renders on a **software rasterizer** on the
  CPU. GLSL is not compiled — `glCompileShader` always fails with an explanatory log, so
  `glLinkProgram`/`glUseProgram` cannot activate a shader pipeline and any real GLES2
  application, Minecraft above all, will not run. No stencil buffer, no multisampling, no
  renderbuffers/FBOs, no instancing, no `glMapBuffer`, no point sprites, no compressed
  textures, one texture unit honoured, lines are always one pixel wide, and
  `glPolygonOffset`/`glHint`/`glTexEnvi` are accepted and ignored.
- **EGL:** `eglGetDisplay`/`eglChooseConfig`/`eglCreateContext` and friends are a software
  EGL. `eglCreateWindowSurface` accepts an `ANativeWindow*` but cannot query or lock it
  (that needs the NDK, which is never linked), so a window surface behaves like a pbuffer:
  `eglSwapBuffers` copies the frame into a snapshot and counts the swap, and **no pixels
  ever reach a display**. Contexts are per-thread handles with no sharing.
- **APK:** signed with a throwaway self-generated certificate, `targetSdkVersion 28` so
  the v1 (JAR) signature alone is acceptable, and verified only by apksigtool 0.1.0 (which
  calls itself a prototype) and androguard. No Play Store, no `bundletool`, no resources,
  no Java, no `uses-feature` declarations, and no installation test.

### 2.3 Quirks database: nothing is confirmed

The quirks structure is designed, but **no entry may be described as confirmed**, because
confirming one requires reproducing a driver bug on that driver. `docs/RESEARCH.md` §5
lists the vendor *classes* that are likely to need entries; none of them is evidence.
A future session must mark every entry it has not reproduced as unverified, and the file
format must carry that flag per entry.

### 2.4 Testing: what is missing entirely

- **82 unit tests pass** on native Linux and under WASI (14 IR, 16 config, 25 fixed-function
  CPU, 10 cache, 7 xtask, 5 plugin, 3 vk-backend, 2 testharness). `./build.sh gpu`
  additionally runs the real-driver suite against SwiftShader.
- The harness covers only: RGBA8 clear, one triangle list, smooth RGBA interpolation,
  repeated renders and a 1×1 target. Of the brief's list (triangle, textured quad,
  fixed-function scene, display lists, MRT, shadow map, compute, SSBO atomics, instancing,
  multi-draw-indirect, MSAA resolve, render-to-texture) only the triangle and readback
  exist. The rest is **NOT-IMPLEMENTED**.
- No fuzz targets. The brief asks for preprocessor/rewriter/translator fuzzing and GL
  argument fuzzing; neither exists.
- No validation-layer runs; the layers are not available in this environment.
- No golden images. Pixels are checked against a CPU-computed analytic reference instead,
  which is stronger than a stored image for interpolation but says nothing about how a real
  GPU rasterizes.

### 2.5 Dependency posture is unproven

`DECISIONS.md` justifies a dependency-light approach, and the CPU libraries have **zero**
dependencies so they stay testable without a registry. The Vulkan backend uses the vendored
`ash` 0.38.0 (D-020); everything else the translation layer will need (a SPIR-V producer,
an allocator, an EGL binding) has still not been built against. Any claim in `DECISIONS.md`
about which translator is more reliable remains an argument, not a measurement.

---

## 3. Assumptions that could invalidate work

These came out of `RESEARCH.md` §2.5 and are repeated here because if one is wrong, code
built on it is wrong:

| # | Assumption | Risk if wrong |
| --- | --- | --- |
| A-1 | The host keeps the `dlopen` handle for the process lifetime | Hematite's globals would disappear |
| A-2 | `mojoexec_renderspec.override_major_version` describes the context the host wants | Wrong ES version requested from the driver |
| A-3 | The `ANativeWindow*` arrives via `eglCreateWindowSurface` | No surface, no output |
| A-4 | `disp_hz` may be 0 | Divide-by-zero in frame pacing |
| A-5 | FCL's `fclPlugin_V2` schema equals the Zalith/Aerix one | That host would not load the plugin |

A-2, A-3 and A-5 can only be settled on a device or by reading `pojavexec` and FCL's
sources, neither of which was possible here.
