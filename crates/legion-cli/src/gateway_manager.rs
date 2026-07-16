//! Local Gateway binary discovery, installation, and lifecycle management.
//!
//! This module implements the CLI side of the CLI/Gateway independent
//! distribution design. It manages the `~/.legion/gateways/` directory,
//! downloads and verifies signed release manifests, and atomically switches
//! between installed Gateway versions.

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use flate2::read::GzDecoder;
use fs2::FileExt;
use legion_core::config::Config;
use legion_protocol::{
    Artifact, ProtocolCompatibility, ReleaseEntry, ReleaseManifest, STABLE_RELEASE_PUBLIC_KEY,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

const GATEWAY_EXECUTABLE_NAME: &str = if cfg!(windows) {
    "legion-gateway.exe"
} else {
    "legion-gateway"
};

/// Errors returned by the gateway manager.
#[derive(Debug, Error)]
pub enum GatewayManagerError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("hex decode error: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("ed25519 verification error: {0}")]
    Signature(#[from] ed25519_dalek::SignatureError),
    #[error("manifest untrusted: {0}")]
    ManifestUntrusted(String),
    #[error("artifact integrity failed: {0}")]
    ArtifactIntegrityFailed(String),
    #[error("platform unsupported: {0}")]
    PlatformUnsupported(String),
    #[error("offline or network error: {0}")]
    OfflineOrProxy(String),
    #[error("protocol incompatible: {0}")]
    ProtocolIncompatible(String),
    #[error("data migration blocked: {0}")]
    DataMigrationBlocked(String),
    #[error("daemon busy: {0}")]
    DaemonBusy(String),
    #[error("install cancelled by user")]
    Cancelled,
    #[error("gateway not installed; run `legion gateway install` first")]
    NotInstalled,
    #[error("{0}")]
    Other(String),
}

/// Short alias used inside this module.
type Result<T> = std::result::Result<T, GatewayManagerError>;

/// Information returned by probing a Gateway binary.
#[derive(Debug, Clone)]
pub struct GatewayVersionInfo {
    pub protocol: ProtocolCompatibility,
    pub executable: PathBuf,
}

/// A locally installed Gateway version.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledVersion {
    pub version: String,
    pub target: String,
    pub release_id: String,
    pub installed_at: DateTime<Utc>,
    pub source: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(skip)]
    pub executable: PathBuf,
}

/// Pointer to the current Gateway version.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentPointer {
    pub version: String,
    pub target: String,
    pub release_id: String,
    pub installed_at: DateTime<Utc>,
    pub last_ok_at: Option<DateTime<Utc>>,
    pub executable: PathBuf,
    pub pid: Option<u32>,
    pub started_at: Option<DateTime<Utc>>,
    pub endpoint: Option<String>,
    pub config_path_hash: Option<String>,
}

/// Per-install metadata written next to the executable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallMetadata {
    version: String,
    target: String,
    release_id: String,
    installed_at: DateTime<Utc>,
    source: String,
}

/// Information about a running Gateway process.
#[derive(Debug, Clone)]
pub struct RunningGateway {
    pub pid: Option<u32>,
    pub info: GatewayVersionInfo,
    pub endpoint: String,
    pub config_path_hash: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
}

/// A single migration ledger entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MigrationEntry {
    ts: DateTime<Utc>,
    from_version: String,
    to_version: String,
    from_schema: u32,
    to_schema: u32,
    reversible: bool,
    backup_path: Option<PathBuf>,
}

/// Manages local Gateway binaries.
#[derive(Debug, Clone)]
pub struct GatewayManager {
    home: PathBuf,
    gateways_dir: PathBuf,
    current_file: PathBuf,
    downloads_dir: PathBuf,
    locks_dir: PathBuf,
    install_lock: PathBuf,
    #[allow(dead_code)]
    daemon_lock: PathBuf,
    migration_file: PathBuf,
}

