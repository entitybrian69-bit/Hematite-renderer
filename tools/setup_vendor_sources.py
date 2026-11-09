#!/usr/bin/env python3
"""Materialise the vendored crates.io sources the GLSL front-end compiles against.

`crates.io` and `static.crates.io` are not reachable from this build environment
(`curl` returns no connection; see `docs/ENVIRONMENT.md`), so the one third-party
Rust dependency Hematite needs — **naga 0.14.0**, used as the GLSL -> SPIR-V compiler
behind `glCompileShader` — is vendored from its upstream GitHub repository, together
with every crate in its dependency closure. The result is a cargo *directory source*
under `vendor/sources/`, selected by the `[source.crates-io]` replacement in
`.cargo/config.toml`, so `cargo build --offline` resolves everything locally.

    tools/setup_vendor_sources.py [--force] [--dest DIR]

Guarantees
----------
* Every tarball is fetched from `codeload.github.com/<repo>/tar.gz/<ref>` and its
  SHA-256 is compared against the pinned value in `SOURCES` before anything is
  unpacked. A mismatch is a hard error.
* Only `Cargo.toml`, `src/`, `build.rs` and licence files are kept; tests, benches
  and examples are dropped, so nothing that is not compiled ships in the tree.
* `[dev-dependencies]`, `[[bench]]`, `[[test]]`, `[[example]]`, `[[bin]]`,
  `[workspace]`, `[patch]` and `[profile.*]` sections are stripped from each vendored
  manifest. Those sections are the only ones that would pull in crates that are not
  vendored (for example naga's `rspirv` git dev-dependency) and cargo ignores them
  for a dependency anyway. This is recorded per crate in `vendor/sources/PROVENANCE`.
* The declared `version` of each unpacked manifest is checked against `SOURCES`.

The vendored tree *is* committed, because CI has no more network access to crates.io
than this sandbox does. Upstream licences are kept next to each crate's sources.
"""

from __future__ import annotations

import argparse
import hashlib
import shutil
import subprocess
import sys
import tarfile
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEST = ROOT / "vendor/sources"

KEEP_FILES = {"Cargo.toml", "build.rs"}
KEEP_DIRS = {"src"}
# Some crates keep their sources next to the manifest instead of under `src/`
# (the vendored `spirv` crate is `lib.rs` + `autogen_spirv.rs`).
KEEP_TOP_LEVEL_SUFFIXES = (".rs",)
KEEP_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "NOTICE")

# Manifest sections that are not needed to build the crate and that would otherwise
# drag in crates which are not vendored here.
DROP_SECTIONS = (
    "[dev-dependencies",
    "[target.'cfg(test)'.dev-dependencies",
    "[[bench]]",
    "[[test]]",
    "[[example]]",
    "[[bin]]",
    "[workspace",
    "[patch",
    "[profile.",
)


@dataclass(frozen=True)
class Source:
    """One pinned upstream crate: where it comes from and what it must contain."""

    name: str
    version: str
    repo: str
    ref: str
    sha256: str
    subdir: str = "."


