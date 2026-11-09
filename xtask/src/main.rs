//! Build-time tasks for Hematite.
//!
//! Run as `cargo run -p xtask -- <task>`.
//!
//! Tasks:
//!   `gen-abi`     parse `abi/gl_export.def`, emit the Rust signature table and the
//!                 expected-symbol manifest used by CI to diff `nm -D` output.
//!   `check-abi`   re-parse the table and report internal inconsistencies (duplicate
//!                 names, unknown canvas classes, unknown C types) without writing.
//!
//! The generator is deliberately dependency-free: the ABI table is the thing being
//! audited, so it must be readable and checkable in an environment where no crates can
//! be downloaded.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Canvas class: what the frontend does with a call *before* a backend sees it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Canvas {
    Target,
    State,
    Resource,
    Draw,
    Query,
    Sync,
    Unsupported,
}

impl Canvas {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "target" => Canvas::Target,
            "state" => Canvas::State,
            "resource" => Canvas::Resource,
            "draw" => Canvas::Draw,
            "query" => Canvas::Query,
            "sync" => Canvas::Sync,
            "unsupported" => Canvas::Unsupported,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Canvas::Target => "Target",
            Canvas::State => "State",
            Canvas::Resource => "Resource",
            Canvas::Draw => "Draw",
            Canvas::Query => "Query",
            Canvas::Sync => "Sync",
            Canvas::Unsupported => "Unsupported",
        }
    }
}

/// A parsed entry from `abi/gl_export.def`.
struct Entry {
    name: String,
    ret: String,
    params: Vec<String>,
    canvas: Canvas,
    since: String,
    line: usize,
}

/// Maps a C scalar or opaque-handle spelling to its Rust type.
///
/// This is the table that makes the brief's "check every signature against the real
/// prototype" demand tractable: every width decision lives here, once.
fn rust_type(c_ty: &str) -> Option<&'static str> {
    Some(match c_ty {
        "void" => "core::ffi::c_void",
        "GLenum" => "u32",
        "GLboolean" => "u8",
        "GLbitfield" => "u32",
        "GLbyte" => "i8",
        "GLubyte" => "u8",
        "GLshort" => "i16",
        "GLushort" => "u16",
        "GLint" => "i32",
        "GLuint" => "u32",
        "GLint64" => "i64",
        "GLuint64" => "u64",
        "GLsizei" => "i32",
        "GLfloat" => "f32",
        "GLclampf" => "f32",
        "GLdouble" => "f64",
        "GLclampd" => "f64",
        // Pointer-sized by definition on the platform. `isize` on 64-bit, `i32` on ILP32,
        // which is what the C typedefs do; using `isize` is correct for both.
        "GLsizeiptr" => "isize",
        "GLintptr" => "isize",
        "GLsync" => "*mut core::ffi::c_void",
        "GLDEBUGPROC" => {
            "Option<unsafe extern \"C\" fn(u32,u32,u32,u32,i32,*const i8,*mut core::ffi::c_void)>"
        }
        // EGL
        "EGLDisplay" => "*mut core::ffi::c_void",
        "EGLConfig" => "*mut core::ffi::c_void",
        "EGLSurface" => "*mut core::ffi::c_void",
        "EGLContext" => "*mut core::ffi::c_void",
        "EGLImageKHR" => "*mut core::ffi::c_void",
        "EGLSyncKHR" => "*mut core::ffi::c_void",
        "EGLSync" => "*mut core::ffi::c_void",
        "EGLNativeDisplayType" => "*mut core::ffi::c_void",
        "EGLNativeWindowType" => "*mut core::ffi::c_void",
        "EGLNativePixmapType" => "*mut core::ffi::c_void",
        "EGLClientBuffer" => "*mut core::ffi::c_void",
        "EGLint" => "i32",
        "EGLBoolean" => "u32",
        "EGLenum" => "u32",
        "EGLAttrib" => "isize",
        "EGLTimeKHR" => "u64",
        "EGLTime" => "u64",
        "EGLnsecsKHR" => "u64",
        "char" => "i8",
        _ => return None,
    })
}

