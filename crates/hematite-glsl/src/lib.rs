//! GLSL -> SPIR-V translation for Hematite's GL front-end.
//!
//! `glCompileShader` has to turn the GLSL a game hands us into something a Vulkan
//! backend can execute. Hematite does that in three stages, and the split matters
//! because it is the middle stage that is *not* Hematite's own code:
//!
//! 1. [`preprocess`] — a small, auditable pass over the source: comment masking,
//!    `#include` expansion, `#version` detection and the legacy-to-Vulkan-GLSL
//!    rewrite that makes GLSL 1.20 / 3.30 / ES 1.00 sources acceptable to the
//!    compiler in stage 2. It also assigns the `location` and `binding` qualifiers
//!    that Vulkan-GLSL requires and that desktop GL leaves implicit, which is what
//!    lets the front-end answer `glGetAttribLocation` and `glGetUniformLocation`.
//! 2. [`naga`] 0.14 (vendored, see `docs/VENDORED.md`) parses and type-checks the
//!    normalised source and builds its IR. This is the real compiler: Hematite does
//!    not implement a GLSL grammar of its own.
//! 3. naga's SPIR-V back end emits the words, after naga's own validator has run
//!    with Vulkan capabilities.
//!
//! What this means in practice: a shader that naga accepts compiles, and the
//! resulting SPIR-V is the same shape a desktop driver would have produced from the
//! normalised source. What naga's GLSL front end does not accept — geometry and
//! tessellation stages, `#version 430`-only constructs that have no 450 equivalent,
//! non-Vulkan extensions — is reported as [`Kind::Unsupported`] rather than
//! silently ignored, so the game sees a failed compile with a reason.
//!
//! # Honesty about coverage
//!
//! The legacy rewrite in stage 1 covers the constructs Hematite's tests exercise:
//! the fixed-function built-ins a GLSL 1.20 shader-pack uses, `texture2D`-family
//! samplers, multiple render targets through `gl_FragData`, `#include` and macro
//! guards. It is a compatibility shim over a subset, not a full GLSL 1.20
//! implementation; see `docs/KNOWN_LIMITATIONS.md`.

pub mod interface;
pub mod normalise;
pub mod preprocess;
pub mod spirv;

pub use interface::{Block, Interface, Member, Sampler, Variable};
pub use normalise::Normalised;
pub use preprocess::Preprocessed;

/// The shader stages Hematite can compile.
///
/// OpenGL exposes more (`GL_GEOMETRY_SHADER`, `GL_TESS_*`, `GL_COMPUTE_SHADER`);
/// those reach the front end as unsupported rather than being silently dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Vertex,
    Fragment,
}

impl Stage {
    /// The `GL_*_SHADER` enum value the front end stores for this stage.
    pub const fn gl_enum(self) -> u32 {
        match self {
            Stage::Vertex => 0x8B31,
            Stage::Fragment => 0x8B30,
        }
    }

    /// `Stage` for a `GL_*_SHADER` enum value, if Hematite supports that stage.
    pub const fn from_gl_enum(value: u32) -> Option<Stage> {
        match value {
            0x8B31 => Some(Stage::Vertex),
            0x8B30 => Some(Stage::Fragment),
            _ => None,
        }
    }

    const fn naga(self) -> naga::ShaderStage {
        match self {
            Stage::Vertex => naga::ShaderStage::Vertex,
            Stage::Fragment => naga::ShaderStage::Fragment,
        }
    }
}

/// Which pass reported a failure. Shown verbatim in `glGetShaderInfoLog` output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `#include` could not be resolved, or the include graph is cyclic.
    Preprocess,
    /// The source uses a construct Hematite does not translate.
    Unsupported,
    /// naga's GLSL front end rejected the normalised source.
    Compile,
    /// naga's IR validator rejected the module.
    Validate,
    /// The SPIR-V back end could not represent the module.
    Emit,
}

