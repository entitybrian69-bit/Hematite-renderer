//! Post-processing of the SPIR-V that naga emits.
//!
//! naga 0.14 declares an image type, a sampler type and a sampled-image type in every
//! shader module it writes, even when the shader never samples anything: they are part of
//! the writer's type table, not of the module's interface. Most drivers ignore them. The
//! SwiftShader build used as this repository's reference driver does not: it maps a
//! standalone sampler type to a descriptor type it does not implement, logs
//! `UNSUPPORTED: Unsupported Descriptor Type` while reflecting the module, and then
//! dereferences the failed layout. So a shader that does not sample must not ship sampler
//! types.
//!
//! [`strip_unused_sampler_types`] removes them when the module contains no sampling
//! instruction at all. That is a conservative test — it never edits a module that samples
//! — and it keeps the module valid because it drops every instruction that mentions a
//! removed id, to a fixpoint.

use std::collections::BTreeSet;

/// SPIR-V magic number, `0x07230203`.
pub const MAGIC: u32 = 0x0723_0203;

const HEADER_WORDS: usize = 5;
/// `OpTypeImage`.
const TYPE_IMAGE: u16 = 25;
/// `OpTypeSampler`.
const TYPE_SAMPLER: u16 = 27;
/// `OpTypeSampledImage`.
const TYPE_SAMPLED_IMAGE: u16 = 28;

/// Opcodes that read a sampled image. If none of these is present, nothing in the module
/// can observe a sampler, so the sampler types are dead.
const SAMPLING_OPCODES: &[u16] = &[
    87,  // OpSampledImage
    94,  // OpImageSampleImplicitLod
    95,  // OpImageSampleExplicitLod
    96,  // OpImageSampleDrefImplicitLod
    97,  // OpImageSampleDrefExplicitLod
    98,  // OpImageSampleProjImplicitLod
    99,  // OpImageSampleProjExplicitLod
    100, // OpImageSampleProjDrefImplicitLod
    101, // OpImageSampleProjDrefExplicitLod
    102, // OpImageFetch
    103, // OpImageGather
    104, // OpImageDrefGather
    105, // OpImageRead
    106, // OpImageWrite
    107, // OpImageQueryFormat
    108, // OpImageQueryOrder
    109, // OpImageQuerySizeLod
    110, // OpImageQuerySize
    111, // OpImageQueryLod
    112, // OpImageQueryLevels
    113, // OpImageQuerySamples
];

/// An instruction with its result id, if it produces one.
struct Instruction {
    opcode: u16,
    length: u16,
    result_id: Option<u32>,
    /// Word index in the original stream.
    start: usize,
}

/// Remove sampler types from a module that never samples.
///
/// Returns the words unchanged when the module is too short to have a header, when it does
/// not start with the SPIR-V magic number, or when it contains any sampling instruction.
pub fn strip_unused_sampler_types(words: &[u32]) -> Vec<u32> {
    let Some(instructions) = decode(words) else {
        return words.to_vec();
    };
    if instructions.iter().any(|instruction| SAMPLING_OPCODES.contains(&instruction.opcode)) {
        return words.to_vec();
    }
    let mut dead: BTreeSet<u32> = instructions
        .iter()
        .filter(|instruction| {
            matches!(instruction.opcode, TYPE_IMAGE | TYPE_SAMPLER | TYPE_SAMPLED_IMAGE)
        })
        .filter_map(|instruction| instruction.result_id)
        .collect();
    if dead.is_empty() {
        return words.to_vec();
    }
    // An instruction that names a dead id is itself dead, which can kill more ids.
    loop {
        let mut grew = false;
        for instruction in &instructions {
            let declares_dead =
                instruction.result_id.is_some_and(|result_id| dead.contains(&result_id));
            if declares_dead {
                continue;
            }
            let operands =
                &words[instruction.start + 1..instruction.start + usize::from(instruction.length)];
            if operands.iter().any(|word| dead.contains(word)) {
                if let Some(result_id) = instruction.result_id {
                    if dead.insert(result_id) {
                        grew = true;
                    }
                }
            }
        }
        if !grew {
            break;
        }
    }
    let kept: Vec<&Instruction> = instructions
        .iter()
        .filter(|instruction| {
            !instruction.result_id.is_some_and(|result_id| dead.contains(&result_id))
        })
        .collect();
    rebuild(words, &kept)
}

/// Split a module into instructions, or return None if it is not a well-formed module.
fn decode(words: &[u32]) -> Option<Vec<Instruction>> {
    if words.len() < HEADER_WORDS || words[0] != MAGIC {
        return None;
    }
    let mut instructions = Vec::new();
    let mut index = HEADER_WORDS;
    while index < words.len() {
        let first = words[index];
        let opcode = (first & 0xffff) as u16;
        let length = (first >> 16) as u16;
        if length == 0 || index + usize::from(length) > words.len() {
            return None;
        }
        instructions.push(Instruction {
            opcode,
            length,
            result_id: result_id_of(opcode, &words[index..index + usize::from(length)]),
            start: index,
        });
        index += usize::from(length);
    }
    Some(instructions)
}

