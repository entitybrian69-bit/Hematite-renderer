//! Stage 1b: the rewrite that makes a GLSL source acceptable to the compiler.
//!
//! naga's GLSL front end implements **Vulkan-GLSL** (440/450/460): `in`/`out`
//! instead of `attribute`/`varying`, explicit `location` and `binding` qualifiers,
//! no `gl_FragColor`, no fixed-function built-ins. Real shader sources — Minecraft's
//! own GLSL 1.20 shaders, OptiFine/Iris packs, GLES 1.00 code — are written against
//! the older language. This pass bridges the two, and it is the only place in
//! Hematite that edits shader text.
//!
//! The rewrite is deliberately conservative: it renames, re-qualifies and declares,
//! and it never reorders or restructures the shader. Anything it cannot express is
//! returned as [`Kind::Unsupported`] so the game sees a compile failure with a
//! reason instead of a shader that quietly draws something else.
//!
//! # What gets rewritten
//!
//! | Source construct | Normalised form |
//! | --- | --- |
//! | `#version 120` (or none, or `300 es`, `330 core`) | `#version 450` |
//! | `attribute T n;` (vertex) | `layout(location = L) in T n;` |
//! | `varying T n;` | `layout(location = L) out T n;` (vertex) / `in` (fragment) |
//! | `in`/`out` without a location | `layout(location = L)` added |
//! | `uniform T n;` | moved into one anonymous `layout(binding = B) uniform` block |
//! | `uniform sampler2D t;` | `layout(binding = B) uniform sampler2D t;` |
//! | `gl_FragColor` | `layout(location = 0) out vec4` |
//! | `gl_FragData[i]` | `layout(location = i) out vec4` (multiple render targets) |
//! | `gl_Vertex`, `gl_Normal`, `gl_Color`, `gl_MultiTexCoordN` | declared vertex inputs |
//! | `gl_ModelView*Matrix`, `gl_ProjectionMatrix`, `gl_NormalMatrix` | declared uniforms |
//! | `ftransform()` | `gl_ModelViewProjectionMatrix * gl_Vertex`, expanded |
//! | `texture2D`, `textureCube`, `shadow2D`, `*Lod`, `*Proj`, `*GradEXT` | `texture*` family |
//!
//! Assigning the locations and bindings here — instead of letting the compiler
//! invent them — is what lets the front end answer `glGetAttribLocation` and
//! `glGetUniformLocation` by name, and what makes `glBindAttribLocation` calls made
//! before `glLinkProgram` take effect.

use std::collections::BTreeSet;

use crate::interface::{
    split_array, std140_layout, Block, Interface, Member, Sampler, Variable, HEMATITE_UNIFORM_BLOCK,
};
use crate::{Error, Kind, Preprocessed, Stage};

/// The GLSL version every normalised source declares.
pub const TARGET_VERSION: u16 = 450;

/// The name given to the rewritten `gl_FragColor`.
pub const FRAGMENT_COLOUR_OUTPUT: &str = "hematite_out_color";

/// The fixed-function built-ins Hematite can emulate, and the type each has.
const LEGACY_VERTEX_INPUTS: &[(&str, &str)] = &[
    ("gl_Vertex", "vec4"),
    ("gl_Normal", "vec3"),
    ("gl_Color", "vec4"),
    ("gl_FogCoord", "float"),
    ("gl_MultiTexCoord0", "vec4"),
    ("gl_MultiTexCoord1", "vec4"),
    ("gl_MultiTexCoord2", "vec4"),
    ("gl_MultiTexCoord3", "vec4"),
    ("gl_MultiTexCoord4", "vec4"),
    ("gl_MultiTexCoord5", "vec4"),
    ("gl_MultiTexCoord6", "vec4"),
    ("gl_MultiTexCoord7", "vec4"),
];

/// Fixed-function matrices, as uniforms. `gl_NormalMatrix` is a mat3 per the spec.
const LEGACY_UNIFORMS: &[(&str, &str)] = &[
    ("gl_ModelViewMatrix", "mat4"),
    ("gl_ProjectionMatrix", "mat4"),
    ("gl_ModelViewProjectionMatrix", "mat4"),
    ("gl_NormalMatrix", "mat3"),
    ("gl_ModelViewMatrixInverse", "mat4"),
    ("gl_ProjectionMatrixInverse", "mat4"),
    ("gl_ModelViewMatrixTranspose", "mat4"),
    ("gl_ProjectionMatrixTranspose", "mat4"),
];

/// Texture functions with a direct GLSL 4.50 equivalent.
const FUNCTION_RENAMES: &[(&str, &str)] = &[
    ("texture2D", "texture"),
    ("texture2DProj", "textureProj"),
    ("texture2DLod", "textureLod"),
    ("texture2DProjLod", "textureProjLod"),
    ("texture2DGradEXT", "textureGrad"),
    ("texture2DProjGradEXT", "textureProjGrad"),
    ("textureCube", "texture"),
    ("textureCubeLod", "textureLod"),
    ("texture3D", "texture"),
    ("texture3DProj", "textureProj"),
    ("texture3DLod", "textureLod"),
    ("shadow1D", "texture"),
    ("shadow2D", "texture"),
    ("shadow1DProj", "textureProj"),
    ("shadow2DProj", "textureProj"),
    ("shadow1DLod", "textureLod"),
    ("shadow2DLod", "textureLod"),
];

/// A combined sampler the source declared, and the two Vulkan-GLSL objects it
/// becomes.
#[derive(Clone, Debug)]
struct Combined {
    /// The name the shader used.
    name: String,
    /// The constructor that recombines the pair (`sampler2D`, `sampler2DShadow`).
    constructor: String,
    image_name: String,
    sampler_name: String,
}

/// Texture sampling functions whose first argument is a combined sampler.
const TEXTURE_FUNCTIONS: &[&str] = &[
    "texture",
    "textureLod",
    "textureProj",
    "textureProjLod",
    "textureGrad",
    "textureProjGrad",
    "textureOffset",
    "textureProjOffset",
    "textureLodOffset",
    "textureGradOffset",
    "textureSize",
    "textureQueryLevels",
    "textureQueryLod",
    "textureGather",
    "textureGatherOffset",
    "texelFetch",
    "texelFetchOffset",
];

/// Qualifiers that may sit between the storage qualifier and the type.
const PREFIX_QUALIFIERS: &[&str] = &[
    "flat",
    "smooth",
    "noperspective",
    "centroid",
    "sample",
    "patch",
    "invariant",
    "lowp",
    "mediump",
    "highp",
    "coherent",
    "readonly",
    "writeonly",
    "restrict",
];

/// The name Hematite gives a rewritten fixed-function variable.
fn injected_name(original: &str) -> String {
    format!("hematite_{}", original.trim_start_matches("gl_").to_lowercase())
}