impl Kind {
    const fn label(self) -> &'static str {
        match self {
            Kind::Preprocess => "preprocess",
            Kind::Unsupported => "unsupported",
            Kind::Compile => "compile",
            Kind::Validate => "validate",
            Kind::Emit => "emit",
        }
    }
}

/// A compile failure, with the source line it came from when one is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: Kind,
    pub message: String,
    /// Line in the *original* source, 1-based, when the failure can be mapped back.
    pub line: Option<u32>,
}

impl Error {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), line: None }
    }

    /// Rendered the way `glGetShaderInfoLog` returns it: one line per problem.
    pub fn to_info_log(&self) -> String {
        match self.line {
            Some(line) => format!("{}: {}:{}\n", self.kind.label(), line, self.message),
            None => format!("{}: {}\n", self.kind.label(), self.message),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(formatter, "{}:{}: {}", self.kind.label(), line, self.message),
            None => write!(formatter, "{}: {}", self.kind.label(), self.message),
        }
    }
}

impl std::error::Error for Error {}

/// Everything a caller can control about one compilation.
///
/// All slices default to empty; the front end fills them from the GL state it has
/// already recorded (`glBindAttribLocation`, `-D`-style defines, the shader pack's
/// virtual file table).
#[derive(Debug)]
pub struct Request<'a> {
    pub stage: Stage,
    pub source: &'a str,
    /// `#include` targets, looked up by the exact string in the directive.
    pub includes: &'a [(&'a str, &'a str)],
    /// Definitions applied as if each were `#define key value`.
    pub defines: &'a [(&'a str, &'a str)],
    /// Locations requested through `glBindAttribLocation` before `glLinkProgram`.
    pub attribute_locations: &'a [(&'a str, u32)],
    /// Locations requested for fragment outputs, same mechanism.
    pub output_locations: &'a [(&'a str, u32)],
}

impl Default for Request<'_> {
    fn default() -> Self {
        Self {
            stage: Stage::Vertex,
            source: "",
            includes: &[],
            defines: &[],
            attribute_locations: &[],
            output_locations: &[],
        }
    }
}

impl<'a> Request<'a> {
    /// A request with no bindings, includes or defines.
    pub fn new(stage: Stage, source: &'a str) -> Self {
        Self { stage, source, ..Self::default() }
    }
}

/// A successfully compiled shader.
#[derive(Clone, Debug)]
pub struct Shader {
    /// The stage this SPIR-V was compiled for.
    pub stage: Stage,
    /// SPIR-V words, ready for `vkCreateShaderModule`.
    pub spirv: Vec<u32>,
    /// What the SPIR-V expects from the pipeline: locations, bindings, offsets.
    pub interface: Interface,
    /// The Vulkan-GLSL source that was actually compiled. Kept for the info log
    /// and because tests assert on it; a driver would not expose this.
    pub normalised: String,
    /// The `#version` the source asked for (100/110/120/300/330/450/...), or 110
    /// when it declared none, which is what a driver assumes.
    pub requested_version: u16,
    pub profile: Profile,
    pub warnings: Vec<String>,
}

/// The `#version` profile, if one was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    None,
    Core,
    Compatibility,
    Es,
}

impl Profile {
    pub const fn label(self) -> &'static str {
        match self {
            Profile::None => "",
            Profile::Core => "core",
            Profile::Compatibility => "compatibility",
            Profile::Es => "es",
        }
    }
}

/// Compile GLSL to SPIR-V.
///
/// The returned [`Shader`] is what the front end hands to the backend; the returned
/// [`Error`] is what `glGetShaderInfoLog` reports. This function never panics on
/// malformed input — a shader that cannot be compiled is an ordinary error path,
/// and Hematite is loaded into someone else's process where unwinding is fatal.
pub fn compile(request: &Request<'_>) -> Result<Shader, Error> {
    let preprocessed = preprocess::run(request)?;
    let normalised = normalise::run(request.stage, &preprocessed)?;
    emit(request.stage, &normalised, &preprocessed)
}

