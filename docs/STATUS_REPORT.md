# Hematite Renderer — Status Report

Updated 2026-11-09. **A real Vulkan path now renders and a signed APK exists, but this is
still not a Minecraft renderer.** No GL call is intercepted, no frame is presented, and no
device has run the APK.

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
| `cargo test --locked --workspace` | VERIFIED, 82 unit tests pass, 1 test ignored by default |
| `./build.sh gpu` → the ignored `real_vulkan_readback_suite` | VERIFIED against SwiftShader: 5 analytic checks per renderer, two renderer instances, no leaked failure |
| `./build.sh shaders` | VERIFIED, checked-in SPIR-V byte-identical to recompiled GLSL (976/380 bytes) |
| Host `libhematite_plugin.so` through its C ABI (Python `ctypes`, SwiftShader) | VERIFIED: abi version, device name, 16×12 render with correct clear/interpolated pixels, short-buffer status, `glOrtho` matrix, destroy |
| `./build.sh android` | VERIFIED structurally: ELF64 AArch64 `ET_DYN`, 398 608 bytes, `DT_NEEDED` = libdl/libunwind/libc, `p_align` 16384, 8 exports, 45 bionic-resolved symbols |
| `./build.sh apk` | VERIFIED: `dist/hematite-debug.apk` (432 049 bytes); manifest parsed by androguard; attribute IDs match AOSP `public.xml`; `.so` stored and 16 KB aligned; `apksigtool verify` prints `v2 verified` |
| `python3 tools/verify_wasi.py` | VERIFIED, 82 tests + ABI checks under WASI, duplicate export rejected with exit 1 |
| `python3 abi/validate_abi.py`, `xtask check-abi`/`gen-abi` | VERIFIED, 866 internally consistent entries |
| Installation on an Android device | NOT-VERIFIED: no device, emulator or `adb` in this environment |
| Validation layers (`VK_LAYER_KHRONOS_validation`) | NOT-IMPLEMENTED: not present in this environment; correctness relies on the analytic pixel checks |
| Hardware GPU rendering | NOT-VERIFIED: the only driver used is SwiftShader (CPU, reports Vulkan 1.3.0) |
| CI on GitHub | Workflow exists with CPU + Android + APK jobs; no green remote run is claimed |

## 2. Component status

| Component | Status | Evidence / missing work |
| --- | --- | --- |
| `hematite-vk-backend` | VERIFIED (offscreen only) | Instance/device selection, RAII ownership, RGBA8 render pass, graphics pipeline, vertex upload, transfer readback. 3 unit tests plus 5 driver checks per render on SwiftShader. No swapchain, textures, depth/stencil, blending, MSAA, async submission, descriptor-driven draws or command reuse. Each render reallocates everything; no performance claim. |
| `hematite-plugin` (`libhematite.so`) | VERIFIED on host, BUILT-BUT-UNTESTED on Android | 8 exported `hematite_*` functions over the backend and `Mat4::ortho`; 5 unit tests; exercised end-to-end on x86_64 through `ctypes`. The arm64 build is structurally verified only — never loaded by a loader. |
| `hematite-testharness` | VERIFIED | Analytic reference (barycentric interpolation, coverage bounds, 1×1 and repeat renders) compared against driver pixels; PPM writer for the actual readback. |
| `hematite-ir` | VERIFIED (CPU data model only) | 14 tests. No backend consumes these commands yet; the Vulkan backend takes its own vertex struct. |
| `hematite-config` | VERIFIED (CPU resolution only) | 16 tests. No runtime effect: the backend reads no configuration. |
| `hematite-fixed-function` | VERIFIED (CPU modules only) | 25 tests; `Mat4::ortho` is additionally reachable through the plugin ABI, and immediate quad conversion feeds a real Vulkan draw in the harness. No lighting, texenv, fog, display lists, attribute stacks or client arrays. |
| `hematite-shader-cache` | VERIFIED (records/disk IO only) | 10 tests. Still no shader compiler or pipeline consuming it. |
| `xtask` | VERIFIED | 7 tests; 866-entry generation and invalid-input rejection. |
| GL/EGL ABI | VERIFIED as data, NOT-IMPLEMENTED as exports | 820 GL + 46 EGL entries. The shipped library exports none of them. |
| GL frontend / EGL lifecycle | NOT-IMPLEMENTED | No context ownership, GL object model, dispatch, error state or `eglGetProcAddress`. **This is why no launcher can use the APK.** |
| GLES fallback backend | NOT-IMPLEMENTED | No EGL/GLES integration. |
| GLSL translator / shader packs | NOT-IMPLEMENTED | Two embedded synthetic shaders exist as SPIR-V; no preprocessor, rewrite, reflection or pack execution. |
| Quirks / diagnostics / fuzz | NOT-IMPLEMENTED | Research/design only. |
| Android packaging | VERIFIED as a container | Hand-written AXML, 16 KB-aligned ZIP, APK Signature Schemes v1 + v2, verified by two independent implementations. No Gradle, no `cargo-ndk`, no resources, no Java, no launcher metadata. |

Canvas counts: target 26, state 553, resource 40, draw 71, query 109, sync 39,
unsupported 28. These classify the planned API, not completed implementations.

## 3. What the APK is

`dist/hematite-debug.apk` (432 049 bytes, `org.hematite.renderer`, minSdk 21, targetSdk 28,
`android:hasCode=false`) contains:

| Entry | Contents |
| --- | --- |
| `AndroidManifest.xml` | binary AXML written by `tools/build_apk.py` |
| `lib/arm64-v8a/libhematite.so` | the cross-compiled Rust library, stored uncompressed and 16 KB aligned |
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

Absent: launcher-loadable renderer, GL/EGL exports, GLES backend, shader translation,
Gradle project, release APK, device testing, fuzz campaign. No panic-only GL library is
substituted for these.

## 5. Risks and next priorities

1. **Symbol-to-library assignment for the Android link is a table, not the NDK.** A wrong
   entry surfaces only as a `dlopen` failure on a device. `tools/android_linker.py` prints
   every assignment; `tools/inspect_library.py` rejects known glibc/musl spellings.
2. **No validation layers and no hardware driver.** SwiftShader accepts what it accepts;
   synchronization and layout mistakes can pass silently on a lenient software driver.
3. **APK v2 signing was verified only by apksigtool 0.1.0** (a prototype) and matched
   against AOSP apksig source, never against Android's own verifier.
4. GL/EGL frontend with ownership, errors and real ABI coverage remains the blocking work.
5. Then GLSL-to-SPIR-V translation, legacy fixed-function variants, GLES fallback, and only
   then device and Minecraft/shader-pack testing. No compatibility or performance claim
   exists anywhere in this repository.
