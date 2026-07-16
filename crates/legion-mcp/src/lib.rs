//! Model Context Protocol (MCP) client integration for Legion.
//!
//! This crate provides the building blocks to connect to MCP servers and expose
//! their tools to the Legion agent runtime. Transports: stdio, http, sse, and ws.
//!
//! # Modules
//!
//! - [`transport`] — transport and server configuration types.
//! - [`client`] — `McpClient` trait plus stdio/http/sse/ws implementations.
//! - [`manager`] — `McpManager`: loads configured servers, exposes adapted
//!   tools, and applies cross-cutting safety limits (auth cache, description
//!   truncation, concurrency limits, timeouts).
//! - [`adapter`] — `McpToolAdapter`: wraps a single MCP tool so it can be
//!   presented to the Legion tool registry.

pub mod adapter;
pub mod client;
pub mod manager;
pub mod metrics;
pub mod transport;

pub use adapter::{MAX_MCP_DESCRIPTION_LENGTH, McpToolAdapter};
pub use client::{McpClient, McpError, McpToolDesc, McpToolResult};
pub use manager::McpManager;
pub use metrics::McpMetrics;
pub use transport::{McpServerConfig, McpTransport};
