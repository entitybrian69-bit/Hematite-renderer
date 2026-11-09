//! Configuration, resolved from the environment and an optional file.
//!
//! Resolution order for every key, highest priority first:
//!
//! 1. The `HEMATITE_*` environment variable (launchers can set these).
//! 2. The key as written in the config file.
//! 3. The built-in default.
//!
//! Legacy variables that Pojav-derived launchers set (`LIBGL_*`, `POJAVEXEC_EGL`,
//! `POJAV_RENDERER`) are *read* — see [`LegacyEnv`] — but never override a `HEMATITE_*`
//! value. They exist so Hematite can be told what the host expects, not so the host can
//! silently reconfigure a renderer it does not understand.
//!
//! No dependencies: this crate must be testable in an environment with no registry access,
//! and a config parser is not worth a dependency.

#![deny(unsafe_op_in_unsafe_fn)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::fmt;

/// Which backend to use.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Backend {
    /// Pick from the capability profile and the quirks database.
    #[default]
    Auto,
    /// Vulkan only; fail loudly if it is not usable rather than silently falling back.
    Vulkan,
    /// OpenGL ES 3.x only.
    Gles,
}

impl Backend {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Backend::Auto),
            "vulkan" | "vk" => Some(Backend::Vulkan),
            "gles" | "gles3" | "opengles" => Some(Backend::Gles),
            _ => None,
        }
    }

    /// The canonical spelling, used when writing the config back out.
    pub const fn as_str(self) -> &'static str {
        match self {
            Backend::Auto => "auto",
            Backend::Vulkan => "vulkan",
            Backend::Gles => "gles",
        }
    }
}

/// What to do when a draw needs a pipeline that is not ready yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PipelinePolicy {
    /// Skip the draw for a bounded number of frames, then create synchronously.
    #[default]
    Skip,
    /// Always create synchronously on first use.
    Block,
    /// Wait for the worker that is already compiling it, up to the timeout, then block.
    Wait,
}

impl PipelinePolicy {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "skip" => Some(PipelinePolicy::Skip),
            "block" => Some(PipelinePolicy::Block),
            "wait" => Some(PipelinePolicy::Wait),
            _ => None,
        }
    }

    /// The canonical spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            PipelinePolicy::Skip => "skip",
            PipelinePolicy::Block => "block",
            PipelinePolicy::Wait => "wait",
        }
    }
}

/// Log verbosity.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum LogLevel {
    /// Nothing but fatal errors.
    Off,
    /// Errors only.
    Error,
    /// Errors and warnings.
    #[default]
    Warn,
    /// Plus lifecycle events.
    Info,
    /// Plus per-frame detail.
    Debug,
    /// Plus hot-path detail; costs real time.
    Trace,
}

impl LogLevel {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(LogLevel::Off),
            "error" => Some(LogLevel::Error),
            "warn" | "warning" => Some(LogLevel::Warn),
            "info" => Some(LogLevel::Info),
            "debug" => Some(LogLevel::Debug),
            "trace" => Some(LogLevel::Trace),
            _ => None,
        }
    }

    /// The canonical spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            LogLevel::Off => "off",
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

/// A configuration problem: an unparseable value, with where it came from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ConfigError {
    /// The key involved.
    pub key: String,
    /// The value as it was written.
    pub value: String,
    /// Human-readable explanation.
    pub reason: String,
    /// Where the value came from: `"env"` or `"file"`.
    pub source: &'static str,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "config `{}` = {:?} from {}: {}", self.key, self.value, self.source, self.reason)
    }
}

impl std::error::Error for ConfigError {}

