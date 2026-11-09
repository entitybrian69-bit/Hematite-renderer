#!/usr/bin/env python3
"""Drive the exported GL/EGL ABI of a built `libhematite` through ctypes.

    cargo build -p hematite-plugin            # or: ./build.sh android
    python3 tools/test_gl_abi.py [path/to/libhematite_plugin.so]

This is the check that matters for a launcher: it loads the shared object the way a
foreign caller would, resolves entry points by name, and issues a real EGL/GL sequence —
`eglGetDisplay`, `eglInitialize`, `eglChooseConfig`, `eglCreatePbufferSurface`,
`eglCreateContext`, `eglMakeCurrent`, `glClear`, immediate-mode triangles, `glReadPixels`
and `eglSwapBuffers` — then compares the pixels it gets back.

It runs against the host build by default, so what it proves is the *ABI and the
renderer*, not Android: no device, emulator or `adb` is involved anywhere here.
"""

from __future__ import annotations

import ctypes
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "abi" / "expected_symbols.txt"

# Enumerants, spelled out so this script has no dependency on the Rust crate.
GL_COLOR_BUFFER_BIT = 0x4000
GL_DEPTH_BUFFER_BIT = 0x0100
GL_RGBA = 0x1908
GL_UNSIGNED_BYTE = 0x1401
GL_TRIANGLES = 0x0004
GL_NO_ERROR = 0
EGL_NONE = 0x3038
EGL_SURFACE_TYPE = 0x3033
EGL_PBUFFER_BIT = 0x0001
EGL_RED_SIZE = 0x3024
EGL_WIDTH = 0x3057
EGL_HEIGHT = 0x3058
EGL_CONTEXT_CLIENT_VERSION = 0x3098
EGL_TRUE = 1


def default_library() -> Path:
    for candidate in (
        ROOT / "target" / "debug" / "libhematite_plugin.so",
        ROOT / "target" / "release" / "libhematite_plugin.so",
        ROOT / "target" / "debug" / "libhematite_plugin.dylib",
        ROOT / "target" / "aarch64-linux-android" / "release" / "libhematite_plugin.so",
    ):
        if candidate.is_file():
            return candidate
    raise SystemExit("no built library found; run `cargo build -p hematite-plugin` first")


