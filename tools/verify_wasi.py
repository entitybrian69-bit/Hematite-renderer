#!/usr/bin/env python3
"""Compile and execute dependency-free crates using a WASI-hosted Rust compiler.

Fallback only: NOT cargo, Clippy, rustfmt, native/Android ABI or GPU validation.
Use --setup once to fetch pinned packages from npm and PyPI. Toolchains and build
outputs stay in ignored .cache/, never in Git. No credentials are required.
"""
import argparse
import pathlib
import shutil
import subprocess
import sys
import tarfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
CACHE = ROOT / ".cache" / "wasi-verify"
GUEST = CACHE / "root"
SDK = CACHE / "sdk" / "package"
WT = CACHE / "wasmtime"
MEM = CACHE / "memory.wasm"
CRATES = ["hematite-ir", "hematite-config", "hematite-fixed-function", "hematite-shader-cache"]


def run(args, **kwargs):
    print("+", " ".join(map(str, args)), flush=True)
    return subprocess.run(list(map(str, args)), check=True, **kwargs)


def setup():
    CACHE.mkdir(parents=True, exist_ok=True)
    venv = CACHE / "venv"
    run([sys.executable, "-m", "venv", venv])
    run([venv / "bin" / "pip", "install", "wasmtime-bin==36.0.9"])
    binaries = list((venv / "lib").glob("python*/site-packages/wasmtime_cli/wasmtime"))
    if len(binaries) != 1:
        raise RuntimeError("wasmtime-bin package layout changed")
    shutil.copyfile(binaries[0], WT)
    WT.chmod(0o755)
    run(["npm", "pack", "@ai-ecoverse/wasi-rustc@1.83.0-2", "--ignore-scripts", "--pack-destination", CACHE],
        stdout=subprocess.DEVNULL)
    archives = list(CACHE.glob("*wasi-rustc-1.83.0-2.tgz"))
    if len(archives) != 1:
        raise RuntimeError("cannot locate pinned compiler archive")
    SDK.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archives[0]) as archive:
        for member in archive.getmembers():
            relative = pathlib.PurePosixPath(member.name)
            if relative.is_absolute() or ".." in relative.parts:
                raise RuntimeError("unsafe compiler archive path")
            destination = SDK.parent.joinpath(*relative.parts)
            if member.isdir():
                destination.mkdir(parents=True, exist_ok=True)
            elif member.isfile():
                destination.parent.mkdir(parents=True, exist_ok=True)
                with archive.extractfile(member) as source, destination.open("wb") as target:
                    shutil.copyfileobj(source, target)
            else:
                raise RuntimeError("unexpected compiler archive member type")
    # One shared memory: min=399, max=16384 wasm pages; NO export. Wasmtime's
    # wasi-threads linker supplies env::memory from the preload's memory slot.
    MEM.write_bytes(bytes.fromhex("0061736d01000000050701038f03808001"))


def rustc(*args):
    run([WT, "run", "--preload", f"env={MEM}", "-W", "threads=y", "-S", "threads=y",
         "--dir", f"{GUEST}::/", "--dir", f"{SDK}::/sdk",
         "--env", "PATH=/usr/bin:/bin", "--env", "HOME=/tmp", "--env", "TMPDIR=/tmp",
         "--env", "RUSTC_SYSROOT=/sdk", "--env", "RUST_MIN_STACK=67108864",
         "--env", "RUSTC_BOOTSTRAP=1", SDK / "bin" / "rustc.wasm",
         "--sysroot", "/sdk", "--target", "wasm32-wasip1", "--edition", "2021",
         "-D", "warnings", *args], cwd=GUEST)


def execute(path, *args):
    run([WT, "run", "--dir", f"{GUEST}::/", GUEST / path, *args], cwd=GUEST)


def verify():
    for required in [WT, MEM, SDK / "bin" / "rustc.wasm"]:
        if not required.exists():
            raise RuntimeError(f"missing {required}; run with --setup")
    GUEST.mkdir(parents=True, exist_ok=True)
    (GUEST / "tmp").mkdir(exist_ok=True)
    for crate in CRATES:
        source = GUEST / "src" / crate
        shutil.copytree(ROOT / "crates" / crate / "src", source, dirs_exist_ok=True)
        name = crate.replace("-", "_")
        rustc("--crate-name", name, "--crate-type", "lib", "-o", f"/lib{name}.rlib",
              f"/src/{crate}/lib.rs")
        rustc("--crate-name", name, "--test", "-o", f"/{crate}-tests.wasm",
              f"/src/{crate}/lib.rs")
        execute(f"{crate}-tests.wasm", "--test-threads=1")
    shutil.copytree(ROOT / "xtask" / "src", GUEST / "src" / "xtask", dirs_exist_ok=True)
    shutil.copytree(ROOT / "abi", GUEST / "abi", dirs_exist_ok=True)
    # `gen-abi` scans the frontend for hand-written entry points, so the two modules it
    # reads have to exist in the guest even though nothing compiles them here.
    frontend = GUEST / "crates" / "hematite-gl-frontend" / "src"
    frontend.mkdir(parents=True, exist_ok=True)
    for module in ["api.rs", "egl.rs"]:
        shutil.copyfile(ROOT / "crates" / "hematite-gl-frontend" / "src" / module,
                        frontend / module)
    rustc("-o", "/xtask.wasm", "/src/xtask/main.rs")
    rustc("--test", "-o", "/xtask-tests.wasm", "/src/xtask/main.rs")
    execute("xtask-tests.wasm", "--test-threads=1")
    execute("xtask.wasm", "check-abi")
    execute("xtask.wasm", "gen-abi")
    gen = "/crates/hematite-gl-frontend/src/gen"
    for module in ["abi", "decls"]:
        rustc("--crate-name", f"generated_{module}", "--crate-type", "lib", "-o",
              f"/generated_{module}.rlib", f"{gen}/{module}.rs")
    manifest = GUEST / "abi" / "expected_symbols.txt"
    if manifest.read_bytes() != (ROOT / "abi" / "expected_symbols.txt").read_bytes():
        raise RuntimeError("tracked symbol manifest differs from generated manifest")
    # The generated exports are formatted by rustfmt in the repository, so compare
    # structure rather than bytes: one #[no_mangle] definition per entry point, and the
    # same number of by-name lookups.
    exports = (frontend / "gen" / "exports.rs").read_text()
    procs = (frontend / "gen" / "procs.rs").read_text()
    expected = len(manifest.read_text().split())
    if exports.count("#[no_mangle]") != expected:
        raise RuntimeError(f"generated {exports.count('#[no_mangle]')} exports, want {expected}")
    if procs.count(" as usize as *mut core::ffi::c_void,") != expected:
        raise RuntimeError("by-name lookup does not cover every entry point")
    # Exercise the actual command's nonzero exit, not a pipeline's exit status.
    table = GUEST / "abi" / "gl_export.def"
    original = table.read_bytes()
    try:
        table.write_bytes(original + b"\nglDuplicate\tvoid\tvoid\tstate\t1.0\n" * 2)
        try:
            execute("xtask.wasm", "check-abi")
        except subprocess.CalledProcessError as exc:
            if exc.returncode != 1:
                raise
        else:
            raise RuntimeError("duplicate ABI name was accepted")
    finally:
        table.write_bytes(original)
    print("PASS: WASI builds, tests, ABI generation and rejection test. "
          "Cargo/Clippy/rustfmt/Android/GPU checks NOT performed.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--setup", action="store_true", help="install pinned fallback toolchain")
    args = parser.parse_args()
    if args.setup:
        setup()
    verify()
