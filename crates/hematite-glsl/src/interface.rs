//! What a compiled shader expects from the pipeline.
//!
//! The GL API answers questions about a program by *name* — `glGetAttribLocation`,
//! `glGetUniformLocation`, `glGetActiveUniform` — while SPIR-V and Vulkan bind
//! resources by *number*: a location for each input or output, a `(set, binding)`
//! for each descriptor. This module is the mapping between the two, and it exists
//! because the normaliser assigns those numbers before the source is compiled, so
//! the front end can answer `glGetUniformLocation` without asking the driver.
//!
//! Uniform offsets follow **std140**, the layout Vulkan requires for uniform
//! buffers, computed by [`std140_layout`]. Hematite's own `glUniform*` emulation
//! writes through those offsets; a driver would compute them internally.

/// One named variable with the number it was bound to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variable {
    pub name: String,
    /// The GLSL type as written, normalised (`vec4`, `mat4`, `float`, ...).
    pub ty: String,
    pub location: u32,
}

/// A texture uniform: a descriptor, not a byte range.
///
/// Desktop GL combines the texture and the sampling state into one object
/// (`sampler2D`). Vulkan-GLSL does not: it has a `texture2D` and a `sampler`, and
/// the shader recombines them at each call site. The normaliser performs that
/// split, and this struct records both halves so the backend can bind them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sampler {
    /// The name the shader used.
    pub name: String,
    /// The GL type the shader declared (`sampler2D`, `sampler2DShadow`, ...).
    pub ty: String,
    /// `2D`, `Cube`, `2DArray`, `3D`, `1D`.
    pub dimension: String,
    /// Binding holding the sampled image.
    pub binding: u32,
    /// Binding holding the sampler object; Vulkan requires a distinct binding.
    pub sampler_binding: u32,
    pub set: u32,
    /// True for `sampler*Shadow`, which Vulkan samples with a comparison sampler.
    pub comparison: bool,
    /// Array size for `sampler2DArray` and friends; 1 for a plain sampler.
    pub array_elements: u32,
    /// The normalised texture variable name.
    pub image_name: String,
    /// The normalised sampler variable name.
    pub sampler_name: String,
}

/// One field of a uniform block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub ty: String,
    /// Byte offset inside the block, per std140.
    pub offset: u32,
}

/// A uniform block: one descriptor binding covering several fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub name: String,
    pub binding: u32,
    pub set: u32,
    pub members: Vec<Member>,
    /// Size in bytes the block occupies, rounded up per std140.
    pub size: u32,
}

/// Everything the pipeline has to provide for a compiled shader.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Interface {
    /// Vertex inputs (`attribute` / `in` in a vertex shader), by location.
    pub attributes: Vec<Variable>,
    /// Stage outputs that are not render targets: `varying` / `out` in a vertex
    /// shader, and `varying` / `in` in a fragment shader.
    pub varyings: Vec<Variable>,
    /// Fragment outputs, i.e. render targets. Multiple entries mean MRT.
    pub outputs: Vec<Variable>,
    pub samplers: Vec<Sampler>,
    /// Uniform blocks in declaration order. Hematite's own block for plain
    /// `uniform` declarations is named [`HEMATITE_UNIFORM_BLOCK`].
    pub blocks: Vec<Block>,
}

/// The block Hematite generates for plain `uniform vec4 x;` declarations.
///
/// Vulkan-GLSL has no such thing as a loose uniform: every uniform lives in a
/// block with an explicit binding. The normaliser therefore gathers them into one
/// anonymous block, which GLSL scopes exactly like the loose declarations were, so
/// the shader body does not change.
pub const HEMATITE_UNIFORM_BLOCK: &str = "HematiteUniforms";

impl Interface {
    /// Every uniform field the program exposes, across all blocks.
    pub fn uniforms(&self) -> Vec<&Member> {
        self.blocks.iter().flat_map(|block| block.members.iter()).collect()
    }