SOURCES: tuple[Source, ...] = (
    # The GLSL -> SPIR-V compiler itself. glsl-in pulls in pp-rs (the GLSL
    # preprocessor) and spv-out pulls in the `spirv` crate.
    Source("naga", "0.14.0", "gfx-rs/naga", "v0.14.0",
           "408128a255eba79763d9b7c5422d9c9d6a62019001f80f5ab28d34436c6189eb"),
    Source("pp-rs", "0.2.1", "Kangz/glslpp-rs", "v0.2.1",
           "f00a169b3e6246a3fa9ab2b7cf973c44e07f7626cc9f4cd11cd862fdb178e4d4"),
    Source("unicode-xid", "0.2.6", "unicode-rs/unicode-xid", "v0.2.6",
           "343aeece0b1c48c5206e5cdc1e267934f7e609f5fe9eebfaac68026af5eab0d4"),
    Source("spirv", "0.2.0+1.5.4", "gfx-rs/rspirv", "spirv-0.2.0%2B1.5.4",
           "3e514573dbf34d0773da0baa9b31a7ad42edff2875d1811251f48e04fd2381ab",
           subdir="spirv"),
    # naga's own dependencies.
    Source("bitflags", "2.4.1", "bitflags/bitflags", "2.4.1",
           "bef8d694bf9e487ce1351c9e5fe5cf73d47ef470d42e28d841614776b0064437"),
    # The `spirv` crate still requires bitflags 1.x; cargo keeps both majors side by side.
    Source("bitflags", "1.3.2", "bitflags/bitflags", "1.3.2",
           "52ab9ccde868e856b38d4033a6bca0340422cb6460fd587fb26bf46659eb6e34"),
    Source("bit-set", "0.5.3", "contain-rs/bit-set", "v0.5.3",
           "4020e4a1adba1e1042a6f40051a0d09b76996186cc404fc8ca9892a6f9193edd"),
    # contain-rs/bit-vec has no tags; this is the newest commit whose manifest still
    # declares 0.6.3, which is what bit-set 0.5.3 requires.
    Source("bit-vec", "0.6.3", "contain-rs/bit-vec",
           "d80f4a1c0a66a0aae77a75eb379c99266eababa1",
           "39a8b8b74da4a8fe03b3a07cae972b5d10cedfb5410ba2b9e8ebfaab595ef1d4"),
    Source("rustc-hash", "1.2.0", "rust-lang/rustc-hash", "v1.2.0",
           "6615cba851e9c216fc6d80b7ef524fc027bcaf180102b534019c54111bb2f000"),
    Source("indexmap", "2.2.6", "indexmap-rs/indexmap", "2.2.6",
           "11a8a9fa470e76b0f23f323511b31ffadb6813392993ca95003284829815d5e4"),
    Source("hashbrown", "0.14.5", "rust-lang/hashbrown", "v0.14.5",
           "71b280974f03457135f3c6c6569a22c518142c7de67d17e9d1aa1b87bb7328e0"),
    Source("equivalent", "1.0.1", "indexmap-rs/equivalent", "v1.0.1",
           "bff81cf08932a4f1c8c666b016b7ebe019e6216af2d434173e1389c72a58308b"),
    Source("log", "0.4.20", "rust-lang/log", "0.4.20",
           "a24acf26738e61c136187dbf1fffb065571126c38551e67152e7afa4a19d4e1f"),
    Source("num-traits", "0.2.17", "rust-num/num-traits", "num-traits-0.2.17",
           "32452a39c4ec81541d64e3a35dff2eda8980340c9a04514368289dc65fbde1e1"),
    Source("autocfg", "1.1.0", "cuviper/autocfg", "1.1.0",
           "06722c78d90bf24e9d43f3631a6e693e4bb66796593e93535c1a7da21e2931d8"),
    Source("thiserror", "1.0.69", "dtolnay/thiserror", "1.0.69",
           "51c7841622b72338d3bb5f4441b452ad414805ebdbe625dca6068fec4fbc25c9"),
    Source("thiserror-impl", "1.0.69", "dtolnay/thiserror", "1.0.69",
           "51c7841622b72338d3bb5f4441b452ad414805ebdbe625dca6068fec4fbc25c9",
           subdir="impl"),
    Source("proc-macro2", "1.0.92", "dtolnay/proc-macro2", "1.0.92",
           "ac4519676856555be81bf41e3504f0e1d2a03cb60ba4355463b5e8b61cfab53c"),
    Source("quote", "1.0.37", "dtolnay/quote", "1.0.37",
           "af026c2ca571b949fd4a9e090426b0b164342c01fa4a324a712b0da911839e4f"),
    Source("syn", "2.0.90", "dtolnay/syn", "2.0.90",
           "bcaa27439ea5f6fe77e84896b0acc3ad24fc23a87b2e3a540ec73eed57bf987a"),
    Source("unicode-ident", "1.0.14", "dtolnay/unicode-ident", "1.0.14",
           "f14ab2113b83b854baf3e009b7c96cf12f51b4a717f4aa9211aa82de46100d27"),
)


class Failure(Exception):
    pass


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def fetch(source: Source, cache: Path) -> Path:
    """Download the pinned tarball into the cache and check its digest."""
    archive = cache / f"{source.name}-{source.version}.tgz"
    if not archive.exists():
        cache.mkdir(parents=True, exist_ok=True)
        url = f"https://codeload.github.com/{source.repo}/tar.gz/{source.ref}"
        print(f"  fetch {source.repo}@{source.ref}")
        completed = subprocess.run(["curl", "-sSL", "--max-time", "300", "-o",
                                    str(archive), url], check=False)
        if completed.returncode != 0:
            raise Failure(f"download of {url} exited {completed.returncode}")
    actual = sha256_of(archive)
    if actual != source.sha256:
        raise Failure(f"{source.name} {source.version}: sha256 {actual} != pinned "
                      f"{source.sha256}")
    if archive.stat().st_size < 1024:
        raise Failure(f"{source.name}: archive is only {archive.stat().st_size} bytes")
    return archive