/// Run stages 2 and 3: naga's front end, validator and SPIR-V back end.
fn emit(
    stage: Stage,
    normalised: &Normalised,
    preprocessed: &Preprocessed,
) -> Result<Shader, Error> {
    let mut options = naga::front::glsl::Options::from(stage.naga());
    for (key, value) in &normalised.defines {
        options.defines.insert(key.clone(), value.clone());
    }

    let mut frontend = naga::front::glsl::Frontend::default();
    let module = frontend.parse(&options, &normalised.source).map_err(|errors| {
        // naga reports every problem it found, each with the byte range it covers.
        let message = errors.iter().map(|error| error.to_string()).collect::<Vec<_>>().join("; ");
        Error {
            kind: Kind::Compile,
            message,
            line: errors
                .first()
                .and_then(|error| error.meta.to_range())
                .map(|range| line_of(&normalised.source, range.start))
                .map(|line| normalised.original_line(line)),
        }
    })?;

    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::default(),
    );
    let info = validator.validate(&module).map_err(|error| Error {
        kind: Kind::Validate,
        message: format!("{error}"),
        line: None,
    })?;

    let spirv_options = naga::back::spv::Options {
        lang_version: (1, 0),
        flags: naga::back::spv::WriterFlags::empty(),
        ..Default::default()
    };
    let pipeline_options = naga::back::spv::PipelineOptions {
        shader_stage: stage.naga(),
        entry_point: "main".to_string(),
    };
    let spirv = naga::back::spv::write_vec(&module, &info, &spirv_options, Some(&pipeline_options))
        .map_err(|error| Error { kind: Kind::Emit, message: format!("{error}"), line: None })?;
    // naga declares sampler types in every module; drivers that reflect them can reject a
    // shader that never samples. See the module documentation for the driver that does.
    let spirv = spirv::strip_unused_sampler_types(&spirv);

    Ok(Shader {
        stage,
        spirv,
        interface: normalised.interface.clone(),
        normalised: normalised.source.clone(),
        requested_version: preprocessed.version,
        profile: preprocessed.profile,
        warnings: normalised.warnings.clone(),
    })
}

/// The 1-based line a byte offset falls on.
fn line_of(source: &str, offset: usize) -> u32 {
    let offset = offset.min(source.len());
    source[..offset].matches('\n').count() as u32 + 1
}

/// The SPIR-V magic number every module must start with.
pub const SPIRV_MAGIC: u32 = 0x0723_0203;

#[cfg(test)]
mod tests {
    use super::*;

    const VERTEX_120: &str = "\
#version 120
attribute vec4 position;
attribute vec2 uv;
varying vec2 texcoord;
uniform mat4 transform;
void main() {
    texcoord = uv;
    gl_Position = transform * position;
}
";

    const FRAGMENT_120: &str = "\
#version 120
varying vec2 texcoord;
uniform sampler2D diffuse;
void main() {
    gl_FragColor = texture2D(diffuse, texcoord);
}
";

    #[test]
    fn legacy_glsl_120_compiles_to_spirv() {
        for (stage, source) in [(Stage::Vertex, VERTEX_120), (Stage::Fragment, FRAGMENT_120)] {
            let shader = compile(&Request::new(stage, source))
                .unwrap_or_else(|error| panic!("{stage:?}: {error}"));
            assert_eq!(shader.spirv[0], SPIRV_MAGIC, "magic word");
            assert_eq!(shader.requested_version, 120);
            assert!(!shader.spirv.is_empty());
        }
    }