/// A normalised, compiler-ready source plus the interface it exposes.
#[derive(Clone, Debug)]
pub struct Normalised {
    /// The Vulkan-GLSL source handed to the compiler.
    pub source: String,
    /// For each line of `source`, the 0-based line of the original source.
    pub origins: Vec<u32>,
    pub interface: Interface,
    pub defines: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

impl Normalised {
    /// The original-source line (1-based) for a line of [`Self::source`].
    pub fn original_line(&self, source_line: u32) -> u32 {
        let index = source_line.saturating_sub(1) as usize;
        self.origins.get(index).copied().or_else(|| self.origins.last().copied()).unwrap_or(0) + 1
    }
}

/// The parsed `layout(...)` qualifiers of a declaration.
#[derive(Clone, Copy, Debug, Default)]
struct Layout {
    location: Option<u32>,
    binding: Option<u32>,
    set: Option<u32>,
}

/// Everything the rewrite accumulates while walking the source.
struct Builder<'a> {
    stage: Stage,
    interface: Interface,
    taken_inputs: BTreeSet<u32>,
    taken_outputs: BTreeSet<u32>,
    taken_bindings: BTreeSet<u32>,
    /// Locations the host reserved with `glBindAttribLocation` / `glBindFragDataLocation`.
    reserved_inputs: BTreeSet<u32>,
    reserved_outputs: BTreeSet<u32>,
    /// Locations requested for a named attribute, from the same GL calls.
    requested_inputs: Vec<(String, u32)>,
    requested_outputs: Vec<(String, u32)>,
    /// Plain `uniform` declarations, gathered into one anonymous block at the end.
    loose_uniforms: Vec<(String, String)>,
    combined_samplers: Vec<Combined>,
    legacy_inputs: Vec<(String, String, u32)>,
    legacy_uniforms: Vec<(String, String)>,
    render_targets: BTreeSet<u32>,
    out: Vec<(String, u32)>,
    warnings: Vec<String>,
    body: &'a [(String, u32)],
}

/// Run the rewrite.
pub fn run(stage: Stage, preprocessed: &Preprocessed) -> Result<Normalised, Error> {
    let body: Vec<(String, u32)> = preprocessed
        .body
        .split('\n')
        .zip(preprocessed.origins())
        .map(|(text, origin)| (text.to_string(), *origin))
        .collect();

    let identifiers = identifiers_of(&body);
    let mut builder = Builder {
        stage,
        interface: Interface::default(),
        taken_inputs: BTreeSet::new(),
        taken_outputs: BTreeSet::new(),
        taken_bindings: bindings_in_use(&body),
        reserved_inputs: BTreeSet::new(),
        reserved_outputs: BTreeSet::new(),
        requested_inputs: preprocessed
            .attribute_locations
            .iter()
            .map(|(name, location)| (name.to_string(), *location))
            .collect(),
        requested_outputs: preprocessed
            .output_locations
            .iter()
            .map(|(name, location)| (name.to_string(), *location))
            .collect(),
        loose_uniforms: Vec::new(),
        combined_samplers: Vec::new(),
        legacy_inputs: Vec::new(),
        legacy_uniforms: Vec::new(),
        render_targets: BTreeSet::new(),
        out: Vec::new(),
        warnings: Vec::new(),
        body: &body,
    };
    builder.reserve_requested_locations();
    builder.collect_combined_samplers();
    builder.collect_legacy(&identifiers);
    builder.rewrite_lines()?;
    builder.finish(preprocessed)
}

/// Every identifier in the source, with the line it first appeared on.
fn identifiers_of(body: &[(String, u32)]) -> Vec<(String, u32)> {
    let mut found: Vec<(String, u32)> = Vec::new();
    for (text, origin) in body {
        if text.trim_start().starts_with('#') {
            continue;
        }
        for token in tokens(text) {
            let identifier = token
                .starts_with(|character: char| character.is_alphabetic() || character == '_')
                && token.chars().all(|character| character.is_alphanumeric() || character == '_');
            if identifier && !found.iter().any(|(seen, _)| *seen == token) {
                found.push((token, *origin));
            }
        }
    }
    found
}

/// Every `binding = N` already written in the source, so injected bindings avoid them.
fn bindings_in_use(body: &[(String, u32)]) -> BTreeSet<u32> {
    let mut bindings = BTreeSet::new();
    for (text, _origin) in body {
        let tokens = tokens(text);
        for (index, token) in tokens.iter().enumerate() {
            if token != "binding" {
                continue;
            }
            if let Some(value) = tokens.get(index + 2).and_then(|token| token.parse().ok()) {
                bindings.insert(value);
            }
        }
    }
    bindings
}

/// Split a line into tokens, keeping punctuation separate.
///
/// `[n]` stays attached to the token before it (`float[4]` is one token) because an
/// array size belongs to its type. Every other non-alphanumeric character is its own
/// token, which is what makes `layout(binding = 3)` parseable without a grammar.
fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let characters: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if character.is_alphanumeric() || character == '_' || character == '.' {
            current.push(character);
        } else if character == '[' {
            match characters[index..].iter().position(|item| *item == ']') {
                Some(close) => {
                    current.extend(&characters[index..=index + close]);
                    index += close;
                }
                None => current.push(character),
            }
        } else if character.is_whitespace() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            out.push(character.to_string());
        }
        index += 1;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// True for texture, sampler and image types, which become descriptors rather than
/// bytes in a uniform buffer.
///
/// Vulkan-GLSL spells a sampled texture as a `texture2D` plus a `sampler`; the
/// combined `sampler2D` a game writes is split into that pair by [`rewrite_text`].
fn is_sampler(ty: &str) -> bool {
    let base = split_array(ty).map_or(ty, |(base, _elements)| base);
    base.starts_with("sampler")
        || base.starts_with("texture")
        || base.starts_with("image")
        || base.starts_with("isampler")
        || base.starts_with("usampler")
}

/// The Vulkan-GLSL texture type a combined sampler type corresponds to.
///
/// `sampler2DShadow` needs `texture2D` plus a *comparison* sampler, so the shadow
/// suffix is dropped here and carried by [`is_comparison_sampler`].
fn separate_texture_type(ty: &str) -> Option<String> {
    let base = split_array(ty).map_or(ty, |(base, _elements)| base);
    let rest = base.strip_prefix("sampler")?;
    let rest = rest.strip_suffix("Shadow").unwrap_or(rest);
    let (prefix, suffix) = match rest {
        "1D" | "2D" | "3D" | "Cube" | "1DArray" | "2DArray" | "CubeArray" | "2DMS"
        | "2DMSArray" => ("", rest),
        "1DShadow" | "CubeShadow" => ("", rest),
        _ => return None,
    };
    let dimension = suffix.strip_suffix("Shadow").unwrap_or(suffix);
    Some(format!("{prefix}texture{dimension}"))
}

fn is_comparison_sampler(ty: &str) -> bool {
    ty.contains("Shadow")
}

