//! Release pipeline: archive extraction, signed manifest fetching and
//! verification, artifact download, and installation.

use chrono::Utc;
use ed25519_dalek::{Verifier, VerifyingKey};
use flate2::read::GzDecoder;
use legion_protocol::{Artifact, ProtocolCompatibility, ReleaseEntry, ReleaseManifest};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use super::{
    CurrentPointer, GATEWAY_EXECUTABLE_NAME, GatewayManager, GatewayManagerError, InstallMetadata,
    Result, decode_signature,
};

impl GatewayManager {
    /// Extract an archive to a staging directory and return the executable path.
    ///
    /// Validates that the archive contains a `legion-gateway` executable.
    /// Accepts `tar.gz`, `tgz`, `tar`, and `zip` archives.
    fn extract_archive(
        &self,
        archive: &Path,
        staging: &Path,
        _expected_target: Option<&str>,
    ) -> Result<PathBuf> {
        let name = archive.file_name().and_then(|s| s.to_str()).unwrap_or("");
        std::fs::create_dir_all(staging)?;
        if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            self.extract_tar(archive, staging, true)?;
        } else if name.ends_with(".tar") {
            self.extract_tar(archive, staging, false)?;
        } else if name.ends_with(".zip") {
            self.extract_zip(archive, staging)?;
        } else {
            return Err(GatewayManagerError::Other(format!(
                "unsupported archive format: {}",
                archive.display()
            )));
        }

