//! Model Context Protocol (MCP) client integration for Legion.
//!
//! This crate provides the building blocks to connect to MCP servers and expose
//! their tools to the Legion agent runtime. Transports: stdio, http, sse, and ws.
//! Protocol revisions from `2024-11-05` through the stateless `2026-07-28`
//! core are negotiated per server (see [`version`]).
//!
//! # Modules
//!
//! - [`transport`] — transport and server configuration types.
//! - [`version`] — protocol version negotiation support.
//! - [`client`] — `McpClient` trait plus stdio/http/sse/ws implementations.
//! - [`manager`] — `McpManager`: loads configured servers, exposes adapted
//!   tools, and applies cross-cutting safety limits (auth cache, description
//!   truncation, concurrency limits, timeouts). Also surfaces per-server
//!   resources/prompts introspection and status snapshots.
//! - [`adapter`] — `McpToolAdapter`: wraps a single MCP tool so it can be
//!   presented to the Legion tool registry.

pub mod adapter;
pub mod client;
mod introspect;
pub mod manager;
pub mod metrics;
pub mod transport;
pub mod version;

pub use adapter::{MAX_MCP_DESCRIPTION_LENGTH, McpToolAdapter};
pub use client::{ClientOptions, McpClient, McpError, McpToolDesc, McpToolResult};
pub use manager::{McpManager, McpServerStatus};
pub use metrics::McpMetrics;
pub use transport::{McpServerConfig, McpTransport};
pub use version::{LATEST_VERSION, SUPPORTED_VERSIONS};
