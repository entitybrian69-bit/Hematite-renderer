#!/usr/bin/env python3
"""Optional x86_64 Linux Rust 1.85.0 bootstrap from bundled npm distributions.

Use official rustup when available. This fallback uses third-party @rustbin
repackaging, verifies observed SHA-256 archive digests, and installs only under
ignored .cache/native-rust/. Hashes pin the observed package contents but are NOT
an independent audit of the compiler or a Rust project signature.
"""
import hashlib
import json
import pathlib
import platform
import shutil
import subprocess
import tarfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
CACHE = ROOT / ".cache" / "native-rust"
PACKAGES = {
    "rustc": ("1.85.0", "aa078c23633de2fa94443ce23c47726cf81e6c17d9efac8f098e3ed803c3b239"),
    "cargo": ("0.86.0", "0d5eafbd9ad97520d35b949f1c128ed07992435a211edde6ca307933bf12eff3"),
    "rust-std": ("1.85.0", "1737553285606b3f2115527869f3132bd4896455da35b12c20683febcf5c9ae6"),
    "clippy": ("0.1.85", "82e919c22ce4d9e0530ef5ae0f2737a52d56d47ad687eeaaf1a1e7bfa89bd0b5"),
    "rustfmt": ("1.8.0", "07af3e51869bd9dcc2f832a9032ba3c74ff2949e4909f378a0a618a08200b988"),
}


def main():
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise SystemExit("This fallback supports x86_64 Linux only; use rustup on other hosts.")
    downloads = CACHE / "downloads"
    downloads.mkdir(parents=True, exist_ok=True)
    prefix = CACHE / "toolchain"
    for component, (version, expected) in PACKAGES.items():
        package = f"@rustbin/{component}-1.85.0-x86_64-unknown-linux-gnu@{version}"
        packed = subprocess.check_output(
            ["npm", "pack", package, "--ignore-scripts", "--json", "--pack-destination", str(downloads)],
            cwd=ROOT, text=True,
        )
        archive = downloads / json.loads(packed)[0]["filename"]
        if hashlib.sha256(archive.read_bytes()).hexdigest() != expected:
            raise SystemExit(f"Archive digest mismatch: {archive}; refusing to execute installer")
        destination = CACHE / component
        destination.mkdir(parents=True, exist_ok=True)
        # Only regular files/directories are allowed; never extract symlinks or devices.
        with tarfile.open(archive) as bundle:
            for member in bundle.getmembers():
                relative = pathlib.PurePosixPath(member.name)
                if relative.is_absolute() or ".." in relative.parts:
                    raise SystemExit("Unsafe archive path")
                path = destination.joinpath(*relative.parts)
                if member.isdir():
                    path.mkdir(parents=True, exist_ok=True)
                elif member.isfile():
                    path.parent.mkdir(parents=True, exist_ok=True)
                    with bundle.extractfile(member) as source, path.open("wb") as target:
                        shutil.copyfileobj(source, target)
                    path.chmod(member.mode & 0o777)
                else:
                    raise SystemExit("Unsafe archive member type")
        subprocess.run(["bash", str(destination / "package" / "install.sh"),
                        f"--prefix={prefix}", "--disable-ldconfig"], check=True)
    subprocess.run([str(prefix / "bin" / "rustc"), "--version"], check=True)
    subprocess.run([str(prefix / "bin" / "cargo"), "--version"], check=True)
    print(f'Use: export PATH="{prefix}/bin:$PATH"')
    print("Only the native host target is installed. No Android stdlib, NDK or JDK.")


if __name__ == "__main__":
    main()
