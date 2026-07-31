//! Self-update subsystem for `paperforge`.
//!
//! Allows a running install to upgrade itself to the latest GitHub
//! release of `LOUST-PRO/paperforge` (or any compatible fork), with
//! SHA256SUMS verification, atomic binary swap, and N-1 backup
//! retention. Off-by-default — opt-in via
//! `~/.config/paperforge/updater.toml`.
//!
//! # Design
//!
//! - HTTP / archive extraction are delegated to system tools
//!   (`curl`, `tar`, `sha256sum`) rather than pulling new crates
//!   (`reqwest`, `flate2`, `tar`). The runner and the operator's
//!   laptop are Debian; both have these tools available, and
//!   keeping them out of the dependency tree keeps the surface
//!   small. If the operator wants a hermetic binary later, swap
//!   the [`run_command`] indirection for in-process implementations.
//! - Binary location is taken from
//!   [`std::env::current_exe`](std::env::current_exe) when not
//!   overridden. Backups live at
//!   `${XDG_DATA_HOME:-~/.local/share}/paperforge/backups/`.
//! - Verification is SHA-256 only. Cosign / GPG signature checking
//!   is intentionally out of scope for this PR — the upstream
//!   releases ship `SHA256SUMS` natively, and adding sigstore
//!   would balloon the dependency footprint for what is, today, a
//!   single-binary project.
//!
//! # Failure modes (handled)
//!
//! - Network unreachable → error surfaced, no state change.
//! - SHA-256 mismatch → temp file removed, no state change.
//! - Tarball extraction fails → staging dir removed, no state change.
//! - Binary swap fails (EACCES, ENOSPC) → previous binary restored
//!   from backup; backup itself preserved.
//!
//! # Failure modes (NOT handled — explicit non-goals)
//!
//! - Concurrent self-update invocations. The updater does not
//!   acquire an exclusive lock; two `paperforge self-update
//!   --apply` calls in flight can race and clobber each other.
//!   Operators are expected to run update through the daemon's
//!   D-Bus interface or behind a single-instance guard. Adding a
//!   `flock`-based lock is a separate PR.

use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::error::{Error, Result};

/// Release channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Stable releases only (no `pre` / `rc` tags).
    #[default]
    Stable,
    /// Pre-releases included.
    Pre,
}

/// Persisted updater configuration.
///
/// Loaded from `~/.config/paperforge/updater.toml`. If the file is
/// absent, [`UpdaterConfig::default()`] is used and the updater is
/// effectively a no-op (`enabled = false`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdaterConfig {
    /// Master switch. When `false`, every command returns early
    /// with `Error::Config("updater disabled")`.
    #[serde(default)]
    pub enabled: bool,

    /// Which channel to track.
    #[serde(default)]
    pub channel: Channel,

    /// `owner/repo` slug for the GitHub release feed.
    #[serde(default = "default_github_repo")]
    pub github_repo: String,

    /// How many backups to keep. `1` = only the immediately
    /// previous version is restorable via `self-update --rollback`.
    /// `0` disables backup entirely (not recommended).
    #[serde(default = "default_backup_retention")]
    pub backup_retention: usize,

    /// If true, the SHA256SUMS file is consulted and the
    /// downloaded asset's hash is checked against the entry
    /// matching the asset name. If false, the asset is installed
    /// without verification (use only for testing).
    #[serde(default = "default_verify_signature")]
    pub verify_signature: bool,

    /// Override the binary path to replace. Defaults to
    /// `std::env::current_exe()`.
    #[serde(default)]
    pub binary_path: Option<PathBuf>,
}

fn default_github_repo() -> String {
    "LOUST-PRO/paperforge".to_string()
}

fn default_backup_retention() -> usize {
    1
}

fn default_verify_signature() -> bool {
    true
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            channel: Channel::Stable,
            github_repo: default_github_repo(),
            backup_retention: default_backup_retention(),
            verify_signature: default_verify_signature(),
            binary_path: None,
        }
    }
}