impl GatewayManager {
    /// Create a manager rooted at the given home directory.
    pub fn new(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self {
            gateways_dir: home.join("gateways"),
            current_file: home.join("gateway-current.json"),
            downloads_dir: home.join("downloads"),
            locks_dir: home.join("locks"),
            install_lock: home.join("locks").join("gateway-install.lock"),
            daemon_lock: home.join("locks").join("gateway-daemon.lock"),
            migration_file: home.join("migration.jsonl"),
            home,
        }
    }

    /// Create a manager using `~/.legion` as the home directory.
    pub fn default_manager() -> Result<Self> {
        match dirs::home_dir() {
            Some(home) => Ok(Self::new(home.join(".legion"))),
            None => Err(GatewayManagerError::Other(
                "unable to determine home directory".to_string(),
            )),
        }
    }

    /// Ensure base directories exist.
    fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.gateways_dir)?;
        std::fs::create_dir_all(&self.downloads_dir)?;
        std::fs::create_dir_all(&self.locks_dir)?;
        Ok(())
    }

    /// Return the path to the current pointer file.
    pub fn current_file(&self) -> &Path {
        &self.current_file
    }

    /// Return the installed Gateway directory for a version/target pair.
    fn version_dir(&self, version: &str, target: &str) -> PathBuf {
        self.gateways_dir.join(version).join(target)
    }

    /// Recursively search a directory for a file with the given name.
    fn find_executable_in_dir(dir: &Path, name: &str) -> Option<PathBuf> {
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = Self::find_executable_in_dir(&path, name) {
                    return Some(found);
                }
            } else if path.file_name().and_then(|s| s.to_str()) == Some(name) {
                return Some(path);
            }
        }
        None
    }

    /// Return the path to the executable for a version/target pair.
    fn executable_path(&self, version: &str, target: &str) -> PathBuf {
        self.version_dir(version, target)
            .join(GATEWAY_EXECUTABLE_NAME)
    }

    /// Compute a stable hash of a config path string.
    pub(crate) fn config_path_hash(path: &Path) -> String {
        let mut hasher = Sha256::new();
        hasher.update(path.to_string_lossy().as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Acquire the install lock, returning the locked file.
    fn acquire_install_lock(&self) -> Result<File> {
        self.ensure_dirs()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.install_lock)?;
        file.lock_exclusive()?;
        Ok(file)
    }

    /// Write `bytes` atomically to `path` using a temp file + rename.
    fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.flush()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Read the current pointer if it exists.
    pub fn current_pointer(&self) -> Result<Option<CurrentPointer>> {
        if !self.current_file.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&self.current_file)?;
        Ok(Some(serde_json::from_str(&text)?))
    }

    /// Write the current pointer atomically.
    pub(crate) fn set_current_pointer(&self, pointer: &CurrentPointer) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(pointer)?;
        Self::atomic_write(&self.current_file, &bytes)
    }

    /// Build the CLI's own protocol compatibility.
    pub fn cli_compatibility() -> ProtocolCompatibility {
        ProtocolCompatibility::current()
    }

    /// Resolve the current platform's Rust target triple.
    ///
    /// Uses the compile-time `LEGION_TARGET` environment variable set by the
    /// build script, which matches the binary the user is running.
    pub fn current_target() -> String {
        env!("LEGION_TARGET").to_string()
    }

    /// Locate a `legion-gateway` executable using the discovery rules.
    ///
    /// Resolution order:
    /// 1. `LEGION_GATEWAY_BIN` environment variable.
    /// 2. The current pointer in `~/.legion/gateway-current.json`.
    /// 3. `legion-gateway` on `PATH`.
    /// 4. Next to the current executable.
    /// 5. Workspace target directories relative to the compile-time manifest.
    pub fn find_gateway_binary(&self) -> Result<PathBuf> {
        // 1. Explicit env override.
        if let Ok(path) = std::env::var("LEGION_GATEWAY_BIN") {
            let path = PathBuf::from(path);
            if path.exists() {
                return Ok(path);
            }
        }

        // 2. Installed current pointer.
        if let Some(pointer) = self.current_pointer()? {
            if pointer.executable.exists() {
                return Ok(pointer.executable);
            }
        }

        // 3. PATH.
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                let candidate = dir.join(GATEWAY_EXECUTABLE_NAME);
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }

        // 4. Next to the current executable.
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let candidate = dir.join(GATEWAY_EXECUTABLE_NAME);
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }

        // 5. Workspace target directories.
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        for profile in ["debug", "release"] {
            let candidate = manifest_dir
                .join("..")
                .join("..")
                .join("target")
                .join(profile)
                .join(GATEWAY_EXECUTABLE_NAME);
            if candidate.exists() {
                return Ok(candidate);
            }
        }

        Err(GatewayManagerError::NotInstalled)
    }

    /// Probe a Gateway binary by running `<binary> --version --json`.
    pub fn probe_version(&self, path: &Path) -> Result<GatewayVersionInfo> {
        let output = Command::new(path)
            .arg("--version")
            .arg("--json")
            .stderr(Stdio::null())
            .output()
            .map_err(|e| {
                GatewayManagerError::Other(format!("failed to run {}: {e}", path.display()))
            })?;
        if !output.status.success() {
            return Err(GatewayManagerError::Other(format!(
                "{} --version --json exited with status {:?}",
                path.display(),
                output.status
            )));
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let protocol = serde_json::from_value::<ProtocolCompatibility>(
            value.get("protocol").cloned().unwrap_or(value),
        )?;
        Ok(GatewayVersionInfo {
            protocol,
            executable: path.to_path_buf(),
        })
    }

    /// Verify that a Gateway binary is compatible with this CLI.
    pub fn ensure_compatible(&self, info: &GatewayVersionInfo) -> Result<()> {
        let cli = Self::cli_compatibility();
        if let Some(err) = cli.compatibility_error(&info.protocol) {
            return Err(GatewayManagerError::ProtocolIncompatible(err));
        }
        Ok(())
    }

    /// List all installed versions.
    pub fn list_versions(&self) -> Result<Vec<InstalledVersion>> {
        let mut versions = Vec::new();
        if !self.gateways_dir.exists() {
            return Ok(versions);
        }
        for version_entry in std::fs::read_dir(&self.gateways_dir)? {
            let version_entry = version_entry?;
            let version_path = version_entry.path();
            if !version_path.is_dir() {
                continue;
            }
            let version = version_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            for target_entry in std::fs::read_dir(&version_path)? {
                let target_entry = target_entry?;
                let target_path = target_entry.path();
                if !target_path.is_dir() {
                    continue;
                }
                let target = target_entry.file_name().to_string_lossy().to_string();
                let meta_path = target_path.join("install.json");
                if let Ok(meta) = std::fs::read_to_string(&meta_path) {
                    if let Ok(meta) = serde_json::from_str::<InstallMetadata>(&meta) {
                        let executable = target_path.join(GATEWAY_EXECUTABLE_NAME);
                        versions.push(InstalledVersion {
                            version: version.clone(),
                            target,
                            release_id: meta.release_id,
                            installed_at: meta.installed_at,
                            source: meta.source,
                            pinned: false,
                            executable,
                        });
                    }
                }
            }
        }
        versions.sort_by_key(|b| std::cmp::Reverse(b.installed_at));
        Ok(versions)
    }

    /// Return the currently selected installed version, if any.
    pub fn current_version(&self) -> Result<Option<InstalledVersion>> {
        let pointer = match self.current_pointer()? {
            Some(p) => p,
            None => return Ok(None),
        };
        let executable = self.executable_path(&pointer.version, &pointer.target);
        Ok(Some(InstalledVersion {
            version: pointer.version,
            target: pointer.target,
            release_id: pointer.release_id,
            installed_at: pointer.installed_at,
            source: "current-pointer".to_string(),
            pinned: false,
            executable,
        }))
    }

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

        let pointer = CurrentPointer {
            version: version.to_string(),
            target: target.to_string(),
            release_id: release_id.to_string(),
            installed_at: metadata.installed_at,
            last_ok_at: None,
            executable: executable.clone(),
            pid: None,
            started_at: None,
            endpoint: None,
            config_path_hash: None,
        };
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

    /// Fetch and parse a release manifest over HTTPS.
    ///
    /// Rejects non-HTTPS URLs.
    pub async fn fetch_manifest(&self, url: &str) -> Result<ReleaseManifest> {
        if !url.starts_with("https://") {
            return Err(GatewayManagerError::Other(
                "manifest URL must use HTTPS".to_string(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| GatewayManagerError::OfflineOrProxy(e.to_string()))?;
        let text = client
            .get(url)
            .send()
            .await
            .map_err(|e| GatewayManagerError::OfflineOrProxy(e.to_string()))?
            .text()
            .await
            .map_err(|e| GatewayManagerError::OfflineOrProxy(e.to_string()))?;
        let manifest: ReleaseManifest = serde_json::from_str(&text)?;
        Ok(manifest)
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
        let public_key = VerifyingKey::from_bytes(&STABLE_RELEASE_PUBLIC_KEY)?;
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

    /// Build a human-readable status report.
    pub async fn status(&self, config: &Config) -> Result<String> {
        let mut lines = Vec::new();
        lines.push(format!("home: {}", self.home.display()));

        match self.current_pointer() {
            Ok(Some(pointer)) => {
                lines.push(format!(
                    "current: {} ({}) target={} path={}",
                    pointer.version,
                    pointer.release_id,
                    pointer.target,
                    pointer.executable.display()
                ));
                if let Some(ts) = pointer.last_ok_at {
                    lines.push(format!("last known good: {ts}"));
                }
            }
            Ok(None) => lines.push("current: none".to_string()),
            Err(e) => lines.push(format!("current: error reading pointer: {e}")),
        }

        match self.list_versions() {
            Ok(versions) => {
                if versions.is_empty() {
                    lines.push("installed versions: none".to_string());
                } else {
                    lines.push("installed versions:".to_string());
                    for v in versions {
                        let pin = if v.pinned { " (pinned)" } else { "" };
                        lines.push(format!(
                            "  {} {} ({}){} at {}",
                            v.version, v.target, v.release_id, pin, v.installed_at
                        ));
                    }
                }
            }
            Err(e) => lines.push(format!("installed versions: error: {e}")),
        }

        match self.running_gateway_info(config).await {
            Ok(Some(running)) => {
                lines.push(format!(
                    "running: pid={:?} endpoint={} version={} protocol={}",
                    running.pid,
                    running.endpoint,
                    running.info.protocol.product_version,
                    running.info.protocol.protocol_revision
                ));
                let cli = Self::cli_compatibility();
                if let Some(err) = cli.compatibility_error(&running.info.protocol) {
                    lines.push(format!("compatibility: INCOMPATIBLE ({err})"));
                } else {
                    lines.push("compatibility: ok".to_string());
                }
            }
            Ok(None) => lines.push("running: no".to_string()),
            Err(e) => lines.push(format!("running: error: {e}")),
        }

        Ok(lines.join("\n"))
    }

    /// Probe the configured endpoint for a running Gateway.
    pub async fn running_gateway_info(&self, config: &Config) -> Result<Option<RunningGateway>> {
        use crate::{GatewayClient, gateway_ws_url};
        let endpoint = gateway_ws_url(config);
        let client = match tokio::time::timeout(
            Duration::from_secs(3),
            GatewayClient::connect(config),
        )
        .await
        {
            Ok(Ok(client)) => client,
            Ok(Err(e)) => return Err(GatewayManagerError::Other(e.to_string())),
            Err(_) => return Ok(None),
        };

        let pointer = self.current_pointer().unwrap_or(None);
        let executable = pointer
            .as_ref()
            .map(|p| p.executable.clone())
            .unwrap_or_default();

        let info = match client.gateway_info() {
            Some(protocol) => GatewayVersionInfo {
                protocol: protocol.clone(),
                executable: executable.clone(),
            },
            None => {
                // Older gateway that does not report protocol compatibility.
                // Treat it as incompatible (revision 0) so the CLI suggests an
                // upgrade rather than trying to reuse it.
                GatewayVersionInfo {
                    protocol: ProtocolCompatibility {
                        protocol_revision: 0,
                        min_peer_revision: 0,
                        max_peer_revision: 0,
                        product_version: "unknown".to_string(),
                        release_id: "legacy".to_string(),
                        capabilities: vec![],
                    },
                    executable,
                }
            }
        };
        client.close().await;

        let pid = crate::existing_gateway_pid();
        Ok(Some(RunningGateway {
            pid,
            info,
            endpoint,
            config_path_hash: pointer.as_ref().and_then(|p| p.config_path_hash.clone()),
            started_at: pointer.as_ref().and_then(|p| p.started_at),
        }))
    }

    /// Remove old unreferenced versions.
    ///
    /// Keeps the current version, the previous known-good version, and pinned
    /// versions. Never removes the binary that is currently running.
    pub fn prune(&self, keep: usize) -> Result<Vec<PathBuf>> {
        let _lock = self.acquire_install_lock()?;
        let current = self.current_pointer()?;
        let previous = self.previous_known_good()?;

        let mut protected: HashSet<PathBuf> = current
            .iter()
            .map(|p| p.executable.clone())
            .chain(previous.iter().map(|p| p.executable.clone()))
            .collect();

        // Also protect the binary currently tracked by the pid file, if any.
        if let Some(pid_path) = crate::pid_file_path() {
            if let Ok(text) = std::fs::read_to_string(&pid_path) {
                if let Ok(_pid) = text.trim().parse::<u32>() {
                    if let Some(ref pointer) = current {
                        protected.insert(pointer.executable.clone());
                    }
                }
            }
        }

        let mut versions = self.list_versions()?;
        versions.retain(|v| !v.pinned);
        versions.sort_by_key(|b| std::cmp::Reverse(b.installed_at));

        let mut removed = Vec::new();
        let mut kept = 0usize;
        for v in versions {
            let is_current = current
                .as_ref()
                .is_some_and(|c| c.version == v.version && c.target == v.target);
            let is_previous = previous
                .as_ref()
                .is_some_and(|p| p.version == v.version && p.target == v.target);
            if is_current || is_previous {
                continue;
            }
            if kept < keep && !protected.contains(&v.executable) {
                kept += 1;
                continue;
            }
            let dir = self.version_dir(&v.version, &v.target);
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
                removed.push(dir);
            }
        }
        Ok(removed)
    }

    /// Return the previous known-good version from the migration ledger, if any.
    fn previous_known_good(&self) -> Result<Option<InstalledVersion>> {
        if !self.migration_file.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&self.migration_file)?;
        let mut last: Option<MigrationEntry> = None;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<MigrationEntry>(line) {
                last = Some(entry);
            }
        }
        let entry = match last {
            Some(e) => e,
            None => return Ok(None),
        };
        let executable = self.executable_path(&entry.from_version, &Self::current_target());
        if !executable.exists() {
            return Ok(None);
        }
        Ok(Some(InstalledVersion {
            version: entry.from_version,
            target: Self::current_target(),
            release_id: "previous-known-good".to_string(),
            installed_at: entry.ts,
            source: "migration-ledger".to_string(),
            pinned: true,
            executable,
        }))
    }

    /// Append a migration ledger entry.
    fn record_migration(
        &self,
        from_version: &str,
        to_version: &str,
        from_schema: u32,
        to_schema: u32,
        reversible: bool,
        backup_path: Option<&Path>,
    ) -> Result<()> {
        self.ensure_dirs()?;
        let entry = MigrationEntry {
            ts: Utc::now(),
            from_version: from_version.to_string(),
            to_version: to_version.to_string(),
            from_schema,
            to_schema,
            reversible,
            backup_path: backup_path.map(|p| p.to_path_buf()),
        };
        let line = serde_json::to_string(&entry)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.migration_file)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// Check whether an upgrade between two versions is data-migration safe.
    ///
    /// MVP: all schema revisions are 1 and changes are reversible. A real
    /// implementation would inspect `migration.jsonl` and config schema tags.
    fn check_migration_compatibility(
        &self,
        from: &InstalledVersion,
        to: &InstalledVersion,
    ) -> Result<()> {
        let _ = (from, to);
        Ok(())
    }

    /// Install and/or switch to a target version, optionally restarting the
    /// running Gateway.
    pub async fn upgrade(
        &self,
        to: Option<&str>,
        restart: bool,
        manifest_url: Option<&str>,
        config_path: Option<&Path>,
    ) -> Result<String> {
        let cli_compat = Self::cli_compatibility();
        let target = Self::current_target();
        let config = crate::load_config()
            .map_err(|e| GatewayManagerError::Other(format!("failed to load config: {e}")))?;

        let target_version = if let Some(v) = to {
            v.to_string()
        } else if let Some(url) = manifest_url {
            let manifest = self.fetch_verified_manifest(url).await?;
            let (entry, _) = self.select_artifact(&manifest, &cli_compat, &target, None)?;
            entry.gateway_version.clone()
        } else {
            return Err(GatewayManagerError::Other(
                "upgrade requires --to <version> or a manifest URL".to_string(),
            ));
        };

        // Ensure the target version is installed.
        let target_exe = self.executable_path(&target_version, &target);
        if !target_exe.exists() {
            if let Some(url) = manifest_url {
                self.install_from_manifest(url, Some(&target_version), "stable", true)
                    .await?;
            } else {
                return Err(GatewayManagerError::Other(format!(
                    "version {target_version} is not installed; install it first or provide a manifest URL"
                )));
            }
        }

        let current = self.current_version()?;
        let target_info = self.probe_version(&target_exe)?;
        self.ensure_compatible(&target_info)?;

        if let Some(ref current) = current {
            self.check_migration_compatibility(
                current,
                &InstalledVersion {
                    version: target_version.clone(),
                    target: target.clone(),
                    release_id: target_info.protocol.release_id.clone(),
                    installed_at: Utc::now(),
                    source: "upgrade-target".to_string(),
                    pinned: false,
                    executable: target_exe.clone(),
                },
            )?;
        }

        let running = self.running_gateway_info(&config).await?;
        if restart {
            if running.is_some() {
                crate::stop_gateway()
                    .map_err(|e| GatewayManagerError::DaemonBusy(e.to_string()))?;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            self.set_current_pointer(&CurrentPointer {
                version: target_version.clone(),
                target: target.clone(),
                release_id: target_info.protocol.release_id.clone(),
                installed_at: Utc::now(),
                last_ok_at: None,
                executable: target_exe.clone(),
                pid: None,
                started_at: None,
                endpoint: None,
                config_path_hash: config_path.map(Self::config_path_hash),
            })?;

            match crate::start_gateway(config_path.map(PathBuf::from), false).await {
                Ok(()) => {
                    self.record_migration(
                        &current
                            .as_ref()
                            .map(|c| c.version.clone())
                            .unwrap_or_default(),
                        &target_version,
                        1,
                        1,
                        true,
                        None,
                    )?;
                    if let Some(mut pointer) = self.current_pointer()? {
                        pointer.last_ok_at = Some(Utc::now());
                        self.set_current_pointer(&pointer)?;
                    }
                    Ok(format!("upgraded to legion-gateway {target_version}"))
                }
                Err(e) => {
                    // Roll back once to previous known-good.
                    if let Some(prev) = self.previous_known_good()? {
                        self.set_current_pointer(&CurrentPointer {
                            version: prev.version.clone(),
                            target: prev.target.clone(),
                            release_id: prev.release_id.clone(),
                            installed_at: prev.installed_at,
                            last_ok_at: Some(Utc::now()),
                            executable: prev.executable.clone(),
                            pid: None,
                            started_at: None,
                            endpoint: None,
                            config_path_hash: None,
                        })?;
                        if crate::start_gateway(config_path.map(PathBuf::from), false)
                            .await
                            .is_ok()
                        {
                            return Ok(format!(
                                "upgrade to {target_version} failed ({e}); rolled back to {}",
                                prev.version
                            ));
                        }
                    }
                    Err(GatewayManagerError::Other(format!(
                        "upgrade failed and rollback failed: {e}"
                    )))
                }
            }
        } else {
            if running.is_some() {
                return Ok(format!(
                    "installed {target_version}; run with --restart to switch from the running gateway"
                ));
            }
            self.set_current_pointer(&CurrentPointer {
                version: target_version.clone(),
                target: target.clone(),
                release_id: target_info.protocol.release_id.clone(),
                installed_at: Utc::now(),
                last_ok_at: None,
                executable: target_exe,
                pid: None,
                started_at: None,
                endpoint: None,
                config_path_hash: config_path.map(Self::config_path_hash),
            })?;
            Ok(format!(
                "switched to legion-gateway {target_version}; start it with `legion gateway start`"
            ))
        }
    }

    /// Roll back to a previously installed version.
    pub async fn rollback(&self, to: Option<&str>, restart: bool) -> Result<String> {
        let config = crate::load_config()
            .map_err(|e| GatewayManagerError::Other(format!("failed to load config: {e}")))?;
        let running = self.running_gateway_info(&config).await?;
        if running.is_some() && !restart {
            return Err(GatewayManagerError::DaemonBusy(
                "a gateway is running; pass --restart to stop it before rollback".to_string(),
            ));
        }

        let versions = self.list_versions()?;
        let target = if let Some(v) = to {
            versions
                .into_iter()
                .find(|x| x.version == v)
                .ok_or_else(|| {
                    GatewayManagerError::Other(format!("version {v} is not installed"))
                })?
        } else {
            self.previous_known_good()?.ok_or_else(|| {
                GatewayManagerError::Other("no previous known-good version found".to_string())
            })?
        };

        let info = self.probe_version(&target.executable)?;
        self.ensure_compatible(&info)?;

        if restart && running.is_some() {
            crate::stop_gateway().map_err(|e| GatewayManagerError::DaemonBusy(e.to_string()))?;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        self.set_current_pointer(&CurrentPointer {
            version: target.version.clone(),
            target: target.target.clone(),
            release_id: target.release_id.clone(),
            installed_at: target.installed_at,
            last_ok_at: Some(Utc::now()),
            executable: target.executable.clone(),
            pid: None,
            started_at: None,
            endpoint: None,
            config_path_hash: None,
        })?;

        if restart {
            crate::start_gateway(None, false)
                .await
                .map_err(|e| GatewayManagerError::Other(e.to_string()))?;
            Ok(format!(
                "rolled back and started legion-gateway {}",
                target.version
            ))
        } else {
            Ok(format!(
                "rolled back to legion-gateway {}; start it with `legion gateway start`",
                target.version
            ))
        }
    }

    /// Run diagnostic checks and return a report.
    pub async fn doctor(&self, config: &Config) -> Result<String> {
        let mut lines = Vec::new();
        lines.push("gateway doctor".to_string());
        lines.push(format!("home directory: {}", self.home.display()));

        match self.current_pointer() {
            Ok(Some(p)) => lines.push(format!(
                "current pointer: {} {} ({})",
                p.version, p.target, p.release_id
            )),
            Ok(None) => lines.push("current pointer: none".to_string()),
            Err(e) => lines.push(format!("current pointer: error: {e}")),
        }

        match self.list_versions() {
            Ok(vs) => lines.push(format!("installed versions: {}", vs.len())),
            Err(e) => lines.push(format!("installed versions: error: {e}")),
        }

        let cli = Self::cli_compatibility();
        lines.push(format!(
            "CLI protocol: revision={} range={}-{}",
            cli.protocol_revision, cli.min_peer_revision, cli.max_peer_revision
        ));

        match self.running_gateway_info(config).await {
            Ok(Some(r)) => {
                lines.push(format!(
                    "running gateway: {} protocol {}",
                    r.info.protocol.product_version, r.info.protocol.protocol_revision
                ));
                if let Some(err) = cli.compatibility_error(&r.info.protocol) {
                    lines.push(format!("  compatibility issue: {err}"));
                }
            }
            Ok(None) => lines.push("running gateway: none".to_string()),
            Err(e) => lines.push(format!("running gateway: error: {e}")),
        }

        let releases_dir = self.gateways_dir.clone();
        lines.push(format!(
            "releases disk usage: {} bytes",
            dir_size(&releases_dir).unwrap_or(0)
        ));

        Ok(lines.join("\n"))
    }
}

/// Recursively compute the total size of a directory in bytes.
fn dir_size(dir: &Path) -> Result<u64> {
    let mut total = 0u64;
    if !dir.exists() {
        return Ok(total);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            total += dir_size(&entry.path())?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
}

/// Decode a signature that may be base64 or hex encoded.
fn decode_signature(bytes: &[u8]) -> Result<Signature> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| {
            GatewayManagerError::ManifestUntrusted("signature is not valid UTF-8".to_string())
        })?
        .trim();
    let raw = if text.len() == 128 {
        hex::decode(text).map_err(|_| {
            GatewayManagerError::ManifestUntrusted("signature is not valid hex".to_string())
        })?
    } else {
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, text).map_err(|_| {
            GatewayManagerError::ManifestUntrusted("signature is not valid base64".to_string())
        })?
    };
    let arr: [u8; 64] = raw.try_into().map_err(|_| {
        GatewayManagerError::ManifestUntrusted("signature has wrong length".to_string())
    })?;
    Ok(Signature::from_bytes(&arr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use legion_protocol::ProtocolRange;

    fn test_manager() -> (GatewayManager, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        (GatewayManager::new(tmp.path()), tmp)
    }

    fn sign_manifest(bytes: &[u8]) -> Vec<u8> {
        let seed = hex::decode("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
            .unwrap();
        let signing_key = SigningKey::from_bytes(&seed.try_into().unwrap());
        let sig = signing_key.sign(bytes);
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, sig.to_bytes())
            .into_bytes()
    }

    #[test]
    fn current_pointer_round_trip() {
        let (mgr, _tmp) = test_manager();
        let pointer = CurrentPointer {
            version: "0.2.0".to_string(),
            target: "aarch64-apple-darwin".to_string(),
            release_id: "r1".to_string(),
            installed_at: Utc::now(),
            last_ok_at: None,
            executable: PathBuf::from("/x"),
            pid: None,
            started_at: None,
            endpoint: None,
            config_path_hash: None,
        };
        mgr.set_current_pointer(&pointer).unwrap();
        let read = mgr.current_pointer().unwrap().unwrap();
        assert_eq!(read.version, "0.2.0");
    }

    #[test]
    fn list_versions_empty() {
        let (mgr, _tmp) = test_manager();
        assert!(mgr.list_versions().unwrap().is_empty());
    }

    #[test]
    fn verify_manifest_signature_accepts_base64() {
        let (mgr, _tmp) = test_manager();
        let manifest = br#"{"formatVersion":1,"channel":"stable","publishedAt":"2026-07-14T00:00:00Z","releases":[]}"#;
        let sig = sign_manifest(manifest);
        mgr.verify_manifest_signature(manifest, &sig).unwrap();
    }

    #[test]
    fn verify_manifest_signature_rejects_tampered() {
        let (mgr, _tmp) = test_manager();
        let manifest = br#"{"formatVersion":1,"channel":"stable","publishedAt":"2026-07-14T00:00:00Z","releases":[]}"#;
        let sig = sign_manifest(manifest);
        let mut tampered = manifest.to_vec();
        tampered[20] ^= 1;
        assert!(mgr.verify_manifest_signature(&tampered, &sig).is_err());
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