    /// The location a name was bound to, or `None` if the shader does not declare it.
    ///
    /// This is what `glGetAttribLocation` and `glGetUniformLocation` return (the
    /// latter as a handle the front end maps back to a block field).
    pub fn location_of(&self, name: &str) -> Option<u32> {
        self.attributes
            .iter()
            .chain(self.varyings.iter())
            .chain(self.outputs.iter())
            .find(|variable| variable.name == name)
            .map(|variable| variable.location)
    }

    /// The uniform field with this name, with the block it lives in.
    pub fn uniform_of(&self, name: &str) -> Option<(&Block, &Member)> {
        self.blocks
            .iter()
            .find_map(|block| block.members.iter().find(|m| m.name == name).map(|m| (block, m)))
    }

    /// The binding a sampler name was given.
    pub fn sampler_of(&self, name: &str) -> Option<&Sampler> {
        self.samplers.iter().find(|sampler| sampler.name == name)
    }

    /// The largest location in use by any input or output, plus one.
    pub fn location_count(&self) -> u32 {
        self.attributes
            .iter()
            .chain(self.varyings.iter())
            .chain(self.outputs.iter())
            .map(|variable| variable.location + 1)
            .max()
            .unwrap_or(0)
    }
}

/// Alignment and size of a GLSL type under std140.
///
/// Returns `(align, size)`. Array and struct rules are handled by
/// [`std140_layout`] using these primitives; an unparseable type is an error
/// rather than a guess, because a wrong offset silently corrupts a uniform.
pub fn scalar_layout(ty: &str) -> Option<(u32, u32)> {
    let base = match ty {
        "float" | "int" | "uint" | "bool" => Some((4, 4)),
        "double" => Some((8, 8)),
        _ => None,
    };
    if let Some((align, size)) = base {
        return Some((align, size));
    }
    let (prefix, width) = match ty {
        "vec2" | "ivec2" | "uvec2" | "bvec2" | "dvec2" => ("vec2", 2),
        "vec3" | "ivec3" | "uvec3" | "bvec3" | "dvec3" => ("vec3", 3),
        "vec4" | "ivec4" | "uvec4" | "bvec4" | "dvec4" => ("vec4", 4),
        _ => return None,
    };
    let element = if ty.starts_with('d') { 8 } else { 4 };
    // std140: a three-component vector has the size of three scalars but the
    // alignment of a four-component one.
    let align = if prefix == "vec3" { 4 * element } else { width * element };
    Some((align, width * element))
}

/// The std140 offset table for a list of `(type, name)` fields.
///
/// `(offsets, total_size)`. Matrices are laid out as columns of `vec4`, and array
/// elements are rounded up to 16 bytes, which is where std140 differs from
/// `packed` and where a hand-written renderer usually goes wrong.
pub fn std140_layout(fields: &[(String, String)]) -> Result<(Vec<u32>, u32), String> {
    let mut offsets = Vec::with_capacity(fields.len());
    let mut offset = 0u32;
    let mut largest_align = 1u32;
    for (ty, name) in fields {
        let (align, size) = layout_of(ty).ok_or_else(|| format!("unknown uniform type `{ty}`"))?;
        largest_align = largest_align.max(align);
        offset = align_up(offset, align);
        offsets.push(offset);
        offset += size;
        if name.is_empty() {
            return Err("uniform block member without a name".to_string());
        }
    }
    Ok((offsets, align_up(offset, largest_align)))
}

/// `(align, size)` for any type [`std140_layout`] accepts, arrays included.
pub fn layout_of(ty: &str) -> Option<(u32, u32)> {
    if let Some((base, elements)) = split_array(ty) {
        let (align, size) = layout_of(base)?;
        let stride = align_up(size, align.max(16));
        return Some((align.max(16), stride * elements));
    }
    if let Some(columns) = matrix_columns(ty) {
        // Each column is a vec4 slot in std140, even for mat2 and mat3.
        return Some((16, 16 * columns));
    }
    scalar_layout(ty)
}

