#!/usr/bin/env python3
"""Cross-link Rust `cdylib` output for `aarch64-linux-android` without an NDK.

Why this exists
---------------
Android system libraries are stub `.so` files even in the official NDK: they carry
symbol *definitions* so the linker can record `DT_NEEDED` entries and check references,
while implementations resolve from bionic at load time. No NDK, bionic sysroot or
Android build tools are reachable in this environment, so this tool reconstructs that
stub set for the symbols the link actually references.

The Zig distribution available here (the `ziglang` PyPI wheel) ships no bionic/Android
libc bundle, and its driver refuses `--target ...-android*` outright ("unable to provide
libc for target ..."), even with `-nostdlib`. So the link is driven by Zig's
`aarch64-linux-musl` target with rustc's own `-nodefaultlibs`, and the Android system
libraries are supplied as generated stubs whose `DT_NEEDED` names are bionic's real ones
(`libc.so`, `libm.so`, `libdl.so`, `liblog.so`). The resulting object is an ELF64 AArch64
`ET_DYN` with the same shape an NDK link produces: undefined symbols resolved by bionic
at `dlopen` time. The stub libraries themselves are link-time artifacts and are never
shipped.

What it does, per link
----------------------
1. Reads every ELF object and `rlib` (ar archive) member on the link line with the ELF
   parser in this file and computes the symbols that are undefined after resolving the
   definitions present in those inputs. (Zig's `cc` driver rejects
   `--unresolved-symbols=ignore-all`, so no throw-away probe link is performed; the set
   is a superset of what survives `--gc-sections`, which is safe for stubs because
   `--as-needed` still derives `DT_NEEDED` from real references.)
2. Writes one stub `.so` per Android library, defining exactly those symbols.
3. Re-runs the link through `zig ld.lld` directly (not `zig cc`, whose driver silently
   substitutes its musl `libc.so`/`libunwind.a` for `-lc`/`-lunwind`), with the stub
   directory as the only `-L` path, rustc's own flags, and `-z max-page-size=16384`
   for Android 15 16 KB pages.
4. Verifies the produced `.so`: ELF64 AArch64 `ET_DYN`, `DT_NEEDED` limited to the
   generated stubs, 16 KB segment alignment, and every remaining undefined symbol being
   one that was requested. Verification failure aborts the build; nothing is ignored.

Limitations that are NOT worked around here
-------------------------------------------
* Symbol -> library assignment below is a table, not the NDK's. Any symbol not listed is
  assumed to live in `libc.so`. That assumption is only truly tested by `dlopen` on a
  device, which this environment cannot do.
* No bionic implementation code is compiled in; this is link-time bookkeeping only.
* No NDK API-level gating: symbols added after API 21 are not detected.

Usage (invoked by Rust as the linker):
    python3 tools/android_linker.py <linker arguments...>
Environment: HEMATITE_ZIG (required), HEMATITE_LINK_TARGET (default
aarch64-linux-musl), HEMATITE_STUB_DIR (default .cache/android/stubs).
"""

from __future__ import annotations

import os
import re
import struct
import subprocess
import sys
from pathlib import Path

# Symbols bionic exports from liblog.so.
LIBLOG = {
    "__android_log_assert", "__android_log_print", "__android_log_vprint",
    "__android_log_write", "__android_log_vwrite", "__android_log_btwrite",
}
# dl* live in libdl.so (a real library on every Android API level Rust supports).
LIBDL = {"dlopen", "dlsym", "dlvsym", "dlclose", "dlerror", "dladdr", "dl_iterate_phdr"}
# Rust's unwinder resolves _Unwind_* from libunwind.so on Android.
LIBUNWIND_PREFIX = "_Unwind_"
# libm.so. The default for everything else is libc.so, which is where bionic keeps the
# remainder of the C library (including pthread, POSIX, and stdio symbols).
LIBM = {
    "acos", "acosh", "asin", "asinh", "atan", "atan2", "atanh", "cbrt", "ceil", "cos",
    "cosh", "exp", "exp2", "expm1", "fabs", "fdim", "floor", "fma", "fmaf", "fmax",
    "fmin", "fmod", "frexp", "frexpf", "hypot", "ilogb", "ldexp", "ldexpf", "lgamma",
    "log", "log10", "log1p", "log2", "logb", "modf", "nan", "nearbyint", "nextafter",
    "pow", "powf", "remainder", "remquo", "rint", "round", "scalbn", "sin", "sinh",
    "sqrt", "sqrtf", "tan", "tanh", "tgamma", "trunc", "copysign", "copysignf",
}