/// The resolved configuration.
///
/// Every field has a documented default; see `docs/CONFIG.md`, which is generated from the
/// same list of keys this struct exposes (kept in sync by a test that checks every field
/// appears in the docs).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Config {
    /// Which backend to use.
    pub backend: Backend,
    /// Present mode request: 0 = choose, 1 = FIFO/vsync, 2 = mailbox, 3 = immediate.
    pub present_mode: u8,
    /// Frame limit; 0 means unlimited.
    pub fps_limit: u32,
    /// Render scale as a percentage (100 = native).
    pub render_scale_percent: u32,
    /// Anti-aliasing sample count for the default framebuffer; 1 disables MSAA.
    pub msaa_samples: u32,
    /// Requested anisotropic filtering level; 0 disables.
    pub anisotropy: u32,
    /// Async pipeline policy.
    pub pipeline_policy: PipelinePolicy,
    /// Maximum frames a draw may be skipped waiting for a pipeline.
    pub pipeline_skip_frames: u8,
    /// Enable the on-disk shader cache.
    pub shader_cache: bool,
    /// Shader cache size limit in mebibytes.
    pub shader_cache_mb: u32,
    /// Enable fast (minimal-validation) mode. `false` selects strict mode.
    pub fast: bool,
    /// Log level.
    pub log_level: LogLevel,
    /// Dump translated shaders and SPIR-V for debugging.
    pub shader_dump: bool,
    /// Enable the profiler.
    pub profiler: bool,
    /// Number of translation worker threads; 0 = automatic.
    pub worker_threads: u32,
    /// GPU memory budget cap in mebibytes; 0 = unlimited.
    pub memory_budget_mb: u32,
    /// Override for `GL_VERSION`; empty means the computed value.
    pub override_gl_version: String,
    /// Override for `GL_RENDERER`; empty means the computed value.
    pub override_gl_renderer: String,
    /// Override for `GL_VENDOR`; empty means the computed value.
    pub override_gl_vendor: String,
    /// Extra quirks entries to disable, by id.
    pub disable_quirks: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: Backend::Auto,
            present_mode: 0,
            fps_limit: 0,
            render_scale_percent: 100,
            msaa_samples: 1,
            anisotropy: 0,
            pipeline_policy: PipelinePolicy::Skip,
            pipeline_skip_frames: 3,
            shader_cache: true,
            shader_cache_mb: 256,
            fast: true,
            log_level: LogLevel::Warn,
            shader_dump: false,
            profiler: false,
            worker_threads: 0,
            memory_budget_mb: 0,
            override_gl_version: String::new(),
            override_gl_renderer: String::new(),
            override_gl_vendor: String::new(),
            disable_quirks: Vec::new(),
        }
    }
}

/// Parses a `key=value` config file.
///
/// Deliberately minimal: `#` comments, blank lines, `key = value`, and no sections. Values
/// keep their surrounding whitespace stripped but are otherwise verbatim, so a `GL_VERSION`
/// override containing spaces works.
pub fn parse_file(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        out.insert(k.to_ascii_lowercase().replace('-', "_"), v.trim().to_string());
    }
    out
}

/// Reads the legacy variables Pojav-derived launchers set.
///
/// These are informational: they describe what the *host* expects. Hematite records them so
/// it can honour the ones that matter (a requested desktop GL version) and log the ones that
/// only apply to gl4es-lineage wrappers.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct LegacyEnv {
    /// `POJAV_RENDERER`, typically `opengles3`.
    pub pojav_renderer: Option<String>,
    /// `POJAVEXEC_EGL`, the path the host loaded.
    pub pojavexec_egl: Option<String>,
    /// `LIBGL_GL`, the desktop GL version the host expects.
    pub libgl_gl: Option<u32>,
    /// `LIBGL_ES`, the ES version the host asks for.
    pub libgl_es: Option<u32>,
    /// `LIBGL_EGL`, the underlying EGL to load.
    pub libgl_egl: Option<String>,
    /// `LIBGL_GLES`, the underlying GLES to load.
    pub libgl_gles: Option<String>,
    /// True when any of the gl4es-only toggles were set, so we can log once that they are
    /// not applicable to Hematite.
    pub saw_gl4es_toggles: bool,
}

impl LegacyEnv {
    /// Reads the legacy variables through `get`.
    pub fn read<F>(mut get: F) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        let parse_u32 = |s: Option<String>| s.and_then(|v| v.trim().parse::<u32>().ok());
        let toggles =
            ["LIBGL_USE_MC_COLOR", "LIBGL_NORMALIZE", "LIBGL_NOINTOVLHACK", "LIBGL_NOERROR"];
        Self {
            pojav_renderer: get("POJAV_RENDERER"),
            pojavexec_egl: get("POJAVEXEC_EGL"),
            libgl_gl: parse_u32(get("LIBGL_GL")),
            libgl_es: parse_u32(get("LIBGL_ES")),
            libgl_egl: get("LIBGL_EGL"),
            libgl_gles: get("LIBGL_GLES"),
            saw_gl4es_toggles: toggles.iter().any(|t| get(t).is_some()),
        }
    }

    /// The desktop GL major/minor the host asked for, if any.
    ///
    /// `LIBGL_GL` is written the way gl4es writes it: `31` means 3.1, `21` means 2.1. This
    /// is the version Hematite should advertise so mods' checks pass.
    pub fn requested_gl_version(&self) -> Option<(u8, u8)> {
        let v = self.libgl_gl?;
        if v == 0 {
            return None;
        }
        Some(((v / 10) as u8, (v % 10) as u8))
    }
}

