# Hematite Renderer — Status Report

Updated 2026-11-09. **GLSL now compiles to SPIR-V, draws are routed to a real Vulkan device,
and a signed plugin APK exists — but this is still not a Minecraft renderer.** A compiled
program is not executed yet, no frame is presented to a display, and no device has run the
APK.

Labels:

- **VERIFIED**: compiled and executed, or exercised by a named check. Scope is explicit.
- **BUILT-BUT-UNTESTED**: compiled artifact exists but its intended behaviour was not run.
- **NOT-IMPLEMENTED**: required implementation absent. Written designs do not change this.

## 1. Verification evidence

Native tools: Rust/Cargo 1.85.0, Clippy, rustfmt, x86_64 Linux, obtained from pinned npm
packages (`tools/setup_native_rust.py`). Additional pinned tools (`tools/setup_android_tools.py`):
Zig 0.17.1 for cross linking, `rust-std` for `aarch64-linux-android`, the SwiftShader
software Vulkan driver, glslang (WASM), and the Python packages used to sign and verify the
APK. Provenance and digests are in `ENVIRONMENT.md`. Nothing here is a supply-chain audit.

| Check | Result |
| --- | --- |
| `./build.sh check` (check, Clippy `-D warnings`, fmt, tests, ABI) | VERIFIED, passes |
| `cargo test --locked --workspace` | VERIFIED, 141 unit tests pass, 6 ignored by default (all 6 need a driver) |
| `./build.sh gpu` → every driver-backed suite | VERIFIED against SwiftShader: the backend session (pipeline cache, vertex/uniform rings, async pipeline builds, a sampling pipeline), the frontend's routed frame compared pixel-for-pixel with the rasterizer, the compiled-GLSL-on-a-real-driver check (5 modules accepted, a GLSL 1.20 program executed, a graphics pipeline built), and 5 analytic readback checks |
| `cargo test -p hematite-glsl` | VERIFIED, 48 tests: preprocessing (`#include`, `#version`, defines), normalisation to Vulkan-GLSL 450, std140 block offsets, the naga/validate/SPIR-V path, and the pass that strips sampler types naga emits unconditionally |
| `./build.sh shaders` | VERIFIED, checked-in SPIR-V byte-identical to recompiled GLSL (976/380 bytes) |
| Host `libhematite_plugin.so` through its C ABI (Python `ctypes`, SwiftShader) | VERIFIED: abi version, device name, 16×12 render with correct clear/interpolated pixels, short-buffer status, `glOrtho` matrix, destroy |
| `./build.sh gl` → the exported GL/EGL ABI through `ctypes` | VERIFIED: 866/866 entry points resolve by name; EGL display/config/pbuffer/context/make-current; `glClear` + `glReadPixels` give the exact clear colour; an immediate-mode triangle covers the centre pixel `[255,0,0,255]`; `eglSwapBuffers` publishes; demo scene non-flat; short buffer reports status 3; an unimplemented call increments the counter; **a GLSL 1.20 shader and a `#version 330 core` shader both compile, a program with a shadow sampler and MRT compiles, `glBindAttribLocation` before linking is honoured, and a broken shader fails with a real info log**. 32 checks, 0 failures |
| `./build.sh android` | VERIFIED structurally: ELF64 AArch64 `ET_DYN`, 1 788 272 bytes, `DT_NEEDED` = libdl/libunwind/libm/libc, `p_align` 16384, 12 `hematite_*` exports, **866/866 `gl*`/`egl*` exports**, 62 bionic-resolved symbols |
| `./build.sh apk` | VERIFIED: `dist/hematite-debug.apk`; manifest parsed by androguard; **the FCL/Zalith v1 renderer-plugin contract is asserted** (ACTION_MAIN activity, boolean `fclPlugin`, `renderer=hematite:libhematite.so:libhematite.so`, `des`, `pojavEnv`) per `docs/PLUGIN_CONTRACT.md`; attribute IDs match AOSP `public.xml`; `.so` stored and 16 KB aligned; `apksigtool verify` prints `v2 verified` |
| `python3 tools/verify_wasi.py` | VERIFIED, 89 tests + ABI checks under WASI, duplicate export rejected with exit 1 |
| `python3 abi/validate_abi.py`, `xtask check-abi`/`gen-abi` | VERIFIED, 866 internally consistent entries |
| Installation on an Android device | NOT-VERIFIED: no device, emulator or `adb` in this environment |
| A launcher actually discovering/loading the plugin APK | BUILT-BUT-UNTESTED: the manifest matches the contract read from `ZalithLauncher2` source (cited in `docs/PLUGIN_CONTRACT.md`), but no launcher has been run against it |
| Validation layers (`VK_LAYER_KHRONOS_validation`) | NOT-IMPLEMENTED: not present in this environment; correctness relies on the analytic pixel checks |
| Hardware GPU rendering | NOT-VERIFIED: the only driver used is SwiftShader (CPU, reports Vulkan 1.3.0) |
| CI on GitHub | VERIFIED: the four-job workflow (CPU, SPIR-V, SwiftShader, cross-compile + signed APK) is green on the branch; artifacts `hematite-debug-apk` and `swiftshader-readback` are published |