/// Split `a,b,c` respecting no nesting (the def format forbids nested commas in types).
fn split_params(s: &str) -> Vec<String> {
    if s.trim().is_empty() || s.trim() == "void" {
        return Vec::new();
    }
    s.split(',').map(|p| p.trim().to_string()).collect()
}

/// A pointer parameter keeps its `*`/`const` decoration; the base type must be known.
///
/// Handles the declarator shapes that actually appear in the Khronos headers, including
/// the array-of-pointers convention (`const char* const*` for `glShaderSource`'s string
/// list) where the inner pointer is itself const-qualified.
fn param_rust_type(c_ty: &str) -> Result<String, String> {
    let parts: Vec<&str> = c_ty.split('*').collect();
    if parts.len() > 3 {
        return Err(format!("more than two pointer levels: {c_ty}"));
    }
    let words: Vec<&str> = parts[0].split_whitespace().collect();
    let base: Vec<&str> = words.iter().copied().filter(|word| *word != "const").collect();
    if base.len() != 1 {
        return Err(format!("invalid base type: {c_ty}"));
    }
    let mut ty = rust_type(base[0]).ok_or_else(|| format!("unknown type {}", base[0]))?.to_string();
    let mut is_const = words.contains(&"const");
    if parts.len() == 1 && is_const {
        return Err(format!("const without pointer: {c_ty}"));
    }
    for (index, qualifier) in parts.iter().enumerate().skip(1) {
        ty = format!("*{} {ty}", if is_const { "const" } else { "mut" });
        is_const = match qualifier.trim() {
            "" => false,
            "const" if index + 1 < parts.len() => true,
            _ => return Err(format!("invalid pointer qualifier: {c_ty}")),
        };
    }
    Ok(ty)
}

fn parse_def(path: &Path) -> Result<Vec<Entry>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
        if cols.len() != 5 {
            return Err(format!(
                "{}:{}: expected 5 tab-separated columns, found {}",
                path.display(),
                idx + 1,
                cols.len()
            ));
        }
        let canvas = Canvas::parse(cols[3]).ok_or_else(|| {
            format!("{}:{}: unknown canvas class `{}`", path.display(), idx + 1, cols[3])
        })?;
        out.push(Entry {
            name: cols[0].to_string(),
            ret: cols[1].to_string(),
            params: split_params(cols[2]),
            canvas,
            since: cols[4].to_string(),
            line: idx + 1,
        });
    }
    Ok(out)
}

/// Cross-checks the table. Returns a list of human-readable problems.
fn validate(entries: &[Entry]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();

    for e in entries {
        if let Some(prev) = seen.insert(&e.name, e.line) {
            problems.push(format!("duplicate export `{}` (lines {prev} and {})", e.name, e.line));
        }
        if !e.name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            || !e.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            problems
                .push(format!("line {}: export name `{}` is not a C identifier", e.line, e.name));
        }
        if param_rust_type(&e.ret).is_err() && e.ret.trim() != "void" {
            if let Err(why) = param_rust_type(&e.ret) {
                problems.push(format!("line {}: return type of `{}`: {why}", e.line, e.name));
            }
        }
        for p in &e.params {
            if let Err(why) = param_rust_type(p) {
                problems.push(format!("line {}: parameter of `{}`: {why}", e.line, e.name));
            }
        }
        // EGL entry points own the display/surface; GL ones must at least be classified.
        if e.name.starts_with("egl") && e.canvas == Canvas::Draw {
            problems.push(format!("line {}: EGL entry `{}` classified as draw", e.line, e.name));
        }
    }
    problems
}

