#!/usr/bin/env python3
"""Expand the hand-audited ABI table into the complete legacy export surface.

`abi/gl_export.def` lists the entry points that carry *design weight*: the ones whose
behaviour, classification or modern extension matters. It deliberately does not list every
member of the GL 1.0 typed-variant families (`glVertex2d`..`glVertex4dv`, `glColor3b`..
`glColor4usv`, `glTexCoord1d`..`glTexCoord4iv`, `glNormal3*`, `glMultiTexCoord*`,
`glRasterPos*`, `glWindowPos*`, `glSecondaryColor*`, `glFog*`, `glLight*`, `glMaterial*`,
`glGet*` scalar variants, ...).

Those families are mechanical, and LWJGL 2 resolves every member of them by name: a
missing symbol is not a graceful failure, it is a crash. Writing ~400 lines by hand invites
transcription errors, so they are generated here from the family definitions below, checked
for collisions against the curated table, and appended exactly once.

The output is committed (`abi/gl_export.def` is edited in place by `--write`), so the
committed table stays the single source of truth for `xtask gen-abi`.
"""

from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEF = ROOT / "abi" / "gl_export.def"

# ---------------------------------------------------------------------------
# Family machinery
# ---------------------------------------------------------------------------

# (type suffix, C type) for the eight GL numeric types. Written out rather than
# derived by string slicing: deriving `GLfloat` from ("f", ...) by chopping characters is
# exactly how `GLubyte` became `GLbyte` in an earlier draft of this generator.
SCALARS = [
    ("b", "GLbyte"),
    ("d", "GLdouble"),
    ("f", "GLfloat"),
    ("i", "GLint"),
    ("s", "GLshort"),
    ("ub", "GLubyte"),
    ("ui", "GLuint"),
    ("us", "GLushort"),
]
TYPE = dict(SCALARS)


def _signed_float():
    return [t for t in SCALARS if t[0] in ("d", "f", "i", "s")]