## 2. Component status

| Component | Status | Evidence / missing work |
| --- | --- | --- |
| `hematite-vk-backend` | VERIFIED (offscreen only) | Two paths. `render.rs` is the one-shot harness behind the 5 analytic driver checks. `session.rs` is the persistent one: one device, a pipeline cache keyed by (stages, layout, state, topology), vertex and uniform rings, descriptor sets reused across frames, pipelines built on a worker thread, a per-frame command buffer, and driver rejections recorded once with their reason instead of retried. No swapchain, texture upload, depth/stencil attachment, MSAA or indexed draw. No performance claim. |
| `hematite-gl-frontend` | VERIFIED on host, routed draws VERIFIED against SwiftShader | 866 generated `#[no_mangle]` exports, 117 forwarding to hand-written implementations that the compiler type-checks against the def file. 9 unit tests plus 32 `ctypes` checks through the real ABI. `vulkan.rs` routes eligible triangle draws to a `Session` and composites the readback; the ignored driver test shows one scene of three draws in a single device frame differing from the rasterizer in 1 of 768 pixels (an edge pixel). No window, no stencil, no MSAA. |
| `hematite-plugin` (`libhematite.so`) | VERIFIED on host, BUILT-BUT-UNTESTED on Android | 12 exported `hematite_*` functions over the GL frontend, the backend and `Mat4::ortho`; 5 unit tests; exercised end-to-end on x86_64 through `ctypes`. The arm64 build is structurally verified only — never loaded by a loader. |
| `hematite-testharness` | VERIFIED | Analytic reference (barycentric interpolation, coverage bounds, 1×1 and repeat renders) compared against driver pixels; PPM writer for the actual readback. |
| `hematite-ir` | VERIFIED (CPU data model only) | 14 tests. No backend consumes these commands yet; the Vulkan backend takes its own vertex struct. |
| `hematite-config` | VERIFIED (CPU resolution only) | 16 tests. No runtime effect: the backend reads no configuration. |
| `hematite-fixed-function` | VERIFIED (CPU modules only) | 25 tests; `Mat4::ortho` is additionally reachable through the plugin ABI, and immediate quad conversion feeds a real Vulkan draw in the harness. No lighting, texenv, fog, display lists, attribute stacks or client arrays. |
| `hematite-shader-cache` | VERIFIED (records/disk IO only) | 10 tests. Still no shader compiler or pipeline consuming it. |
| `xtask` | VERIFIED | 9 tests; 866-entry generation (table, declarations, exports, by-name lookup) and invalid-input rejection. |
| GL/EGL ABI | VERIFIED as exports | 820 GL + 46 EGL entries; the arm64 library exports 866/866 (checked by `tools/inspect_library.py` and `tools/verify_apk.py`), and every name resolves through `hematite_gl_proc_address`. |
| GL frontend / EGL lifecycle | PARTIALLY IMPLEMENTED, VERIFIED on host | Error state, capability toggles, matrix stacks, buffer objects, attribute arrays, immediate mode, textures, depth, blending, scissor, culling, readback, **shader and program objects with real compilation and linking**; EGL display/config/surface/context/make-current/swap on a software backbuffer. 749 entry points only record the call. **A linked program's SPIR-V is not executed by any draw yet, so shader-driven applications still cannot run.** |
| GLES fallback backend | NOT-IMPLEMENTED | No EGL/GLES integration. |
| `hematite-glsl` (GLSL → SPIR-V) | VERIFIED as a compiler, NOT-IMPLEMENTED as a runtime | Preprocessor (`#include` from a caller-supplied table, `#version`, defines), normalisation of legacy and modern GLSL to Vulkan-GLSL 450 (attributes/varyings, `gl_FragColor`/`gl_FragData`, `ftransform()`, combined samplers split into texture+sampler, `shadow2D`), interface reflection, naga 0.14 front end, validator, SPIR-V back end, and a pass that removes the sampler types naga emits unconditionally. Legacy 1.20, `#version 330/450 core` and a shader-pack-style stage all compile, and the modules load into SwiftShader. No pack is executed and no draw uses a compiled program. |
| Quirks / diagnostics / fuzz | NOT-IMPLEMENTED | Research/design only. |
| Android packaging | VERIFIED as a container | Hand-written AXML, 16 KB-aligned ZIP, APK Signature Schemes v1 + v2, verified by two independent implementations. No Gradle, no `cargo-ndk`, no resources, no Java, no launcher metadata. |

