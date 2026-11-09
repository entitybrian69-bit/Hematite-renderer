#!/usr/bin/env python3
"""Inspect and validate a cross-compiled `libhematite.so` for aarch64-linux-android.

    python3 tools/inspect_library.py target/aarch64-linux-android/release/libhematite_plugin.so

This is the structural half of Android verification. It answers: is the object really an
AArch64 shared library, which libraries will the dynamic loader pull in, are segments
aligned for 16 KB page devices, which symbols does it export, and which symbols must
bionic provide at `dlopen` time.

It cannot answer whether the library actually runs on a device: this environment has no
Android device, emulator or `adb`, so nothing here is execution on Android. A symbol that
is assigned to the wrong library by `tools/android_linker.py` would only surface at load
time, which is why the undefined list is printed in full for review.
"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import android_linker as elf  # noqa: E402

BIONIC = {"libc.so", "libm.so", "libdl.so", "liblog.so", "libunwind.so"}
EXPECTED_EXPORTS = {
    "hematite_abi_version", "hematite_status_string", "hematite_last_error",
    "hematite_plugin_create", "hematite_plugin_destroy", "hematite_plugin_device_name",
    "hematite_plugin_render_offscreen", "hematite_fixed_function_ortho",
}
# glibc/musl spellings that bionic does not export; their presence means a foreign libc
# leaked into the link.
FORBIDDEN = {"__errno_location", "stderr", "stdout", "stdin", "__progname",
             "pthread_rwlock_rdlock", "pthread_rwlock_wrlock", "pthread_rwlock_unlock",
             "fflush", "fprintf", "fwrite", "snprintf", "dladdr"}


def main(path: Path) -> int:
    blob = path.read_bytes()
    parsed = elf.read_elf64_bytes(blob, path)
    failures = []

    def check(condition: bool, message: str) -> None:
        print(("  ok:   " if condition else "  FAIL: ") + message)
        if not condition:
            failures.append(message)

    print(f"{path} ({len(blob)} bytes)")
    check(blob[4] == 2 and blob[5] == 1, "ELF64, little-endian")
    check(parsed["type"] == 3, f"ET_DYN shared object (e_type={parsed['type']})")
    check(parsed["machine"] == 183, f"EM_AARCH64 (e_machine={parsed['machine']})")

    needed = elf.dynamic_needed(parsed)
    check(bool(needed), f"DT_NEEDED recorded: {needed}")
    check(set(needed) <= BIONIC, "DT_NEEDED limited to bionic libraries")

    aligns = {header["align"] for header in parsed["program_headers"] if header["type"] == 1}
    check(aligns == {16384}, f"every PT_LOAD has p_align=16384 (got {sorted(aligns)})")

    undefined, defined = elf.symbol_table(parsed, elf.SHT_DYNSYM)
    exported = sorted(name for name in defined if name.startswith("hematite_"))
    check(EXPECTED_EXPORTS <= set(exported), f"exports: {exported}")
    leaked = sorted(FORBIDDEN & set(undefined))
    check(not leaked, f"no non-bionic symbols among undefined ({leaked or 'none'})")
    print(f"  info: {len(set(undefined))} symbols resolved by bionic at load time:")
    for name in sorted(set(undefined)):
        print(f"        {name}")
    print("\nNot verified: execution on an Android device (no device, emulator or adb "
          "in this environment).")
    return 1 if failures else 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    sys.exit(main(Path(sys.argv[1])))