/// Every configuration key this build understands.
///
/// This is the authoritative list: `docs/CONFIG.md` documents exactly these keys, the
/// environment reader iterates it, and `apply` must handle every one of them (enforced by a
/// test). Adding a key in one place and forgetting another is the failure mode this list
/// exists to prevent.
pub const KEYS: &[&str] = &[
    "backend",
    "present_mode",
    "fps_limit",
    "render_scale",
    "msaa",
    "anisotropy",
    "pipeline_policy",
    "pipeline_skip_frames",
    "shader_cache",
    "shader_cache_mb",
    "fast",
    "log_level",
    "shader_dump",
    "profiler",
    "worker_threads",
    "memory_budget_mb",
    "gl_version",
    "gl_renderer",
    "gl_vendor",
    "disable_quirks",
];

/// Alternate spellings accepted for a key, mapped to the canonical one.
fn canonical_key(key: &str) -> &str {
    match key {
        "render_scale_percent" => "render_scale",
        "msaa_samples" => "msaa",
        "fast_mode" => "fast",
        "log" => "log_level",
        other => other,
    }
}

/// Whether `apply` recognised the key.
///
/// Unknown keys are accepted silently on purpose (a newer launcher may set a key this build
/// does not know), so callers that want to *audit* coverage need this distinction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Applied {
    /// The key was understood and the value applied.
    Known,
    /// The key is not one of [`KEYS`]; nothing was changed.
    Unknown,
}

/// Applies `key`/`value` from one source to `cfg`, returning an error for bad values.
fn apply(
    cfg: &mut Config,
    key: &str,
    value: &str,
    source: &'static str,
) -> Result<Applied, ConfigError> {
    let key = canonical_key(key);
    let bad = |reason: &str| ConfigError {
        key: key.to_string(),
        value: value.to_string(),
        reason: reason.to_string(),
        source,
    };
    let num = |v: &str| v.trim().parse::<u32>().map_err(|_| bad("expected a non-negative integer"));
    match key {
        "backend" => {
            cfg.backend =
                Backend::parse(value).ok_or_else(|| bad("expected auto, vulkan or gles"))?
        }
        "present_mode" => cfg.present_mode = num(value)? as u8,
        "fps_limit" => cfg.fps_limit = num(value)?,
        "render_scale" | "render_scale_percent" => {
            let v = num(value)?;
            if !(25..=400).contains(&v) {
                return Err(bad("expected a percentage between 25 and 400"));
            }
            cfg.render_scale_percent = v;
        }
        "msaa" | "msaa_samples" => cfg.msaa_samples = num(value)?,
        "anisotropy" => cfg.anisotropy = num(value)?,
        "pipeline_policy" => {
            cfg.pipeline_policy =
                PipelinePolicy::parse(value).ok_or_else(|| bad("expected skip, block or wait"))?
        }
        "pipeline_skip_frames" => cfg.pipeline_skip_frames = num(value)? as u8,
        "shader_cache" => {
            cfg.shader_cache = parse_bool(value).ok_or_else(|| bad("expected on/off"))?
        }
        "shader_cache_mb" => cfg.shader_cache_mb = num(value)?,
        "fast" | "fast_mode" => {
            cfg.fast = parse_bool(value).ok_or_else(|| bad("expected on/off"))?
        }
        "log_level" | "log" => {
            cfg.log_level = LogLevel::parse(value).ok_or_else(|| bad("expected a log level"))?
        }
        "shader_dump" => {
            cfg.shader_dump = parse_bool(value).ok_or_else(|| bad("expected on/off"))?
        }
        "profiler" => cfg.profiler = parse_bool(value).ok_or_else(|| bad("expected on/off"))?,
        "worker_threads" => cfg.worker_threads = num(value)?,
        "memory_budget_mb" => cfg.memory_budget_mb = num(value)?,
        "gl_version" => cfg.override_gl_version = value.to_string(),
        "gl_renderer" => cfg.override_gl_renderer = value.to_string(),
        "gl_vendor" => cfg.override_gl_vendor = value.to_string(),
        "disable_quirks" => {
            cfg.disable_quirks = value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        }
        // Unknown keys are not an error: a newer launcher may set a key this build does not
        // know, and refusing to start over it would be worse than ignoring it.
        _ => return Ok(Applied::Unknown),
    }
    Ok(Applied::Known)
}