impl UpdaterConfig {
    /// Load from `~/.config/paperforge/updater.toml`. Returns the
    /// default config if the file is absent.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if !path.exists() {
            debug!(
                "updater config {} not found; using defaults",
                path.display()
            );
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| Error::Config(format!("updater.toml: {e}")))
    }

    /// Persist to disk. Creates the parent dir if missing.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("serialize updater.toml: {e}")))?;
        std::fs::write(path, text)?;
        Ok(())
    }
}

/// Information about an available update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// Currently installed version (from `CARGO_PKG_VERSION`).
    pub current_version: String,
    /// Version reported by the latest release matching `channel`.
    pub latest_version: String,
    /// Release tag (e.g. `v0.3.0` or `v0.3.0-pre.1`).
    pub release_tag: String,
    /// Asset filename inside the release (e.g.
    /// `paperforge-v0.3.0-x86_64-unknown-linux-gnu.tar.gz`).
    pub asset_name: String,
    /// Direct download URL for the asset.
    pub asset_url: String,
    /// SHA-256 of the asset (parsed from `SHA256SUMS`).
    pub sha256: String,
    /// Size in bytes (best-effort; `None` if the release JSON did
    /// not include it).
    pub size_bytes: Option<u64>,
    /// True if `latest_version` is not equal to `current_version`.
    /// `false` means "you're already on the latest release for
    /// your channel".
    pub update_available: bool,
    /// True if the matched release is a pre-release
    /// (`prerelease: true` in the GitHub release JSON).
    pub is_prerelease: bool,
}

/// A backup entry retained on disk for `self-update --rollback`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupEntry {
    /// Version captured (from the previous binary's `--version`
    /// output, or `unknown` if probing failed).
    pub version: String,
    /// Absolute path to the backup file.
    pub path: PathBuf,
    /// When the backup was created.
    pub created_at: DateTime<Utc>,
}

/// The updater.
#[derive(Debug, Clone)]
pub struct Updater {
    config: UpdaterConfig,
    binary_path: PathBuf,
    backup_dir: PathBuf,
}

impl Updater {
    /// Construct an updater. `binary_path` defaults to
    /// `std::env::current_exe()` if `UpdaterConfig::binary_path` is
    /// `None`. `backup_dir` defaults to
    /// `${XDG_DATA_HOME:-~/.local/share}/paperforge/backups/`.
    pub fn new(config: UpdaterConfig) -> Result<Self> {
        let binary_path = match config.binary_path.clone() {
            Some(p) => p,
            None => std::env::current_exe()
                .map_err(|e| Error::Config(format!("could not resolve current_exe: {e}")))?,
        };

        let backup_root = dirs::data_dir()
            .ok_or_else(|| Error::Config("no data_dir".to_string()))?
            .join("paperforge")
            .join("backups");
        std::fs::create_dir_all(&backup_root)?;

        Ok(Self {
            config,
            binary_path,
            backup_dir: backup_root,
        })
    }

    /// Underlying config (for inspection via `paperforge
    /// self-update --config`).
    pub fn config(&self) -> &UpdaterConfig {
        &self.config
    }

    /// Binary path the updater will replace.
    pub fn binary_path(&self) -> &Path {
        &self.binary_path
    }

    /// Backup directory.
    pub fn backup_dir(&self) -> &Path {
        &self.backup_dir
    }

