//! Stage 1: comment masking, `#include` expansion and `#version` detection.
//!
//! This pass is deliberately small and mechanical. It does not understand GLSL
//! beyond the preprocessor directives it has to act on, and it never changes a
//! token that the compiler will see — except for removing comments and splicing in
//! included files. Everything that rewrites shader text lives in [`crate::normalise`],
//! which can then assume a comment-free, include-free source.
//!
//! Every line of the output remembers which line of the original source it came
//! from, so a diagnostic from the compiler can be mapped back to the file the game
//! handed to `glShaderSource` instead of pointing into Hematite's own rewriting.

use crate::{Error, Kind, Profile, Request};

/// How deep `#include` may nest before Hematite refuses. Shader packs chain a
/// handful of levels; this is a guard against a cyclic include graph.
pub const MAX_INCLUDE_DEPTH: usize = 32;

/// The version a driver assumes when a shader declares none.
pub const DEFAULT_VERSION: u16 = 110;

/// A comment-free, include-expanded source with per-line provenance.
#[derive(Clone, Debug)]
pub struct Preprocessed {
    /// The source the normaliser works on. The `#version` line is removed here so
    /// there is exactly one place that decides the version the compiler sees.
    pub body: String,
    /// The version the source declared, or [`DEFAULT_VERSION`].
    pub version: u16,
    pub profile: Profile,
    /// `#extension` lines, in order, without the leading `#extension`.
    pub extensions: Vec<String>,
    /// Definitions to hand to the compiler as if the source had written them.
    pub defines: Vec<(String, String)>,
    /// Locations the host reserved through `glBindAttribLocation`.
    pub attribute_locations: Vec<(String, u32)>,
    /// Locations the host reserved for fragment outputs.
    pub output_locations: Vec<(String, u32)>,
    /// For each line of `body`, the 0-based line of the original source.
    origins: Vec<u32>,
}

impl Preprocessed {
    /// The original-source line for a line of [`Self::body`].
    ///
    /// `body_line` is 1-based, the way compilers count; lines Hematite injected
    /// map back to the line that caused them, and out-of-range input maps to the
    /// last line rather than panicking — this runs on a path where a panic would
    /// take the host process down.
    pub fn original_line(&self, body_line: u32) -> u32 {
        let index = body_line.saturating_sub(1) as usize;
        self.origins.get(index).copied().or_else(|| self.origins.last().copied()).unwrap_or(0)
    }

    pub fn line_count(&self) -> usize {
        self.origins.len()
    }

    /// The origins, one per line of [`Self::body`].
    pub fn origins(&self) -> &[u32] {
        &self.origins
    }
}

/// Run stage 1 over a compile request.
pub fn run(request: &Request<'_>) -> Result<Preprocessed, Error> {
    let masked = mask_comments(request.source);
    let mut stack: Vec<&str> = Vec::new();
    let mut lines = expand(masked, 0, request, &mut stack)?;

    let mut version = DEFAULT_VERSION;
    let mut profile = Profile::None;
    let mut extensions = Vec::new();
    let mut seen_directive = false;
    let mut version_index = None;

    for (index, (text, _origin)) in lines.iter().enumerate() {
        let trimmed = text.trim_start();
        if !trimmed.starts_with('#') {
            if !trimmed.is_empty() {
                seen_directive = true;
            }
            continue;
        }
        let directive = directive_of(trimmed);
        match directive {
            "version" => {
                if seen_directive || version_index.is_some() {
                    return Err(Error {
                        kind: Kind::Preprocess,
                        message: "`#version` must appear before any other statement, ".to_string()
                            + "exactly once",
                        line: Some(_origin + 1),
                    });
                }
                let (parsed, parsed_profile) = parse_version(trimmed).ok_or_else(|| Error {
                    kind: Kind::Preprocess,
                    message: format!("unreadable `#version` directive: {trimmed}"),
                    line: Some(_origin + 1),
                })?;
                version = parsed;
                profile = parsed_profile;
                version_index = Some(index);
            }
            "extension" => {
                seen_directive = true;
                extensions.push(trimmed.trim_start_matches("#extension").trim().to_string());
            }
            "line" | "pragma" => seen_directive = true,
            _ => seen_directive = true,
        }
    }
    if let Some(index) = version_index {
        lines.remove(index);
    }

    let origins: Vec<u32> = lines.iter().map(|(_text, origin)| *origin).collect();
    let body = lines.iter().map(|(text, _origin)| text.as_str()).collect::<Vec<_>>().join("\n");
    Ok(Preprocessed {
        body,
        version,
        profile,
        extensions,
        defines: request
            .defines
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
        attribute_locations: request
            .attribute_locations
            .iter()
            .map(|(name, location)| (name.to_string(), *location))
            .collect(),
        output_locations: request
            .output_locations
            .iter()
            .map(|(name, location)| (name.to_string(), *location))
            .collect(),
        origins,
    })
}

