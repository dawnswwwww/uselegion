//! Nodes protocol: companion devices that connect to the Gateway WebSocket with
//! `role: "node"` and expose a command surface via `node.invoke`.

pub mod manager;
pub mod policy;
pub mod registry;

pub use manager::{NodeInvokeError, NodeManager};
pub use policy::is_allowed;
pub use registry::{Node, NodeRegistry};