ET_DYN = 3
EM_AARCH64 = 183
SHT_DYNSYM = 11
SHT_SYMTAB = 2
ET_REL = 1
SHT_DYNAMIC = 6
DT_NEEDED = 1
DT_NULL = 0
SHN_UNDEF = 0


def log(message: str) -> None:
    print(f"[android-linker] {message}", file=sys.stderr)


class ElfError(RuntimeError):
    """The produced object is not the ELF this tool promises to deliver."""


def read_elf64(path: Path) -> dict:
    """Parse the ELF64 headers this tool needs to verify. No third-party parser."""
    return read_elf64_bytes(path.read_bytes(), path)


def read_elf64_bytes(data: bytes, path: object) -> dict:
    """Parse ELF64 headers from bytes, with the same checks as `read_elf64`."""
    if data[:4] != b"\x7fELF":
        raise ElfError(f"{path}: not an ELF file")
    if data[4] != 2 or data[5] != 1:
        raise ElfError(f"{path}: expected ELF64 little-endian")
    e_type, e_machine = struct.unpack_from("<HH", data, 16)
    e_shoff, = struct.unpack_from("<Q", data, 40)
    e_shentsize, e_shnum, e_shstrndx = struct.unpack_from("<HHH", data, 58)
    sections = []
    for index in range(e_shnum):
        offset = e_shoff + index * e_shentsize
        name, kind, _flags, _addr, sec_offset, size, link, _info, align, entsize = \
            struct.unpack_from("<IIQQQQIIQQ", data, offset)
        sections.append(dict(name=name, type=kind, offset=sec_offset, size=size,
                             link=link, entsize=entsize))
    program_headers = []
    e_phoff, = struct.unpack_from("<Q", data, 32)
    e_phentsize, e_phnum = struct.unpack_from("<HH", data, 54)
    for index in range(e_phnum):
        offset = e_phoff + index * e_phentsize
        p_type, _flags, _off, _vaddr, _paddr, _filesz, _memsz, p_align = \
            struct.unpack_from("<IIQQQQQQ", data, offset)
        program_headers.append(dict(type=p_type, align=p_align))
    return dict(type=e_type, machine=e_machine, sections=sections,
                program_headers=program_headers, data=data)


def section_data(elf: dict, section: dict) -> bytes:
    return elf["data"][section["offset"]:section["offset"] + section["size"]]