impl Builder<'_> {
    /// Record host-reserved locations so nothing else is assigned them.
    fn reserve_requested_locations(&mut self) {
        for (_name, location) in &self.requested_inputs {
            self.reserved_inputs.insert(*location);
        }
        for (_name, location) in &self.requested_outputs {
            self.reserved_outputs.insert(*location);
        }
    }

    /// Find every combined sampler the source declares.
    ///
    /// This has to happen before any line is rewritten, because a sampling call
    /// site can precede its declaration in the same file, and because the split
    /// names have to be stable for the call-site rewrite.
    fn collect_combined_samplers(&mut self) {
        for (text, _origin) in self.body {
            if text.trim_start().starts_with('#') {
                continue;
            }
            if let Some(Declaration::Uniform { ty, names, .. }) = Declaration::parse(text) {
                if !is_sampler(&ty) || separate_texture_type(&ty).is_none() {
                    continue;
                }
                for name in names {
                    if self.combined_samplers.iter().any(|seen| seen.name == name) {
                        continue;
                    }
                    let dimension = sampler_dimension(&ty);
                    self.combined_samplers.push(Combined {
                        constructor: ty.clone(),
                        name: name.clone(),
                        image_name: format!("hematite_{name}_image"),
                        sampler_name: format!("hematite_{name}_sampler"),
                    });
                    let _ = dimension;
                }
            }
        }
    }

    /// Record the legacy built-ins this source needs, so they can be declared.
    fn collect_legacy(&mut self, identifiers: &[(String, u32)]) {
        for (name, origin) in identifiers {
            if let Some((_, ty)) =
                LEGACY_VERTEX_INPUTS.iter().find(|(built_in, _)| *built_in == name.as_str())
            {
                if self.stage == Stage::Vertex {
                    let injected = injected_name(name);
                    if !self.legacy_inputs.iter().any(|(seen, _, _)| *seen == injected) {
                        let location = self.allocate_input(None, &injected);
                        self.legacy_inputs.push((injected, ty.to_string(), location));
                    }
                } else {
                    self.warnings.push(format!(
                        "line {}: `{name}` is a fixed-function vertex input and has no \
                         meaning in a fragment shader",
                        origin + 1
                    ));
                }
            }
            if let Some((_, ty)) =
                LEGACY_UNIFORMS.iter().find(|(built_in, _)| *built_in == name.as_str())
            {
                let injected = injected_name(name);
                if !self.legacy_uniforms.iter().any(|(_ty, seen)| *seen == injected) {
                    self.legacy_uniforms.push((ty.to_string(), injected));
                }
            }
            match name.as_str() {
                "gl_FragColor" if self.stage == Stage::Fragment => {
                    self.add_render_target(0);
                }
                "gl_TexCoord" if self.stage == Stage::Fragment => {
                    for index in 0..4 {
                        let injected = injected_name(&format!("gl_MultiTexCoord{index}"));
                        if !self.legacy_inputs.iter().any(|(seen, _, _)| *seen == injected) {
                            let location = self.allocate_input(None, &injected);
                            self.legacy_inputs.push((injected, "vec4".to_string(), location));
                        }
                    }
                }
                "ftransform" => {
                    self.need_legacy_uniform("gl_ModelViewProjectionMatrix", "mat4");
                    self.need_legacy_input("gl_Vertex", "vec4");
                }
                "gl_FrontColor"
                | "gl_BackColor"
                | "gl_FrontSecondaryColor"
                | "gl_BackSecondaryColor"
                | "gl_ClipVertex"
                | "gl_SecondaryColor" => {
                    self.warnings.push(format!(
                        "line {}: `{name}` is a fixed-function output Hematite does not \
                         implement",
                        origin + 1
                    ));
                }
                _ => {}
            }
        }
        // `gl_FragData[i]` needs one render target per index actually written. The
        // indexer is glued to the name by the tokeniser, so this scans the text.
        let mut highest: Option<u32> = None;
        for (text, _origin) in self.body {
            for target in indexed_uses(text, "gl_FragData") {
                highest = Some(highest.map_or(target, |seen| seen.max(target)));
            }
        }
        if let Some(highest) = highest {
            for index in 0..=highest.min(7) {
                self.add_render_target(index);
            }
        }
    }

    fn need_legacy_uniform(&mut self, original: &str, ty: &str) {
        let injected = injected_name(original);
        if !self.legacy_uniforms.iter().any(|(_ty, seen)| *seen == injected) {
            self.legacy_uniforms.push((ty.to_string(), injected));
        }
    }

    fn need_legacy_input(&mut self, original: &str, ty: &str) {
        let injected = injected_name(original);
        if self.stage != Stage::Vertex {
            return;
        }
        if !self.legacy_inputs.iter().any(|(seen, _, _)| *seen == injected) {
            let location = self.allocate_input(None, &injected);
            self.legacy_inputs.push((injected, ty.to_string(), location));
        }
    }

    fn add_render_target(&mut self, location: u32) {
        self.render_targets.insert(location);
        self.taken_outputs.insert(location);
    }

    /// Rewrite every line of the body.
    fn rewrite_lines(&mut self) -> Result<(), Error> {
        let mut index = 0;
        while index < self.body.len() {
            let (text, origin) = self.body[index].clone();
            let trimmed = text.trim_start();
            if trimmed.is_empty() {
                self.out.push((String::new(), origin));
                index += 1;
                continue;
            }
            if trimmed.starts_with('#') {
                let rewritten = self.rewrite_text(&text);
                self.out.push((rewritten, origin));
                index += 1;
                continue;
            }
            if let Some(Declaration::InterfaceBlock { layout }) = Declaration::parse(&text) {
                let end = self.interface_block_end(index);
                let block: Vec<String> =
                    (index..=end).map(|position| self.body[position].0.clone()).collect();
                let rewritten = self.block_declaration(&block, origin, layout)?;
                self.out.push((rewritten, origin));
                index = end + 1;
                continue;
            }
            match Declaration::parse(&text) {
                Some(declaration) => self.declaration(declaration, &text, origin)?,
                None => {
                    let rewritten = self.rewrite_text(&text);
                    self.out.push((rewritten, origin));
                }
            }
            index += 1;
        }
        Ok(())
    }

    /// The last line of the interface block that starts at `index`.
    fn interface_block_end(&self, index: usize) -> usize {
        let mut depth = 0i32;
        for position in index..self.body.len() {
            let text = &self.body[position].0;
            depth += text.matches('{').count() as i32 - text.matches('}').count() as i32;
            if depth <= 0 {
                return position;
            }
        }
        self.body.len().saturating_sub(1)
    }

    /// Handle one single-line declaration.
    fn declaration(
        &mut self,
        declaration: Declaration,
        text: &str,
        origin: u32,
    ) -> Result<(), Error> {
        match declaration {
            Declaration::Uniform { layout, ty, names } => {
                if is_sampler(&ty) {
                    let set = layout.set.unwrap_or(0);
                    let set_suffix =
                        if set == 0 { String::new() } else { format!(", set = {set}") };
                    for name in names {
                        let binding = match layout.binding {
                            Some(binding) => binding,
                            None => self.next_binding(),
                        };
                        self.taken_bindings.insert(binding);
                        // Copied out of the borrow so the fields below stay writable.
                        let split = self.combined_of(&name).map(|combined| {
                            (combined.image_name.clone(), combined.sampler_name.clone())
                        });
                        match split {
                            Some((image_name, sampler_name)) => {
                                // Split the combined sampler into Vulkan-GLSL's
                                // texture + sampler pair. They need distinct bindings:
                                // one binding number holds one descriptor.
                                let sampler_binding = self.next_binding_excluding(binding);
                                self.taken_bindings.insert(sampler_binding);
                                let image_type = separate_texture_type(&ty)
                                    .unwrap_or_else(|| "texture2D".to_string());
                                let sampler_type = if is_comparison_sampler(&ty) {
                                    "samplerShadow"
                                } else {
                                    "sampler"
                                };
                                self.interface.samplers.push(Sampler {
                                    name: name.clone(),
                                    ty: ty.clone(),
                                    dimension: sampler_dimension(&ty),
                                    binding,
                                    sampler_binding,
                                    set,
                                    comparison: is_comparison_sampler(&ty),
                                    array_elements: split_array(&ty)
                                        .map_or(1, |(_base, elements)| elements),
                                    image_name: image_name.clone(),
                                    sampler_name: sampler_name.clone(),
                                });
                                self.out.push((
                                    format!(
                                        "layout(binding = {binding}{set_suffix}) uniform \
                                         {image_type} {};",
                                        image_name
                                    ),
                                    origin,
                                ));
                                self.out.push((
                                    format!(
                                        "layout(binding = {sampler_binding}{set_suffix}) uniform \
                                         {sampler_type} {};",
                                        sampler_name
                                    ),
                                    origin,
                                ));
                            }
                            None => {
                                // Already written in Vulkan-GLSL form.
                                self.interface.samplers.push(Sampler {
                                    name: name.clone(),
                                    ty: ty.clone(),
                                    dimension: sampler_dimension(&ty),
                                    binding,
                                    sampler_binding: binding,
                                    set,
                                    comparison: is_comparison_sampler(&ty),
                                    array_elements: split_array(&ty)
                                        .map_or(1, |(_base, elements)| elements),
                                    image_name: name.clone(),
                                    sampler_name: String::new(),
                                });
                                self.out.push((
                                    format!(
                                        "layout(binding = {binding}{set_suffix}) uniform {ty} {name};"
                                    ),
                                    origin,
                                ));
                            }
                        }
                    }
                    return Ok(());
                }
                // Plain uniforms move into Hematite's anonymous block. GLSL scopes an
                // anonymous block exactly like loose declarations, so the body does
                // not change.
                for name in names {
                    if !self.loose_uniforms.iter().any(|(_ty, seen)| *seen == name) {
                        self.loose_uniforms.push((ty.clone(), name));
                    }
                }
                Ok(())
            }
            Declaration::Input { layout, ty, names } => {
                for name in names {
                    let location = self.allocate_input(layout.location, &name);
                    self.interface.attributes.push(Variable {
                        name: name.clone(),
                        ty: ty.clone(),
                        location,
                    });
                    self.out
                        .push((format!("layout(location = {location}) in {ty} {name};"), origin));
                }
                Ok(())
            }
            Declaration::Varying { layout, ty, names } => {
                // `varying` is a fragment input and a vertex output.
                for name in names {
                    if self.stage == Stage::Fragment {
                        let location = self.allocate_input(layout.location, &name);
                        self.interface.varyings.push(Variable {
                            name: name.clone(),
                            ty: ty.clone(),
                            location,
                        });
                        self.out.push((
                            format!("layout(location = {location}) in {ty} {name};"),
                            origin,
                        ));
                    } else {
                        let location = self.allocate_output(layout.location, &name);
                        self.interface.varyings.push(Variable {
                            name: name.clone(),
                            ty: ty.clone(),
                            location,
                        });
                        self.out.push((
                            format!("layout(location = {location}) out {ty} {name};"),
                            origin,
                        ));
                    }
                }
                Ok(())
            }
            Declaration::Output { layout, ty, names } => {
                // `out` is a vertex output and a fragment render target.
                for name in names {
                    let location = self.allocate_output(layout.location, &name);
                    if self.stage == Stage::Fragment {
                        self.interface.outputs.push(Variable {
                            name: name.clone(),
                            ty: ty.clone(),
                            location,
                        });
                    } else {
                        self.interface.varyings.push(Variable {
                            name: name.clone(),
                            ty: ty.clone(),
                            location,
                        });
                    }
                    self.out
                        .push((format!("layout(location = {location}) out {ty} {name};"), origin));
                }
                Ok(())
            }
            Declaration::InterfaceBlock { .. } => {
                // Handled by `rewrite_lines`, which can see the whole block.
                unreachable!("interface blocks never reach the single-line path")
            }
            Declaration::Other => {
                let rewritten = self.rewrite_text(text);
                self.out.push((rewritten, origin));
                Ok(())
            }
        }
    }

    /// Re-emit a user-declared interface block, adding a binding if it lacks one.
    fn block_declaration(
        &mut self,
        lines: &[String],
        origin: u32,
        layout: Layout,
    ) -> Result<String, Error> {
        let joined = lines.join("\n");
        if !joined.contains('}') {
            return Err(Error {
                kind: Kind::Unsupported,
                message: "unterminated interface block".to_string(),
                line: Some(origin + 1),
            });
        }
        let tokens = tokens(&joined);
        let qualifier = tokens
            .iter()
            .find(|token| matches!(token.as_str(), "uniform" | "buffer" | "in" | "out"))
            .cloned()
            .unwrap_or_else(|| "uniform".to_string());
        let open = tokens.iter().position(|token| token == "{").unwrap_or(tokens.len());
        let block_name = if open >= 2 && tokens[open - 1] != qualifier {
            tokens[open - 1].clone()
        } else {
            "HematiteBlock".to_string()
        };

        let mut members: Vec<String> = Vec::new();
        let mut inside = false;
        for token in &tokens[open.min(tokens.len())..] {
            match token.as_str() {
                "{" => inside = true,
                "}" => inside = false,
                ";" => {}
                _ if inside => members.push(token.clone()),
                _ => {}
            }
        }
        let mut fields: Vec<(String, String)> = Vec::new();
        let mut iterator = members.iter();
        while let Some(ty) = iterator.next() {
            if let Some(name) = iterator.next() {
                fields.push((ty.clone(), name.clone()));
            }
        }
        let (offsets, size) = std140_layout(&fields).map_err(|message| Error {
            kind: Kind::Unsupported,
            message: format!("interface block `{block_name}`: {message}"),
            line: Some(origin + 1),
        })?;
        let binding = match layout.binding {
            Some(binding) => binding,
            None => self.next_binding(),
        };
        self.taken_bindings.insert(binding);
        let set = layout.set.unwrap_or(0);
        self.interface.blocks.push(Block {
            name: block_name.clone(),
            binding,
            set,
            members: fields
                .iter()
                .zip(offsets)
                .map(|((ty, name), offset)| Member { name: name.clone(), ty: ty.clone(), offset })
                .collect(),
            size,
        });

        // Everything after the closing brace is the instance name and the `;`.
        let close = joined.rfind('}').unwrap_or(joined.len());
        let tail = joined[close..].trim_end().to_string();
        let indent = &lines[0][..lines[0].len() - lines[0].trim_start().len()];
        let mut out = format!(
            "{indent}layout(binding = {binding}, set = {set}) {qualifier} {block_name} {{\n"
        );
        for (ty, name) in &fields {
            out.push_str(&format!("    {ty} {name};\n"));
        }
        out.push_str(&format!("{indent}{tail}"));
        Ok(out)
    }

    /// The final source: version, extensions, injected declarations, body.
    fn finish(mut self, preprocessed: &Preprocessed) -> Result<Normalised, Error> {
        let mut head: Vec<(String, u32)> = vec![(format!("#version {TARGET_VERSION}"), 0)];
        for extension in &preprocessed.extensions {
            head.push((format!("#extension {extension}"), 0));
        }
        for (name, ty, location) in &self.legacy_inputs {
            head.push((format!("layout(location = {location}) in {ty} {name};"), 0));
        }
        if self.stage == Stage::Fragment {
            for location in self.render_targets.iter().copied().collect::<Vec<_>>() {
                let name = match location {
                    0 => FRAGMENT_COLOUR_OUTPUT.to_string(),
                    other => format!("{FRAGMENT_COLOUR_OUTPUT}_{other}"),
                };
                self.interface.outputs.push(Variable {
                    name: name.clone(),
                    ty: "vec4".to_string(),
                    location,
                });
                head.push((format!("layout(location = {location}) out vec4 {name};"), 0));
            }
        }
        head.extend(self.injected_uniform_block()?);
        if !head.last().is_none_or(|(text, _)| text.is_empty()) {
            head.push((String::new(), 0));
        }

        let origins: Vec<u32> = head
            .iter()
            .map(|(_text, origin)| *origin)
            .chain(self.out.iter().map(|item| item.1))
            .collect();
        let source = head
            .iter()
            .map(|(text, _origin)| text.as_str())
            .chain(self.out.iter().map(|item| item.0.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(Normalised {
            source,
            origins,
            interface: self.interface,
            defines: preprocessed.defines.clone(),
            warnings: self.warnings,
        })
    }

    /// One anonymous block holding every plain uniform, legacy matrices included.
    fn injected_uniform_block(&mut self) -> Result<Vec<(String, u32)>, Error> {
        let mut fields: Vec<(String, String)> = Vec::new();
        for (ty, name) in self.legacy_uniforms.iter().chain(self.loose_uniforms.iter()) {
            if !fields.iter().any(|(_ty, seen)| seen == name) {
                fields.push((ty.clone(), name.clone()));
            }
        }
        if fields.is_empty() {
            return Ok(Vec::new());
        }
        let (offsets, size) = std140_layout(&fields).map_err(|message| Error {
            kind: Kind::Unsupported,
            message: format!("uniform declarations: {message}"),
            line: None,
        })?;
        let binding = self.next_binding();
        self.taken_bindings.insert(binding);
        self.interface.blocks.push(Block {
            name: HEMATITE_UNIFORM_BLOCK.to_string(),
            binding,
            set: 0,
            members: fields
                .iter()
                .zip(offsets)
                .map(|((ty, name), offset)| Member { name: name.clone(), ty: ty.clone(), offset })
                .collect(),
            size,
        });
        let mut out =
            vec![(format!("layout(binding = {binding}) uniform HematiteUniformBlock {{"), 0)];
        for (ty, name) in &fields {
            out.push((format!("    {ty} {name};"), 0));
        }
        out.push(("};".to_string(), 0));
        Ok(out)
    }

    /// The location for an input, honouring a written or host-requested one.
    fn allocate_input(&mut self, written: Option<u32>, name: &str) -> u32 {
        if let Some(existing) = self.interface.attributes.iter().find(|item| item.name == name) {
            return existing.location;
        }
        let location = written
            .or_else(|| {
                self.requested_inputs
                    .iter()
                    .find(|(requested, _)| requested == name)
                    .map(|(_name, location)| *location)
            })
            .unwrap_or_else(|| self.free_location(&self.taken_inputs, &self.reserved_inputs));
        self.taken_inputs.insert(location);
        location
    }

    /// The location for an output or a fragment render target.
    fn allocate_output(&mut self, written: Option<u32>, name: &str) -> u32 {
        let existing = self
            .interface
            .varyings
            .iter()
            .chain(self.interface.outputs.iter())
            .find(|item| item.name == name)
            .map(|item| item.location);
        if let Some(existing) = existing {
            return existing;
        }
        let location = written
            .or_else(|| {
                self.requested_outputs
                    .iter()
                    .find(|(requested, _)| requested == name)
                    .map(|(_name, location)| *location)
            })
            .unwrap_or_else(|| self.free_location(&self.taken_outputs, &self.reserved_outputs));
        self.taken_outputs.insert(location);
        location
    }

    /// The lowest location that is neither taken nor reserved by the host.
    fn free_location(&self, taken: &BTreeSet<u32>, reserved: &BTreeSet<u32>) -> u32 {
        let mut location = 0;
        while taken.contains(&location) || reserved.contains(&location) {
            location += 1;
        }
        location
    }

    fn next_binding(&self) -> u32 {
        let mut binding = 0;
        while self.taken_bindings.contains(&binding) {
            binding += 1;
        }
        binding
    }

    /// The split halves of a combined sampler, if this source declared it.
    fn combined_of(&self, name: &str) -> Option<&Combined> {
        self.combined_samplers.iter().find(|combined| combined.name == name)
    }

    /// The lowest binding that is free and is not `reserved`.
    fn next_binding_excluding(&self, reserved: u32) -> u32 {
        let mut binding = 0;
        while binding == reserved || self.taken_bindings.contains(&binding) {
            binding += 1;
        }
        binding
    }

    /// Rename legacy identifiers, split combined samplers and adapt their results.
    ///
    /// One pass over the line, so that a call site and the function name it came
    /// from are still visible together: legacy `shadow2D` returns a `vec4` whose
    /// `.r` holds the comparison result, while its Vulkan-GLSL spelling returns a
    /// `float`, so the rewrite has to know which one the source meant.
    fn rewrite_text(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 32);
        let mut last = 0usize;
        let mut index = 0usize;
        while index < text.len() {
            let byte = text.as_bytes()[index];
            if !(byte.is_ascii_alphabetic() || byte == b'_') {
                index += 1;
                continue;
            }
            let end = word_end(text, index);
            let word = &text[index..end];
            let rest = &text[end..];
            match self.rewrite_word(word, rest) {
                Some(rewrite) => {
                    out.push_str(&text[last..index]);
                    out.push_str(&rewrite.text);
                    last = index + rewrite.consumed;
                    index = last;
                }
                None => index = end,
            }
        }
        out.push_str(&text[last..]);
        out
    }

    /// The replacement for one identifier, and how much of the line it replaces.
    fn rewrite_word(&self, word: &str, rest: &str) -> Option<Rewrite> {
        let call = rest.trim_start().starts_with('(');
        if call {
            if let Some(rewrite) = self.rewrite_call(word, rest) {
                return Some(rewrite);
            }
        }
        match word {
            "ftransform" => {
                let matrix = injected_name("gl_ModelViewProjectionMatrix");
                let vertex = injected_name("gl_Vertex");
                return Some(Rewrite::word(format!("({matrix} * {vertex})"), word.len(), rest));
            }
            "gl_FragColor" if self.stage == Stage::Fragment => {
                return Some(Rewrite::word(FRAGMENT_COLOUR_OUTPUT.to_string(), word.len(), rest));
            }
            "gl_FragData" | "gl_TexCoord" => {
                let index = indexed_at(rest)?;
                let consumed = word.len() + index.syntax.len();
                let name = match word {
                    "gl_FragData" => format!("{FRAGMENT_COLOUR_OUTPUT}_{}", index.value),
                    _ => injected_name(&format!("gl_MultiTexCoord{}", index.value)),
                };
                return Some(Rewrite { text: name, consumed });
            }
            _ => {}
        }
        LEGACY_VERTEX_INPUTS
            .iter()
            .chain(LEGACY_UNIFORMS.iter())
            .find(|(built_in, _)| *built_in == word)
            .map(|(built_in, _)| Rewrite::word(injected_name(built_in), word.len(), rest))
    }

    /// Rewrite a texture sampling call, splitting a combined sampler argument.
    fn rewrite_call(&self, word: &str, rest: &str) -> Option<Rewrite> {
        let renamed = FUNCTION_RENAMES
            .iter()
            .find(|(from, _)| *from == word)
            .map(|(_from, to)| *to)
            .or_else(|| TEXTURE_FUNCTIONS.contains(&word).then_some(word))?;
        let open = rest.find('(')?;
        let argument_end = first_argument_end(rest, open + 1)?;
        let argument = rest[open + 1..argument_end].trim();
        let combined = self.combined_of(argument)?;
        // Legacy `shadow*` returned a vec4 with the result in `.r`; the Vulkan-GLSL
        // comparison sample returns the float directly, so keep the old shape.
        let wraps = word.starts_with("shadow");
        let close = matching_paren(rest, open)?;
        let tail = &rest[argument_end..=close];
        Some(Rewrite {
            text: format!(
                "{}{renamed}({constructor}({}, {}){tail}{close_wrap}",
                if wraps { "vec4(" } else { "" },
                combined.image_name,
                combined.sampler_name,
                constructor = combined.constructor,
                close_wrap = if wraps { ")" } else { "" },
            ),
            consumed: word.len() + close + 1,
        })
    }
}

