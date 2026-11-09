#!/usr/bin/env bash
# Hematite build and verification entry points.
#
#   ./build.sh check     native CPU checks: check, clippy -D warnings, fmt, tests, ABI
#   ./build.sh abi       regenerate and validate the 866-symbol GL/EGL ABI
#   ./build.sh shaders   recompile the original GLSL to SPIR-V and compare with the
#                        checked-in .spv files (fails if they drift apart)
#   ./build.sh gpu       real Vulkan offscreen rendering against the SwiftShader driver
#   ./build.sh android   cross-compile libhematite.so for aarch64-linux-android
#   ./build.sh apk       package, sign and verify dist/hematite-debug.apk
#   ./build.sh wasi      WASI test run for the CPU crates
#
# `gpu`, `android` and `apk` need the pinned third-party tools in .cache/; run
# `python3 tools/setup_native_rust.py` and `python3 tools/setup_android_tools.py` first.
# Every target prints what it verified and exits non-zero otherwise.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

if ! command -v cargo >/dev/null 2>&1 && [[ -x .cache/native-rust/toolchain/bin/cargo ]]; then
    export PATH="$PWD/.cache/native-rust/toolchain/bin:$PATH"
fi

# Tools for the Android and GPU targets. Paths are absolute because rustc resolves the
# linker shim from the workspace root.
export HEMATITE_ZIG="$PWD/.cache/android/zig/ziglang/zig"
export HEMATITE_STUB_DIR="$PWD/.cache/android/stubs"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$PWD/tools/android-cc.sh"
export PYTHONPATH="$PWD/.cache/pylibs${PYTHONPATH:+:$PYTHONPATH}"
export VK_ICD_FILENAMES="$PWD/.cache/vulkan/driver/vk_swiftshader_icd.json"
export HEMATITE_VULKAN_LIBRARY="$PWD/.cache/vulkan/driver/libvulkan.so.1"

require() {
    if [[ ! -e "$1" ]]; then
        echo "missing $1" >&2
        echo "run: python3 tools/setup_native_rust.py && python3 tools/setup_android_tools.py" >&2
        exit 2
    fi
}

case "${1:-check}" in
    check)
        cargo check --locked --workspace --all-targets
        cargo clippy --locked --workspace --all-targets -- -D warnings
        cargo fmt --all -- --check
        cargo test --locked --workspace
        bash "$PWD/build.sh" abi
        ;;
    abi)
        python3 abi/validate_abi.py
        python3 tools/gen_families.py --check
        cargo run --locked -p xtask -- check-abi
        expected=$(mktemp)
        trap 'rm -f "$expected"' EXIT
        cp abi/expected_symbols.txt "$expected"
        cargo run --locked -p xtask -- gen-abi
        cmp "$expected" abi/expected_symbols.txt
        mkdir -p target/abi-check
        for module in abi decls; do
            rustc --edition 2021 -D warnings --crate-type lib \
                --crate-name "generated_$module" \
                "crates/hematite-gl-frontend/src/gen/$module.rs" \
                -o "target/abi-check/$module.rlib"
        done
        ;;
    shaders)
        require .cache/vulkan/glslang/package/dist/node-devel/glslang.wasm
        node tools/compile_shaders.mjs
        ;;
    gpu)
        require .cache/vulkan/driver/libvk_swiftshader.so
        require .cache/vulkan/driver/libvulkan.so.1
        cargo test --locked -p hematite-testharness -- --ignored --exact \
            tests::real_vulkan_readback_suite --nocapture
        cargo run --locked -p hematite-testharness -- \
            --loader "$PWD/.cache/vulkan/driver/libvulkan.so.1" \
            --output "$PWD/target/triangle.ppm"
        ;;
    android)
        require "$HEMATITE_ZIG"
        require .cache/native-rust/toolchain/lib/rustlib/aarch64-linux-android
        cargo build --locked -p hematite-plugin --release --target aarch64-linux-android
        python3 tools/inspect_library.py target/aarch64-linux-android/release/libhematite_plugin.so
        ;;
    apk)
        require "$HEMATITE_ZIG"
        require .cache/pylibs/apksigtool.py
        bash "$PWD/build.sh" android
        python3 tools/build_apk.py
        python3 tools/verify_apk.py
        ;;
    wasi)
        python3 tools/verify_wasi.py
        ;;
    *)
        echo "Usage: $0 [check|abi|shaders|gpu|android|apk|wasi]" >&2
        exit 2
        ;;
esac