def dynamic_needed(elf: dict) -> list[str]:
    """Return `DT_NEEDED` library names, in link order."""
    dynamic = next((s for s in elf["sections"] if s["type"] == SHT_DYNAMIC), None)
    if dynamic is None:
        raise ElfError("no .dynamic section")
    strings = section_data(elf, elf["sections"][dynamic["link"]])
    needed = []
    for index in range(dynamic["size"] // 16):
        tag, value = struct.unpack_from("<qQ", section_data(elf, dynamic), index * 16)
        if tag == DT_NULL:
            break
        if tag == DT_NEEDED:
            end = strings.index(b"\0", value)
            needed.append(strings[value:end].decode("utf-8"))
    return needed


def symbol_table(elf: dict, kind: int) -> tuple[list[str], list[str]]:
    """Return (undefined, defined) symbol names from a `.dynsym`/`.symtab` section."""
    section = next((s for s in elf["sections"] if s["type"] == kind), None)
    if section is None:
        return [], []
    strings = section_data(elf, elf["sections"][section["link"]])
    table = section_data(elf, section)
    entry = section["entsize"] or 24
    undefined, defined = [], []
    for index in range(len(table) // entry):
        name_offset, _info, _other, shndx, _value, _size = \
            struct.unpack_from("<IBBHQQ", table, index * entry)
        if not name_offset:
            continue
        end = strings.index(b"\0", name_offset)
        name = strings[name_offset:end].decode("utf-8")
        (undefined if shndx == SHN_UNDEF else defined).append(name)
    return undefined, defined


def undefined_symbols(elf: dict) -> list[str]:
    """Undefined `.dynsym` entries, i.e. symbols this shared object expects at load."""
    undefined, _ = symbol_table(elf, SHT_DYNSYM)
    return sorted(set(undefined))


def ar_members(blob: bytes) -> list[bytes]:
    """Yield each ELF member of a GNU `ar` archive, supporting the `//` name table."""
    if not blob.startswith(b"!<arch>\n"):
        raise ElfError("expected an ar archive")
    offset, long_names, members = 8, b"", []
    while offset + 60 <= len(blob):
        header = blob[offset:offset + 60]
        name = header[:16].decode("ascii", "replace").strip()
        size = int(header[48:58].decode("ascii").strip())
        body = blob[offset + 60:offset + 60 + size]
        if name == "//":
            long_names = body
        elif name not in ("/", "/SYM64/"):
            if name.startswith("/"):
                start = int(name[1:])
                name = long_names[start:long_names.index(b"\n", start)].decode("ascii")
            if body.startswith(b"\x7fELF"):
                members.append(body)
        offset += 60 + size + (size % 2)
    return members


def link_inputs(arguments: list[str]) -> list[str]:
    """Files on the link line that can contribute symbols."""
    inputs = []
    skip = False
    for argument in arguments:
        if skip:
            skip = False
            continue
        if argument == "-o":
            skip = True
        elif not argument.startswith("-") and Path(argument).is_file():
            inputs.append(argument)
    return inputs


def unresolved_from_inputs(arguments: list[str]) -> list[str]:
    """Symbols referenced but not defined by any object/archive on the link line."""
    undefined: set[str] = set()
    defined: set[str] = set()
    parsed = 0
    for path in link_inputs(arguments):
        blob = Path(path).read_bytes()
        chunks = ar_members(blob) if blob.startswith(b"!<arch>") else [blob]
        for chunk in chunks:
            elf = read_elf64_bytes(chunk, path)
            for kind in (SHT_SYMTAB, SHT_DYNSYM):
                missing, present = symbol_table(elf, kind)
                undefined.update(missing)
                defined.update(present)
            parsed += 1
    if parsed == 0:
        raise SystemExit("no ELF inputs found on the link line; refusing to guess")
    names = sorted(undefined - defined)
    log(f"parsed {parsed} objects/archives: {len(names)} symbols unresolved by the link inputs")
    return names


def assign(symbols: list[str]) -> dict[str, list[str]]:
    """Assign every referenced symbol to the Android library that exports it."""
    buckets: dict[str, list[str]] = {"liblog.so": [], "libdl.so": [], "libm.so": [],
                                     "libunwind.so": [], "libc.so": []}
    for symbol in symbols:
        if symbol in LIBLOG:
            buckets["liblog.so"].append(symbol)
        elif symbol in LIBDL:
            buckets["libdl.so"].append(symbol)
        elif symbol.startswith(LIBUNWIND_PREFIX):
            buckets["libunwind.so"].append(symbol)
        elif symbol in LIBM:
            buckets["libm.so"].append(symbol)
        else:
            # Documented assumption: bionic exports the rest of the C library from libc.
            buckets["libc.so"].append(symbol)
    return {library: names for library, names in buckets.items() if names}


def write_stub(directory: Path, library: str, symbols: list[str]) -> Path:
    """Generate and compile a link-time-only stub shared library defining `symbols`.

    Symbols are emitted as global `STT_NOTYPE` labels rather than typed C functions:
    the stub only has to satisfy the linker's bookkeeping, and inventing a C signature
    for a symbol whose real type is unknown would be a guess presented as fact.
    """
    directory.mkdir(parents=True, exist_ok=True)
    source = directory / f"{library}.s"
    body = "".join(f"\t.globl {name}\n{name}:\n" for name in sorted(symbols))
    if not body:
        # rustc links -l<name> unconditionally; an empty stub keeps the link working
        # while --as-needed correctly records no DT_NEEDED entry for it.
        body = "hematite_empty_stub:\n"
    source.write_text(f"// generated link-time stub for {library} ({len(symbols)} symbols)\n{body}")
    output = directory / library
    run([zig(), "cc", "-target", target(), "-nostdlib", "-shared",
         f"-Wl,-soname,{library}", "-o", str(output), str(source)])
    return output


def zig() -> str:
    value = os.environ.get("HEMATITE_ZIG")
    if not value:
        raise SystemExit("HEMATITE_ZIG must point at the zig executable")
    return value


def target() -> str:
    """Driver target. Not the Android triple: see the module docstring."""
    return os.environ.get("HEMATITE_LINK_TARGET", "aarch64-linux-musl")


# Compiler-driver-only flags that lld must not see. Zig's own `cc` driver rewrites
# `-lc`/`-lunwind`/`-lm`/`-ldl` into paths from its musl sysroot (verified with `zig cc
# -v`), which would embed musl-flavoured unwind code and leave `__errno_location` and
# friends undefined for bionic. Driving `zig ld.lld` directly keeps library resolution
# entirely inside this tool's stub directory.
DRIVER_ONLY = {"-nodefaultlibs", "-nostdlib", "-nostartfiles", "-static", "-O1"}


def to_lld_arguments(arguments: list[str]) -> list[str]:
    """Translate rustc's compiler-driver link arguments into `ld.lld` arguments."""
    out: list[str] = []
    for argument in arguments:
        if argument.startswith("-Wl,"):
            for piece in argument[4:].split(","):
                if piece and piece not in DRIVER_ONLY:
                    out.append(piece)
        elif argument not in DRIVER_ONLY:
            out.append(argument)
    return out


def run(command: list[str]) -> None:
    completed = subprocess.run(command, capture_output=True, text=True)
    if completed.returncode != 0:
        sys.stderr.write(completed.stdout)
        sys.stderr.write(completed.stderr)
        raise SystemExit(f"command failed ({completed.returncode}): {' '.join(command[:6])} ...")


def main() -> int:
    arguments = sys.argv[1:]
    if "-o" not in arguments:
        raise SystemExit("expected rustc linker arguments containing -o")
    output = Path(arguments[arguments.index("-o") + 1])
    stub_directory = Path(os.environ.get("HEMATITE_STUB_DIR", ".cache/android/stubs")).resolve()

    # 1. Symbols the link inputs do not satisfy.
    symbols = unresolved_from_inputs(arguments)

    # 2. Stub libraries: every library rustc names, populated with the symbols that
    #    were actually requested. --as-needed then records only the ones used.
    buckets = assign(symbols)
    requested = set(buckets)
    for library in ("libc.so", "libm.so", "libdl.so", "liblog.so", "libunwind.so"):
        write_stub(stub_directory, library, buckets.get(library, []))
        log(f"{library}: {len(buckets.get(library, []))} stub symbols")
    if buckets.get("libc.so"):
        log("libc.so assumption applies to: " + ", ".join(sorted(buckets["libc.so"])))

    # 3. Real link through lld directly, keeping rustc's flags and --as-needed
    #    semantics. The stub directory is the only library search path, so every `-l`
    #    resolves here and no foreign libc/unwinder code can be embedded.
    link = [zig(), "ld.lld", f"-L{stub_directory}", *to_lld_arguments(arguments),
            "-z", "max-page-size=16384"]
    if os.environ.get("HEMATITE_LINKER_VERBOSE"):
        log("link: " + " ".join(link[:8]) + " ...")
    run(link)

    # 4. Verify what was produced.
    elf = read_elf64(output)
    problems = []
    if elf["type"] != ET_DYN:
        problems.append(f"expected ET_DYN, got {elf['type']}")
    if elf["machine"] != EM_AARCH64:
        problems.append(f"expected EM_AARCH64, got {elf['machine']}")
    needed = dynamic_needed(elf)
    if not needed:
        problems.append("undefined symbols remain but no DT_NEEDED entries were recorded")
    unexpected = [name for name in needed if name not in requested]
    if unexpected:
        problems.append(f"unexpected DT_NEEDED: {unexpected}")
    weak = [header["align"] for header in elf["program_headers"]
            if header["type"] == 1 and header["align"] < 16384]
    if weak:
        problems.append(f"PT_LOAD segments with p_align < 16384: {weak}")
    remaining = undefined_symbols(elf)
    surprise = sorted(set(remaining) - set(symbols))
    if surprise:
        problems.append(f"undefined symbols never requested by the inputs: {surprise[:8]}")
    log(f"DT_NEEDED: {needed}")
    log(f"undefined at load time: {len(remaining)} symbols (resolved by bionic)")
    if problems:
        for problem in problems:
            log(f"VERIFICATION FAILED: {problem}")
        return 1
    log(f"verified {output}: ELF64 AArch64 ET_DYN, {len(needed)} DT_NEEDED, "
        f"{len(remaining)} dynamic undefined symbols")
    return 0


if __name__ == "__main__":
    sys.exit(main())