/// A piece of rewritten text and how many bytes of the source it replaces.
struct Rewrite {
    text: String,
    consumed: usize,
}

impl Rewrite {
    fn word(text: String, length: usize, _rest: &str) -> Self {
        Self { text, consumed: length }
    }
}

/// The byte index just past the identifier starting at `start`.
fn word_end(text: &str, start: usize) -> usize {
    let mut end = start;
    for (offset, character) in text[start..].char_indices() {
        if !(character.is_alphanumeric() || character == '_') {
            end = start + offset;
            return end;
        }
        end = start + offset + character.len_utf8();
    }
    end
}

/// A `[n]` indexer and the text it occupies.
struct Indexer {
    value: u32,
    syntax: String,
}

/// Read the `[n]` that follows a name, if the next thing is one.
fn indexed_at(rest: &str) -> Option<Indexer> {
    let trimmed = rest.trim_start();
    let opened = trimmed.strip_prefix('[')?;
    let digits: String =
        opened.chars().take_while(|character| character.is_ascii_digit()).collect();
    let value: u32 = digits.parse().ok()?;
    let closed = opened[digits.len()..].strip_prefix(']')?;
    let _ = closed;
    let consumed = rest.len() - rest.trim_start().len() + 1 + digits.len() + 1;
    Some(Indexer { value, syntax: rest[..consumed].to_string() })
}