/// Parses the booleans people actually write.
pub fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" | "enabled" => Some(true),
        "0" | "false" | "off" | "no" | "disabled" => Some(false),
        _ => None,
    }
}

/// Resolves the configuration from an optional file body and an environment reader.
///
/// Returns the config plus every problem found. Problems never abort startup: a bad value
/// keeps its default and is reported, because a renderer that refuses to launch over a typo
/// in an env var is worse than one that logs and continues.
pub fn resolve<F>(file: Option<&str>, mut get: F) -> (Config, Vec<ConfigError>)
where
    F: FnMut(&str) -> Option<String>,
{
    let mut cfg = Config::default();
    let mut errors = Vec::new();

    if let Some(body) = file {
        for (k, v) in parse_file(body) {
            if let Err(e) = apply(&mut cfg, &k, &v, "file") {
                errors.push(e);
            }
        }
    }

    // Environment wins, so it is applied last.
    for key in KEYS {
        let env_name = format!("HEMATITE_{}", key.to_ascii_uppercase());
        if let Some(v) = get(&env_name) {
            if let Err(e) = apply(&mut cfg, key, &v, "env") {
                errors.push(e);
            }
        }
    }

    (cfg, errors)
}

impl Config {
    /// Resolves from the process environment and an optional config file body.
    pub fn from_env(file: Option<&str>) -> (Self, Vec<ConfigError>) {
        resolve(file, |k| std::env::var(k).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn defaults_match_the_documented_ones() {
        let c = Config::default();
        assert_eq!(c.backend, Backend::Auto);
        assert_eq!(c.render_scale_percent, 100);
        assert_eq!(c.msaa_samples, 1);
        assert!(c.fast);
        assert_eq!(c.pipeline_policy, PipelinePolicy::Skip);
        assert_eq!(c.pipeline_skip_frames, 3);
        assert_eq!(c.log_level, LogLevel::Warn);
        assert!(c.shader_cache);
    }

    #[test]
    fn file_is_parsed_and_comments_ignored() {
        let body = "\
# a comment
backend = vulkan

render-scale = 75   # trailing comment is NOT stripped
fps_limit=60
";
        let map = parse_file(body);
        assert_eq!(map.get("backend").map(String::as_str), Some("vulkan"));
        assert_eq!(
            map.get("render_scale").map(String::as_str),
            Some("75   # trailing comment is NOT stripped")
        );
        assert_eq!(map.get("fps_limit").map(String::as_str), Some("60"));
    }

    #[test]
    fn environment_overrides_file() {
        let body = "backend = gles\n";
        let (cfg, errs) =
            resolve(Some(body), |k| (k == "HEMATITE_BACKEND").then(|| "vulkan".to_string()));
        assert!(errs.is_empty());
        assert_eq!(cfg.backend, Backend::Vulkan);
    }

    #[test]
    fn bad_values_keep_the_default_and_report() {
        let (cfg, errs) =
            resolve(None, |k| (k == "HEMATITE_RENDER_SCALE").then(|| "9000".to_string()));
        assert_eq!(cfg.render_scale_percent, 100, "default survives a bad value");
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].source, "env");
        assert!(errs[0].reason.contains("between 25 and 400"));
    }

    #[test]
    fn unknown_keys_are_ignored_not_fatal() {
        let (_, errs) = resolve(Some("some_future_key = 1\n"), no_env);
        assert!(errs.is_empty());
    }