def strip_sections(manifest: str) -> str:
    """Remove manifest sections that cargo would need to resolve but we do not vendor."""
    kept: list[str] = []
    dropping = False
    for line in manifest.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            dropping = any(stripped.startswith(prefix) for prefix in DROP_SECTIONS)
        if not dropping:
            kept.append(line)
    return "\n".join(kept).rstrip() + "\n"


def dependency_names(manifest: str) -> set[str]:
    """Every crate the kept manifest depends on, however it is spelled."""
    names: set[str] = set()
    section = ""
    for line in manifest.splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            section = stripped[1:-1]
            if section.startswith("dependencies."):
                names.add(section.split(".", 1)[1])          # [dependencies.foo]
            elif section.startswith("target.") and ".dependencies." in section:
                names.add(section.rsplit(".", 1)[-1])        # [target.'cfg'.dependencies.foo]
            continue
        if section == "dependencies" or (
                section.startswith("target.") and section.endswith(".dependencies")):
            if "=" in line and not stripped.startswith("#"):
                names.add(line.split("=", 1)[0].strip())
    return names


def prune_dangling_features(manifest: str) -> str:
    """Comment out `[features]` entries that reference something that is not vendored.

    Stripping `[dev-dependencies]` can leave a feature list pointing at a crate that is
    no longer declared (hashbrown 0.14.5 enables `bumpalo/allocator_api` from a
    dev-dependency), and cargo rejects the whole manifest over one such entry. Dropping
    an entry can in turn dangle another one that listed it, so this repeats until the
    feature table is self-consistent. Every removal stays visible as a comment.
    """
    crates = dependency_names(manifest)
    lines = manifest.splitlines()
    table_start = next((index for index, line in enumerate(lines)
                        if line.strip() == "[features]"), None)
    if table_start is None:
        return "\n".join(lines) + "\n"
    table_end = next((index for index in range(table_start + 1, len(lines))
                      if lines[index].startswith("[")), len(lines))

    # One entry: name -> (first line, line after the last, tokens). Entries may span
    # several lines, so tokens are collected until the brackets balance.
    entries: dict[str, tuple[int, int, list[str]]] = {}
    index = table_start + 1
    while index < table_end:
        stripped = lines[index].strip()
        if not stripped or stripped.startswith("#") or "=" not in stripped:
            index += 1
            continue
        name = stripped.split("=", 1)[0].strip()
        tokens: list[str] = []
        last = index
        open_brackets = 0
        while last < table_end:
            body = lines[last]
            tokens += body.replace("[", " ").replace("]", " ").replace(",", " ").split()[
                1 if last == index else 0:]
            open_brackets += body.count("[") - body.count("]")
            if open_brackets <= 0:
                break
            last += 1
        entries[name] = (index, last + 1, [token for token in tokens if token != "="])
        index = last + 1

    def dangling(tokens: list[str], defined: set[str]) -> bool:
        for token in tokens:
            if "/" in token:
                crate = token.split("/")[0].strip().strip("\"'").rstrip("?")
                if crate not in crates:
                    return True
            else:
                feature = token.strip().strip("\"'").removeprefix("dep:")
                if feature and feature not in defined:
                    return True
        return False

    dropped: set[str] = set()
    while True:
        defined = (set(entries) - dropped) | crates
        newly = {name for name, (_first, _last, tokens) in entries.items()
                 if name not in dropped and dangling(tokens, defined)}
        if not newly:
            break
        dropped |= newly
    for name in sorted(dropped):
        first, last, _tokens = entries[name]
        for position in range(first, last):
            lines[position] = (
                "# unvendored reference, disabled by tools/setup_vendor_sources.py: "
                if position == first else "# ") + lines[position]
    return "\n".join(lines) + "\n"


def declared_version(manifest: str) -> str | None:
    in_package = False
    for line in manifest.splitlines():
        if line.strip().startswith("["):
            in_package = line.strip() == "[package]"
            continue
        if in_package and line.startswith("version"):
            value = line.split("=", 1)[1]
            # `version = "0.4.20" # remember to update html_root_url` is legal TOML.
            return value.split("#", 1)[0].strip().strip('"')
    return None