    #[test]
    fn interface_survives_the_rewrite() {
        let vertex = compile(&Request::new(Stage::Vertex, VERTEX_120)).expect("vertex");
        let positions: Vec<&str> =
            vertex.interface.attributes.iter().map(|item| item.name.as_str()).collect();
        assert!(positions.iter().any(|name| *name == "position"), "{positions:?}");
        assert!(positions.iter().any(|name| *name == "uv"));
        assert!(vertex.interface.uniforms().iter().any(|member| member.name == "transform"));

        let fragment = compile(&Request::new(Stage::Fragment, FRAGMENT_120)).expect("fragment");
        assert!(fragment.interface.samplers.iter().any(|sampler| sampler.name == "diffuse"));
        assert_eq!(
            fragment
                .interface
                .outputs
                .iter()
                .map(|output| output.name.as_str())
                .collect::<Vec<_>>(),
            vec!["hematite_out_color"],
            "gl_FragColor becomes a location-0 output"
        );
    }

    #[test]
    fn bind_attrib_location_is_honoured() {
        let request = Request {
            stage: Stage::Vertex,
            source: VERTEX_120,
            attribute_locations: &[("uv", 5)],
            ..Request::default()
        };
        let shader = compile(&request).expect("compile");
        let uv = shader
            .interface
            .attributes
            .iter()
            .find(|attribute| attribute.name == "uv")
            .expect("uv declared");
        assert_eq!(uv.location, 5);
        let position = shader
            .interface
            .attributes
            .iter()
            .find(|attribute| attribute.name == "position")
            .expect("position declared");
        assert_ne!(position.location, 5, "the remaining attribute avoids the taken location");
    }

    #[test]
    fn glsl_330_and_450_core_compile() {
        let vertex = "\
#version 330 core
layout(location = 0) in vec3 position;
layout(location = 1) in vec2 uv;
out vec2 texcoord;
uniform mat4 mvp;
void main() {
    texcoord = uv;
    gl_Position = mvp * vec4(position, 1.0);
}
";
        let fragment = "\
#version 450 core
in vec2 texcoord;
layout(location = 0) out vec4 colour;
layout(binding = 3) uniform sampler2D diffuse;
void main() {
    colour = texture(diffuse, texcoord);
}
";
        for (stage, source, version) in
            [(Stage::Vertex, vertex, 330), (Stage::Fragment, fragment, 450)]
        {
            let shader = compile(&Request::new(stage, source))
                .unwrap_or_else(|error| panic!("{stage:?}: {error}"));
            assert_eq!(shader.spirv[0], SPIRV_MAGIC);
            assert_eq!(shader.requested_version, version);
        }
        let shader = compile(&Request::new(Stage::Vertex, vertex)).expect("330");
        assert!(shader
            .interface
            .attributes
            .iter()
            .any(|attribute| attribute.name == "position" && attribute.location == 0));
        let shader = compile(&Request::new(Stage::Fragment, fragment)).expect("450");
        assert!(shader
            .interface
            .samplers
            .iter()
            .any(|sampler| sampler.name == "diffuse" && sampler.binding == 3));
    }

    #[test]
    fn opengl_es_sources_compile() {
        let vertex = "\
#version 100
attribute vec4 position;
varying vec2 texcoord;
void main() {
    texcoord = position.xy;
    gl_Position = position;
}
";
        let fragment = "\
#version 300 es
precision mediump float;
in vec2 texcoord;
out vec4 colour;
void main() {
    colour = vec4(texcoord, 0.0, 1.0);
}
";
        for (stage, source) in [(Stage::Vertex, vertex), (Stage::Fragment, fragment)] {
            let shader = compile(&Request::new(stage, source))
                .unwrap_or_else(|error| panic!("{stage:?}: {error}"));
            assert_eq!(shader.spirv[0], SPIRV_MAGIC);
        }
    }

