//! Generated from `abi/gl_export.def` by `cargo run -p xtask -- gen-abi`.
//!
//! These files are checked in so a fresh `cargo check --workspace` works before anyone
//! runs the generator; `./build.sh abi` regenerates them and fails if they drift.

pub mod abi;
pub mod decls;
pub mod exports;
pub mod procs;