        // Search recursively for the executable.
        let candidate =
            Self::find_executable_in_dir(staging, GATEWAY_EXECUTABLE_NAME).ok_or_else(|| {
                GatewayManagerError::Other(format!(
                    "archive does not contain a {} executable",
                    GATEWAY_EXECUTABLE_NAME
                ))
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&candidate)?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            std::fs::set_permissions(&candidate, perms)?;
        }
        Ok(candidate)
    }

    fn extract_tar(&self, archive: &Path, staging: &Path, gz: bool) -> Result<()> {
        let file = File::open(archive)?;
        let tar: Box<dyn Read> = if gz {
            Box::new(GzDecoder::new(file))
        } else {
            Box::new(file)
        };
        let mut archive = tar::Archive::new(tar);
        archive.unpack(staging)?;
        Ok(())
    }

    fn extract_zip(&self, archive: &Path, staging: &Path) -> Result<()> {
        let file = File::open(archive)?;
        let mut archive = zip::ZipArchive::new(file)
            .map_err(|e| GatewayManagerError::Other(format!("failed to open zip archive: {e}")))?;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| {
                GatewayManagerError::Other(format!("failed to read zip entry: {e}"))
            })?;
            let outpath = staging.join(entry.mangled_name());
            if entry.is_dir() {
                std::fs::create_dir_all(&outpath)?;
            } else {
                if let Some(parent) = outpath.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut outfile = File::create(&outpath)?;
                std::io::copy(&mut entry, &mut outfile)?;
            }
        }
        Ok(())
    }

    /// Move a staged installation into the final version directory and update
    /// the current pointer.
    fn commit_install(
        &self,
        staging_executable: PathBuf,
        version: &str,
        target: &str,
        release_id: &str,
        source: &str,
    ) -> Result<PathBuf> {
        let dest_dir = self.version_dir(version, target);
        if dest_dir.exists() {
            std::fs::remove_dir_all(&dest_dir)?;
        }
        let staging_dir = staging_executable
            .parent()
            .ok_or_else(|| {
                GatewayManagerError::Other("staging executable has no parent".to_string())
            })?
            .to_path_buf();
        std::fs::create_dir_all(dest_dir.parent().unwrap())?;
        std::fs::rename(&staging_dir, &dest_dir)?;

        let executable = dest_dir.join(GATEWAY_EXECUTABLE_NAME);
        let metadata = InstallMetadata {
            version: version.to_string(),
            target: target.to_string(),
            release_id: release_id.to_string(),
            installed_at: Utc::now(),
            source: source.to_string(),
        };
        let meta_path = dest_dir.join("install.json");
        Self::atomic_write(&meta_path, &serde_json::to_vec_pretty(&metadata)?)?;

        let pointer = CurrentPointer::switched(
            version,
            target,
            release_id,
            executable.clone(),
            metadata.installed_at,
            None,
        );
        self.set_current_pointer(&pointer)?;
        Ok(executable)
    }

    /// Install a Gateway from a local archive.
    ///
    /// `version` is required. `target` defaults to the current target triple.
    pub fn install_from_archive(
        &self,
        archive: &Path,
        version: &str,
        target: Option<&str>,
    ) -> Result<PathBuf> {
        let _lock = self.acquire_install_lock()?;
        let target = target
            .map(String::from)
            .unwrap_or_else(Self::current_target);
        let staging = self.downloads_dir.join(format!(
            "staging.{}.{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let executable = self.extract_archive(archive, &staging, Some(&target))?;
        let info = self.probe_version(&executable)?;
        if info.protocol.product_version != version {
            return Err(GatewayManagerError::ArtifactIntegrityFailed(format!(
                "archive claims version {version} but binary reports {}",
                info.protocol.product_version
            )));
        }
        self.commit_install(
            executable,
            version,
            &target,
            &info.protocol.release_id,
            &format!("archive:{}", archive.display()),
        )
    }

    /// Verify an Ed25519 signature over the manifest bytes.
    ///
    /// `signature_bytes` may be base64 or hex encoded.
    pub fn verify_manifest_signature(
        &self,
        manifest_bytes: &[u8],
        signature_bytes: &[u8],
    ) -> Result<()> {
        let sig = decode_signature(signature_bytes)?;
        let public_key = VerifyingKey::from_bytes(&self.release_public_key)?;
        public_key
            .verify(manifest_bytes, &sig)
            .map_err(GatewayManagerError::Signature)
    }

    /// Fetch a manifest and its signature and verify the signature.
    pub async fn fetch_verified_manifest(&self, url: &str) -> Result<ReleaseManifest> {
        if !url.starts_with("https://") {
            return Err(GatewayManagerError::Other(
                "manifest URL must use HTTPS".to_string(),
            ));
        }
        let manifest_url = url.to_string();
        let sig_url = format!("{}.sig", manifest_url);

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| GatewayManagerError::OfflineOrProxy(e.to_string()))?;

        let manifest_bytes = client
            .get(&manifest_url)
            .send()
            .await
            .map_err(|e| GatewayManagerError::OfflineOrProxy(e.to_string()))?
            .bytes()
            .await
            .map_err(|e| GatewayManagerError::OfflineOrProxy(e.to_string()))?
            .to_vec();

        let sig_bytes = client
            .get(&sig_url)
            .send()
            .await
            .map_err(|e| GatewayManagerError::OfflineOrProxy(e.to_string()))?
            .bytes()
            .await
            .map_err(|e| GatewayManagerError::OfflineOrProxy(e.to_string()))?
            .to_vec();

        self.verify_manifest_signature(&manifest_bytes, &sig_bytes)?;
        let manifest: ReleaseManifest = serde_json::from_slice(&manifest_bytes)?;
        Ok(manifest)
    }

    /// Select an artifact from the manifest matching the CLI version and target.
    pub fn select_artifact<'a>(
        &self,
        manifest: &'a ReleaseManifest,
        cli_compat: &ProtocolCompatibility,
        target: &str,
        version: Option<&str>,
    ) -> Result<(&'a ReleaseEntry, &'a Artifact)> {
        let cli_version = semver::Version::parse(&cli_compat.product_version)
            .map_err(|e| GatewayManagerError::Other(format!("invalid CLI version: {e}")))?;

        let parse_range = |s: &str| -> Option<semver::VersionReq> {
            semver::VersionReq::parse(s).ok().or_else(|| {
                let normalized = s.split_whitespace().collect::<Vec<_>>().join(",");
                semver::VersionReq::parse(&normalized).ok()
            })
        };

        let mut entries: Vec<&ReleaseEntry> = manifest
            .releases
            .iter()
            .filter(|r| {
                if let Some(req) = parse_range(&r.cli_version_range) {
                    if !req.matches(&cli_version) {
                        return false;
                    }
                } else {
                    return false;
                }
                if r.protocol.min_peer_revision > cli_compat.protocol_revision
                    || r.protocol.max_peer_revision < cli_compat.protocol_revision
                {
                    return false;
                }
                if let Some(v) = version {
                    return r.gateway_version == v;
                }
                true
            })
            .collect();

        entries.sort_by(|a, b| {
            let av = semver::Version::parse(&a.gateway_version).ok();
            let bv = semver::Version::parse(&b.gateway_version).ok();
            match (av, bv) {
                (Some(av), Some(bv)) => bv.cmp(&av),
                _ => b.gateway_version.cmp(&a.gateway_version),
            }
        });

        let entry = entries.into_iter().next().ok_or_else(|| {
            if let Some(v) = version {
                GatewayManagerError::PlatformUnsupported(format!(
                    "no release for version {v} compatible with CLI {} and target {target}",
                    cli_compat.product_version
                ))
            } else {
                GatewayManagerError::PlatformUnsupported(format!(
                    "no release compatible with CLI {} and target {target}",
                    cli_compat.product_version
                ))
            }
        })?;

        let artifact = entry.artifact_for(target).ok_or_else(|| {
            GatewayManagerError::PlatformUnsupported(format!(
                "release {} has no artifact for target {}",
                entry.release_id, target
            ))
        })?;

        Ok((entry, artifact))
    }

    /// Download an artifact, verify size and SHA-256, and return the path.
    async fn download_artifact(&self, artifact: &Artifact) -> Result<PathBuf> {
        if !artifact.url.starts_with("https://") {
            return Err(GatewayManagerError::Other(
                "artifact URL must use HTTPS".to_string(),
            ));
        }
        let partial = self
            .downloads_dir
            .join(format!("{}.partial", uuid::Uuid::new_v4()));
        self.ensure_dirs()?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| GatewayManagerError::OfflineOrProxy(e.to_string()))?;
        let mut response = client
            .get(&artifact.url)
            .send()
            .await
            .map_err(|e| GatewayManagerError::OfflineOrProxy(e.to_string()))?;

        let mut file = tokio::fs::File::create(&partial).await?;
        let mut hasher = Sha256::new();
        let mut downloaded: u64 = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| GatewayManagerError::OfflineOrProxy(e.to_string()))?
        {
            downloaded += chunk.len() as u64;
            if downloaded
                > artifact
                    .size_bytes
                    .saturating_mul(10)
                    .max(artifact.size_bytes + 100_000_000)
            {
                drop(file);
                let _ = std::fs::remove_file(&partial);
                return Err(GatewayManagerError::ArtifactIntegrityFailed(
                    "download exceeded expected size".to_string(),
                ));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);

        if downloaded != artifact.size_bytes {
            let _ = std::fs::remove_file(&partial);
            return Err(GatewayManagerError::ArtifactIntegrityFailed(format!(
                "size mismatch: expected {} bytes, got {}",
                artifact.size_bytes, downloaded
            )));
        }

        let actual_hash = hex::encode(hasher.finalize());
        if actual_hash != artifact.sha256 {
            let _ = std::fs::remove_file(&partial);
            return Err(GatewayManagerError::ArtifactIntegrityFailed(format!(
                "SHA-256 mismatch: expected {}, got {}",
                artifact.sha256, actual_hash
            )));
        }

        Ok(partial)
    }

    /// Install a Gateway from a signed release manifest.
    ///
    /// If `version` is `None`, the latest compatible release is selected.
    /// `channel` is informational (used for source reporting).
    pub async fn install_from_manifest(
        &self,
        url: &str,
        version: Option<&str>,
        channel: &str,
        auto_confirm: bool,
    ) -> Result<PathBuf> {
        let manifest = self.fetch_verified_manifest(url).await?;
        let cli_compat = Self::cli_compatibility();
        let target = Self::current_target();
        let (entry, artifact) = self.select_artifact(&manifest, &cli_compat, &target, version)?;

        if !auto_confirm {
            let stdin = std::io::stdin();
            if atty::is(atty::Stream::Stdin) {
                println!(
                    "About to install legion-gateway {} ({}) for {} from {}",
                    entry.gateway_version, entry.release_id, target, artifact.url
                );
                println!("Size: {} bytes", artifact.size_bytes);
                println!("Signature: verified");
                print!("Proceed? [y/N] ");
                std::io::Write::flush(&mut std::io::stdout())?;
                let mut input = String::new();
                stdin.read_line(&mut input)?;
                if !input.trim().eq_ignore_ascii_case("y") {
                    return Err(GatewayManagerError::Cancelled);
                }
            } else {
                return Err(GatewayManagerError::Other(
                    "non-interactive install requires --install or pre-installation".to_string(),
                ));
            }
        }

        let archive = self.download_artifact(artifact).await?;
        let _lock = self.acquire_install_lock()?;
        let staging = self.downloads_dir.join(format!(
            "staging.{}.{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let executable = self.extract_archive(&archive, &staging, Some(&target))?;
        let info = self.probe_version(&executable)?;
        if info.protocol.product_version != entry.gateway_version {
            return Err(GatewayManagerError::ArtifactIntegrityFailed(format!(
                "manifest claims version {} but binary reports {}",
                entry.gateway_version, info.protocol.product_version
            )));
        }
        let result = self.commit_install(
            executable,
            &entry.gateway_version,
            &target,
            &entry.release_id,
            &format!("manifest:{channel}:{}", url),
        )?;
        let _ = std::fs::remove_file(&archive);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway_manager::tests::{test_manager, test_manager_with_key};
    use ed25519_dalek::{Signer, SigningKey};
    use legion_protocol::ProtocolRange;
    use std::fs::File;

    /// A fresh Ed25519 keypair for the manifest-signing tests. Each test run
    /// uses a deterministic key so the manager and signer agree, without ever
    /// touching the production `STABLE_RELEASE_PUBLIC_KEY`.
    fn test_keypair() -> (SigningKey, [u8; 32]) {
        let seed: [u8; 32] = *b"legion-test-signing-keypair-seed"; // exactly 32 bytes
        let signing = SigningKey::from_bytes(&seed);
        let verifying = signing.verifying_key().to_bytes();
        (signing, verifying)
    }

    fn sign_manifest(bytes: &[u8]) -> Vec<u8> {
        let (signing_key, _) = test_keypair();
        let sig = signing_key.sign(bytes);
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, sig.to_bytes())
            .into_bytes()
    }

    fn manager_with_test_key() -> GatewayManager {
        let (_, public_key) = test_keypair();
        let (mgr, _tmp) = test_manager_with_key(public_key);
        mgr
    }

    const TEST_MANIFEST: &[u8] = br#"{"formatVersion":1,"channel":"stable","publishedAt":"2026-07-14T00:00:00Z","releases":[]}"#;

    #[test]
    fn verify_manifest_signature_accepts_base64() {
        let mgr = manager_with_test_key();
        let sig = sign_manifest(TEST_MANIFEST);
        mgr.verify_manifest_signature(TEST_MANIFEST, &sig).unwrap();
    }

    #[test]
    fn verify_manifest_signature_accepts_hex() {
        let mgr = manager_with_test_key();
        let sig_b64 = sign_manifest(TEST_MANIFEST);
        let raw =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &sig_b64).unwrap();
        let sig_hex = hex::encode(raw).into_bytes();
        mgr.verify_manifest_signature(TEST_MANIFEST, &sig_hex)
            .unwrap();
    }

    #[test]
    fn verify_manifest_signature_rejects_tampered() {
        let mgr = manager_with_test_key();
        let sig = sign_manifest(TEST_MANIFEST);
        let mut tampered = TEST_MANIFEST.to_vec();
        tampered[20] ^= 1;
        assert!(mgr.verify_manifest_signature(&tampered, &sig).is_err());
    }

    #[test]
    fn verify_manifest_signature_rejects_wrong_key() {
        let mgr = manager_with_test_key();
        // A well-formed Ed25519 signature from a key that is not the release key.
        let other_key = SigningKey::from_bytes(&[42u8; 32]);
        let sig = other_key.sign(TEST_MANIFEST);
        let sig_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, sig.to_bytes())
                .into_bytes();
        let err = mgr
            .verify_manifest_signature(TEST_MANIFEST, &sig_b64)
            .unwrap_err();
        assert!(matches!(err, GatewayManagerError::Signature(_)));
    }

    #[test]
    fn verify_manifest_signature_rejects_malformed() {
        let (mgr, _tmp) = test_manager();
        let err = mgr
            .verify_manifest_signature(TEST_MANIFEST, b"not-a-signature")
            .unwrap_err();
        assert!(matches!(err, GatewayManagerError::ManifestUntrusted(_)));
    }

    #[test]
    fn select_artifact_matches_cli_version_and_target() {
        let (mgr, _tmp) = test_manager();
        let manifest = ReleaseManifest {
            format_version: 1,
            channel: "stable".to_string(),
            published_at: "2026-07-14T00:00:00Z".to_string(),
            releases: vec![ReleaseEntry {
                release_id: "r1".to_string(),
                cli_version_range: ">=0.1.0 <0.2.0".to_string(),
                gateway_version: "0.1.5".to_string(),
                protocol: ProtocolRange {
                    min_peer_revision: 1,
                    max_peer_revision: 1,
                },
                artifacts: vec![
                    Artifact {
                        target: "aarch64-apple-darwin".to_string(),
                        url: "https://x/a.tar.gz".to_string(),
                        sha256: "a".to_string(),
                        size_bytes: 1,
                    },
                    Artifact {
                        target: "x86_64-unknown-linux-gnu".to_string(),
                        url: "https://x/l.tar.gz".to_string(),
                        sha256: "b".to_string(),
                        size_bytes: 1,
                    },
                ],
            }],
        };
        let cli = ProtocolCompatibility::with_release("0.1.0", "r0");
        let (entry, artifact) = mgr
            .select_artifact(&manifest, &cli, "aarch64-apple-darwin", None)
            .unwrap();
        assert_eq!(entry.gateway_version, "0.1.5");
        assert_eq!(artifact.target, "aarch64-apple-darwin");
    }

    #[test]
    fn extract_tar_gz_archive() {
        let (mgr, tmp) = test_manager();
        let archive = tmp.path().join("gw.tar.gz");
        let executable_bytes: Vec<u8> = if cfg!(windows) {
            b"fake windows exe".to_vec()
        } else {
            b"#!/bin/sh\necho '{\"productVersion\":\"0.2.0\",\"protocolRevision\":1,\"minPeerRevision\":1,\"maxPeerRevision\":1,\"releaseId\":\"r1\",\"capabilities\":[]}'\n".to_vec()
        };

        {
            let file = File::create(&archive).unwrap();
            let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut tar = tar::Builder::new(enc);
            let mut header = tar::Header::new_gnu();
            header.set_path(GATEWAY_EXECUTABLE_NAME).unwrap();
            header.set_size(executable_bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append(&header, executable_bytes.as_slice()).unwrap();
            tar.finish().unwrap();
        }

        let staging = tmp.path().join("staging");
        let exe = mgr.extract_archive(&archive, &staging, None).unwrap();
        assert!(exe.exists());
        let contents = std::fs::read_to_string(&exe).unwrap();
        assert!(contents.contains("productVersion"));
    }
}
