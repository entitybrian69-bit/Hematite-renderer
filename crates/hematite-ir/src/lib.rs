//! Backend-neutral representation of GL work.
//!
//! This crate is the only thing the GL frontend and the backends share. It contains no
//! Vulkan, no GLES and no FFI types: the point is that a frontend can record a frame
//! without knowing what will consume it.
//!
//! Two ideas do all the work:
//!
//! * **Commands are plain data.** The render thread pushes [`Command`] values into a
//!   per-context buffer; nothing is executed until a submission drains it. This is what
//!   keeps translation and pipeline work off the render thread.
//! * **State is compared, not applied.** [`StateBlock`] is a struct of comparable fields,
//!   so the frontend can answer "did anything actually change?" with one comparison
//!   instead of a chain of `if`s, and the backend can diff against what it last set.
//!
//! Most values are fixed-size data. `Command::SetState` owns a boxed snapshot, so creating
//! or cloning that command can allocate; zero-allocation submission is not proven here.

#![deny(unsafe_op_in_unsafe_fn)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod command;
pub mod state;

pub use command::{Command, DrawDesc, IndexRef, PipelineId, ResourceId, Topology};
pub use state::{BlendFactor, BlendOp, CmpFunc, Face, StateBlock, Viewport};