/// The result id of an instruction, for the opcodes this module can make dead.
///
/// Only types, constants and variables declare ids that a sampler type can be reachable
/// through, so only those are tracked; every other instruction is kept unconditionally,
/// which is the conservative direction.
fn result_id_of(opcode: u16, words: &[u32]) -> Option<u32> {
    // (opcode, word index of the result id) for the declarations SPIR-V allows a type to
    // appear in: OpTypeVoid 19, OpTypeBool 20, OpTypeInt 21, OpTypeFloat 22, OpTypeVector
    // 23, OpTypeMatrix 24, OpTypeImage 25, OpTypeSampler 27, OpTypeSampledImage 28,
    // OpTypeArray 29, OpTypeStruct 30, OpTypePointer 32, OpTypeFunction 33,
    // OpConstant* 43..46, OpVariable 59.
    let index: u16 = match opcode {
        19..=25 | 27..=30 | 32 | 33 | 43..=46 | 59 => 1,
        _ => return None,
    };
    words.get(usize::from(index)).copied()
}

/// Reassemble a module from kept instructions, with a corrected bound.
fn rebuild(words: &[u32], kept: &[&Instruction]) -> Vec<u32> {
    let mut out: Vec<u32> = words[..HEADER_WORDS].to_vec();
    for instruction in kept {
        out.extend_from_slice(
            &words[instruction.start..instruction.start + usize::from(instruction.length)],
        );
    }
    let mut bound = 1u32;
    let mut index = HEADER_WORDS;
    while index < out.len() {
        let length = (out[index] >> 16) as usize;
        if length == 0 || index + length > out.len() {
            break;
        }
        if let Some(id) = result_id_of((out[index] & 0xffff) as u16, &out[index..index + length]) {
            bound = bound.max(id + 1);
        }
        index += length;
    }
    out[3] = bound;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{compile, Request, Stage};

    const NO_SAMPLING: &str = "#version 450 core
layout(location = 0) in vec4 colour;
layout(location = 0) out vec4 out_colour;
void main() { out_colour = colour; }
";
    const SAMPLING: &str = "#version 450 core
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_colour;
layout(binding = 0) uniform sampler2D diffuse;
void main() { out_colour = texture(diffuse, uv); }
";

    fn opcodes(words: &[u32]) -> Vec<u16> {
        let instructions = decode(words).expect("a module this crate emitted is decodable");
        instructions.iter().map(|instruction| instruction.opcode).collect()
    }

    #[test]
    fn a_module_that_never_samples_ships_no_sampler_types() {
        let shader =
            compile(&Request::new(Stage::Fragment, NO_SAMPLING)).expect("the shader compiles");
        let codes = opcodes(&shader.spirv);
        assert!(
            !codes.contains(&TYPE_SAMPLER),
            "the emitted module must not declare OpTypeSampler: {codes:?}"
        );
        assert!(!codes.contains(&TYPE_IMAGE), "nor OpTypeImage");
        assert!(!codes.contains(&TYPE_SAMPLED_IMAGE), "nor OpTypeSampledImage");
        assert_eq!(shader.spirv[0], MAGIC, "the magic number survives");
        assert!(shader.spirv[3] >= 1, "the bound is still populated");
    }

    #[test]
    fn a_module_that_samples_is_left_alone() {
        let shader =
            compile(&Request::new(Stage::Fragment, SAMPLING)).expect("the shader compiles");
        assert!(
            opcodes(&shader.spirv).contains(&TYPE_SAMPLER),
            "a shader that samples must keep its sampler type"
        );
    }

    #[test]
    fn the_vertex_stage_never_carries_sampler_types_either() {
        let shader = compile(&Request::new(
            Stage::Vertex,
            "#version 120\nattribute vec4 position;\nvoid main() { gl_Position = position; }\n",
        ))
        .expect("the shader compiles");
        let codes = opcodes(&shader.spirv);
        assert!(!codes.contains(&TYPE_SAMPLER), "no OpTypeSampler: {codes:?}");
    }

    #[test]
    fn input_that_is_not_a_module_is_returned_unchanged() {
        assert_eq!(strip_unused_sampler_types(&[]), Vec::<u32>::new());
        let truncated = [MAGIC, 0x0001_0000, 0, 5, 0];
        assert_eq!(strip_unused_sampler_types(&truncated), truncated.to_vec());
        let bad_magic = [0xdead_beef, 0x0001_0000, 0, 5, 0];
        assert_eq!(strip_unused_sampler_types(&bad_magic), bad_magic.to_vec());
    }

    #[test]
    fn the_bound_shrinks_with_the_module() {
        let shader =
            compile(&Request::new(Stage::Fragment, NO_SAMPLING)).expect("the shader compiles");
        let highest = decode(&shader.spirv)
            .expect("decodable")
            .iter()
            .filter_map(|instruction| instruction.result_id)
            .max()
            .unwrap_or(0);
        assert!(
            shader.spirv[3] > highest,
            "bound {} must exceed the highest id {}",
            shader.spirv[3],
            highest
        );
    }
}
