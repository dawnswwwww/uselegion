//! Built-in tool implementations, split by domain. The public surface is
//! re-exported here so existing `crate::tools::X` references keep working.

mod exec;
mod fs;
mod memory;
mod orchestration;
mod web;
mod web_search;

pub use exec::ExecTool;
pub use fs::{ApplyPatchTool, EditTool, ReadTool, WriteTool, resolve_tool_path};
pub use memory::{MemoryGetTool, MemoryIndexTool, MemorySearchTool};
pub use orchestration::{
    AgentToAgentSendTool, RunCoordinatorTool, SpawnSubagentTool, SwarmSendTool, SwarmSpawnTool,
    SwarmStatusTool,
};
pub use web::WebFetchTool;
pub use web_search::WebSearchTool;