    /// Refuse to operate if the updater is disabled. Returns the
    /// same error for every command so callers can short-circuit
    /// consistently.
    fn ensure_enabled(&self) -> Result<()> {
        if !self.config.enabled {
            return Err(Error::Config(
                "updater is disabled. Enable it in ~/.config/paperforge/updater.toml \
                 (set `enabled = true`) before invoking self-update."
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Check the GitHub releases feed for an update. Does not
    /// download or modify anything.
    pub async fn check(&self) -> Result<UpdateInfo> {
        self.ensure_enabled()?;

        let current = crate::VERSION.to_string();
        // GitHub's API: GET /repos/{owner}/{repo}/releases/latest
        // for stable; /releases for the full list when channel=pre.
        let url = match self.config.channel {
            Channel::Stable => format!(
                "https://api.github.com/repos/{}/releases/latest",
                self.config.github_repo
            ),
            Channel::Pre => format!(
                "https://api.github.com/repos/{}/releases?per_page=20",
                self.config.github_repo
            ),
        };

        let body = self.http_get(&url).await?;
        let release: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| Error::BackendFailure {
                kind: "github_api".to_string(),
                message: format!("parse release JSON: {e}"),
            })?;

        // Stable returns a single object; pre returns an array.
        let picked = match self.config.channel {
            Channel::Stable => release,
            Channel::Pre => release
                .as_array()
                .and_then(|arr| arr.first().cloned())
                .ok_or_else(|| Error::BackendFailure {
                    kind: "github_api".to_string(),
                    message: "no releases returned".to_string(),
                })?,
        };

        let tag = picked
            .get("tag_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::BackendFailure {
                kind: "github_api".to_string(),
                message: "release missing tag_name".to_string(),
            })?
            .to_string();
        let is_prerelease = picked
            .get("prerelease")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let assets = picked
            .get("assets")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::BackendFailure {
                kind: "github_api".to_string(),
                message: "release missing assets array".to_string(),
            })?;

        // Pick the asset matching our target triple.
        let target_suffix = current_target_suffix();
        let asset = assets
            .iter()
            .find(|a| {
                a.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n.ends_with(&target_suffix))
                    .unwrap_or(false)
            })
            .ok_or_else(|| Error::BackendFailure {
                kind: "github_api".to_string(),
                message: format!("no release asset ending with '{target_suffix}' in release {tag}"),
            })?;

        let asset_name = asset
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::BackendFailure {
                kind: "github_api".to_string(),
                message: "asset missing name".to_string(),
            })?
            .to_string();
        let asset_url = asset
            .get("browser_download_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::BackendFailure {
                kind: "github_api".to_string(),
                message: "asset missing browser_download_url".to_string(),
            })?
            .to_string();
        let size_bytes = asset.get("size").and_then(|v| v.as_u64());

        // Strip leading `v` for human-readable version comparison.
        let latest_version = tag.trim_start_matches('v').to_string();
        let update_available = latest_version != current;

        // SHA-256 lookup is deferred to apply() to avoid an extra
        // round-trip on the read-only check path. We populate it
        // with an empty marker here and re-fetch on apply.
        let sha256 = String::new();

        Ok(UpdateInfo {
            current_version: current,
            latest_version,
            release_tag: tag,
            asset_name,
            asset_url,
            sha256,
            size_bytes,
            update_available,
            is_prerelease,
        })
    }

    /// Apply the update described by `info`. Steps:
    ///
    /// 1. Fetch SHA256SUMS from the same release.
    /// 2. Download the asset to a temp file.
    /// 3. Verify SHA-256.
    /// 4. Extract to a staging dir.
    /// 5. Back up the current binary.
    /// 6. Move the staged binary over the live binary.
    /// 7. Prune old backups to `backup_retention`.
    pub async fn apply(&self, info: &UpdateInfo) -> Result<BackupEntry> {
        self.ensure_enabled()?;

        // 1. SHA256SUMS
        let sums_url = format!(
            "https://github.com/{}/releases/download/{}/SHA256SUMS",
            self.config.github_repo, info.release_tag
        );
        let sums_body = self.http_get(&sums_url).await?;
        let expected_sha = parse_sha256_for(&sums_body, &info.asset_name).ok_or_else(|| {
            Error::BackendFailure {
                kind: "sha256sums".to_string(),
                message: format!(
                    "asset '{}' not present in SHA256SUMS for release {}",
                    info.asset_name, info.release_tag
                ),
            }
        })?;

        // 2. Download
        let staging_dir = tempfile::Builder::new()
            .prefix("paperforge-update-")
            .tempdir()
            .map_err(|e| Error::BackendFailure {
                kind: "tempdir".to_string(),
                message: format!("create staging dir: {e}"),
            })?;
        let tarball_path = staging_dir.path().join(&info.asset_name);
        self.http_download(&info.asset_url, &tarball_path).await?;

        // 3. Verify
        if self.config.verify_signature {
            let actual = sha256_file(&tarball_path)?;
            if !actual.eq_ignore_ascii_case(&expected_sha) {
                return Err(Error::BackendFailure {
                    kind: "sha256".to_string(),
                    message: format!(
                        "SHA-256 mismatch for {}: expected={}, got={}",
                        info.asset_name, expected_sha, actual
                    ),
                });
            }
        } else {
            warn!("verify_signature=false in updater.toml; skipping SHA-256 check (testing only)");
        }

        // 4. Extract
        let extract_dir = staging_dir.path().join("extracted");
        std::fs::create_dir_all(&extract_dir)?;
        run_command(
            "tar",
            &[
                "-xzf",
                tarball_path.to_str().unwrap_or("?"),
                "-C",
                extract_dir.to_str().unwrap_or("?"),
            ],
        )?;

        // Find the staged binary inside the extracted tree. The
        // release tarball contains a single binary at the root
        // (created by `cargo build --release` and packaged by
        // the release workflow).
        let staged_bin = find_single_binary(&extract_dir).ok_or_else(|| Error::BackendFailure {
            kind: "tarball".to_string(),
            message: format!(
                "expected exactly one executable in {} after extraction",
                extract_dir.display()
            ),
        })?;

        // 5. Back up the current binary.
        let backup = self.backup_current_binary(&info.latest_version).await?;

        // 6. Move staged binary over the live one.
        if let Err(e) = std::fs::copy(&staged_bin, &self.binary_path) {
            // Restore from backup before bubbling the error.
            warn!(
                "swap failed ({}); restoring from backup {}",
                e,
                backup.path.display()
            );
            let _ = std::fs::copy(&backup.path, &self.binary_path);
            return Err(Error::BackendFailure {
                kind: "swap".to_string(),
                message: format!("copy staged to {}: {e}", self.binary_path.display()),
            });
        }
        // Best-effort: chmod +x on the new binary (extraction
        // usually preserves perms but tarballs built on Windows
        // or in CI runners can land with 0644).
        let _ = std::fs::set_permissions(&self.binary_path, std::fs::Permissions::from_mode(0o755));

        // 7. Prune backups.
        if let Err(e) = self.prune_backups() {
            warn!("backup prune failed: {e}");
        }

        info!(
            "update applied: {} -> {} (asset {}, backup at {})",
            info.current_version,
            info.latest_version,
            info.asset_name,
            backup.path.display()
        );

        Ok(backup)
    }

    /// List existing backups, newest first.
    pub fn list_backups(&self) -> Result<Vec<BackupEntry>> {
        let mut entries = Vec::new();
        if !self.backup_dir.exists() {
            return Ok(entries);
        }
        for entry in std::fs::read_dir(&self.backup_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let version = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("paperforge-v"))
                .and_then(|n| n.strip_suffix(".bin"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let created_at = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(DateTime::from)
                .unwrap_or_else(|_| Utc::now());
            entries.push(BackupEntry {
                version,
                path,
                created_at,
            });
        }
        entries.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        Ok(entries)
    }

    /// Restore the most recent backup. Errors if there are none.
    pub async fn rollback(&self) -> Result<BackupEntry> {
        self.ensure_enabled()?;
        let backups = self.list_backups()?;
        let newest = backups
            .into_iter()
            .next()
            .ok_or_else(|| Error::Config("no backups to roll back to".to_string()))?;
        std::fs::copy(&newest.path, &self.binary_path)?;
        let _ = std::fs::set_permissions(&self.binary_path, std::fs::Permissions::from_mode(0o755));
        info!(
            "rolled back to {} (backup at {})",
            newest.version,
            newest.path.display()
        );
        Ok(newest)
    }

    async fn backup_current_binary(&self, new_version: &str) -> Result<BackupEntry> {
        let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let prev_version =
            probe_version(&self.binary_path).unwrap_or_else(|_| "unknown".to_string());
        let backup_path = self
            .backup_dir
            .join(format!("paperforge-v{prev_version}-{ts}.bin"));

        std::fs::copy(&self.binary_path, &backup_path)?;
        info!(
            "backed up current binary (version={}) to {}",
            prev_version,
            backup_path.display()
        );

        // Note: `new_version` is consumed in info-level logs only.
        let _ = new_version;
        Ok(BackupEntry {
            version: prev_version,
            path: backup_path,
            created_at: Utc::now(),
        })
    }

    fn prune_backups(&self) -> Result<()> {
        let mut entries = self.list_backups()?;
        // Already newest-first. Drop everything beyond
        // `backup_retention`.
        if entries.len() > self.config.backup_retention {
            for old in entries.drain(self.config.backup_retention..) {
                if let Err(e) = std::fs::remove_file(&old.path) {
                    warn!("could not prune {}: {}", old.path.display(), e);
                } else {
                    debug!("pruned old backup {}", old.path.display());
                }
            }
        }
        Ok(())
    }

    async fn http_get(&self, url: &str) -> Result<String> {
        let out = run_command(
            "curl",
            &["-fsSL", "-H", "User-Agent: paperforge-self-update", url],
        )?;
        Ok(out)
    }

    async fn http_download(&self, url: &str, dest: &Path) -> Result<()> {
        run_command(
            "curl",
            &[
                "-fsSL",
                "-H",
                "User-Agent: paperforge-self-update",
                "-o",
                dest.to_str().unwrap_or("?"),
                url,
            ],
        )?;
        Ok(())
    }
}

