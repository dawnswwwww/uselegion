//! Operational commands: status reporting, upgrade, rollback, pruning, and
//! diagnostics.

use chrono::Utc;
use legion_core::config::Config;
use legion_protocol::ProtocolCompatibility;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    CurrentPointer, GatewayManager, GatewayManagerError, GatewayVersionInfo, InstalledVersion,
    MigrationEntry, Result, RunningGateway, dir_size,
};

impl GatewayManager {
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
            self.set_current_pointer(&CurrentPointer::switched(
                &target_version,
                &target,
                &target_info.protocol.release_id,
                target_exe.clone(),
                Utc::now(),
                config_path,
            ))?;

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
                        self.set_current_pointer(&CurrentPointer::restored(&prev))?;
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
            self.set_current_pointer(&CurrentPointer::switched(
                &target_version,
                &target,
                &target_info.protocol.release_id,
                target_exe,
                Utc::now(),
                config_path,
            ))?;
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

        self.set_current_pointer(&CurrentPointer::restored(&target))?;

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

#[cfg(test)]
mod tests {
    use super::super::{GATEWAY_EXECUTABLE_NAME, InstallMetadata};
    use super::*;
    use crate::gateway_manager::tests::test_manager;

    /// Install a fake version directory the way `commit_install` lays it out.
    fn install_fake_version(mgr: &GatewayManager, version: &str, target: &str) -> PathBuf {
        let dir = mgr.version_dir(version, target);
        std::fs::create_dir_all(&dir).unwrap();
        let executable = dir.join(GATEWAY_EXECUTABLE_NAME);
        std::fs::write(&executable, b"#!/bin/sh\n").unwrap();
        let metadata = InstallMetadata {
            version: version.to_string(),
            target: target.to_string(),
            release_id: format!("rel-{version}"),
            installed_at: Utc::now(),
            source: "test".to_string(),
        };
        std::fs::write(
            dir.join("install.json"),
            serde_json::to_vec_pretty(&metadata).unwrap(),
        )
        .unwrap();
        executable
    }

    #[test]
    fn previous_known_good_reads_last_migration_entry() {
        let (mgr, _tmp) = test_manager();
        let target = GatewayManager::current_target();
        let executable = install_fake_version(&mgr, "0.1.0", &target);

        mgr.record_migration("0.1.0", "0.2.0", 1, 1, true, None)
            .unwrap();
        let prev = mgr.previous_known_good().unwrap().unwrap();
        assert_eq!(prev.version, "0.1.0");
        assert_eq!(prev.target, target);
        assert!(prev.pinned);
        assert_eq!(prev.executable, executable);
    }

    #[test]
    fn previous_known_good_none_without_ledger_or_binary() {
        let (mgr, _tmp) = test_manager();
        // No ledger at all.
        assert!(mgr.previous_known_good().unwrap().is_none());

        // Ledger entry whose binary is missing.
        mgr.record_migration("0.1.0", "0.2.0", 1, 1, true, None)
            .unwrap();
        assert!(mgr.previous_known_good().unwrap().is_none());
    }

    #[test]
    fn rollback_restores_previous_pointer() {
        // Mirrors the upgrade-failure path in `upgrade`: the pointer is first
        // switched to the new version, then restored to the previous
        // known-good version from the migration ledger.
        let (mgr, _tmp) = test_manager();
        let target = GatewayManager::current_target();
        let old_exe = install_fake_version(&mgr, "0.1.0", &target);
        let new_exe = install_fake_version(&mgr, "0.2.0", &target);
        mgr.record_migration("0.1.0", "0.2.0", 1, 1, true, None)
            .unwrap();

        // Pointer is on the new version after the upgrade attempt.
        mgr.set_current_pointer(&CurrentPointer::switched(
            "0.2.0",
            &target,
            "rel-0.2.0",
            new_exe,
            Utc::now(),
            None,
        ))
        .unwrap();
        assert_eq!(mgr.current_pointer().unwrap().unwrap().version, "0.2.0");

        // Roll back once to previous known-good.
        let prev = mgr.previous_known_good().unwrap().unwrap();
        mgr.set_current_pointer(&CurrentPointer::restored(&prev))
            .unwrap();

        let pointer = mgr.current_pointer().unwrap().unwrap();
        assert_eq!(pointer.version, "0.1.0");
        assert_eq!(pointer.target, target);
        assert_eq!(pointer.executable, old_exe);
        assert!(pointer.last_ok_at.is_some());
        assert_eq!(pointer.pid, None);
    }
}
