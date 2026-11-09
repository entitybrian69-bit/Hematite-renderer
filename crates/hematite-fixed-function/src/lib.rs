//! CPU-side building blocks for legacy GL (not a GL implementation).
//!
//! Column-major matrix math and bounded stacks follow desktop GL post-multiplication.
//! Immediate-mode assembly snapshots current attributes and expands legacy topologies
//! into indexed point/line/triangle lists. No GPU, FFI or shader generation is included.
//! See `docs/KNOWN_LIMITATIONS.md` for the remaining fixed-function work.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod immediate;
pub mod matrix;

/// Errors to be mapped to GL errors by the future frontend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Unknown primitive enum.
    InvalidEnum,
    /// Invalid dimensions, depth or capacity.
    InvalidValue,
    /// A begin/end operation is illegal in the current state.
    InvalidOperation,
    /// Push would exceed the advertised stack depth.
    StackOverflow,
    /// Pop would remove the final stack element.
    StackUnderflow,
    /// Configured vertex/index capacity exhausted or allocation failed.
    Capacity,
}