fn emit_rust(entries: &[Entry]) -> String {
    let mut s = String::new();
    s.push_str("// @generated by `cargo run -p xtask -- gen-abi` from abi/gl_export.def.\n");
    s.push_str(
        "// Do not edit by hand. The def file is the source of truth; header audit is pending.\n",
    );
    s.push_str("#![allow(non_snake_case, clippy::too_many_arguments)]\n\n");

    s.push_str("/// What the frontend does with a call before any backend sees it.\n");
    s.push_str("#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n");
    s.push_str("pub enum Canvas {\n");
    for c in [
        Canvas::Target,
        Canvas::State,
        Canvas::Resource,
        Canvas::Draw,
        Canvas::Query,
        Canvas::Sync,
        Canvas::Unsupported,
    ] {
        let _ = writeln!(s, "    {},", c.as_str());
    }
    s.push_str("}\n\n");

    s.push_str("/// Static description of one exported entry point.\n");
    s.push_str("pub struct Entry {\n");
    s.push_str("    pub name: &'static str,\n");
    s.push_str("    pub canvas: Canvas,\n");
    s.push_str("    /// The GL/EGL version that introduced it, or `\"ext\"`.\n");
    s.push_str("    pub since: &'static str,\n");
    s.push_str("}\n\n");

    s.push_str(
        "/// Planned entry points, in def-file order; this table does not implement exports.\n",
    );
    s.push_str(&format!("pub const ENTRIES: [Entry; {}] = [\n", entries.len()));
    for e in entries {
        let _ = writeln!(
            s,
            "    Entry {{ name: {:?}, canvas: Canvas::{}, since: {:?} }},",
            e.name,
            e.canvas.as_str(),
            e.since
        );
    }
    s.push_str("];\n\n");

    s.push_str("/// Every exported symbol, for the CI check against `nm -D`.\n");
    s.push_str(&format!("pub const SYMBOLS: [&str; {}] = [\n", entries.len()));
    for e in entries {
        let _ = writeln!(s, "    {:?},", e.name);
    }
    s.push_str("];\n");

    s
}

/// Emits `extern "C"` declarations for the entry points, so a future implementation module
/// can `#[no_mangle]` a fn per name and have the compiler prove the signature matches.
fn emit_decls(entries: &[Entry]) -> Result<String, String> {
    let mut s = String::new();
    s.push_str("// @generated by `cargo run -p xtask -- gen-abi`.\n");
    s.push_str("#![allow(non_snake_case, clippy::too_many_arguments)]\n\n");
    s.push_str("extern \"C\" {\n");
    for e in entries {
        let ret = if e.ret.trim() == "void" { "()".to_string() } else { param_rust_type(&e.ret)? };
        let mut params = String::new();
        for (i, p) in e.params.iter().enumerate() {
            if i > 0 {
                params.push_str(", ");
            }
            let _ = write!(params, "a{i}: {}", param_rust_type(p)?);
        }
        let _ = writeln!(s, "    pub fn {}({}) -> {};", e.name, params, ret);
    }
    s.push_str("}\n");
    Ok(s)
}

fn repo_root() -> PathBuf {
    // xtask runs from the workspace root under `cargo run`, but be robust to being run
    // from inside the crate directory.
    let mut p = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if !p.join("abi").is_dir() {
        p.push("..");
    }
    p
}