def build() -> list[tuple[str, str, str, str, str]]:
    """Every GL 1.0-1.5 typed-variant family member, with the exact type sets and C
    signatures the Khronos headers declare.

    The type sets are NOT uniform across families. Claiming a symbol that does not exist
    (e.g. `glVertex2b`, `glFogd`, `glUniform1i_v`) is a lie in the ABI manifest, so each
    family below states its types explicitly and the result is checked by
    `abi/validate_abi.py`.
    """
    rows: list[tuple[str, str, str, str, str]] = []

    def add(entries, canvas, since):
        for name, ret, params in entries:
            rows.append((name, ret, params, canvas, since))

    def family(prefix, counts, types, first_arg=""):
        """Scalar `prefix{count}{suffix}` and vector `prefix{count}{suffix}v` forms.

        `counts` of 0 means the family has no component digit (glIndex, glEdgeFlag...).
        """
        out = []
        for n in counts:
            for suffix, cty in types:
                head = f"{prefix}{n}{suffix}" if n else f"{prefix}{suffix}"
                args = ",".join([cty] * n) if n else cty
                if first_arg:
                    out.append((head, "void", f"{first_arg},{args}"))
                    out.append((head + "v", "void", f"{first_arg},const {cty}*"))
                elif n:
                    out.append((head, "void", args))
                    out.append((head + "v", "void", f"const {cty}*"))
                else:
                    out.append((head, "void", cty))
                    out.append((head + "v", "void", f"const {cty}*"))
        return out

    SF = _signed_float()

    # glVertex2/3/4: signed and float only; there is no byte/ubyte/uint/ushort vertex.
    add(family("glVertex", (2, 3, 4), SF), "draw", "1.0")
    # glNormal3: b/d/f/i/s. No ub/ui/us.
    add(family("glNormal", (3,), [t for t in SCALARS if t[0] not in ("ub", "ui", "us")]), "state", "1.0")
    # glColor3/4: all eight types (the only family with ubyte/uint/ushort).
    add(family("glColor", (3, 4), SCALARS), "state", "1.0")
    # glTexCoord1..4: d/f/i/s.
    add(family("glTexCoord", (1, 2, 3, 4), SF), "state", "1.0")
    # glMultiTexCoord1..4 (GL 1.3): d/f/i/s, texture unit first.
    add(family("glMultiTexCoord", (1, 2, 3, 4), SF, first_arg="GLenum"), "state", "1.3")
    # glRasterPos2..4: d/f/i/s.
    add(family("glRasterPos", (2, 3, 4), SF), "state", "1.0")
    # glWindowPos2..4 (GL 1.4): d/f/i/s.
    add(family("glWindowPos", (2, 3, 4), SF), "state", "1.4")
    # glSecondaryColor3: GL 1.4 (arb) and the EXT spelling, both all eight types.
    add(family("glSecondaryColor", (3,), SCALARS), "state", "1.4")
    add(
        [(n + "EXT", r, p) for (n, r, p) in family("glSecondaryColor", (3,), SCALARS)],
        "state",
        "ext",
    )
    # glIndex: d/f/i/s/ub (no float vectors for ub; the header has glIndexubv).
    add(family("glIndex", (0,), [t for t in SCALARS if t[0] in ("d", "f", "i", "s", "ub")]), "state", "1.0")
    # glFog: float and int only.
    add(
        [(f"glFog{t}", "void", f"GLenum,{TYPE[s]}") for s, t in (("f", "f"), ("i", "i"))]
        + [(f"glFog{v}", "void", f"GLenum,const {TYPE[s]}*") for s, v in (("f", "fv"), ("i", "iv"))],
        "state",
        "1.0",
    )
    # glLight and glMaterial take (light/face, pname, value): three arguments.
    # glLightModel takes only (pname, value): two. Collapsing these into one loop would
    # claim a two-argument glLightf that does not exist.
    light_like = [
        ("glLightf", "GLenum,GLenum,GLfloat"),
        ("glLightfv", "GLenum,GLenum,const GLfloat*"),
        ("glLighti", "GLenum,GLenum,GLint"),
        ("glLightiv", "GLenum,GLenum,const GLint*"),
        ("glMaterialf", "GLenum,GLenum,GLfloat"),
        ("glMaterialfv", "GLenum,GLenum,const GLfloat*"),
        ("glMateriali", "GLenum,GLenum,GLint"),
        ("glMaterialiv", "GLenum,GLenum,const GLint*"),
    ]
    model_like = [
        ("glLightModelf", "GLenum,GLfloat"),
        ("glLightModelfv", "GLenum,const GLfloat*"),
        ("glLightModeli", "GLenum,GLint"),
        ("glLightModeliv", "GLenum,const GLint*"),
    ]
    add([(n, "void", p) for n, p in light_like + model_like], "state", "1.0")
    # glTexEnv: float and int (2 parameters after target).
    add(
        [
            ("glTexEnvf", "void", "GLenum,GLenum,GLfloat"),
            ("glTexEnvfv", "void", "GLenum,GLenum,const GLfloat*"),
            ("glTexEnvi", "void", "GLenum,GLenum,GLint"),
            ("glTexEnviv", "void", "GLenum,GLenum,const GLint*"),
        ],
        "state",
        "1.0",
    )
    # glTexGen: double, float and int (glTexGend is real and used by legacy Minecraft).
    add(
        [
            ("glTexGend", "void", "GLenum,GLenum,GLdouble"),
            ("glTexGendv", "void", "GLenum,GLenum,const GLdouble*"),
            ("glTexGenf", "void", "GLenum,GLenum,GLfloat"),
            ("glTexGenfv", "void", "GLenum,GLenum,const GLfloat*"),
            ("glTexGeni", "void", "GLenum,GLenum,GLint"),
            ("glTexGeniv", "void", "GLenum,GLenum,const GLint*"),
        ],
        "state",
        "1.0",
    )
    # glPointParameter (1.4) float and int; EXT_point_parameters is the float scalar.
    add(
        [
            ("glPointParameterf", "void", "GLenum,GLfloat"),
            ("glPointParameterfv", "void", "GLenum,const GLfloat*"),
            ("glPointParameteri", "void", "GLenum,GLint"),
            ("glPointParameteriv", "void", "GLenum,const GLint*"),
            ("glPointParameterfEXT", "void", "GLenum,GLfloat"),
            ("glPointParameterfvEXT", "void", "GLenum,const GLfloat*"),
        ],
        "state",
        "1.4",
    )
    # Curve evaluation (GL 1.0, legacy; Minecraft does not use it but LWJGL exposes it).
    add(
        [
            ("glEvalCoord1d", "void", "GLdouble"),
            ("glEvalCoord1dv", "void", "const GLdouble*"),
            ("glEvalCoord1f", "void", "GLfloat"),
            ("glEvalCoord1fv", "void", "const GLfloat*"),
            ("glEvalCoord2d", "void", "GLdouble,GLdouble"),
            ("glEvalCoord2dv", "void", "const GLdouble*"),
            ("glEvalCoord2f", "void", "GLfloat,GLfloat"),
            ("glEvalCoord2fv", "void", "const GLfloat*"),
            ("glMap1d", "void", "GLenum,GLdouble,GLdouble,GLint,GLint,const GLdouble*"),
            ("glMap1f", "void", "GLenum,GLfloat,GLfloat,GLint,GLint,const GLfloat*"),
            ("glMap2d", "void", "GLenum,GLdouble,GLdouble,GLint,GLint,GLdouble,GLdouble,GLint,GLint,const GLdouble*"),
            ("glMap2f", "void", "GLenum,GLfloat,GLfloat,GLint,GLint,GLfloat,GLfloat,GLint,GLint,const GLfloat*"),
            ("glGetMapdv", "void", "GLenum,GLenum,GLdouble*"),
            ("glGetMapfv", "void", "GLenum,GLenum,GLfloat*"),
            ("glGetMapiv", "void", "GLenum,GLenum,GLint*"),
            ("glMapGrid1d", "void", "GLint,GLdouble,GLdouble"),
            ("glMapGrid1f", "void", "GLint,GLfloat,GLfloat"),
            ("glMapGrid2d", "void", "GLint,GLdouble,GLdouble,GLint,GLdouble,GLdouble"),
            ("glMapGrid2f", "void", "GLint,GLfloat,GLfloat,GLint,GLfloat,GLfloat"),
            ("glEvalMesh1", "void", "GLenum,GLint,GLint"),
            ("glEvalMesh2", "void", "GLenum,GLint,GLint,GLint,GLint"),
            ("glEvalPoint1", "void", "GLint"),
            ("glEvalPoint2", "void", "GLint,GLint"),
        ],
        "unsupported",
        "1.0",
    )
    # Pixel transfer families.
    add(
        [
            ("glPixelMapfv", "void", "GLenum,GLsizei,const GLfloat*"),
            ("glPixelMapuiv", "void", "GLenum,GLsizei,const GLuint*"),
            ("glPixelMapusv", "void", "GLenum,GLsizei,const GLushort*"),
            ("glGetPixelMapfv", "void", "GLenum,GLfloat*"),
            ("glGetPixelMapuiv", "void", "GLenum,GLuint*"),
            ("glGetPixelMapusv", "void", "GLenum,GLushort*"),
            ("glPixelTransferf", "void", "GLenum,GLfloat"),
            ("glPixelTransferi", "void", "GLenum,GLint"),
            ("glPixelZoom", "void", "GLfloat,GLfloat"),
            ("glCopyPixels", "void", "GLint,GLint,GLsizei,GLsizei,GLenum"),
        ],
        "unsupported",
        "1.0",
    )
    # Vertex attributes: 1..4 for f/d/s/i, the vector forms, the 4N* normalized forms,
    # and the int-only I* forms (GL 3.0).
    add(
        [
            (f"glVertexAttrib{n}{s}", "void", f"GLuint,{','.join([cty] * n)}")
            for n in (1, 2, 3, 4)
            for s, cty in [t for t in SCALARS if t[0] in ("f", "d", "s", "i")]
        ]
        + [
            (f"glVertexAttrib{n}{s}v", "void", f"GLuint,const {cty}*")
            for n in (1, 2, 3, 4)
            for s, cty in [t for t in SCALARS if t[0] in ("f", "d", "s", "i")]
        ]
        + [
            ("glVertexAttrib4Nbv", "void", "GLuint,const GLbyte*"),
            ("glVertexAttrib4Nsv", "void", "GLuint,const GLshort*"),
            ("glVertexAttrib4Niv", "void", "GLuint,const GLint*"),
            ("glVertexAttrib4Nub", "void", "GLuint,GLubyte,GLubyte,GLubyte,GLubyte"),
            ("glVertexAttrib4Nubv", "void", "GLuint,const GLubyte*"),
            ("glVertexAttrib4Nusv", "void", "GLuint,const GLushort*"),
            ("glVertexAttrib4Nuiv", "void", "GLuint,const GLuint*"),
            ("glVertexAttrib4bv", "void", "GLuint,const GLbyte*"),
            ("glVertexAttrib4ubv", "void", "GLuint,const GLubyte*"),
            ("glVertexAttrib4usv", "void", "GLuint,const GLushort*"),
            ("glVertexAttrib4uiv", "void", "GLuint,const GLuint*"),
        ]
        + [
            (f"glVertexAttribI{n}i", "void", f"GLuint,{','.join(['GLint'] * n)}")
            for n in (1, 2, 3, 4)
        ]
        + [(f"glVertexAttribI{n}iv", "void", "GLuint,const GLint*") for n in (1, 2, 3, 4)]
        + [
            (f"glVertexAttribI{n}ui", "void", f"GLuint,{','.join(['GLuint'] * n)}")
            for n in (1, 2, 3, 4)
        ]
        + [(f"glVertexAttribI{n}uiv", "void", "GLuint,const GLuint*") for n in (1, 2, 3, 4)],
        "state",
        "2.0",
    )
    # Uniform scalars: f/i/ui for 1..4 (GL 2.0/3.0), d for 1..4 (GL 4.0).
    add(
        [
            (f"glUniform{n}{s}", "void", f"GLint,{','.join([cty] * n)}")
            for n in (1, 2, 3, 4)
            for s, cty in [t for t in SCALARS if t[0] in ("f", "i", "ui")]
        ]
        + [
            (f"glUniform{n}{s}v", "void", f"GLint,GLsizei,const {cty}*")
            for n in (1, 2, 3, 4)
            for s, cty in [t for t in SCALARS if t[0] in ("f", "i", "ui")]
        ],
        "state",
        "2.0",
    )
    add(
        [
            (f"glUniform{n}d", "void", f"GLint,{','.join(['GLdouble'] * n)}")
            for n in (1, 2, 3, 4)
        ]
        + [(f"glUniform{n}dv", "void", "GLint,GLsizei,const GLdouble*") for n in (1, 2, 3, 4)],
        "state",
        "4.0",
    )
    # Uniform matrices: square 2/3/4 in float and double, plus the six non-square float forms.
    add(
        [
            (f"glUniformMatrix{n}fv", "void", "GLint,GLsizei,GLboolean,const GLfloat*")
            for n in (2, 3, 4)
        ]
        + [
            (f"glUniformMatrix{n}dv", "void", "GLint,GLsizei,GLboolean,const GLdouble*")
            for n in (2, 3, 4)
        ]
        + [
            (f"glUniformMatrix{n}x{m}fv", "void", "GLint,GLsizei,GLboolean,const GLfloat*")
            for (n, m) in ((2, 3), (2, 4), (3, 2), (3, 4), (4, 2), (4, 3))
        ],
        "state",
        "2.0",
    )
    # glGetUniform* output variants.
    add(
        [
            (f"glGetUniform{s}v", "void", f"GLuint,GLint,{cty}*")
            for s, cty in [t for t in SCALARS if t[0] in ("f", "i", "ui")]
        ],
        "query",
        "2.0",
    )
    # Rect helpers.
    add(
        [
            ("glRectd", "void", "GLdouble,GLdouble,GLdouble,GLdouble"),
            ("glRectdv", "void", "const GLdouble*,const GLdouble*"),
            ("glRectf", "void", "GLfloat,GLfloat,GLfloat,GLfloat"),
            ("glRectfv", "void", "const GLfloat*,const GLfloat*"),
            ("glRecti", "void", "GLint,GLint,GLint,GLint"),
            ("glRectiv", "void", "const GLint*,const GLint*"),
            ("glRects", "void", "GLshort,GLshort,GLshort,GLshort"),
            ("glRectsv", "void", "const GLshort*,const GLshort*"),
        ],
        "draw",
        "1.0",
    )
    return rows


