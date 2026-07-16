//! Tool taxonomy and canonical envelope (Grok CLI gap Phase 5).
//!
//! This module defines a shared classification for every tool the runtime can
//! invoke: a [`ToolKind`] describing the operation semantically, a
//! [`ToolNamespace`] identifying where the tool originates, and a
//! [`CanonicalToolMeta`] envelope that can be attached to telemetry/events
//! without changing the wire-format tool name.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

/// Semantic category of a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Write,
    Edit,
    Delete,
    ListDir,
    Search,
    Execute,
    Plan,
    WebSearch,
    WebFetch,
    BackgroundTaskAction,
    WaitTasksAction,
    KillTaskAction,
    Skill,
    MemorySearch,
    MemoryGet,
    Task,
    AskUser,
    ImageGen,
    VideoGen,
    Lsp,
    Monitor,
    Other,
}

/// Namespace identifying the origin of a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolNamespace {
    /// Built-in Legion core tool.
    Legion,
    /// Tool exposed by an MCP server.
    Mcp { server: String },
    /// Tool provided by a Legion plugin.
    Plugin { plugin: String },
}

/// Canonical metadata envelope for a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalToolMeta {
    pub version: u32,
    pub name: String,
    pub kind: ToolKind,
    pub namespace: ToolNamespace,
    pub label: Cow<'static, str>,
    pub read_only: bool,
    pub input: Option<serde_json::Value>,
}

impl CanonicalToolMeta {
    pub const VERSION: u32 = 1;

    pub fn new(
        name: impl Into<String>,
        kind: ToolKind,
        namespace: ToolNamespace,
        label: impl Into<Cow<'static, str>>,
        read_only: bool,
        input: Option<serde_json::Value>,
    ) -> Self {
        Self {
            version: Self::VERSION,
            name: name.into(),
            kind,
            namespace,
            label: label.into(),
            read_only,
            input,
        }
    }
}
