//! Shared wire protocol DTOs for the Legion Gateway/CLI split.
//!
//! This crate owns the JSON shape of WebSocket frames and agent RPC requests
//! so that `legion-gateway` and `legion-cli` can evolve independently without
//! silently breaking the wire format.

pub mod agent;
pub mod compatibility;
pub mod harness;
pub mod manifest;
pub mod websocket;

pub use agent::{AgentAccepted, AgentParams, UserMessage};
pub use compatibility::{
    CURRENT_PROTOCOL_REVISION, DEFAULT_MAX_PEER_REVISION, DEFAULT_MIN_PEER_REVISION,
    ProtocolCompatibility,
};
pub use manifest::{
    Artifact, ProtocolRange, ReleaseEntry, ReleaseManifest, STABLE_RELEASE_PUBLIC_KEY,
};
pub use websocket::{AuthCreds, ConnectParams, Features, HelloPayload, WsFrame};
