#!/usr/bin/env python3
"""Independent verification of `dist/hematite-debug.apk`.

Checks performed, each of which fails loudly rather than reporting success:

1. Manifest: parsed by androguard (a separate implementation of the binary AXML format)
   and compared with the values `tools/build_apk.py` intends to write.
2. Attribute resource IDs: every ID in `build_apk.ATTR_IDS` is compared with androguard's
   bundled copy of AOSP `public.xml`, so a hand-copied ID cannot drift silently.
3. Layout: the ZIP parses, `lib/arm64-v8a/libhematite.so` is stored uncompressed and its
   data starts on a 16 KB boundary (Android 15 requirement for uncompressed libraries).
4. Packaged library: ELF64 AArch64 `ET_DYN`, `DT_NEEDED` limited to bionic libraries,
   16 KB `PT_LOAD` alignment, the eight `hematite_*` exports present, and no glibc/musl
   symbol (for example `__errno_location`) that bionic does not provide.
5. Signatures: `apksigtool verify` (independent implementation of APK Signature Schemes
   v1/v2/v3) is run and its verdict is reported verbatim.

Not verified here, and not verifiable in this environment: installation on a device,
behaviour inside a Pojav-derived launcher, and any OpenGL or Minecraft functionality.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
sys.path.insert(0, str(ROOT / ".cache/pylibs"))

import android_linker as elf  # noqa: E402

APK = ROOT / "dist/hematite-debug.apk"
LIBRARY = "lib/arm64-v8a/libhematite.so"
EXPECTED_EXPORTS = {
    "hematite_abi_version", "hematite_status_string", "hematite_last_error",
    "hematite_plugin_create", "hematite_plugin_destroy", "hematite_plugin_device_name",
    "hematite_plugin_render_offscreen", "hematite_fixed_function_ortho",
    "hematite_gl_export_count", "hematite_gl_proc_address", "hematite_gl_render_demo",
    "hematite_gl_unsupported_count",
}
FORBIDDEN = {"__errno_location", "stderr", "stdout", "__progname", "pthread_rwlock_rdlock"}


class Failure(Exception):
    """A verification check did not hold."""


def check(condition: bool, message: str) -> None:
    if not condition:
        raise Failure(message)
    print(f"  ok: {message}")


def verify_manifest() -> None:
    from androguard.core.apk import APK as AndroguardAPK
    print("manifest (androguard):")
    apk = AndroguardAPK(str(APK))
    check(apk.get_package() == "org.hematite.renderer", f"package = {apk.get_package()}")
    check(apk.get_androidversion_code() == "1", f"versionCode = {apk.get_androidversion_code()}")
    check(apk.get_androidversion_name() == "0.1.0", f"versionName = {apk.get_androidversion_name()}")
    check(apk.get_min_sdk_version() == "21", f"minSdkVersion = {apk.get_min_sdk_version()}")
    check(apk.get_target_sdk_version() == "28", f"targetSdkVersion = {apk.get_target_sdk_version()}")
    check(not apk.get_activities(), "no activities declared (this is not a runnable app)")
    check(LIBRARY in apk.get_files(), f"{LIBRARY} is listed in the archive")


def verify_resource_ids() -> None:
    from build_apk import ATTR_IDS
    print("attribute resource IDs (vs androguard's AOSP public.xml):")
    table = json.loads((ROOT / ".cache/pylibs/androguard/core/resources/public.json")
                       .read_text(encoding="utf-8"))["attr"]
    for name, value in sorted(ATTR_IDS.items()):
        authoritative = table.get(name)
        check(authoritative == value,
              f"android:{name} = 0x{value:08x} (AOSP 0x{authoritative or 0:08x})")


def verify_layout() -> None:
    print("zip layout:")
    with zipfile.ZipFile(APK) as archive:
        names = archive.namelist()
        check("AndroidManifest.xml" in names, "AndroidManifest.xml present")
        check("META-INF/MANIFEST.MF" in names and "META-INF/CERT.SF" in names
              and "META-INF/CERT.RSA" in names, "APK Signature Scheme v1 files present")
        info = archive.getinfo(LIBRARY)
        check(info.compress_type == zipfile.ZIP_STORED, f"{LIBRARY} stored uncompressed")
        # Walk the local headers directly: 30 bytes + name + extra field, then the data.
        raw = APK.read_bytes()
        offset = 0
        while True:
            name_length, extra_length = (int.from_bytes(raw[offset + 26:offset + 28], "little"),
                                         int.from_bytes(raw[offset + 28:offset + 30], "little"))
            entry_name = raw[offset + 30:offset + 30 + name_length].decode("utf-8")
            compressed = int.from_bytes(raw[offset + 18:offset + 22], "little")
            data_start = offset + 30 + name_length + extra_length
            if raw[offset:offset + 4] != b"PK\x03\x04":
                raise Failure(f"malformed local header at {offset}")
            if entry_name == LIBRARY:
                check(data_start % 16384 == 0,
                    f"{LIBRARY} data starts at {data_start} (16 KB aligned)")
                check(compressed == info.file_size, "stored size equals uncompressed size")
                break
            offset = data_start + compressed
        if offset == 0:
            raise Failure("no local headers walked")


def verify_library() -> None:
    print("packaged native library:")
    with zipfile.ZipFile(APK) as archive:
        blob = archive.read(LIBRARY)
    parsed = elf.read_elf64_bytes(blob, LIBRARY)
    check(blob[4] == 2 and blob[5] == 1, "ELF64 little-endian")
    check(parsed["type"] == 3, f"ET_DYN shared object (e_type={parsed['type']})")
    check(parsed["machine"] == 183, f"EM_AARCH64 (e_machine={parsed['machine']})")
    needed = elf.dynamic_needed(parsed)
    check(set(needed) <= {"libc.so", "libm.so", "libdl.so", "liblog.so", "libunwind.so"},
          f"DT_NEEDED limited to bionic libraries: {needed}")
    aligns = {header["align"] for header in parsed["program_headers"] if header["type"] == 1}
    check(aligns == {16384}, f"PT_LOAD p_align = {sorted(aligns)}")
    undefined, defined = elf.symbol_table(parsed, elf.SHT_DYNSYM)
    missing = sorted(EXPECTED_EXPORTS - set(defined))
    check(not missing, f"all {len(EXPECTED_EXPORTS)} hematite_* exports present "
                       f"({missing or 'none missing'})")
    expected_gl = set((ROOT / "abi" / "expected_symbols.txt").read_text().split())
    missing_gl = sorted(expected_gl - set(defined))
    check(not missing_gl, f"{len(expected_gl) - len(missing_gl)}/{len(expected_gl)} "
                          f"gl*/egl* entry points exported ({missing_gl[:5] or 'none missing'})")
    forbidden = FORBIDDEN & set(undefined)
    check(not forbidden, f"no non-bionic undefined symbols (checked {sorted(FORBIDDEN)})")
    print(f"  info: {len(blob)} bytes, {len(set(undefined))} symbols resolved by bionic at load")


def verify_signatures() -> str:
    print("signatures (apksigtool):")
    environment = dict(os.environ, PYTHONPATH=str(ROOT / ".cache/pylibs"))
    completed = subprocess.run([sys.executable, "-m", "apksigtool", "verify", str(APK)],
                               capture_output=True, text=True, env=environment)
    output = (completed.stdout + completed.stderr).strip()
    print("  " + output.replace("\n", "\n  "))
    check(completed.returncode == 0, f"apksigtool verify exited {completed.returncode}")
    return output


def main() -> int:
    if not APK.is_file():
        raise SystemExit(f"{APK} is missing; run ./build.sh apk first")
    print(f"verifying {APK} ({APK.stat().st_size} bytes)")
    failures = []
    for step in (verify_manifest, verify_resource_ids, verify_layout, verify_library,
                 verify_signatures):
        try:
            step()
        except Failure as error:
            failures.append(str(error))
            print(f"  FAIL: {error}")
    if failures:
        print(f"\n{len(failures)} check(s) failed")
        return 1
    print("\nall verification checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
