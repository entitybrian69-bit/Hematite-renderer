# Hematite Renderer

A Minecraft: Java Edition renderer for Android launchers, written in Rust, that translates
the game's desktop OpenGL calls to Vulkan (primary) or OpenGL ES 3.x (fallback).

Hematite is a **renderer plugin**, not a mod and not a launcher. It occupies the same slot as
GL4ES, MobileGlues and Zink inside PojavLauncher-derived launchers: inside the game process
it *is* the OpenGL implementation, and LWJGL resolves every `gl*` call against it.

---

## Project status: the GL ABI is exported and fixed-function GL really renders

A signed `dist/hematite-debug.apk` containing a cross-compiled `libhematite.so` is built by
`./build.sh apk`. That library now exports **all 866 `gl*`/`egl*` entry points** with the
signatures `abi/gl_export.def` declares, and 117 of them do real work on a software
rasterizer: state, matrix stacks, buffer objects, client and VBO attribute arrays,
immediate mode, textures (nearest and bilinear), depth test, blending, scissor, culling,
clear and readback. `./build.sh gl` drives that ABI through `ctypes` — EGL bring-up, a
`glClear`, an immediate-mode triangle, `glReadPixels`, `eglSwapBuffers` — and checks the
pixels. A separate real Vulkan offscreen path is verified against SwiftShader.

What is still **not** true: **GLSL is not compiled** (`glCompileShader` reports failure on
purpose), so shader-driven GLES2 applications — Minecraft included — cannot run; EGL
publishes frames to a software backbuffer and **nothing reaches a window**; and the APK has
never been installed on a device.

- `hematite-gl-frontend`: the 866-symbol export surface, the GL state machine, the software
  rasterizer and the software EGL (5 tests, plus 19 `ctypes` checks).
- `hematite-vk-backend`: instance/device selection, offscreen render pass, pipeline,
  readback (3 tests + 5 driver checks per render).
- `hematite-plugin`: the 12-function `hematite_*` C ABI compiled into `libhematite.so`
  (5 tests, plus an end-to-end host run through `ctypes`).
- `hematite-testharness`: analytic pixel reference for real driver readback.
- `hematite-ir` (14), `hematite-config` (16), `hematite-fixed-function` (25),
  `hematite-shader-cache` (10), `xtask` (7).

**89 unit tests pass** on native Linux and under WASI; `./build.sh gpu` runs the additional
real-driver suite. Cargo check, Clippy `-D warnings` and rustfmt pass. None of this
establishes Minecraft, shader-pack or device compatibility. The GLSL translator, the GLES
fallback backend, window presentation and 749 of the 866 entry points are
**NOT-IMPLEMENTED**.

See [`docs/STATUS_REPORT.md`](docs/STATUS_REPORT.md) for scoped verification evidence and
[`docs/ENVIRONMENT.md`](docs/ENVIRONMENT.md) for reproducible toolchain setup.

---

## What it is meant to be

| | |
| --- | --- |
| **Target** | Minecraft: Java Edition on Android, via Pojav-derived launchers |
| **Versions** | Legacy 1.0–1.12.2 (LWJGL 2, fixed function) and modern 1.13–1.21.x (LWJGL 3, core profile) |
| **Primary backend** | Vulkan 1.1+, using 1.2/1.3 features and extensions when the device has them |
| **Fallback backend** | OpenGL ES 3.x, for devices with broken or missing Vulkan |
| **Priorities** | 1. frame time and stutter-free behaviour, 2. Iris/OptiFine shader packs, 3. everything else |

The design is in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); the choices behind it are in
[`docs/DECISIONS.md`](docs/DECISIONS.md).

---

## Why the ABI table matters

Pojav-derived hosts load the renderer as one shared object and hand the *same* handle to
LWJGL as both EGL and OpenGL. Two consequences drive the design:

- A missing `gl*` symbol is not a graceful failure; it is a crash. So the export table is
  dense and generated, not hand-written.
- `eglGetProcAddress` must resolve every extension above GL 1.1.

`abi/gl_export.def` is therefore a single auditable line per function, validated by
`abi/validate_abi.py` in any environment with Python, and turned into Rust by
`cargo run -p xtask -- gen-abi`.

```sh
python3 abi/validate_abi.py     # check the table (works without a toolchain)
cargo run -p xtask -- check-abi # same checks, from Rust
cargo run -p xtask -- gen-abi   # emit gen/{abi,decls,exports,procs}.rs and the manifest
```

---

## Building and validating

```sh
python3 tools/setup_native_rust.py    # pinned Rust 1.85.0 toolchain (see ENVIRONMENT.md)
python3 tools/setup_android_tools.py  # zig, Android rust-std, SwiftShader, glslang, APK tools

./build.sh check    # check + clippy -D warnings + fmt + 89 tests + ABI and GL checks
./build.sh gl       # drive the exported GL/EGL ABI through ctypes and check pixels
./build.sh shaders  # recompile GLSL and prove the checked-in SPIR-V matches
./build.sh gpu      # real Vulkan offscreen rendering against SwiftShader
./build.sh android  # cross-compile libhematite.so for aarch64-linux-android
./build.sh apk      # write, sign and verify dist/hematite-debug.apk
./build.sh wasi     # independent WASI run of the CPU crates
```

There is no Gradle project and no `cargo-ndk`: the Android library is linked by
`tools/android_linker.py` (Zig's `ld.lld` plus generated Android system-library stubs,
because no NDK is reachable), and the APK is assembled and signed by
`tools/build_apk.py`, then verified by `tools/verify_apk.py` with androguard and
apksigtool. Both scripts explain their own limits in their module docstrings.

## Repository layout

```
abi/            the GL/EGL export table, its validator, and the generated symbol manifest
crates/
  hematite-ir/      backend-neutral commands and comparable state  (no FFI, no GPU types)
  hematite-config/  environment + file configuration resolution
  hematite-fixed-function/  CPU matrix stacks and immediate primitive assembly
  hematite-shader-cache/    content-addressed records and disk IO
  hematite-vk-backend/      offscreen Vulkan 1.1 rendering with RAII ownership
  hematite-plugin/          the C ABI compiled into libhematite.so
  hematite-testharness/     analytic pixel checks against real driver readback
vendor/ash/     pinned ash 0.38.0 bindings (crates.io is unreachable here)
xtask/          build-time generator for the ABI tables
tools/          ABI checks, shader compile check, Android linker, APK build/verify,
                native/WASI/Android toolchain bootstrap
build.sh        check | abi | shaders | gpu | android | apk | wasi
docs/           research, architecture, decisions, status, limitations, roadmap
```

## Documentation

| Document | Contents |
| --- | --- |
| [`docs/RESEARCH.md`](docs/RESEARCH.md) | The launcher plugin contract, read out of source; the renderers Hematite is measured against; what Minecraft actually uses; the Android GPU landscape |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Module layout, threading model, frame lifecycle, shader pipeline, cache layout |
| [`docs/DECISIONS.md`](docs/DECISIONS.md) | Decisions, alternatives and reasons; implemented vs planned scope is explicit |
| [`docs/STATUS_REPORT.md`](docs/STATUS_REPORT.md) | Every feature with a status label and its evidence |
| [`docs/KNOWN_LIMITATIONS.md`](docs/KNOWN_LIMITATIONS.md) | What is missing, and what will never work |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | The ordered plan, with an exit criterion per step |
| [`docs/ENVIRONMENT.md`](docs/ENVIRONMENT.md) | What the build environment could and could not do |

## License

MIT. Not affiliated with Mojang, Microsoft, or any launcher project. Minecraft code and
assets belong to Mojang and Microsoft.