def want_member(relative: str) -> bool:
    parts = Path(relative).parts
    if parts[0] in KEEP_DIRS:
        return True
    if len(parts) == 1 and parts[0] in KEEP_FILES:
        return True
    if len(parts) == 1 and parts[0].endswith(KEEP_TOP_LEVEL_SUFFIXES):
        return True
    return len(parts) == 1 and parts[0].startswith(KEEP_PREFIXES)


def unpack(source: Source, archive: Path, dest: Path) -> None:
    crate = dest / f"{source.name}-{source.version}"
    if crate.exists():
        shutil.rmtree(crate)
    crate.mkdir(parents=True)
    prefix: str | None = None
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar.getmembers():
            relative = Path(member.name)
            if prefix is None:
                prefix = relative.parts[0]
            parts = relative.parts[1:]
            if source.subdir != "." and not relative.is_relative_to(
                    f"{prefix}/{source.subdir}"):
                continue
            if source.subdir != ".":
                parts = relative.parts[1 + len(Path(source.subdir).parts):]
            if not parts or not want_member(str(Path(*parts))):
                continue
            if any(part.startswith("..") or Path(part).is_absolute() for part in parts):
                raise Failure(f"{source.name}: unsafe path {member.name}")
            target = crate.joinpath(*parts)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            handle = tar.extractfile(member)
            if handle is None:
                raise Failure(f"{source.name}: {member.name} is not a regular file")
            target.write_bytes(handle.read())
    manifest_path = crate / "Cargo.toml"
    if not manifest_path.exists():
        raise Failure(f"{source.name}: no Cargo.toml after unpacking")
    manifest = prune_dangling_features(
        strip_sections(manifest_path.read_text(encoding="utf-8")))
    version = declared_version(manifest)
    if version != source.version:
        raise Failure(f"{source.name}: manifest declares {version!r}, expected "
                      f"{source.version!r}")
    if "workspace = true" in manifest:
        raise Failure(f"{source.name}: manifest uses workspace inheritance, which a "
                      f"standalone vendored crate cannot resolve")
    manifest_path.write_text(manifest, encoding="utf-8")
    # A directory source needs this file; `files: {}` means "trust the tree as-is" and
    # `package: null` means "not a .crate, do not verify a package checksum".
    (crate / ".cargo-checksum.json").write_text('{"files":{},"package":null}\n',
                                                encoding="utf-8")


def write_provenance(dest: Path) -> None:
    lines = [
        "# Vendored crate sources",
        "",
        "Materialised by `tools/setup_vendor_sources.py`; each tarball is SHA-256 pinned",
        "there. Manifest sections that cargo does not need to build the crate",
        "(`[dev-dependencies]`, benches, tests, examples, bins, `[workspace]`, `[patch]`,",
        "`[profile.*]`) are stripped so that no un-vendored crate is referenced.",
        "",
        "| crate | version | upstream | ref | subdirectory |",
        "| --- | --- | --- | --- | --- |",
    ]
    for source in SOURCES:
        lines.append(f"| {source.name} | {source.version} | "
                     f"https://github.com/{source.repo} | `{source.ref}` | "
                     f"{source.subdir} |")
    lines += [
        "",
        "naga and pp-rs are MIT OR Apache-2.0; the licences travel with each crate's",
        "sources. See `docs/VENDORED.md` for why vendoring is used at all.",
        "",
    ]
    (dest / "PROVENANCE.md").write_text("\n".join(lines), encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--force", action="store_true", help="re-unpack every crate")
    parser.add_argument("--dest", type=Path, default=DEST)
    arguments = parser.parse_args()

    dest = arguments.dest
    cache = ROOT / ".cache" / "vendor"
    print(f"vendoring {len(SOURCES)} crates into {dest.relative_to(ROOT)}")
    for source in SOURCES:
        crate = dest / f"{source.name}-{source.version}"
        if crate.exists() and (crate / ".cargo-checksum.json").exists() \
                and not arguments.force:
            print(f"  cached {source.name} {source.version}")
            continue
        unpack(source, fetch(source, cache), dest)
        print(f"  ok     {source.name} {source.version}")
    write_provenance(dest)
    print("done")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Failure as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