Canvas counts: target 26, state 553, resource 40, draw 71, query 109, sync 39,
unsupported 28. These classify the planned API, not completed implementations.

## 3. What the APK is

`dist/hematite-debug.apk` (1 817 983 bytes, sha256
`310fc9e0a47739ec1bfef2e30a40205b7376333224c9603d6c1839c19eaa54f8`,
`org.hematite.renderer`, minSdk 21, targetSdk 28,
`android:hasCode=false`) contains:

| Entry | Contents |
| --- | --- |
| `AndroidManifest.xml` | binary AXML written by `tools/build_apk.py` |
| `lib/arm64-v8a/libhematite.so` | the cross-compiled Rust library (1 788 272 bytes — it now contains the GLSL compiler), stored uncompressed and 16 KB aligned |
| `assets/HEMATITE_STATUS.txt` | a copy of an earlier status report, so the artifact states its own limits |
| `META-INF/{MANIFEST.MF,CERT.SF,CERT.RSA}` | APK Signature Scheme v1 over a self-signed debug certificate |
| APK Signing Block | APK Signature Scheme v2 signature, `v2 verified` by apksigtool |

It is **not** installable as a working renderer: the library intercepts no GL call, has no
swapchain and never presents anything. It is a genuine, signed, structurally verified
container for the native library, produced without a JDK, NDK, Gradle, `aapt2` or
`apksigner`. It has never been installed or run on Android.

## 4. Deliverables

Present: Cargo workspace (7 libraries + xtask), vendored `ash` 0.38.0 (crates.io is
unreachable), lockfile, MIT license, research, architecture, decision log, README,
integration/configuration/compatibility/benchmark documents, status/limitations/roadmap,
executable build targets (`check`, `abi`, `shaders`, `gpu`, `android`, `apk`, `wasi`),
reproducible tool bootstrap scripts, and a CPU + Android + APK CI workflow.

Absent: a launcher that has actually loaded the renderer, execution of a compiled program,
texture upload, a GLES fallback backend, a Gradle project, a release-signed APK, device
testing, a fuzz campaign. No panic-only GL library is substituted for these.

## 5. Risks and next priorities

1. **Symbol-to-library assignment for the Android link is a table, not the NDK.** A wrong
   entry surfaces only as a `dlopen` failure on a device. `tools/android_linker.py` prints
   every assignment; `tools/inspect_library.py` rejects known glibc/musl spellings.
2. **No validation layers and no hardware driver.** SwiftShader accepts what it accepts;
   synchronization and layout mistakes can pass silently on a lenient software driver.
3. **APK v2 signing was verified only by apksigtool 0.1.0** (a prototype) and matched
   against AOSP apksig source, never against Android's own verifier.
4. **A compiled program is not executed.** `glCompileShader`/`glLinkProgram` produce SPIR-V
   and an interface, but no draw records with them, so a GLES2 application still renders
   nothing of its own. This is the blocking work.
5. **Only a narrow set of draws reaches the device.** Textures are not uploaded, so textured
   geometry cannot be routed; blended, depth-tested, culled and sub-viewport draws fall back
   to the CPU. Framebuffer objects, a depth attachment, instancing and stencil come next,
   then device and Minecraft/shader-pack testing. No compatibility or performance claim
   exists anywhere in this repository.