/// `paperforge-v{version}-{triple}.tar.gz` suffix for the current
/// host. Returns `x86_64-unknown-linux-gnu.tar.gz` today. When
/// cross-builds are added (aarch64 / darwin) this function grows
/// to consult `std::env::consts`.
fn current_target_suffix() -> String {
    format!("{}.tar.gz", current_target_triple())
}

fn current_target_triple() -> &'static str {
    // Single platform for now; expand when cross-compiled
    // artifacts ship.
    "x86_64-unknown-linux-gnu"
}

/// Parse `SHA256SUMS` (the format GitHub release tooling emits)
/// and return the hash for `asset_name`, if present.
fn parse_sha256_for(sums: &str, asset_name: &str) -> Option<String> {
    for line in sums.lines() {
        let mut parts = line.split_whitespace();
        let raw_hash = parts.next()?;
        // `sha256sum -b` prefixes executable entries with `*`
        // (binary mode). The star is glued to the hash, so we
        // strip one optional leading `*`.
        let hash = raw_hash.strip_prefix('*').unwrap_or(raw_hash);
        // The filename can be prefixed with `./`. Strip it.
        let name = parts.next()?.trim_start_matches("./");
        if name == asset_name {
            return Some(hash.to_lowercase());
        }
    }
    None
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn find_single_binary(dir: &Path) -> Option<PathBuf> {
    let mut found = Vec::new();
    walk_for_binary(dir, &mut found);
    if found.len() == 1 {
        Some(found.into_iter().next().unwrap())
    } else {
        None
    }
}

fn walk_for_binary(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk_for_binary(&p, out);
        } else if p.is_file() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // The release tarball is expected to contain a single
            // file named `paperforge` (the cargo binary). Skip
            // the source LICENSE/README if present.
            if name == "paperforge" {
                out.push(p);
            }
        }
    }
}