/// Split `float[4]` into `("float", 4)`.
pub fn split_array(ty: &str) -> Option<(&str, u32)> {
    let open = ty.find('[')?;
    let close = ty.find(']')?;
    if close < open {
        return None;
    }
    let elements: u32 = ty[open + 1..close].trim().parse().ok()?;
    Some((ty[..open].trim(), elements))
}

/// Column count for `matN`/`matNxM`, which std140 lays out as N columns of vec4.
pub fn matrix_columns(ty: &str) -> Option<u32> {
    let rest = ty.strip_prefix("mat")?;
    let rest = rest.strip_prefix('d').unwrap_or(rest);
    match rest.split_once('x') {
        // `matNxM` is N columns of M rows, so the column count comes first.
        Some((columns, _rows)) => columns.parse().ok(),
        None => rest.parse().ok(),
    }
}

/// Round `value` up to a multiple of `align`.
pub const fn align_up(value: u32, align: u32) -> u32 {
    if align <= 1 {
        value
    } else {
        value.div_ceil(align) * align
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(ty, name)| (ty.to_string(), name.to_string())).collect()
    }

    #[test]
    fn std140_matches_the_specification_examples() {
        // std140 reference: a vec3 then a float pack together, a mat4 starts fresh.
        let (offsets, size) =
            std140_layout(&fields(&[("vec3", "a"), ("float", "b"), ("mat4", "m")])).unwrap();
        assert_eq!(offsets, vec![0, 12, 16]);
        assert_eq!(size, 80);
    }

    #[test]
    fn arrays_stride_to_sixteen_bytes() {
        let (offsets, size) =
            std140_layout(&fields(&[("vec3", "v"), ("float[2]", "a"), ("float", "f")])).unwrap();
        assert_eq!(offsets, vec![0, 16, 48]);
        assert_eq!(size, 64);
    }

    #[test]
    fn mat3_occupies_three_vec4_columns() {
        assert_eq!(layout_of("mat3"), Some((16, 48)));
        assert_eq!(layout_of("mat2"), Some((16, 32)));
        assert_eq!(layout_of("mat4x3"), Some((16, 64)));
    }

    #[test]
    fn unknown_types_are_an_error_not_a_guess() {
        assert!(std140_layout(&fields(&[("sampler2D", "t")])).is_err());
        assert!(layout_of("texture2D").is_none());
    }

    #[test]
    fn lookups_find_what_was_declared() {
        let interface = Interface {
            attributes: vec![Variable { name: "position".into(), ty: "vec4".into(), location: 0 }],
            varyings: vec![Variable { name: "texcoord".into(), ty: "vec2".into(), location: 1 }],
            outputs: vec![Variable {
                name: "hematite_out_color".into(),
                ty: "vec4".into(),
                location: 0,
            }],
            samplers: vec![Sampler {
                name: "diffuse".into(),
                ty: "sampler2D".into(),
                dimension: "2D".into(),
                binding: 0,
                sampler_binding: 1,
                set: 0,
                comparison: false,
                array_elements: 1,
                image_name: "hematite_diffuse_image".into(),
                sampler_name: "hematite_diffuse_sampler".into(),
            }],
            blocks: vec![Block {
                name: HEMATITE_UNIFORM_BLOCK.into(),
                binding: 1,
                set: 0,
                members: vec![Member { name: "mvp".into(), ty: "mat4".into(), offset: 0 }],
                size: 64,
            }],
        };
        assert_eq!(interface.location_of("position"), Some(0));
        assert_eq!(interface.location_of("texcoord"), Some(1));
        assert_eq!(interface.location_of("nope"), None);
        let (block, member) = interface.uniform_of("mvp").expect("uniform");
        assert_eq!(block.binding, 1);
        assert_eq!(member.offset, 0);
        assert_eq!(interface.sampler_of("diffuse").map(|s| s.binding), Some(0));
        assert_eq!(interface.location_count(), 2);
    }

    #[test]
    fn align_up_handles_the_edges() {
        assert_eq!(align_up(0, 16), 0);
        assert_eq!(align_up(1, 16), 16);
        assert_eq!(align_up(16, 16), 16);
        assert_eq!(align_up(7, 1), 7);
    }
}