/// The index of the `)` matching the `(` at `open`.
fn matching_paren(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, character) in text[open..].char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// One global declaration the rewrite recognises.
enum Declaration {
    Uniform { layout: Layout, ty: String, names: Vec<String> },
    Input { layout: Layout, ty: String, names: Vec<String> },
    Output { layout: Layout, ty: String, names: Vec<String> },
    Varying { layout: Layout, ty: String, names: Vec<String> },
    InterfaceBlock { layout: Layout },
    Other,
}

impl Declaration {
    /// Classify a single line. `Declaration::Other` means "copy it through".
    fn parse(text: &str) -> Option<Declaration> {
        let tokens = tokens(text);
        if tokens.is_empty() {
            return None;
        }
        let mut index = 0;
        let mut layout = Layout::default();
        if tokens[0] == "layout" {
            let (parsed, next) = parse_layout(&tokens);
            layout = parsed;
            index = next;
        }
        while tokens.get(index).is_some_and(|token| PREFIX_QUALIFIERS.contains(&token.as_str())) {
            index += 1;
        }
        if tokens.get(index).map(String::as_str) == Some("precision") {
            return Some(Declaration::Other);
        }
        let qualifier = match tokens.get(index).map(String::as_str) {
            Some(qualifier) => qualifier,
            None => return Some(Declaration::Other),
        };
        let varying = qualifier == "varying";
        if !matches!(qualifier, "uniform" | "in" | "out" | "attribute" | "varying" | "buffer") {
            return Some(Declaration::Other);
        }
        index += 1;
        if tokens.iter().skip(index).any(|token| token == "{") {
            return Some(Declaration::InterfaceBlock { layout });
        }
        let ty = match tokens.get(index) {
            Some(ty) => ty.clone(),
            None => return Some(Declaration::Other),
        };
        index += 1;
        if ty == "struct" || ty == "{" || ty == ";" {
            return Some(Declaration::Other);
        }
        let mut names = Vec::new();
        while index < tokens.len() {
            let token = tokens[index].clone();
            match token.as_str() {
                ";" | "," => {}
                "=" => break,
                _ if token.starts_with(|character: char| {
                    character.is_alphabetic() || character == '_'
                }) =>
                {
                    names.push(
                        split_array(&token)
                            .map_or(token.clone(), |(base, _elements)| base.to_string()),
                    );
                }
                _ => {}
            }
            index += 1;
        }
        if names.is_empty() {
            return Some(Declaration::Other);
        }
        Some(match qualifier {
            "uniform" => Declaration::Uniform { layout, ty, names },
            "in" | "attribute" => Declaration::Input { layout, ty, names },
            "out" if varying => Declaration::Varying { layout, ty, names },
            "out" => Declaration::Output { layout, ty, names },
            _ => Declaration::Varying { layout, ty, names },
        })
    }
}