    #[test]
    fn booleans_accept_the_usual_spellings() {
        for t in ["1", "true", "ON", "yes", "enabled"] {
            assert_eq!(parse_bool(t), Some(true), "{t}");
        }
        for f in ["0", "false", "Off", "no", "disabled"] {
            assert_eq!(parse_bool(f), Some(false), "{f}");
        }
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn disable_quirks_splits_and_trims() {
        let (cfg, _) =
            resolve(None, |k| (k == "HEMATITE_DISABLE_QUIRKS").then(|| " a , b ,, c ".to_string()));
        assert_eq!(cfg.disable_quirks, vec!["a", "b", "c"]);
    }

    #[test]
    fn legacy_libgl_version_decodes() {
        let env = LegacyEnv::read(|k| match k {
            "LIBGL_GL" => Some("31".into()),
            _ => None,
        });
        assert_eq!(env.requested_gl_version(), Some((3, 1)));
    }

    #[test]
    fn legacy_version_of_zero_is_absent() {
        let env = LegacyEnv::read(|k| match k {
            "LIBGL_GL" => Some("0".into()),
            _ => None,
        });
        assert_eq!(env.requested_gl_version(), None);
    }

    #[test]
    fn legacy_gl4es_toggles_are_noticed() {
        let env = LegacyEnv::read(|k| match k {
            "LIBGL_NOERROR" => Some("1".into()),
            _ => None,
        });
        assert!(env.saw_gl4es_toggles);
    }

    #[test]
    fn gl_string_overrides_pass_through_verbatim() {
        let (cfg, errs) = resolve(None, |k| {
            (k == "HEMATITE_GL_RENDERER").then(|| "Hematite (Adreno 740)".to_string())
        });
        assert!(errs.is_empty());
        assert_eq!(cfg.override_gl_renderer, "Hematite (Adreno 740)");
    }

    #[test]
    fn backend_parsing_is_case_insensitive() {
        for s in ["VULKAN", "Vulkan", "vk"] {
            assert_eq!(Backend::parse(s), Some(Backend::Vulkan));
        }
        assert_eq!(Backend::parse("metal"), None);
    }

    /// Every key in `KEYS` must actually be handled by `apply`, and every canonical key
    /// must be rejectable so a typo cannot slip through as "unknown, therefore ignored".
    #[test]
    fn every_declared_key_is_handled_and_validated() {
        for key in KEYS {
            let mut cfg = Config::default();
            let bogus = "!!!not a valid value!!!";
            match apply(&mut cfg, key, bogus, "test") {
                Ok(Applied::Unknown) => panic!("key `{key}` is in KEYS but apply ignores it"),
                Ok(Applied::Known) => {
                    // Free-form string keys accept anything, which is correct for
                    // gl_version/gl_renderer/gl_vendor/disable_quirks.
                    let free_form = matches!(
                        *key,
                        "gl_version" | "gl_renderer" | "gl_vendor" | "disable_quirks"
                    );
                    assert!(free_form, "key `{key}` accepted a bogus value; it should validate");
                }
                Err(_) => { /* validated, as it should */ }
            }
        }
    }

    #[test]
    fn no_declared_key_is_reported_unknown() {
        // "0" is acceptable to numbers, booleans and free-form strings; the enum-valued
        // keys (backend, pipeline_policy, log_level) reject it, which is fine. What must
        // never happen is `Applied::Unknown` for a key we advertise support for.
        for key in KEYS {
            let mut cfg = Config::default();
            let applied = apply(&mut cfg, key, "0", "test");
            assert_ne!(
                applied,
                Ok(Applied::Unknown),
                "key `{key}` is advertised in KEYS but apply does not recognise it"
            );
        }
    }

    #[test]
    fn alternate_spellings_map_to_canonical_keys() {
        assert_eq!(canonical_key("render_scale_percent"), "render_scale");
        assert_eq!(canonical_key("msaa_samples"), "msaa");
        assert_eq!(canonical_key("fast_mode"), "fast");
        assert_eq!(canonical_key("log"), "log_level");
        assert_eq!(canonical_key("backend"), "backend");
    }

    #[test]
    fn unknown_keys_report_unknown() {
        let mut cfg = Config::default();
        assert_eq!(apply(&mut cfg, "nonsense", "1", "test"), Ok(Applied::Unknown));
    }
}
