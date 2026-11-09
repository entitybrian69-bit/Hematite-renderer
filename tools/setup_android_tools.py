#!/usr/bin/env python3
"""Reproducibly fetch every non-Rust tool the Android and GPU targets need.

Nothing here is available from apt, rustup or a package registry that this environment
can reach, so each tool is downloaded as a pinned archive and unpacked into `.cache/`
(git-ignored). Every archive is SHA-256 checked before use, extraction is path-validated,
and the script is idempotent: a matching cache entry is left alone.

    tools/setup_android_tools.py [--force]

What it installs and where
--------------------------
Zig 0.17.1 (PyPI `ziglang`)        .cache/android/zig/ziglang/zig
    Cross compiler/linker driver. Used as `zig ld.lld`; see tools/android_linker.py for
    why the Android target triple is not used.
rust-std aarch64-linux-android    .cache/native-rust/toolchain/lib/rustlib/...
    The Rust standard library for the APK's ABI, from the npm `@rustbin` mirror.
Python tools                      .cache/pylibs
    cryptography (APK signing), androguard and apksigtool (independent APK verification).
SwiftShader Vulkan driver         .cache/vulkan/driver
    A real software Vulkan 1.x implementation, extracted from the npm
    `@sparticuz/chromium` package, so `./build.sh gpu` renders without a GPU.
glslang (WebAssembly)             .cache/vulkan/glslang
    GLSL -> SPIR-V compiler used to regenerate `shaders/*.spv` from the checked-in
    original GLSL sources.

These are third-party binaries fetched for local verification only. None of them is
committed, vendored into the workspace source tree, or shipped inside the APK.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CACHE = ROOT / ".cache"

ZIG_VERSION = "0.17.1"
ZIG_SHA256 = "ea6cc5d87b033cf0aedfbc8f9549a908cae6c50ac31f1698188fa48a74524b54"
RUST_STD_NPM = "@rustbin/rust-std-1.85.0-aarch64-linux-android"
CHROMIUM_NPM = "@sparticuz/chromium@133.0.0"
CHROMIUM_SHA256 = "9eb9776cb8623a7e80e2f3acbbf8d7e686d0652eb98a57e9abbd2e618c9263ad"
GLSLANG_NPM = "@webgpu/glslang@0.0.15"
GLSLANG_SHA256 = "7b193e085256435213b281d4782154ebbdf2c1ef1c8cb1d38cf75d8be82d97f8"
REQUIREMENTS = ROOT / "tools/android-requirements.txt"


def run(command: list[str], **kwargs) -> subprocess.CompletedProcess:
    """Run a command, surfacing its output when it fails."""
    completed = subprocess.run(command, capture_output=True, text=True, **kwargs)
    if completed.returncode != 0:
        sys.stderr.write(completed.stdout + completed.stderr)
        raise SystemExit(f"failed ({completed.returncode}): {' '.join(command[:8])}")
    return completed


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def check(path: Path, expected: str) -> None:
    actual = sha256(path)
    if actual != expected:
        raise SystemExit(f"{path.name}: sha256 {actual} != pinned {expected}")
    print(f"  sha256 verified: {path.name}")


def safe_extract_tar(archive: Path, destination: Path) -> None:
    """Extract a tarball, refusing absolute paths and parent traversal."""
    destination.mkdir(parents=True, exist_ok=True)
    root = destination.resolve()
    with tarfile.open(archive) as tar:
        for member in tar.getmembers():
            target = (root / member.name).resolve()
            if not str(target).startswith(str(root) + os.sep):
                raise SystemExit(f"unsafe path in {archive.name}: {member.name}")
            if member.issym() or member.islnk():
                raise SystemExit(f"link in {archive.name}: {member.name}")
            if member.isfile():
                handle = tar.extractfile(member)
                if handle is None:
                    continue
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(handle.read())
            elif member.isdir():
                target.mkdir(parents=True, exist_ok=True)


def safe_extract_zip(archive: Path, destination: Path) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    root = destination.resolve()
    with zipfile.ZipFile(archive) as archive_file:
        for name in archive_file.namelist():
            if name.endswith("/"):
                continue
            target = (root / name).resolve()
            if not str(target).startswith(str(root) + os.sep):
                raise SystemExit(f"unsafe path in {archive.name}: {name}")
            target.parent.mkdir(parents=True, exist_ok=True)
            with archive_file.open(name) as source, target.open("wb") as out:
                shutil.copyfileobj(source, out)


def npm_pack(spec: str, destination: Path) -> Path:
    """Download an npm tarball without running its install scripts."""
    destination.mkdir(parents=True, exist_ok=True)
    listing = run(["npm", "pack", spec, "--ignore-scripts", "--silent",
                   "--pack-destination", str(destination)])
    name = listing.stdout.strip().splitlines()[-1].strip()
    path = destination / name
    if not path.is_file():
        raise SystemExit(f"npm pack did not produce {path}")
    return path


def install_zig(force: bool) -> Path:
    """Zig from its PyPI wheel; the executable bit is restored after unpacking."""
    zig = CACHE / "android/zig/ziglang/zig"
    if zig.is_file() and not force:
        print(f"zig already present: {zig}")
        return zig
    if force and (CACHE / "android/zig").exists():
        shutil.rmtree(CACHE / "android/zig")
    downloads = CACHE / "android/pip"
    run([sys.executable, "-m", "pip", "download", "--no-deps", "-q", "-d", str(downloads),
         f"ziglang=={ZIG_VERSION}"])
    wheels = sorted(downloads.glob(f"ziglang-{ZIG_VERSION}-*.whl"))
    if not wheels:
        raise SystemExit("ziglang wheel not downloaded")
    check(wheels[0], ZIG_SHA256)
    safe_extract_zip(wheels[0], CACHE / "android/zig")
    zig.chmod(0o755)
    print(f"zig installed: {zig} ({run([str(zig), 'version']).stdout.strip()})")
    return zig


def install_rust_std(force: bool) -> None:
    """Add the aarch64-linux-android std to the local Rust toolchain."""
    toolchain = CACHE / "native-rust/toolchain/lib/rustlib/aarch64-linux-android"
    if toolchain.is_dir() and not force:
        print(f"rust-std already present: {toolchain}")
        return
    if not (CACHE / "native-rust/toolchain/bin/cargo").is_file():
        raise SystemExit("run tools/setup_native_rust.py first")
    archive = npm_pack(RUST_STD_NPM, CACHE / "android/downloads")
    staging = CACHE / "android/rust-std"
    if staging.exists():
        shutil.rmtree(staging)
    safe_extract_tar(archive, staging)
    source = staging / "package/rust-std-aarch64-linux-android/lib/rustlib/aarch64-linux-android"
    if not source.is_dir():
        raise SystemExit("rust-std archive layout changed")
    toolchain.parent.mkdir(parents=True, exist_ok=True)
    if toolchain.exists():
        shutil.rmtree(toolchain)
    shutil.copytree(source, toolchain)
    print(f"rust-std installed: {toolchain}")


def install_python_tools(force: bool) -> None:
    """cryptography, androguard and apksigtool into a private prefix."""
    target = CACHE / "pylibs"
    if (target / "apksigtool.py").is_file() and not force:
        print(f"python tools already present: {target}")
        return
    if force and target.exists():
        shutil.rmtree(target)
    if not REQUIREMENTS.is_file():
        raise SystemExit(f"missing {REQUIREMENTS}")
    run([sys.executable, "-m", "pip", "install", "-q", "--target", str(target),
         "-r", str(REQUIREMENTS)])
    print(f"python tools installed: {target}")


def install_vulkan(force: bool) -> None:
    """SwiftShader driver and the glslang WASM compiler from pinned npm tarballs."""
    driver = CACHE / "vulkan/driver/libvk_swiftshader.so"
    glslang = CACHE / "vulkan/glslang/package/dist/node-devel/glslang.wasm"
    if driver.is_file() and glslang.is_file() and not force:
        print(f"vulkan tools already present: {driver.parent}")
        return
    downloads = CACHE / "vulkan/downloads"
    chromium = npm_pack(CHROMIUM_NPM, downloads)
    check(chromium, CHROMIUM_SHA256)
    staging = CACHE / "vulkan/chromium"
    if staging.exists():
        shutil.rmtree(staging)
    safe_extract_tar(chromium, staging)
    compressed = staging / "package/bin/swiftshader.tar.br"
    if not compressed.is_file():
        raise SystemExit("chromium package no longer bundles swiftshader.tar.br")
    # Brotli lives in Node; the tarball is 5 MB and decompresses without executing it.
    raw = CACHE / "vulkan/driver.tar"
    run(["node", "-e",
         "const f=require('fs'),z=require('zlib');"
         f"f.writeFileSync('{raw}',z.brotliDecompressSync(f.readFileSync('{compressed}')))"])
    driver.parent.mkdir(parents=True, exist_ok=True)
    safe_extract_tar(raw, driver.parent)
    for name in ("libvk_swiftshader.so", "libvulkan.so.1", "libEGL.so", "libGLESv2.so"):
        (driver.parent / name).chmod(0o755)

    glslang_archive = npm_pack(GLSLANG_NPM, downloads)
    check(glslang_archive, GLSLANG_SHA256)
    glslang_root = CACHE / "vulkan/glslang"
    if glslang_root.exists():
        shutil.rmtree(glslang_root)
    safe_extract_tar(glslang_archive, glslang_root)
    manifest = json.loads((driver.parent / "vk_swiftshader_icd.json").read_text())
    print(f"swiftshader installed: {driver} (ICD api {manifest['ICD']['api_version']})")
    print(f"glslang installed: {glslang}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--force", action="store_true", help="re-download and reinstall")
    arguments = parser.parse_args()
    for step in (install_zig, install_rust_std, install_python_tools, install_vulkan):
        print(f"{step.__name__}:")
        step(arguments.force)
    print("\nAndroid and GPU tooling is ready. Next:")
    print("  ./build.sh gpu      # real Vulkan offscreen rendering on SwiftShader")
    print("  ./build.sh android  # cross-compile libhematite.so for arm64-v8a")
    print("  ./build.sh apk      # assemble, sign and verify dist/hematite-debug.apk")
    return 0


if __name__ == "__main__":
    sys.exit(main())