/// The directive name of a `#`-prefixed line, or "" if it is malformed.
fn directive_of(trimmed: &str) -> &str {
    trimmed
        .trim_start_matches('#')
        .split(|character: char| character.is_whitespace() || character == '(')
        .next()
        .unwrap_or("")
}

/// Parse `#version 450 core` / `#version 300 es` / `#version 120`.
fn parse_version(trimmed: &str) -> Option<(u16, Profile)> {
    let mut parts = trimmed
        .trim_start_matches("#version")
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return None;
    }
    let version: u16 = parts.remove(0).parse().ok()?;
    let profile = match parts.first().map(String::as_str) {
        Some("core") => Profile::Core,
        Some("compatibility") => Profile::Compatibility,
        Some("es") => Profile::Es,
        Some(other) => return Some((version, profile_for(other))),
        None => Profile::None,
    };
    Some((version, profile))
}

/// An unrecognised profile word is kept as "none" but reported by the caller.
fn profile_for(_other: &str) -> Profile {
    Profile::None
}

/// Replace comments with spaces, keeping line structure and token boundaries.
///
/// Comments are removed here rather than left for the compiler because the
/// normaliser rewrites identifiers, and an identifier inside a comment must not be
/// rewritten. Preprocessor string literals (`#include "x"`) are preserved.
pub fn mask_comments(source: &str) -> Vec<(String, u32)> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Code,
        Line,
        Block,
        Quote(char),
    }
    let mut state = State::Code;
    let mut line = String::new();
    let mut lines: Vec<(String, u32)> = Vec::new();
    let mut origin = 0u32;
    let characters = source.chars().peekable().collect::<Vec<char>>();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        let next = characters.get(index + 1).copied();
        match state {
            State::Code => match character {
                '/' if next == Some('/') => {
                    state = State::Line;
                    line.push(' ');
                    index += 2;
                    line.push(' ');
                    continue;
                }
                '/' if next == Some('*') => {
                    state = State::Block;
                    line.push_str("  ");
                    index += 2;
                    continue;
                }
                '"' | '\'' if line.trim_start().starts_with('#') => {
                    state = State::Quote(character);
                    line.push(character);
                }
                '\n' => {
                    lines.push((std::mem::take(&mut line), origin));
                    origin += 1;
                }
                _ => line.push(character),
            },
            State::Line => {
                if character == '\n' {
                    state = State::Code;
                    lines.push((std::mem::take(&mut line), origin));
                    origin += 1;
                } else {
                    line.push(' ');
                }
            }
            State::Block => {
                if character == '*' && next == Some('/') {
                    state = State::Code;
                    line.push_str("  ");
                    index += 2;
                    continue;
                }
                if character == '\n' {
                    lines.push((std::mem::take(&mut line), origin));
                    origin += 1;
                } else {
                    line.push(' ');
                }
            }
            State::Quote(opening) => {
                line.push(character);
                if character == '\\' {
                    if let Some(escaped) = next {
                        line.push(escaped);
                        index += 2;
                        continue;
                    }
                } else if character == opening {
                    state = State::Code;
                }
            }
        }
        index += 1;
    }
    lines.push((line, origin));
    lines
}

/// Splice `#include` targets in, depth-first.
fn expand<'a>(
    lines: Vec<(String, u32)>,
    depth: usize,
    request: &'a Request<'a>,
    stack: &mut Vec<&'a str>,
) -> Result<Vec<(String, u32)>, Error> {
    if depth > MAX_INCLUDE_DEPTH {
        return Err(Error::new(
            Kind::Preprocess,
            format!("#include nesting deeper than {MAX_INCLUDE_DEPTH} levels"),
        ));
    }
    let mut expanded = Vec::with_capacity(lines.len());
    for (text, origin) in lines {
        let target = match include_target(text.trim_start()) {
            Some(target) => target,
            None => {
                expanded.push((text, origin));
                continue;
            }
        };
        let found = request.includes.iter().find(|(name, _source)| *name == target);
        let (name, source) = match found {
            Some(found) => *found,
            None => {
                return Err(Error {
                    kind: Kind::Preprocess,
                    message: format!(
                        "cannot resolve `#include \"{target}\"`; the host supplied {} \
                         include(s) and none matched",
                        request.includes.len()
                    ),
                    line: Some(origin + 1),
                });
            }
        };
        if stack.contains(&name) {
            return Err(Error {
                kind: Kind::Preprocess,
                message: format!("`#include \"{name}\"` is already being expanded (cycle)"),
                line: Some(origin + 1),
            });
        }
        stack.push(name);
        let nested = expand(mask_comments(source), depth + 1, request, stack)?;
        stack.pop();
        // Every spliced line points back at the `#include` that pulled it in: the
        // included file has no identity in a GL error message, and pointing at the
        // directive is the closest honest answer.
        expanded.extend(nested.into_iter().map(|(text, _origin)| (text, origin)));
    }
    Ok(expanded)
}