def main(path: Path) -> int:
    library = ctypes.CDLL(str(path))
    failures: list[str] = []

    def check(condition: bool, message: str) -> None:
        print(("  ok:   " if condition else "  FAIL: ") + message)
        if not condition:
            failures.append(message)

    print(f"{path}")

    library.hematite_abi_version.restype = ctypes.c_uint32
    check(library.hematite_abi_version() == 1, "hematite_abi_version() == 1")

    library.hematite_gl_export_count.restype = ctypes.c_int32
    exported = library.hematite_gl_export_count()
    expected = MANIFEST.read_text().split()
    check(exported == len(expected) == 866, f"hematite_gl_export_count() == {len(expected)}")

    library.hematite_gl_proc_address.restype = ctypes.c_void_p
    library.hematite_gl_proc_address.argtypes = [ctypes.c_char_p]
    missing = [name for name in expected
               if not library.hematite_gl_proc_address(name.encode())]
    check(not missing, f"all {len(expected)} entry points resolve by name "
                       f"({missing[:5] or 'none missing'})")

    # --- EGL bring-up -------------------------------------------------------
    library.eglGetDisplay.restype = ctypes.c_void_p
    library.eglGetDisplay.argtypes = [ctypes.c_void_p]
    display = library.eglGetDisplay(None)
    check(bool(display), "eglGetDisplay returned a display")

    major, minor = ctypes.c_int32(0), ctypes.c_int32(0)
    library.eglInitialize.restype = ctypes.c_uint32
    library.eglInitialize.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_int32),
                                      ctypes.POINTER(ctypes.c_int32)]
    check(library.eglInitialize(display, ctypes.byref(major), ctypes.byref(minor)) == EGL_TRUE,
          f"eglInitialize reported {major.value}.{minor.value}")

    library.eglChooseConfig.restype = ctypes.c_uint32
    library.eglChooseConfig.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_int32),
                                        ctypes.POINTER(ctypes.c_void_p), ctypes.c_int32,
                                        ctypes.POINTER(ctypes.c_int32)]
    config = ctypes.c_void_p(0)
    count = ctypes.c_int32(0)
    attribs = (ctypes.c_int32 * 7)(EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RED_SIZE, 8,
                                   EGL_NONE, 0, 0)
    check(library.eglChooseConfig(display, attribs, ctypes.byref(config), 1,
                                 ctypes.byref(count)) == EGL_TRUE and count.value == 1,
          f"eglChooseConfig matched {count.value} config(s)")

    library.eglCreatePbufferSurface.restype = ctypes.c_void_p
    library.eglCreatePbufferSurface.argtypes = [ctypes.c_void_p, ctypes.c_void_p,
                                                ctypes.POINTER(ctypes.c_int32)]
    side = 32
    size = (ctypes.c_int32 * 5)(EGL_WIDTH, side, EGL_HEIGHT, side, EGL_NONE)
    surface = library.eglCreatePbufferSurface(display, config, size)
    check(bool(surface), f"eglCreatePbufferSurface returned {side}x{side}")

    library.eglCreateContext.restype = ctypes.c_void_p
    library.eglCreateContext.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p,
                                         ctypes.POINTER(ctypes.c_int32)]
    context_attribs = (ctypes.c_int32 * 3)(EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE)
    context = library.eglCreateContext(display, config, None, context_attribs)
    check(bool(context), "eglCreateContext returned a context")

    library.eglMakeCurrent.restype = ctypes.c_uint32
    library.eglMakeCurrent.argtypes = [ctypes.c_void_p] * 4
    check(library.eglMakeCurrent(display, surface, surface, context) == EGL_TRUE,
          "eglMakeCurrent bound the surface and context")

    # --- A real GL frame ----------------------------------------------------
    library.glViewport.argtypes = [ctypes.c_int32] * 4
    library.glViewport(0, 0, side, side)
    library.glClearColor.argtypes = [ctypes.c_float] * 4
    library.glClearColor(0.0, 0.0, 1.0, 1.0)
    library.glClear.argtypes = [ctypes.c_uint32]
    library.glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT)

    library.glReadPixels.argtypes = [ctypes.c_int32] * 4 + [ctypes.c_uint32, ctypes.c_uint32,
                                                            ctypes.c_void_p]
    pixels = (ctypes.c_uint8 * (side * side * 4))()
    library.glReadPixels(0, 0, side, side, GL_RGBA, GL_UNSIGNED_BYTE, pixels)
    cleared = bytes(pixels)
    check(cleared[:4] == bytes([0, 0, 255, 255]), f"glClear+glReadPixels gave {list(cleared[:4])}")
    check(len(set(cleared[i:i + 4] for i in range(0, len(cleared), 4))) == 1,
          "the whole target cleared to one colour")

    library.glBegin.argtypes = [ctypes.c_uint32]
    library.glColor4f.argtypes = [ctypes.c_float] * 4
    library.glVertex2f.argtypes = [ctypes.c_float] * 2
    library.glBegin(GL_TRIANGLES)
    library.glColor4f(1.0, 0.0, 0.0, 1.0)
    library.glVertex2f(-1.0, -1.0)
    library.glVertex2f(1.0, -1.0)
    library.glVertex2f(0.0, 1.0)
    library.glEnd()
    library.glReadPixels(0, 0, side, side, GL_RGBA, GL_UNSIGNED_BYTE, pixels)
    drawn = bytes(pixels)
    centre = drawn[((side // 2) * side + side // 2) * 4:][:4]
    check(centre == bytes([255, 0, 0, 255]), f"the immediate-mode triangle covers the centre "
                                             f"{list(centre)}")
    check(drawn != cleared, "drawing changed the framebuffer")

    library.glGetError.restype = ctypes.c_uint32
    check(library.glGetError() == GL_NO_ERROR, "glGetError is clean after the frame")

    library.eglSwapBuffers.restype = ctypes.c_uint32
    library.eglSwapBuffers.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
    check(library.eglSwapBuffers(display, surface) == EGL_TRUE, "eglSwapBuffers published a frame")

    # --- The self-contained demo scene --------------------------------------
    library.hematite_gl_render_demo.restype = ctypes.c_int32
    library.hematite_gl_render_demo.argtypes = [ctypes.c_uint32, ctypes.c_uint32,
                                                ctypes.c_void_p, ctypes.c_size_t,
                                                ctypes.POINTER(ctypes.c_size_t)]
    demo = (ctypes.c_uint8 * (side * side * 4))()
    status = library.hematite_gl_render_demo(side, side, demo, len(demo), None)
    check(status == 0, f"hematite_gl_render_demo returned {status}")
    check(len(set(bytes(demo)[i:i + 4] for i in range(0, len(demo), 4))) > 8,
          "the demo scene is not a flat colour")
    required = ctypes.c_size_t(0)
    status = library.hematite_gl_render_demo(side, side, demo, 4, ctypes.byref(required))
    check(status == 3 and required.value == side * side * 4,
          f"a short buffer reports {status} and needs {required.value} bytes")

    # --- Unimplemented entry points must be reported, not silent -------------
    library.hematite_gl_unsupported_count.restype = ctypes.c_uint64
    before = library.hematite_gl_unsupported_count()
    resident = library.hematite_gl_proc_address(b"glAreTexturesResident")
    if resident:
        call = ctypes.CFUNCTYPE(ctypes.c_uint8, ctypes.c_int32, ctypes.POINTER(ctypes.c_uint32),
                                ctypes.POINTER(ctypes.c_uint8))(resident)
        call(0, None, None)
    check(library.hematite_gl_unsupported_count() == before + 1,
          "an unimplemented entry point increments the counter")

    print(f"\n{len(failures)} failure(s). Not verified: execution on an Android device.")
    return 1 if failures else 0


if __name__ == "__main__":
    target = Path(sys.argv[1]) if len(sys.argv) > 1 else default_library()
    sys.exit(main(target))