    #[test]
    fn shader_pack_style_sources_compile() {
        let common = "// shared definitions\n#define SATURATE(x) clamp(x, 0.0, 1.0)\n";
        let vertex = "\
#version 120
#include \"/shaders/lib/common.glsl\"
attribute vec4 position;
attribute vec2 uv;
varying vec2 texcoord;
void main() {
    texcoord = SATURATE(uv);
    gl_Position = gl_ModelViewProjectionMatrix * position;
}
";
        let fragment = "\
#version 120
/* RENDERTARGETS: 0,1,2 */
#include \"/shaders/lib/common.glsl\"
varying vec2 texcoord;
uniform sampler2D diffuse;
uniform sampler2DShadow shadowtex;
void main() {
    vec4 colour = texture2D(diffuse, texcoord);
    float shadow = shadow2D(shadowtex, vec3(texcoord, 0.5)).r;
    gl_FragColor = vec4(colour.rgb * shadow, 1.0);
    gl_FragData[1] = vec4(texcoord, 0.0, 1.0);
    gl_FragData[2] = vec4(0.0);
}
";
        let request = Request {
            stage: Stage::Vertex,
            source: vertex,
            includes: &[("/shaders/lib/common.glsl", common)],
            ..Request::default()
        };
        let shader = compile(&request).unwrap_or_else(|error| panic!("vertex: {error}"));
        assert_eq!(shader.spirv[0], SPIRV_MAGIC);
        assert!(
            shader.normalised.contains("SATURATE"),
            "the included definition reaches the compiler: {}",
            shader.normalised
        );

        let request = Request {
            stage: Stage::Fragment,
            source: fragment,
            includes: &[("/shaders/lib/common.glsl", common)],
            ..Request::default()
        };
        let shader = compile(&request).unwrap_or_else(|error| panic!("fragment: {error}"));
        let outputs: Vec<&str> =
            shader.interface.outputs.iter().map(|output| output.name.as_str()).collect();
        assert_eq!(outputs.len(), 3, "three render targets, got {outputs:?}");
        let locations: Vec<u32> =
            shader.interface.outputs.iter().map(|output| output.location).collect();
        assert_eq!(locations, vec![0, 1, 2]);
        assert!(shader
            .interface
            .samplers
            .iter()
            .any(|sampler| sampler.name == "shadowtex" && sampler.comparison));
    }

    #[test]
    fn missing_include_is_reported_with_a_line() {
        let source = "#version 120\n#include \"missing.glsl\"\nvoid main() {}\n";
        let error = compile(&Request::new(Stage::Vertex, source)).expect_err("must fail");
        assert_eq!(error.kind, Kind::Preprocess);
        assert_eq!(error.line, Some(2));
        assert!(error.message.contains("missing.glsl"), "{error}");
    }

    #[test]
    fn broken_shader_fails_with_a_diagnostic() {
        let source = "#version 330 core\nvoid main() { gl_Position = 1.0 * ; }\n";
        let error = compile(&Request::new(Stage::Vertex, source)).expect_err("must fail");
        assert!(matches!(error.kind, Kind::Compile | Kind::Validate), "{error:?}");
        assert!(!error.message.is_empty());
    }

    #[test]
    fn unsupported_stage_is_refused_not_ignored() {
        for value in [0x8DD9u32, 0x8E88, 0x8E89, 0x91B9, 0x0000] {
            assert_eq!(Stage::from_gl_enum(value), None, "stage 0x{value:04x} is unsupported");
        }
        assert_eq!(Stage::from_gl_enum(0x8B31), Some(Stage::Vertex));
        assert_eq!(Stage::from_gl_enum(0x8B30), Some(Stage::Fragment));
    }

    #[test]
    fn spirv_module_is_well_formed() {
        let shader = compile(&Request::new(Stage::Vertex, VERTEX_120)).expect("compile");
        let words = &shader.spirv;
        assert!(words.len() >= 5, "header plus at least one instruction");
        assert_eq!(words[0], SPIRV_MAGIC);
        // The generator field records the tool: Khronos tool id 1 is naga.
        assert_eq!(words[1] >> 16, 1, "generator tool id is naga's");
        assert!(words[3] >= 1, "bound is at least one id");
    }
}