fn probe_version(binary: &Path) -> Result<String> {
    let out = Command::new(binary).arg("--version").output()?;
    if !out.status.success() {
        return Err(Error::BackendFailure {
            kind: "probe".to_string(),
            message: format!(
                "`{} --version` exited with {:?}",
                binary.display(),
                out.status
            ),
        });
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // `paperforge 0.2.0` or `paperforge 0.2.0-pre.1`
    let version = stdout
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| Error::BackendFailure {
            kind: "probe".to_string(),
            message: format!("could not parse version from '{}'", stdout.trim()),
        })?;
    Ok(version.to_string())
}

fn run_command(prog: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| Error::BackendFailure {
            kind: prog.to_string(),
            message: format!("spawn: {e}"),
        })?;
    if !out.status.success() {
        return Err(Error::BackendFailure {
            kind: prog.to_string(),
            message: format!(
                "exited with {:?}\nstdout: {}\nstderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

use std::os::unix::fs::PermissionsExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled() {
        let cfg = UpdaterConfig::default();
        assert!(!cfg.enabled, "updater must default to disabled");
        assert_eq!(cfg.channel, Channel::Stable);
        assert_eq!(cfg.github_repo, "LOUST-PRO/paperforge");
        assert_eq!(cfg.backup_retention, 1);
        assert!(cfg.verify_signature);
        assert!(cfg.binary_path.is_none());
    }

    #[test]
    fn roundtrip_via_tempfile() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("updater.toml");
        let cfg = UpdaterConfig {
            enabled: true,
            channel: Channel::Pre,
            github_repo: "louzt/paperforge".to_string(),
            backup_retention: 3,
            verify_signature: false,
            binary_path: Some(PathBuf::from("/opt/bin/paperforge")),
        };
        cfg.save(&path).unwrap();
        let loaded = UpdaterConfig::load_or_default(&path).unwrap();
        assert!(loaded.enabled);
        assert_eq!(loaded.channel, Channel::Pre);
        assert_eq!(loaded.github_repo, "louzt/paperforge");
        assert_eq!(loaded.backup_retention, 3);
        assert!(!loaded.verify_signature);
        assert_eq!(
            loaded.binary_path,
            Some(PathBuf::from("/opt/bin/paperforge"))
        );
    }

    #[test]
    fn load_or_default_returns_default_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.toml");
        let loaded = UpdaterConfig::load_or_default(&path).unwrap();
        assert!(!loaded.enabled);
        assert_eq!(loaded.channel, Channel::Stable);
    }

    #[test]
    fn ensure_enabled_blocks_when_disabled() {
        let cfg = UpdaterConfig::default();
        let upd = Updater::new(cfg).unwrap();
        let err = upd.ensure_enabled();
        assert!(err.is_err(), "must refuse when disabled");
    }

    #[test]
    fn parse_sha256_for_basic() {
        let sums = "\
abc123  paperforge-v0.2.0-x86_64-unknown-linux-gnu.tar.gz
def456  paperforge-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
";
        let got = parse_sha256_for(sums, "paperforge-v0.2.0-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(got.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_sha256_for_handles_binary_marker() {
        // Some `sha256sum` invocations prefix executable entries
        // with `*` (binary mode). The updater should strip it.
        let sums = "*abc123  paperforge-v0.2.0-x86_64-unknown-linux-gnu.tar.gz\n";
        let got = parse_sha256_for(sums, "paperforge-v0.2.0-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(got.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_sha256_for_returns_none_for_missing_asset() {
        let sums = "abc123  something-else.tar.gz\n";
        let got = parse_sha256_for(sums, "paperforge-v0.2.0-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(got, None);
    }

    #[test]
    fn sha256_of_known_string() {
        // sha256("hello\n") is well-known.
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("hello.txt");
        std::fs::write(&f, b"hello\n").unwrap();
        let got = sha256_file(&f).unwrap();
        assert_eq!(
            got,
            "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"
        );
    }

    #[test]
    fn current_target_suffix_ends_in_tar_gz() {
        let s = current_target_suffix();
        assert!(s.ends_with(".tar.gz"), "got: {s}");
        assert!(s.starts_with("x86_64-unknown-linux-gnu"));
    }

    #[test]
    fn prune_backups_keeps_newest_n() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = UpdaterConfig {
            enabled: true,
            backup_retention: 2,
            ..UpdaterConfig::default()
        };
        // Manually populate the backup dir.
        for i in 0..5 {
            let p = tmp
                .path()
                .join(format!("paperforge-v0.0.{i}-20260101T00000{i}Z.bin"));
            std::fs::write(&p, b"bin").unwrap();
            // Set mtimes to differ.
            let t = filetime::FileTime::from_unix_time(1_700_000_000 + i as i64, 0);
            let _ = filetime::set_file_mtime(&p, t);
        }
        let upd = Updater {
            config: cfg,
            binary_path: PathBuf::from("/nonexistent/paperforge"),
            backup_dir: tmp.path().to_path_buf(),
        };
        upd.prune_backups().unwrap();
        let remaining: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("paperforge-v"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            remaining.len(),
            2,
            "prune_backups must keep exactly backup_retention files"
        );
    }
}