def main() -> int:
    write = "--write" in sys.argv
    check = "--check" in sys.argv
    existing_names: set[str] = set()
    lines: list[str] = []
    for raw in DEF.read_text().splitlines():
        s = raw.strip()
        if s and not s.startswith("#"):
            existing_names.add(s.split("\t")[0])
        lines.append(raw)

    generated = build()
    added: list[str] = []
    seen: set[str] = set()
    for name, ret, params, canvas, since in generated:
        if name in existing_names or name in seen:
            continue
        seen.add(name)
        if not name.isidentifier():
            print(f"skip malformed name {name!r}", file=sys.stderr)
            continue
        added.append(f"{name}\t{ret}\t{params}\t{canvas}\t{since}")

    print(f"curated entries: {len(existing_names)}")
    print(f"generated entries to add: {len(added)}")
    print(f"total after append: {len(existing_names) + len(added)}")

    if check:
        if added:
            print(
                f"\nFAIL: {len(added)} family members are missing from {DEF}. "
                "Run tools/gen_families.py --write and commit the result.",
                file=sys.stderr,
            )
            for line in added[:15]:
                print("  + " + line, file=sys.stderr)
            return 1
        print("\nOK: every generated family member is already present")
        return 0

    if not write:
        print("\nsample of what would be added:")
        for line in added[:15]:
            print("  " + line)
        print("\n(run with --write to append, or --check to fail if any are missing)")
        return 0

    lines.append("")
    lines.append("# --- generated legacy typed-variant families (tools/gen_families.py) ---")
    lines.extend(added)
    DEF.write_text("\n".join(lines) + "\n")
    print(f"\nappended {len(added)} entries to {DEF}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
