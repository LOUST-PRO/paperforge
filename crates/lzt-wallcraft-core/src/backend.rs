//! `WallpaperBackend` trait + LWE backend implementation.
//!
//! ## Design
//!
//! The trait abstracts the operations a wallpaper manager needs:
//! set / pause / resume / query running instances. Implementations
//! talk to the actual wallpaper daemon via the mechanism appropriate
//! to that daemon:
//!
//! - **LWE** today: spawn subprocess + POSIX signals
//!   (SIGSTOP/SIGCONT for pause/resume, SIGUSR1/SIGUSR2 reserved for
//!   audio control — see [`crate::audio`]).
//! - Future: swww IPC socket, hyprpaper IPC, mpvpaper input-ipc-server.
//!
//! All backends are read/write at the IPC layer (spawning processes,
//! sending signals), but the library does NOT embed GPL code from
//! any backend — license compatibility is preserved by process
//! isolation.

use std::{path::Path, process::Command};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{audio::LweAudioController, error::{Error, Result}};

/// Identifier for a backend implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    /// `linux-wallpaperengine` (Almamu + louzt fork).
    LinuxWallpaperEngine,
}

impl BackendKind {
    /// Process basename used in `pgrep -f` / `pgrep` lookups.
    pub fn process_basename(self) -> &'static str {
        match self {
            Self::LinuxWallpaperEngine => "linux-wallpaperengine",
        }
    }
}

/// Runtime state of a wallpaper process observed by a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendState {
    /// Process is running and rendering frames.
    Running,
    /// Process is paused via SIGSTOP (decoder frozen, frames stopped).
    Paused,
    /// Process is not running.
    NotRunning,
}

/// Abstraction over a wallpaper daemon.
///
/// All methods take `&self` (no interior mutability needed); backends
/// are cheap to construct per-call.
#[async_trait]
pub trait WallpaperBackend: Send + Sync {
    /// Which backend kind this is.
    fn kind(&self) -> BackendKind;

    /// List the PIDs of running instances of this backend.
    async fn list_pids(&self) -> Result<Vec<i32>>;

    /// Spawn a new instance with the given scene directory + Wayland
    /// output. The default LWE impl uses `--screen-root <output>` to
    /// pin the instance to a specific output.
    async fn set(&self, scene: &Path, output: Option<&str>) -> Result<()>;

    /// Pause all running instances of this backend (SIGSTOP).
    async fn pause(&self) -> Result<usize>;

    /// Resume all paused instances (SIGCONT).
    async fn resume(&self) -> Result<usize>;

    /// Query the state of a single PID. `Paused` is reported by
    /// inspecting `/proc/<pid>/status` `State` field — a process that
    /// received SIGSTOP has state `T (stopped)`.
    async fn state(&self, pid: i32) -> Result<BackendState>;

    /// Whether this backend can render the given [`crate::WallpaperEntry`].
    fn supports(&self, entry: &crate::WallpaperEntry) -> bool;
}

/// Backend implementation for the `linux-wallpaperengine` fork.
///
/// Talks to LWE via:
/// 1. `pgrep -f <pattern>` to find running PIDs.
/// 2. `kill -STOP <pid>` / `kill -CONT <pid>` to pause/resume.
/// 3. Spawning `linux-wallpaperengine --screen-root <output> <scene>`
///    to start a new instance.
#[derive(Debug, Clone, Default)]
pub struct LweBackend {
    /// Optional override for the binary path. `None` means look up
    /// via `PATH` (default behaviour).
    pub binary_path: Option<std::path::PathBuf>,
}

impl LweBackend {
    /// Construct with default binary resolution.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with an explicit binary path (used by tests or
    /// operators with non-standard installs).
    pub fn with_binary(path: impl Into<std::path::PathBuf>) -> Self {
        Self { binary_path: Some(path.into()) }
    }

    fn binary(&self) -> &str {
        self.binary_path
            .as_ref()
            .map(|p| p.to_str().unwrap_or("linux-wallpaperengine"))
            .unwrap_or("linux-wallpaperengine")
    }

    /// Returns the audio controller for this backend (lives here
    /// because SIGUSR1/SIGUSR2 are tied to LWE).
    pub fn audio(&self) -> LweAudioController {
        LweAudioController::new(self.clone())
    }
}