/// Every `[n]` index used with `name` in one line of source.
fn indexed_uses(text: &str, name: &str) -> Vec<u32> {
    let mut found = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find(name) {
        let start = cursor + offset;
        let end = start + name.len();
        let word_boundary_before = start == 0
            || !text[..start]
                .chars()
                .next_back()
                .is_some_and(|character| character.is_alphanumeric() || character == '_');
        let rest = &text[end..];
        let word_boundary_after = rest
            .chars()
            .next()
            .is_none_or(|character| !(character.is_alphanumeric() || character == '_'));
        if word_boundary_before && word_boundary_after {
            let digits: String = rest
                .trim_start()
                .trim_start_matches('[')
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect();
            if let Ok(index) = digits.parse::<u32>() {
                found.push(index);
            }
        }
        cursor = end;
    }
    found
}

/// The dimension part of a sampler or texture type: `2D`, `Cube`, `2DArray`, `3D`.
fn sampler_dimension(ty: &str) -> String {
    let base = split_array(ty).map_or(ty, |(base, _elements)| base);
    for prefix in ["sampler", "texture", "image"] {
        if let Some(rest) = base.strip_prefix(prefix) {
            let rest = rest.strip_suffix("Shadow").unwrap_or(rest);
            return rest.trim_start_matches(['i', 'u']).to_string();
        }
    }
    base.to_string()
}

