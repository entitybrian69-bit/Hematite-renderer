# Build environment and reproducible verification

## Current result (2026-11-09)

**Native compilation now works.** The initial environment had no Rust tools, and official
Rust distribution servers were inaccessible. Two working alternatives were subsequently
found on the reachable npm/PyPI registries. Earlier claims that compilation was impossible
are obsolete.

| Toolchain | What actually ran | Scope |
| --- | --- | --- |
| Native Rust 1.85.0, cargo 1.85.0, Clippy, rustfmt | `./build.sh check` | Native Linux CPU compilation, lints, formatting, tests and ABI declaration generation/compilation |
| WASI-hosted rustc 1.83.0-dev on Wasmtime 36.0.9 | `python3 tools/verify_wasi.py` | Each dependency-free crate compiled with `-D warnings`, tests executed under WASI, ABI table checked, generated Rust compiled, duplicate table rejected with exit 1 |
| SwiftShader Vulkan (CPU) via the pinned npm Chromium package | `./build.sh gpu` | Real Vulkan instance, device, pipeline, draw and readback; analytic pixel comparison. Not hardware, not validation layers. |
| Zig 0.17.1 (`zig ld.lld`) + Android `rust-std` | `./build.sh android` | Cross-compiles and links `libhematite.so` for `aarch64-linux-android` with generated system-library stubs. Structural ELF verification only. |
| Python `cryptography` / `androguard` / `apksigtool` | `./build.sh apk` | Writes and then independently verifies the APK's manifest, layout and signature schemes. |
| JDK / NDK / Gradle / device / emulator / hardware GPU | Nothing | Still absent; see KNOWN_LIMITATIONS E-2, E-3, E-6. |

The two toolchains are third-party repackagings. This is useful build evidence, not an
independent supply-chain audit. Prefer the official Rust distribution in normal CI.

## Preferred native verification

On a machine with the pinned toolchain:

```sh
./build.sh check
```

In this x86_64 Linux sandbox, where the Rust distribution host is blocked:

```sh
python3 tools/setup_native_rust.py
export PATH="$PWD/.cache/native-rust/toolchain/bin:$PATH"
./build.sh check
```

`setup_native_rust.py` downloads **bundled** `@rustbin/*-1.85.0-x86_64-unknown-linux-gnu`
packages, verifies recorded SHA-256 digests, extracts regular files only, and invokes their
installers under `.cache/native-rust/toolchain`. It installs rustc, native stdlib, Cargo,
Clippy and rustfmt. It does **not** install Android stdlibs or an NDK. All downloads,
installed tools, logs and outputs are ignored by Git. `build.sh` discovers this local
installation when Cargo is otherwise absent.

The earlier npm `rustc`/`rustup` wrappers did not work because they merely downloaded from
`static.rust-lang.org`; the `@rustbin` packages actually contain the executables.

## Android and GPU tools

```sh
python3 tools/setup_android_tools.py    # idempotent; --force re-downloads and reinstalls
./build.sh shaders && ./build.sh gpu && ./build.sh android && ./build.sh apk
```

Everything is fetched as a pinned archive into `.cache/` (git-ignored) and SHA-256 checked:

| Tool | Source | Digest |
| --- | --- | --- |
| Zig 0.17.1 | PyPI `ziglang` wheel | `ea6cc5d8…48a74524b54` |
| Android stdlib | npm `@rustbin/rust-std-1.85.0-aarch64-linux-android` | recorded by the script |
| SwiftShader + Vulkan loader | npm `@sparticuz/chromium@133.0.0` (`bin/swiftshader.tar.br`) | `9eb9776c…e9263ad` |
| glslang (WASM) | npm `@webgpu/glslang@0.0.15` | `7b193e08…82d97f8` |
| cryptography 50.0.2, androguard 4.1.4, apksigtool 0.1.0 | PyPI (`tools/android-requirements.txt`) | pinned by version |

Notes that cost time to discover and should not be rediscovered:

- The PyPI Zig wheel ships **no bionic/Android libc**, and `zig cc --target …-android*`
  fails with "unable to provide libc for target" even with `-nostdlib`. Hence D-022.
- `zig cc` silently replaces `-lc`/`-lunwind` with its musl `libc.so` and static
  `libunwind.a` (check with `zig cc -v`), which embeds musl code. Drive `zig ld.lld`.
- `zig cc` rejects `-Wl,--unresolved-symbols=…`, so the undefined-symbol set is computed by
  parsing the link inputs' ELF symbol tables instead of a probe link.
- SwiftShader reports Vulkan 1.3.0 while its ICD JSON advertises interface 1.0.5.

## WASI fallback

```sh
python3 tools/verify_wasi.py --setup  # first run: install tools and verify
python3 tools/verify_wasi.py          # repeat without reinstalling
```

Requires Python 3, npm, and an x86_64 Linux runtime for the bundled Wasmtime executable.
Uses `@ai-ecoverse/wasi-rustc@1.83.0-2` and PyPI `wasmtime-bin==36.0.9`. Files stay under
`.cache/wasi-verify`. No native Cargo is needed. The script validates generated manifests
against the tracked file and checks actual failing subprocess exit codes (not a pipeline's
exit status).

Important hosting details, preserved to avoid repeating dead ends:

- Compiler imports `env::memory` and `wasi::thread-spawn`. Node's WASI host cannot run the
  compilation path correctly. A hand-written wasmtime-py thread bridge also trapped.
- Wasmtime **36.0.9** supports `-W threads=y -S threads=y`; v49 rejects the needed WASI
  threads mode.
- Preload a module defining shared memory (399 minimum / 16384 maximum pages) **without
  exporting it**, otherwise the linker defines `env::memory` twice.
- Preopen a writable directory as guest `/` and the compiler package as `/sdk`; rustc's
  temp-file path otherwise fails. Set `RUST_MIN_STACK=67108864`.
- WASI tests prove Rust behaviour for that target, **not** native pointer/ABI correctness,
  Clippy, rustfmt, Android loading, GPU rendering or driver compatibility.

## Defects found by actual checks

First compilation caught missing `StateBlock` imports, `Eq` derives involving floating-point
clear values, and `Copy` on a command containing `Box<StateBlock>`. These are fixed.
Native Clippy then caught four derivable default implementations and an inefficient test
hex formatter. These are fixed without suppressing the lints. The unsupported test-profile
`panic = "abort"` setting was removed (Cargo ignores it for test harnesses).

## Remaining restrictions

Reachable hosts: GitHub API/source, npm, PyPI. Official Rust distributions, GitHub release
asset hosts and crates.io are not available here. There is still no JDK, NDK, Gradle,
Android device, emulator, `adb`, hardware GPU, GLES implementation or validation layer, so
there are no on-device results, no shader-pack results, no validation-layer logs and no
performance measurements. Rendering evidence exists only on the SwiftShader software
driver.