#[async_trait]
impl WallpaperBackend for LweBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::LinuxWallpaperEngine
    }

    async fn list_pids(&self) -> Result<Vec<i32>> {
        // `pgrep -f linux-wallpaperengine` matches both the binary and
        // its argv. We accept that because there's no other process
        // matching that substring on the operator's box.
        let out = Command::new("pgrep")
            .args(["-f", "linux-wallpaperengine"])
            .output()?;
        if !out.status.success() {
            // pgrep returns 1 when no process matches. That's fine.
            return Ok(Vec::new());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .filter_map(|l| l.trim().parse::<i32>().ok())
            .collect())
    }

    async fn set(&self, scene: &Path, output: Option<&str>) -> Result<()> {
        if !scene.exists() {
            return Err(Error::BackendUnreachable {
                kind: self.kind().process_basename().to_string(),
                message: format!("scene path does not exist: {}", scene.display()),
            });
        }

        let mut cmd = Command::new(self.binary());
        if let Some(out) = output {
            cmd.args(["--screen-root", out]);
        }
        cmd.arg(scene);

        // We spawn detached: do NOT block waiting for LWE to exit.
        // Use `spawn()` (not `status()`) so the parent (lzt-wallcraft
        // CLI) returns immediately.
        let child = cmd.spawn().map_err(|e| Error::BackendFailure {
            kind: self.kind().process_basename().to_string(),
            message: format!("spawn failed: {e}"),
        })?;

        tracing::info!(
            "spawned {} for output={:?} scene={} pid={}",
            self.binary(),
            output,
            scene.display(),
            child.id(),
        );
        Ok(())
    }

    async fn pause(&self) -> Result<usize> {
        let pids = self.list_pids().await?;
        for pid in &pids {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(*pid),
                nix::sys::signal::Signal::SIGSTOP,
            )
            .map_err(|e| Error::BackendFailure {
                kind: self.kind().process_basename().to_string(),
                message: format!("SIGSTOP to pid {pid} failed: {e}"),
            })?;
        }
        Ok(pids.len())
    }

    async fn resume(&self) -> Result<usize> {
        let pids = self.list_pids().await?;
        for pid in &pids {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(*pid),
                nix::sys::signal::Signal::SIGCONT,
            )
            .map_err(|e| Error::BackendFailure {
                kind: self.kind().process_basename().to_string(),
                message: format!("SIGCONT to pid {pid} failed: {e}"),
            })?;
        }
        Ok(pids.len())
    }

    async fn state(&self, pid: i32) -> Result<BackendState> {
        let status_path = format!("/proc/{pid}/status");
        let content = match std::fs::read_to_string(&status_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BackendState::NotRunning)
            }
            Err(e) => return Err(e.into()),
        };
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("State:") {
                // State line looks like: "State:\tT (stopped)"
                //                       or   "State:\tR (running)"
                if rest.contains('T') {
                    return Ok(BackendState::Paused);
                }
                return Ok(BackendState::Running);
            }
        }
        Ok(BackendState::NotRunning)
    }

    fn supports(&self, entry: &crate::WallpaperEntry) -> bool {
        entry.kind.lwe_compatible()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_kind_basename() {
        assert_eq!(
            BackendKind::LinuxWallpaperEngine.process_basename(),
            "linux-wallpaperengine"
        );
    }

    #[test]
    fn lwe_backend_kind() {
        let b = LweBackend::new();
        assert_eq!(b.kind(), BackendKind::LinuxWallpaperEngine);
    }

    #[test]
    fn supports_video_workshop_only() {
        use crate::inventory::{WallpaperEntry, WallpaperKind};
        use std::path::PathBuf;
        use std::time::SystemTime;

        let b = LweBackend::new();
        let mk = |kind: WallpaperKind| WallpaperEntry {
            path: PathBuf::from("/dummy"),
            mtime: SystemTime::UNIX_EPOCH,
            kind,
            title: None,
            workshop_id: None,
        };
        assert!(b.supports(&mk(WallpaperKind::WorkshopScene)));
        assert!(b.supports(&mk(WallpaperKind::LooseVideo)));
        assert!(!b.supports(&mk(WallpaperKind::LooseImage)));
    }

    #[test]
    fn binary_resolution_default() {
        let b = LweBackend::new();
        assert_eq!(b.binary(), "linux-wallpaperengine");
    }

    #[test]
    fn binary_resolution_explicit() {
        let b = LweBackend::with_binary("/opt/lwe/bin/linux-wallpaperengine");
        assert_eq!(b.binary(), "/opt/lwe/bin/linux-wallpaperengine");
    }
}