/// The index of the first top-level `,` after `start`, or of the matching `)`.
fn first_argument_end(text: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, character) in text[start..].char_indices() {
        match character {
            '(' | '[' => depth += 1,
            ')' | ']' => {
                if depth == 0 {
                    return Some(start + index);
                }
                depth -= 1;
            }
            ',' if depth == 0 => return Some(start + index),
            _ => {}
        }
    }
    None
}

/// Read `layout(location = 2, binding = 3)` from the front of a token list.
fn parse_layout(tokens: &[String]) -> (Layout, usize) {
    let mut layout = Layout::default();
    let mut index = 1; // past `layout`
    if tokens.get(index).map(String::as_str) != Some("(") {
        return (layout, index);
    }
    index += 1;
    while index < tokens.len() && tokens[index] != ")" {
        let key = tokens[index].clone();
        if tokens.get(index + 1).map(String::as_str) == Some("=") {
            if let Some(value) = tokens.get(index + 2).and_then(|token| token.parse().ok()) {
                match key.as_str() {
                    "location" => layout.location = Some(value),
                    "binding" => layout.binding = Some(value),
                    "set" => layout.set = Some(value),
                    _ => {}
                }
            }
            index += 3;
            continue;
        }
        index += 1;
    }
    (layout, index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{preprocess, Profile, Request};

    fn normalise(stage: Stage, source: &str) -> Normalised {
        let preprocessed = preprocess::run(&Request::new(stage, source)).expect("preprocess");
        run(stage, &preprocessed).expect("normalise")
    }

    #[test]
    fn version_is_forced_to_the_target() {
        let normalised = normalise(Stage::Vertex, "#version 120\nvoid main() {}\n");
        assert!(normalised.source.starts_with(&format!("#version {TARGET_VERSION}")));
        assert!(!normalised.source.contains("#version 120"));
    }

    #[test]
    fn attributes_and_varyings_get_locations() {
        let normalised = normalise(
            Stage::Vertex,
            "#version 120\nattribute vec4 position;\nattribute vec2 uv;\nvarying vec2 texcoord;\n",
        );
        assert!(normalised.source.contains("layout(location = 0) in vec4 position;"));
        assert!(normalised.source.contains("layout(location = 1) in vec2 uv;"));
        assert!(normalised.source.contains("layout(location = 0) out vec2 texcoord;"));
        assert_eq!(normalised.interface.attributes.len(), 2);
        assert_eq!(normalised.interface.varyings.len(), 1);
    }

    #[test]
    fn varying_becomes_in_for_a_fragment_shader() {
        let normalised = normalise(Stage::Fragment, "#version 120\nvarying vec2 texcoord;\n");
        assert!(normalised.source.contains("layout(location = 0) in vec2 texcoord;"));
        assert_eq!(normalised.interface.varyings.len(), 1);
        assert!(normalised.interface.outputs.is_empty());
    }

    #[test]
    fn fragment_out_is_a_render_target() {
        let normalised =
            normalise(Stage::Fragment, "#version 330 core\nout vec4 colour;\nvoid main() {}\n");
        assert!(normalised.source.contains("layout(location = 0) out vec4 colour;"));
        assert_eq!(normalised.interface.outputs.len(), 1);
    }

    #[test]
    fn plain_uniforms_move_into_one_block_with_std140_offsets() {
        let normalised = normalise(
            Stage::Vertex,
            "#version 120\nuniform vec3 offset;\nuniform float scale;\nuniform mat4 mvp;\n",
        );
        assert!(normalised.source.contains("layout(binding = 0) uniform HematiteUniformBlock {"));
        let block = normalised
            .interface
            .blocks
            .iter()
            .find(|block| block.name == HEMATITE_UNIFORM_BLOCK)
            .expect("block");
        let offsets: Vec<u32> = block.members.iter().map(|member| member.offset).collect();
        assert_eq!(offsets, vec![0, 12, 16], "vec3 then float pack, mat4 follows");
        assert_eq!(block.size, 80);
        assert!(!normalised.source.contains("uniform vec3 offset;"));
    }

    #[test]
    fn samplers_get_bindings_and_stay_out_of_the_block() {
        let normalised = normalise(
            Stage::Fragment,
            "#version 120\nuniform sampler2D diffuse;\nuniform sampler2DShadow shadow;\n\
             uniform vec4 tint;\n",
        );
        // A combined sampler becomes a texture and a sampler object, each with its
        // own binding, because one Vulkan binding holds one descriptor.
        assert!(normalised
            .source
            .contains("layout(binding = 0) uniform texture2D hematite_diffuse_image;"));
        assert!(
            normalised
                .source
                .contains("layout(binding = 1) uniform sampler hematite_diffuse_sampler;"),
            "{}",
            normalised.source
        );
        assert!(
            normalised
                .source
                .contains("layout(binding = 3) uniform samplerShadow hematite_shadow_sampler;"),
            "{}",
            normalised.source
        );
        let shadow = normalised.interface.sampler_of("shadow").expect("shadow");
        assert!(shadow.comparison);
        assert!(!normalised.interface.uniforms().iter().any(|member| member.name == "diffuse"));
        assert!(normalised.interface.uniforms().iter().any(|member| member.name == "tint"));
    }

    #[test]
    fn written_bindings_are_not_reused() {
        let normalised = normalise(
            Stage::Fragment,
            "#version 450 core\nlayout(binding = 7) uniform sampler2D tex;\nuniform vec4 c;\n",
        );
        assert!(normalised
            .source
            .contains("layout(binding = 7) uniform texture2D hematite_tex_image;"));
        assert!(normalised
            .source
            .contains("layout(binding = 0) uniform sampler hematite_tex_sampler;"));
        let block = normalised
            .interface
            .blocks
            .iter()
            .find(|block| block.name == HEMATITE_UNIFORM_BLOCK)
            .expect("block");
        assert_eq!(block.binding, 1, "7 and 0 are taken, so the block takes 1");
    }

    #[test]
    fn frag_color_and_frag_data_become_render_targets() {
        let normalised = normalise(
            Stage::Fragment,
            "#version 120\nvoid main() { gl_FragColor = vec4(1.0); gl_FragData[2] = vec4(0.0); }\n",
        );
        assert!(
            normalised
                .source
                .contains(&format!("layout(location = 0) out vec4 {FRAGMENT_COLOUR_OUTPUT};")),
            "{}",
            normalised.source
        );
        assert!(normalised
            .source
            .contains(&format!("layout(location = 2) out vec4 {FRAGMENT_COLOUR_OUTPUT}_2;")));
        assert!(normalised.source.contains(&format!("{FRAGMENT_COLOUR_OUTPUT} = vec4(1.0);")));
        assert!(normalised.source.contains(&format!("{FRAGMENT_COLOUR_OUTPUT}_2 = vec4(0.0);")));
        let locations: Vec<u32> =
            normalised.interface.outputs.iter().map(|output| output.location).collect();
        assert_eq!(locations, vec![0, 1, 2], "target 1 is declared even though unused");
    }

    #[test]
    fn fixed_function_builtins_are_declared() {
        let normalised =
            normalise(Stage::Vertex, "#version 120\nvoid main() { gl_Position = ftransform(); }\n");
        assert!(normalised.source.contains("in vec4 hematite_vertex;"), "{}", normalised.source);
        assert!(normalised.source.contains("mat4 hematite_modelviewprojectionmatrix;"));
        assert!(normalised
            .source
            .contains("(hematite_modelviewprojectionmatrix * hematite_vertex)"));
        assert!(normalised.source.contains("gl_Position ="));
    }

    #[test]
    fn texture_functions_are_renamed_only_at_call_sites() {
        let normalised = normalise(
            Stage::Fragment,
            "#version 120\nuniform sampler2D t;\nuniform sampler2DShadow s;\nvoid main() { \
             vec4 a = texture2D(t, vec2(0.0)); vec4 b = shadow2D(s, vec3(0.0)); \
             gl_FragColor = a + b; }\n",
        );
        assert!(
            normalised
                .source
                .contains("texture(sampler2D(hematite_t_image, hematite_t_sampler), vec2(0.0))"),
            "{}",
            normalised.source
        );
        // Legacy shadow2D returned a vec4 with the result in `.r`; the Vulkan-GLSL
        // comparison sample returns a float, so the rewrite restores the vec4.
        assert!(
            normalised.source.contains(
                "vec4(texture(sampler2DShadow(hematite_s_image, hematite_s_sampler), vec3(0.0)))"
            ),
            "{}",
            normalised.source
        );
        assert!(!normalised.source.contains("texture2D("));
        assert!(!normalised.source.contains("shadow2D("));
    }

    #[test]
    fn a_modern_comparison_sample_is_not_wrapped() {
        let normalised = normalise(
            Stage::Fragment,
            "#version 450 core\nuniform sampler2DShadow s;\nvoid main() { \
             float depth = texture(s, vec3(0.0)); gl_FragColor = vec4(depth); }\n",
        );
        assert!(
            normalised.source.contains(
                "texture(sampler2DShadow(hematite_s_image, hematite_s_sampler), vec3(0.0))"
            ),
            "{}",
            normalised.source
        );
        assert!(!normalised.source.contains("vec4(texture("), "{}", normalised.source);
    }

    #[test]
    fn explicit_locations_are_preserved() {
        let normalised = normalise(
            Stage::Vertex,
            "#version 330 core\nlayout(location = 4) in vec3 position;\n\
             layout(location = 9) out vec2 texcoord;\n",
        );
        assert!(normalised.source.contains("layout(location = 4) in vec3 position;"));
        assert!(normalised.source.contains("layout(location = 9) out vec2 texcoord;"));
        assert_eq!(normalised.interface.location_of("position"), Some(4));
        assert_eq!(normalised.interface.location_of("texcoord"), Some(9));
    }

    #[test]
    fn user_uniform_blocks_keep_their_members() {
        let normalised = normalise(
            Stage::Vertex,
            "#version 330 core\nlayout(binding = 2) uniform Material {\n    vec4 colour;\n    \
             float shininess;\n} material;\nvoid main() { gl_Position = material.colour; }\n",
        );
        let block = normalised.interface.blocks.iter().find(|block| block.name == "Material");
        let block = block.expect("block recorded");
        assert_eq!(block.binding, 2);
        assert_eq!(block.members.len(), 2);
        assert_eq!(block.members[1].offset, 16);
        assert!(normalised.source.contains("layout(binding = 2, set = 0) uniform Material {"));
    }

    #[test]
    fn preprocessor_lines_are_left_alone() {
        let normalised = normalise(
            Stage::Vertex,
            "#version 120\n#define WHITE vec4(1.0)\n#ifdef FOG\nuniform float fog;\n#endif\n\
             void main() {}\n",
        );
        assert!(normalised.source.contains("#define WHITE vec4(1.0)"));
        assert!(normalised.source.contains("#ifdef FOG"));
        assert!(normalised.source.contains("#endif"));
        assert!(normalised.interface.uniforms().iter().any(|member| member.name == "fog"));
    }

    #[test]
    fn origins_track_the_original_source() {
        let preprocessed =
            preprocess::run(&Request::new(Stage::Vertex, "#version 120\n\n\nattribute vec4 p;\n"))
                .expect("preprocess");
        let normalised = run(Stage::Vertex, &preprocessed).expect("normalise");
        let line = normalised
            .source
            .lines()
            .enumerate()
            .find(|(_index, line)| line.contains("in vec4 p"))
            .map(|(index, _line)| index)
            .expect("declaration present");
        assert_eq!(normalised.origins[line], 3, "the attribute was on source line 4");
        assert_eq!(normalised.original_line(line as u32 + 1), 4);
    }

    #[test]
    fn profile_information_survives_preprocessing() {
        let preprocessed =
            preprocess::run(&Request::new(Stage::Fragment, "#version 300 es\n")).expect("ok");
        assert_eq!(preprocessed.profile, Profile::Es);
        assert_eq!(preprocessed.version, 300);
    }

    #[test]
    fn a_uniform_block_without_a_closing_brace_is_refused() {
        let source = "#version 330 core\nuniform Broken {\n    vec4 a;\nvoid main() {}\n";
        let preprocessed = preprocess::run(&Request::new(Stage::Vertex, source)).expect("ok");
        let error = run(Stage::Vertex, &preprocessed).expect_err("must fail");
        assert_eq!(error.kind, Kind::Unsupported);
    }

    #[test]
    fn a_sampler_type_is_recognised_in_every_spelling() {
        assert!(is_sampler("sampler2D"));
        assert!(is_sampler("sampler2DShadow"));
        assert!(is_sampler("sampler2DArray[4]"));
        assert!(is_sampler("image2D"));
        assert!(is_sampler("texture2D"));
        assert_eq!(separate_texture_type("sampler2D").as_deref(), Some("texture2D"));
        assert_eq!(separate_texture_type("sampler2DShadow").as_deref(), Some("texture2D"));
        assert_eq!(separate_texture_type("samplerCube").as_deref(), Some("textureCube"));
        assert_eq!(separate_texture_type("sampler2DArray").as_deref(), Some("texture2DArray"));
        assert_eq!(separate_texture_type("vec4"), None);
        assert!(!is_sampler("vec4"));
        assert!(!is_sampler("mat4"));
    }

    #[test]
    fn tokens_keep_array_sizes_with_their_type() {
        assert_eq!(tokens("uniform float w[4];"), vec!["uniform", "float", "w[4]", ";"]);
        assert_eq!(
            tokens("layout(binding = 3) uniform sampler2D t;"),
            vec!["layout", "(", "binding", "=", "3", ")", "uniform", "sampler2D", "t", ";"]
        );
    }
}
