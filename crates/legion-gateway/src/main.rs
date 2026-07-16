//! Standalone `legion-gateway` server binary.
//!
//! This binary is intentionally thin: it loads the config and delegates to
//! `legion_gateway::run_gateway`. The CLI can spawn this binary instead of
//! linking the Gateway server code into the `legion` executable.

use legion_protocol::ProtocolCompatibility;
use std::path::PathBuf;

#[derive(Debug, Default)]
struct Args {
    config: Option<PathBuf>,
    version: bool,
    json: bool,
}

fn parse_args() -> Args {
    let mut args = Args::default();
    let mut i = 1;
    while i < std::env::args().len() {
        let arg = std::env::args().nth(i).unwrap_or_default();
        match arg.as_str() {
            "--config" => {
                if i + 1 < std::env::args().len() {
                    args.config = std::env::args().nth(i + 1).map(PathBuf::from);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--version" => {
                args.version = true;
                i += 1;
            }
            "--json" => {
                args.json = true;
                i += 1;
            }
            _ => i += 1,
        }
    }
    args
}

fn print_version_json() {
    let compat = ProtocolCompatibility::current();
    let value = serde_json::json!({
        "productVersion": compat.product_version,
        "protocolRevision": compat.protocol_revision,
        "minPeerRevision": compat.min_peer_revision,
        "maxPeerRevision": compat.max_peer_revision,
        "releaseId": compat.release_id,
        "capabilities": compat.capabilities,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&value).unwrap_or_default()
    );
}

fn print_version_text() {
    let compat = ProtocolCompatibility::current();
    println!(
        "legion-gateway {} (protocol {})",
        compat.product_version, compat.protocol_revision
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args();

    if args.version {
        if args.json {
            print_version_json();
        } else {
            print_version_text();
        }
        return Ok(());
    }

    legion_gateway::run_gateway(args.config).await?;
    Ok(())
}