/// The file named by a `#include` line, or `None` if the line is not an include.
fn include_target(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix("#include")?.trim_start();
    let (opening, closing) = match rest.chars().next()? {
        '"' => ('"', '"'),
        '<' => ('<', '>'),
        _ => return None,
    };
    let inner = rest.strip_prefix(opening)?;
    let end = inner.find(closing)?;
    Some(&inner[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stage;

    fn preprocess(source: &str, includes: &[(&str, &str)]) -> Preprocessed {
        run(&Request { stage: Stage::Vertex, source, includes, ..Request::default() })
            .expect("preprocess")
    }

    #[test]
    fn comments_become_spaces_but_lines_survive() {
        let masked = mask_comments("int a; // trailing\n/* multi\nline */ int b;\n");
        let texts: Vec<&str> = masked.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(texts.len(), 4, "{texts:?}");
        assert!(!texts[0].contains("trailing"), "{}", texts[0]);
        assert!(!texts.join("\n").contains("multi"));
        assert!(texts[2].contains("int b;"), "{}", texts[2]);
    }

    #[test]
    fn include_string_literals_survive_masking() {
        let masked = mask_comments("#include \"keep me.glsl\" // gone\n");
        assert!(masked[0].0.contains("keep me.glsl"), "{}", masked[0].0);
        assert!(!masked[0].0.contains("gone"));
    }

    #[test]
    fn version_and_profile_are_read() {
        let preprocessed = preprocess("#version 300 es\nvoid main() {}\n", &[]);
        assert_eq!(preprocessed.version, 300);
        assert_eq!(preprocessed.profile, Profile::Es);
        assert!(!preprocessed.body.contains("#version"), "{}", preprocessed.body);

        let preprocessed = preprocess("#version 450 core\n", &[]);
        assert_eq!(preprocessed.version, 450);
        assert_eq!(preprocessed.profile, Profile::Core);

        let preprocessed = preprocess("void main() {}\n", &[]);
        assert_eq!(preprocessed.version, DEFAULT_VERSION);
        assert_eq!(preprocessed.profile, Profile::None);
    }

    #[test]
    fn a_late_version_directive_is_rejected() {
        let error =
            run(&Request::new(Stage::Vertex, "int a;\n#version 450\n")).expect_err("must fail");
        assert_eq!(error.kind, Kind::Preprocess);
        assert_eq!(error.line, Some(2));
    }

    #[test]
    fn includes_are_spliced_and_tracked() {
        let common = "#define WHITE vec4(1.0)\n#include \"deeper.glsl\"\n";
        let preprocessed = preprocess(
            "#version 120\n#include \"common.glsl\"\nvoid main() { gl_Position = WHITE; }\n",
            &[("common.glsl", common), ("deeper.glsl", "#define DEEP 1\n")],
        );
        assert!(preprocessed.body.contains("#define WHITE"), "{}", preprocessed.body);
        assert!(preprocessed.body.contains("#define DEEP"), "{}", preprocessed.body);
        assert!(!preprocessed.body.contains("#include"));
        // Line 1 of the body is the included `#define WHITE`, which came from the
        // `#include` line (line 2 of the source, 1-based).
        assert_eq!(preprocessed.original_line(1), 1);
        // Three body lines, minus the removed `#version`, plus the three lines the
        // include expanded to (its own two plus a trailing empty line).
        assert_eq!(preprocessed.line_count(), 6);
        assert_eq!(preprocessed.original_line(5), 2, "the `main` line keeps its origin");
    }

    #[test]
    fn an_unresolvable_include_reports_the_line() {
        let error =
            run(&Request::new(Stage::Fragment, "\n\n#include <missing.glsl>\n")).expect_err("fail");
        assert_eq!(error.line, Some(3));
        assert!(error.message.contains("missing.glsl"));
    }

    #[test]
    fn a_cyclic_include_graph_is_refused() {
        let error = run(&Request {
            stage: Stage::Fragment,
            source: "#include \"a.glsl\"\n",
            includes: &[("a.glsl", "#include \"b.glsl\"\n"), ("b.glsl", "#include \"a.glsl\"\n")],
            ..Request::default()
        })
        .expect_err("cycle must fail");
        assert!(error.message.contains("cycle"), "{error}");
    }

    #[test]
    fn extensions_are_collected() {
        let preprocessed = preprocess(
            "#version 330\n#extension GL_ARB_explicit_attrib_location : require\nint a;\n",
            &[],
        );
        assert_eq!(preprocessed.extensions, vec!["GL_ARB_explicit_attrib_location : require"]);
    }
}