fn main() {
    let mut args = std::env::args().skip(1);
    let task = args.next().unwrap_or_else(|| "check-abi".to_string());
    let root = repo_root();
    let def = root.join("abi").join("gl_export.def");

    let entries = match parse_def(&def) {
        Ok(e) => e,
        Err(why) => {
            eprintln!("error: {why}");
            std::process::exit(1);
        }
    };

    let problems = validate(&entries);
    if !problems.is_empty() {
        for p in &problems {
            eprintln!("abi: {p}");
        }
        eprintln!("abi: {} problem(s) in {}", problems.len(), def.display());
        std::process::exit(1);
    }

    match task.as_str() {
        "check-abi" => {
            println!("abi ok: {} exported entry points", entries.len());
            let mut by_canvas: BTreeMap<&str, usize> = BTreeMap::new();
            for e in &entries {
                *by_canvas.entry(e.canvas.as_str()).or_default() += 1;
            }
            for (k, v) in by_canvas {
                println!("  {k:<12} {v}");
            }
        }
        "gen-abi" => {
            let gen_dir = root.join("crates").join("hematite-gl-frontend").join("src").join("gen");
            if let Err(why) = std::fs::create_dir_all(&gen_dir) {
                eprintln!("error: cannot create {}: {why}", gen_dir.display());
                std::process::exit(1);
            }
            let abi_rs = emit_rust(&entries);
            let abi_path = gen_dir.join("abi.rs");
            if let Err(why) = std::fs::write(&abi_path, &abi_rs) {
                eprintln!("error: cannot write {}: {why}", abi_path.display());
                std::process::exit(1);
            }
            let decls = match emit_decls(&entries) {
                Ok(d) => d,
                Err(why) => {
                    eprintln!("error: {why}");
                    std::process::exit(1);
                }
            };
            let decls_path = gen_dir.join("decls.rs");
            if let Err(why) = std::fs::write(&decls_path, &decls) {
                eprintln!("error: cannot write {}: {why}", decls_path.display());
                std::process::exit(1);
            }
            // The symbol manifest CI diffs `nm -D` against.
            let manifest: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
            let manifest_path = root.join("abi").join("expected_symbols.txt");
            if let Err(why) = std::fs::write(&manifest_path, manifest.join("\n") + "\n") {
                eprintln!("error: cannot write {}: {why}", manifest_path.display());
                std::process::exit(1);
            }
            println!(
                "generated {} entries -> {}, {}, {}",
                entries.len(),
                abi_path.display(),
                decls_path.display(),
                manifest_path.display()
            );
        }
        other => {
            eprintln!("unknown task `{other}`; expected `gen-abi` or `check-abi`");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.to_string(),
            ret: "void".to_string(),
            params: vec!["const char* const*".to_string()],
            canvas: Canvas::State,
            since: "2.0".to_string(),
            line: 1,
        }
    }

    #[test]
    fn pointer_qualifiers_are_preserved() {
        for (c, rust) in [
            ("const char* const*", "*const *const i8"),
            ("const char**", "*mut *const i8"),
            ("char* const*", "*const *mut i8"),
            ("char**", "*mut *mut i8"),
            ("char const*", "*const i8"),
            ("void*", "*mut core::ffi::c_void"),
            ("GLintptr", "isize"),
        ] {
            assert_eq!(param_rust_type(c).unwrap(), rust);
        }
    }

    #[test]
    fn invalid_types_rejected() {
        for ty in
            ["Mystery", "const GLint", "char***", "char* volatile*", "char*const", "constchar*", ""]
        {
            assert!(param_rust_type(ty).is_err(), "{ty}");
        }
    }

    #[test]
    fn duplicate_names_rejected() {
        let problems = validate(&[entry("glExample"), entry("glExample")]);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("duplicate export"));
    }

    #[test]
    fn c_identifiers_require_valid_first_character() {
        for name in ["", "0bad", "gl-bad", "glé"] {
            assert!(!validate(&[entry(name)]).is_empty());
        }
        for name in ["glValid", "_valid", "gl1"] {
            assert!(validate(&[entry(name)]).is_empty());
        }
    }

    #[test]
    fn declarations_are_not_panic_stubs() {
        let s = emit_decls(&[entry("glExample")]).unwrap();
        assert!(s.contains("extern \"C\" {"));
        assert!(s.contains("pub fn glExample(a0: *const *const i8) -> ();"));
        assert!(!s.contains("unimplemented!"));
        assert!(!s.contains("dispatch"));
    }

    #[test]
    fn unknown_canvas_and_type_rejected() {
        assert!(Canvas::parse("typo").is_none());
        let mut bad = entry("eglBad");
        bad.canvas = Canvas::Draw;
        bad.ret = "UnknownType".to_string();
        assert_eq!(validate(&[bad]).len(), 2);
    }

    #[test]
    fn generated_table_escapes_metadata() {
        let mut e = entry("glExample");
        e.since = "a\"b\\c".to_string();
        assert!(emit_rust(&[e]).contains("a\\\"b\\\\c"));
    }
}
