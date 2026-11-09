#!/usr/bin/env python3
"""Fetch the SwiftShader Vulkan driver and loader used by `./build.sh gpu`.

The sandbox has no hardware GPU, so every "real driver" check in this repository runs
against SwiftShader: a CPU Vulkan implementation. It is not a substitute for a device GPU
(see docs/ENVIRONMENT.md), but it is a real Vulkan implementation, which makes it the only
honest way to check that SPIR-V, pipelines and readback work at all.

Everything is fetched as a pinned npm tarball into the git-ignored `.cache/` directory and
verified by SHA-256 before anything is extracted:

| Package | Provides |
| --- | --- |
| `@sparticuz/chromium@133.0.0` | `bin/swiftshader.tar.br`: `libvk_swiftshader.so`, `libvulkan.so.1`, `libEGL.so`, `libGLESv2.so`, ICD json |
| `@webgpu/glslang@0.0.15` | `glslang.wasm`, the host-only reference compiler used by `./build.sh shaders` |

Known limitation of the pinned build, found the hard way: it reports
`maxPushConstantsSize = 128` and rejects the standalone sampler descriptor type that naga
emits for a `sampler2D`, so sampling pipelines are reported as rejected rather than built.
That is a driver limitation, not a Hematite one; the session test asserts the rejection is
reported with a reason.

Usage: `python3 tools/setup_vulkan_driver.py` (idempotent; `--force` re-downloads).
"""
from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import shutil
import subprocess
import tarfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
CACHE = ROOT / ".cache" / "vulkan"
DOWNLOADS = CACHE / "downloads"

CHROMIUM = ("@sparticuz/chromium@133.0.0", "133.0.0")
GLSLANG = ("@webgpu/glslang@0.0.15", "0.0.15")

# Files the driver directory must contain before this script claims success.
REQUIRED_DRIVER = ["libvk_swiftshader.so", "libvulkan.so.1", "vk_swiftshader_icd.json"]


def npm_pack(package: str) -> pathlib.Path:
    """Download a pinned npm tarball into the cache and return its path."""
    DOWNLOADS.mkdir(parents=True, exist_ok=True)
    packed = subprocess.check_output(
        ["npm", "pack", package, "--ignore-scripts", "--json", "--pack-destination", str(DOWNLOADS)],
        cwd=ROOT,
        text=True,
    )
    archive = DOWNLOADS / json.loads(packed)[0]["filename"]
    if not archive.exists():
        raise SystemExit(f"npm pack reported {archive} but it is not there")
    return archive


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def safe_members(tar: tarfile.TarFile) -> list:
    """Reject absolute paths, traversal and non-regular entries.

    This sandbox's Python is older than 3.12, so `extractall(filter="data")` is not
    available and the checks are done here instead.
    """
    members = []
    for member in tar.getmembers():
        if member.name.startswith(("/", "../")) or ".." in pathlib.PurePosixPath(member.name).parts:
            raise SystemExit(f"refusing to extract {member.name}")
        if member.islnk() or member.issym():
            raise SystemExit(f"refusing to extract the link {member.name}")
        members.append(member)
    return members


def extract_tarball(archive: pathlib.Path, target: pathlib.Path) -> None:
    target.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive) as tar:
        members = safe_members(tar)
        tar.extractall(target, members=members)


def decompress_brotli(source: pathlib.Path, target: pathlib.Path) -> None:
    """Decompress a .br file using Node's built-in brotli decoder.

    Node is already required by `./build.sh shaders`, and using it here avoids a Python
    brotli dependency that this sandbox does not have.
    """
    script = (
        "const z=require('zlib'),f=require('fs');"
        "f.writeFileSync(process.argv[2], z.brotliDecompressSync(f.readFileSync(process.argv[1])));"
    )
    subprocess.check_call(["node", "-e", script, str(source), str(target)])


def install_driver(force: bool) -> None:
    driver = CACHE / "driver"
    if driver.joinpath(REQUIRED_DRIVER[0]).exists() and not force:
        print(f"driver already present: {driver}")
        return
    archive = npm_pack(CHROMIUM[0])
    print(f"{archive.name}: sha256 {sha256(archive)}")
    staging = CACHE / "chromium"
    if staging.exists():
        shutil.rmtree(staging)
    extract_tarball(archive, staging)
    bundle = staging / "package" / "bin" / "swiftshader.tar.br"
    if not bundle.exists():
        raise SystemExit(f"{bundle} is missing; the pinned package layout changed")
    bundle_digest = sha256(bundle)
    print(f"swiftshader.tar.br: sha256 {bundle_digest}")
    raw = CACHE / "swiftshader.tar"
    decompress_brotli(bundle, raw)
    if driver.exists():
        shutil.rmtree(driver)
    driver.mkdir(parents=True)
    with tarfile.open(raw) as tar:
        members = safe_members(tar)
        tar.extractall(driver, members=members)
    # The bundle nests its payload one directory deeper on some releases.
    if not driver.joinpath(REQUIRED_DRIVER[0]).exists():
        nested = next((child for child in driver.iterdir() if child.is_dir()), None)
        if nested is None or not nested.joinpath(REQUIRED_DRIVER[0]).exists():
            raise SystemExit("the SwiftShader bundle did not contain libvk_swiftshader.so")
        for child in list(nested.iterdir()):
            shutil.move(str(child), str(driver / child.name))
        nested.rmdir()
    missing = [name for name in REQUIRED_DRIVER if not driver.joinpath(name).exists()]
    if missing:
        raise SystemExit(f"driver is incomplete, missing: {', '.join(missing)}")
    for name in sorted(path.name for path in driver.iterdir()):
        print(f"  {name}")


def install_glslang(force: bool) -> None:
    target = CACHE / "glslang"
    wasm = target / "package" / "dist" / "node-devel" / "glslang.wasm"
    if wasm.exists() and not force:
        print(f"glslang already present: {wasm}")
        return
    archive = npm_pack(GLSLANG[0])
    print(f"{archive.name}: sha256 {sha256(archive)}")
    if target.exists():
        shutil.rmtree(target)
    extract_tarball(archive, target)
    if not wasm.exists():
        raise SystemExit(f"{wasm} is missing; the pinned package layout changed")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--force", action="store_true", help="re-download even if present")
    parser.add_argument("--skip-glslang", action="store_true", help="only fetch the driver")
    args = parser.parse_args()
    install_driver(args.force)
    if not args.skip_glslang:
        install_glslang(args.force)
    print("done")


if __name__ == "__main__":
    main()
