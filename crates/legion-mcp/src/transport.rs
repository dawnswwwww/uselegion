//! Transport and server configuration types for MCP.
//!
//! The actual schema lives in `legion-core` so it can be referenced from the
//! top-level `Config` without introducing a dependency on this crate. This
//! module re-exports those types for convenience.

pub use legion_core::config::{McpServerConfig, McpTransport};
