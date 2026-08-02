//! Local Gateway binary discovery, installation, and lifecycle management.
//!
//! This module implements the CLI side of the CLI/Gateway independent
//! distribution design. It manages the `~/.legion/gateways/` directory,
//! downloads and verifies signed release manifests, and atomically switches
//! between installed Gateway versions.
//!
//! The implementation is split by concern: this root keeps the shared types
//! and the core discovery/pointer logic, [`installer`] holds the
//! download/verify/install pipeline, and [`ops`] holds the operational
//! commands (status, upgrade, rollback, doctor).

mod installer;
mod ops;

use chrono::{DateTime, Utc};
use ed25519_dalek::Signature;
use fs2::FileExt;
use legion_protocol::{ProtocolCompatibility, STABLE_RELEASE_PUBLIC_KEY};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use thiserror::Error;

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

impl CurrentPointer {
    /// Build a pointer for switching to an installed version, with no runtime
    /// state recorded yet.
    fn switched(
        version: &str,
        target: &str,
        release_id: &str,
        executable: PathBuf,
        installed_at: DateTime<Utc>,
        config_path: Option<&Path>,
    ) -> Self {
        Self {
            version: version.to_string(),
            target: target.to_string(),
            release_id: release_id.to_string(),
            installed_at,
            last_ok_at: None,
            executable,
            pid: None,
            started_at: None,
            endpoint: None,
            config_path_hash: config_path.map(GatewayManager::config_path_hash),
        }
    }

    /// Build a pointer restored to a previously installed version, marked as
    /// last-known-good now (rollback path).
    fn restored(version: &InstalledVersion) -> Self {
        Self {
            version: version.version.clone(),
            target: version.target.clone(),
            release_id: version.release_id.clone(),
            installed_at: version.installed_at,
            last_ok_at: Some(Utc::now()),
            executable: version.executable.clone(),
            pid: None,
            started_at: None,
            endpoint: None,
            config_path_hash: None,
        }
    }
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
    /// Ed25519 public key used to verify release manifests. Defaults to the
    /// production stable-channel key; overridable in tests.
    release_public_key: [u8; 32],
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
            release_public_key: STABLE_RELEASE_PUBLIC_KEY,
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
        legion_core::fs::atomic_write(path, bytes)?;
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

    pub(crate) fn test_manager() -> (GatewayManager, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        (GatewayManager::new(tmp.path()), tmp)
    }

    /// Like [`test_manager`] but overrides the release public key, so signature
    /// tests can sign with an ephemeral keypair instead of the production key.
    pub(crate) fn test_manager_with_key(
        public_key: [u8; 32],
    ) -> (GatewayManager, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = GatewayManager::new(tmp.path());
        mgr.release_public_key = public_key;
        (mgr, tmp)
    }

    #[test]
    fn current_pointer_round_trip() {
        let (mgr, _tmp) = test_manager();
        let installed_at = Utc::now();
        let pointer = CurrentPointer::switched(
            "0.2.0",
            "aarch64-apple-darwin",
            "r1",
            PathBuf::from("/x"),
            installed_at,
            None,
        );
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
    fn switched_pointer_carries_install_fields_and_no_runtime_state() {
        let installed_at = Utc::now();
        let config_path = Path::new("/tmp/legion.toml");
        let pointer = CurrentPointer::switched(
            "0.3.0",
            "aarch64-apple-darwin",
            "r9",
            PathBuf::from("/gw/legion-gateway"),
            installed_at,
            Some(config_path),
        );
        assert_eq!(pointer.version, "0.3.0");
        assert_eq!(pointer.target, "aarch64-apple-darwin");
        assert_eq!(pointer.release_id, "r9");
        assert_eq!(pointer.installed_at, installed_at);
        assert_eq!(pointer.executable, PathBuf::from("/gw/legion-gateway"));
        assert_eq!(pointer.last_ok_at, None);
        assert_eq!(pointer.pid, None);
        assert_eq!(pointer.started_at, None);
        assert_eq!(pointer.endpoint, None);
        assert_eq!(
            pointer.config_path_hash,
            Some(GatewayManager::config_path_hash(config_path))
        );
    }

    #[test]
    fn restored_pointer_marks_last_known_good() {
        let installed_at = Utc::now();
        let previous = InstalledVersion {
            version: "0.1.0".to_string(),
            target: "aarch64-apple-darwin".to_string(),
            release_id: "previous-known-good".to_string(),
            installed_at,
            source: "migration-ledger".to_string(),
            pinned: true,
            executable: PathBuf::from("/gw/0.1.0/legion-gateway"),
        };
        let pointer = CurrentPointer::restored(&previous);
        assert_eq!(pointer.version, previous.version);
        assert_eq!(pointer.target, previous.target);
        assert_eq!(pointer.release_id, previous.release_id);
        assert_eq!(pointer.installed_at, previous.installed_at);
        assert_eq!(pointer.executable, previous.executable);
        assert!(pointer.last_ok_at.is_some());
        assert_eq!(pointer.pid, None);
        assert_eq!(pointer.config_path_hash, None);
    }

    #[test]
    fn decode_signature_accepts_hex() {
        let raw = [7u8; 64];
        let sig = decode_signature(hex::encode(raw).as_bytes()).unwrap();
        assert_eq!(sig.to_bytes(), raw);
    }

    #[test]
    fn decode_signature_accepts_base64() {
        let raw = [9u8; 64];
        let encoded =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw).into_bytes();
        let sig = decode_signature(&encoded).unwrap();
        assert_eq!(sig.to_bytes(), raw);
    }

    #[test]
    fn decode_signature_rejects_garbage() {
        let err = decode_signature(b"!!! not a signature !!!").unwrap_err();
        assert!(matches!(err, GatewayManagerError::ManifestUntrusted(_)));
    }

    #[test]
    fn decode_signature_rejects_wrong_length() {
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [1u8; 32])
            .into_bytes();
        let err = decode_signature(&encoded).unwrap_err();
        assert!(matches!(err, GatewayManagerError::ManifestUntrusted(_)));
    }

    #[test]
    fn decode_signature_rejects_non_utf8() {
        let err = decode_signature(&[0xff, 0xfe, 0xfd]).unwrap_err();
        assert!(matches!(err, GatewayManagerError::ManifestUntrusted(_)));
    }
}
