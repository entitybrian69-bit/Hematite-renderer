#!/usr/bin/env python3
"""Validate abi/gl_export.def: column count, name syntax, duplicates, known C types.

This mirrors the checks in `xtask/src/main.rs` so the table can be verified in
environments without a Rust toolchain (see docs/ENVIRONMENT.md).
"""
import sys, re
TYPES = {
 "void","GLenum","GLboolean","GLbitfield","GLbyte","GLubyte","GLshort","GLushort","GLint",
 "GLuint","GLint64","GLuint64","GLsizei","GLfloat","GLclampf","GLdouble","GLclampd",
 "GLsizeiptr","GLintptr","GLsync","GLDEBUGPROC",
 "EGLDisplay","EGLConfig","EGLSurface","EGLContext","EGLImageKHR","EGLSyncKHR","EGLSync",
 "EGLNativeDisplayType","EGLNativeWindowType","EGLNativePixmapType","EGLClientBuffer",
 "EGLint","EGLBoolean","EGLenum","EGLAttrib","EGLTimeKHR","EGLTime","EGLnsecsKHR","char",
}
CANVAS = {"target","state","resource","draw","query","sync","unsupported"}


def base_ok(t):
    t = t.strip()
    parts = t.split("*")
    if len(parts) > 3:
        return False, t
    words = parts[0].split()
    base_words = [word for word in words if word != "const"]
    if len(base_words) != 1 or base_words[0] not in TYPES:
        return False, t
    if len(parts) == 1 and "const" in words:
        return False, t
    for index, qualifier in enumerate(parts[1:], 1):
        if qualifier.strip() not in ("", "const"):
            return False, t
        if qualifier.strip() == "const" and index + 1 == len(parts):
            return False, t
    return True, base_words[0]

problems=[]
rows=[]
names={}
for i,line in enumerate(open("abi/gl_export.def"),1):
    s=line.rstrip("\n").strip()
    if not s or s.startswith("#"): continue
    cols=[c.strip() for c in s.split("\t")]
    if len(cols)!=5:
        problems.append(f"line {i}: {len(cols)} columns"); continue
    name,ret,params,canvas,since=cols
    if canvas not in CANVAS: problems.append(f"line {i}: bad canvas {canvas}")
    if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name): problems.append(f"line {i}: bad name {name}")
    if name in names: problems.append(f"line {i}: DUPLICATE {name} (first line {names[name]})")
    names[name]=i
    for label,t in [("ret",ret)]+[("p",p) for p in ([] if params in ("","void") else params.split(","))]:
        ok,b=base_ok(t)
        if not ok: problems.append(f"line {i} {name}: unknown {label} type `{t}` (base `{b}`)")
    if name.startswith("egl") and canvas=="draw": problems.append(f"line {i}: egl as draw")
    rows.append((name,canvas,since))

print(f"entries: {len(rows)}")
from collections import Counter
print("by canvas:", dict(Counter(c for _,c,_ in rows)))
print("by since:", dict(sorted(Counter(s for _,_,s in rows).items())))
if problems:
    print(f"\n{len(problems)} PROBLEM(S):")
    for p in problems[:40]: print("  ",p)
    sys.exit(1)
print("\nOK: table is internally consistent")
