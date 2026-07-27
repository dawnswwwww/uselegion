//! Session goals: durable per-session objectives managed via `/goal`.
//!
//! The goal model and store live in `legion_runtime::goal` (shared with the
//! agent loop's goal gate and the model-facing goal tools); this module
//! re-exports them so existing `crate::goal::...` paths keep working.

pub use legion_runtime::goal::*;
