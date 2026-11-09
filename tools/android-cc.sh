#!/bin/sh
# Rust linker shim for aarch64-linux-android.
#
# cargo invokes this as the linker; it forwards every argument to tools/android_linker.py,
# which resolves Android system libraries against generated stubs and drives `zig ld.lld`
# directly. See that file's module docstring for why no NDK is involved.
#
# Required environment: HEMATITE_ZIG (zig executable). Optional: HEMATITE_STUB_DIR,
# HEMATITE_LINK_TARGET, HEMATITE_LINKER_VERBOSE.
set -eu
exec python3 "$(dirname "$0")/android_linker.py" "$@"
