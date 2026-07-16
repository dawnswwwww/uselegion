//! `legion-host` — transport-neutral runtime composition root.
//!
//! This crate contains the pieces that both the Gateway distribution layer
//! and the embedded CLI host need: runtime assembly, session storage, agent
//! routing, system plugin loading, and turn lifecycle helpers. It does **not**
//! contain WebSocket handlers, HTTP routes, channel lifecycle, or market/node
//! HTTP APIs — those stay in `legion-gateway`.

pub mod agent_messenger;
pub mod assembly;
pub mod error;
pub mod host;
pub mod image_tool;
pub mod metrics;
pub mod routing;
pub mod session;
pub mod session_tools;
pub mod system_plugins;
pub mod tts_tool;
pub mod turn;

pub use error::HostError;
pub use host::AgentHost;
pub use metrics::{Metric, MetricValue, MetricsRegistry};
pub use routing::{Router, resolve_session_key};
pub use session::SessionStore;
pub use session::repair::recover_orphaned_tool_results;
pub use turn::{SessionAccumulator, drive_run_stream, prepare_run, run_event_to_payload};
