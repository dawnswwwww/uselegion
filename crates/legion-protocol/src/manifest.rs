//! Signed release manifest types shared between CLI release tooling and tests.

use serde::{Deserialize, Serialize};

/// Root Ed25519 public key for the stable release channel.
///
/// This is the well-known test vector public key whose seed is
/// `9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60`.
/// In production the release pipeline replaces this with the real stable
/// channel public key.
pub const STABLE_RELEASE_PUBLIC_KEY: [u8; 32] = [
    0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07, 0x3a,
    0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07, 0x51, 0x1a,
];

/// Signed release manifest consumed by the CLI installer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseManifest {
    pub format_version: u32,
    pub channel: String,
    pub published_at: String,
    pub releases: Vec<ReleaseEntry>,
}

/// One release entry in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseEntry {
    pub release_id: String,
    /// Semantic version range of the CLI that can install this Gateway.
    pub cli_version_range: String,
    pub gateway_version: String,
    pub protocol: ProtocolRange,
    pub artifacts: Vec<Artifact>,
}

/// Protocol compatibility range for a release.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolRange {
    pub min_peer_revision: u32,
    pub max_peer_revision: u32,
}

/// Downloadable artifact for a specific target triple.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub target: String,
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
}

impl ReleaseEntry {
    /// Find the artifact for a target triple, if present.
    pub fn artifact_for(&self, target: &str) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.target == target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trip() {
        let manifest = ReleaseManifest {
            format_version: 1,
            channel: "stable".to_string(),
            published_at: "2026-07-14T00:00:00Z".to_string(),
            releases: vec![ReleaseEntry {
                release_id: "2026.07.14-0.2.0".to_string(),
                cli_version_range: ">=0.2.0 <0.3.0".to_string(),
                gateway_version: "0.2.0".to_string(),
                protocol: ProtocolRange {
                    min_peer_revision: 1,
                    max_peer_revision: 1,
                },
                artifacts: vec![Artifact {
                    target: "aarch64-apple-darwin".to_string(),
                    url:
                        "https://releases.example/legion-gateway-0.2.0-aarch64-apple-darwin.tar.gz"
                            .to_string(),
                    sha256: "abcd".to_string(),
                    size_bytes: 12345678,
                }],
            }],
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: ReleaseManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.channel, "stable");
        assert_eq!(parsed.releases[0].gateway_version, "0.2.0");
    }
}
