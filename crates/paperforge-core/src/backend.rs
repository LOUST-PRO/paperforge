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

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    audio::LweAudioController,
    error::{Error, Result},
};

/// Identifier for a backend implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    /// `linux-wallpaperengine` (Almamu + louzt fork).
    LinuxWallpaperEngine,
    /// `swww` (https://github.com/Horus645/swww) — Wayland wallpaper
    /// daemon for static images. Differentiation: pause/resume not
    /// supported (swww runs as a single daemon, no per-output
    /// processes); only supports `LooseImage` entries.
    SwwwDaemon,
}

impl BackendKind {
    /// Substring used to identify this backend's process in
    /// `/proc/<pid>/cmdline`. Not validated against an exact path —
    /// any process whose argv contains this string is considered
    /// an instance.
    pub fn process_pattern(self) -> &'static str {
        match self {
            Self::LinuxWallpaperEngine => "linux-wallpaperengine",
            Self::SwwwDaemon => "swww-daemon",
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
/// 1. Walking `/proc/<pid>/cmdline` to find running PIDs (substring
///    match against `BackendKind::process_pattern`).
/// 2. `kill -STOP <pid>` / `kill -CONT <pid>` to pause/resume.
/// 3. Spawning `linux-wallpaperengine --screen-root <output> <scene>`
///    to start a new instance.
#[derive(Debug, Clone, Default)]
pub struct LweBackend {
    /// Optional override for the binary path. `None` means look up
    /// via `PATH` (default behaviour).
    pub binary_path: Option<PathBuf>,
}

impl LweBackend {
    /// Construct with default binary resolution.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with an explicit binary path (used by tests or
    /// operators with non-standard installs).
    pub fn with_binary(path: impl Into<PathBuf>) -> Self {
        Self {
            binary_path: Some(path.into()),
        }
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

/// Walk `proc_root` (typically `/proc`) and return PIDs whose
/// `/proc/<pid>/cmdline` argv contains `pattern` as a substring.
///
/// Sync, no allocations beyond the returned Vec. Split out from
/// [`LweBackend::list_pids`] for testability — tests construct a
/// fake `/proc` tree on disk and pass the path in.
fn list_pids_in_proc(proc_root: &Path, pattern: &str) -> Result<Vec<i32>> {
    let mut pids = Vec::new();
    for entry in std::fs::read_dir(proc_root)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(name_str) = file_name.to_str() else {
            continue;
        };
        let Ok(pid) = name_str.parse::<i32>() else {
            continue;
        };

        let cmdline_path = proc_root.join(pid.to_string()).join("cmdline");
        let Ok(cmdline) = std::fs::read(&cmdline_path) else {
            continue;
        };

        // /proc/<pid>/cmdline is NUL-separated argv. We split on NUL
        // and check each arg for the pattern. This is robust to
        // /proc/<pid>/comm being truncated to 15 chars (TASK_COMM_LEN).
        let matches = cmdline
            .split(|b| *b == 0)
            .filter(|arg| !arg.is_empty())
            .any(|arg| {
                std::str::from_utf8(arg)
                    .map(|s| s.contains(pattern))
                    .unwrap_or(false)
            });

        if matches {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

#[async_trait]
impl WallpaperBackend for LweBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::LinuxWallpaperEngine
    }

    async fn list_pids(&self) -> Result<Vec<i32>> {
        let pids = list_pids_in_proc(Path::new("/proc"), self.kind().process_pattern())?;
        tracing::debug!("found {} LWE pid(s)", pids.len());
        Ok(pids)
    }

    async fn set(&self, scene: &Path, output: Option<&str>) -> Result<()> {
        if !scene.exists() {
            return Err(Error::BackendUnreachable {
                kind: self.kind().process_pattern().to_string(),
                message: format!("scene path does not exist: {}", scene.display()),
            });
        }

        let mut cmd = Command::new(self.binary());
        if let Some(out) = output {
            cmd.args(["--screen-root", out]);
        }
        cmd.arg(scene);

        // We spawn detached: do NOT block waiting for LWE to exit.
        // Use `spawn()` (not `status()`) so the parent (paperforge
        // CLI) returns immediately.
        let child = cmd.spawn().map_err(|e| Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
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
                kind: self.kind().process_pattern().to_string(),
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
                kind: self.kind().process_pattern().to_string(),
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

// `BackendKind::process_basename` was renamed to `process_pattern` in
// 0.1.1. Keep an alias so external callers that imported the old name
// keep compiling until the next major bump.
impl BackendKind {
    /// Deprecated alias for [`BackendKind::process_pattern`].
    #[deprecated(since = "0.1.1", note = "renamed to `process_pattern`")]
    pub fn process_basename(self) -> &'static str {
        self.process_pattern()
    }
}

/// Backend implementation for `swww-daemon`.
///
/// Talks to swww via the `swww` CLI (the binary the upstream project
/// installs alongside `swww-daemon`). Both binaries must be on PATH
/// or configured via [`SwwwBackend::with_binaries`].
///
/// ## Limitations vs `LweBackend`
///
/// - swww runs as a single daemon for all outputs. `list_pids`
///   returns the daemon's PID at most once; per-output PIDs are not
///   a thing in swww.
/// - **No pause/resume.** swww renders frames as a daemon; the only
///   way to "pause" is `swww clear <color>` (sets all outputs to a
///   flat color) which is a destructive set, not a suspend.
/// - Only handles `LooseImage` entries (swww is a static-image
///   wallpaper tool, not a scene player).
#[derive(Debug, Clone, Default)]
pub struct SwwwBackend {
    /// Path to the `swww` CLI binary (used to dispatch set/clear
    /// commands). `None` means PATH lookup.
    pub cli_binary: Option<PathBuf>,
    /// Path to the `swww-daemon` binary (used to detect running
    /// state). `None` means PATH lookup.
    pub daemon_binary: Option<PathBuf>,
}

impl SwwwBackend {
    /// Construct with default PATH lookup for both binaries.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with explicit binary paths.
    pub fn with_binaries(cli: impl Into<PathBuf>, daemon: impl Into<PathBuf>) -> Self {
        Self {
            cli_binary: Some(cli.into()),
            daemon_binary: Some(daemon.into()),
        }
    }

    fn cli(&self) -> &str {
        self.cli_binary
            .as_ref()
            .map(|p| p.to_str().unwrap_or("swww"))
            .unwrap_or("swww")
    }
}

#[async_trait]
impl WallpaperBackend for SwwwBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::SwwwDaemon
    }

    async fn list_pids(&self) -> Result<Vec<i32>> {
        list_pids_in_proc(Path::new("/proc"), self.kind().process_pattern())
    }

    async fn set(&self, scene: &Path, output: Option<&str>) -> Result<()> {
        if !scene.exists() {
            return Err(Error::BackendUnreachable {
                kind: self.kind().process_pattern().to_string(),
                message: format!("image path does not exist: {}", scene.display()),
            });
        }

        let mut cmd = Command::new(self.cli());
        cmd.arg("img").arg(scene);
        if let Some(out) = output {
            // swww's --outputs flag accepts comma-separated output names.
            cmd.args(["--outputs", out]);
        }
        let status = cmd.status().map_err(|e| Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
            message: format!("spawn {} failed: {e}", self.cli()),
        })?;
        if !status.success() {
            return Err(Error::BackendFailure {
                kind: self.kind().process_pattern().to_string(),
                message: format!("`{} img` exited with {:?}", self.cli(), status.code()),
            });
        }
        tracing::info!(
            "swww set: output={:?} scene={} (exit {:?})",
            output,
            scene.display(),
            status.code()
        );
        Ok(())
    }

    async fn pause(&self) -> Result<usize> {
        Err(Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
            message: "swww does not support pause/resume; use `swww clear <color>` instead"
                .to_string(),
        })
    }

    async fn resume(&self) -> Result<usize> {
        Err(Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
            message: "swww does not support pause/resume; the previous wallpaper is lost when cleared"
                .to_string(),
        })
    }

    async fn state(&self, pid: i32) -> Result<BackendState> {
        // swww runs as a single daemon; the only state we can
        // report is "running" (process exists) vs "not running".
        let status_path = format!("/proc/{pid}/status");
        match std::fs::read_to_string(&status_path) {
            Ok(content) => {
                for line in content.lines() {
                    if let Some(rest) = line.strip_prefix("State:") {
                        if rest.contains('T') {
                            return Ok(BackendState::Paused);
                        }
                        return Ok(BackendState::Running);
                    }
                }
                Ok(BackendState::NotRunning)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BackendState::NotRunning),
            Err(e) => Err(e.into()),
        }
    }

    fn supports(&self, entry: &crate::WallpaperEntry) -> bool {
        matches!(entry.kind, crate::inventory::WallpaperKind::LooseImage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_kind_pattern() {
        assert_eq!(
            BackendKind::LinuxWallpaperEngine.process_pattern(),
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

    /// Build a fake `/proc/<pid>/cmdline` with NUL-separated argv.
    fn write_cmdline(proc_root: &Path, pid: i32, argv: &[&str]) {
        let pid_dir = proc_root.join(pid.to_string());
        std::fs::create_dir_all(&pid_dir).unwrap();
        let mut bytes = Vec::new();
        for arg in argv {
            bytes.extend_from_slice(arg.as_bytes());
            bytes.push(0);
        }
        std::fs::write(pid_dir.join("cmdline"), &bytes).unwrap();
    }

    #[test]
    fn list_pids_in_proc_finds_matching_argv() {
        let tmp = tempfile::tempdir().unwrap();
        let proc = tmp.path();
        write_cmdline(
            proc,
            100,
            &["/usr/bin/linux-wallpaperengine", "--screen-root", "DP-1"],
        );
        write_cmdline(
            proc,
            200,
            &[
                "/usr/bin/linux-wallpaperengine",
                "--screen-root",
                "HDMI-A-1",
            ],
        );
        write_cmdline(proc, 300, &["/usr/bin/other", "noise"]);

        let pids = list_pids_in_proc(proc, "linux-wallpaperengine").unwrap();
        assert_eq!(pids, vec![100, 200]);
    }

    #[test]
    fn list_pids_in_proc_skips_non_numeric_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let proc = tmp.path();
        std::fs::create_dir_all(proc.join("self")).unwrap();
        std::fs::create_dir_all(proc.join("thread-self")).unwrap();
        write_cmdline(proc, 42, &["/usr/bin/linux-wallpaperengine"]);

        let pids = list_pids_in_proc(proc, "linux-wallpaperengine").unwrap();
        assert_eq!(pids, vec![42]);
    }

    #[test]
    fn list_pids_in_proc_ignores_missing_cmdline() {
        let tmp = tempfile::tempdir().unwrap();
        let proc = tmp.path();
        // PID dir without cmdline should be silently skipped (process
        // likely exited between readdir and read).
        std::fs::create_dir_all(proc.join("999")).unwrap();
        write_cmdline(proc, 100, &["/usr/bin/linux-wallpaperengine"]);
        let pids = list_pids_in_proc(proc, "linux-wallpaperengine").unwrap();
        assert_eq!(pids, vec![100]);
    }

    #[test]
    fn list_pids_in_proc_empty_when_no_match() {
        let tmp = tempfile::tempdir().unwrap();
        let proc = tmp.path();
        write_cmdline(proc, 100, &["/usr/bin/swww"]);
        let pids = list_pids_in_proc(proc, "linux-wallpaperengine").unwrap();
        assert!(pids.is_empty());
    }

    #[test]
    fn list_pids_in_proc_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let proc = tmp.path();
        write_cmdline(proc, 500, &["/usr/bin/linux-wallpaperengine"]);
        write_cmdline(proc, 100, &["/usr/bin/linux-wallpaperengine"]);
        write_cmdline(proc, 300, &["/usr/bin/linux-wallpaperengine"]);
        let pids = list_pids_in_proc(proc, "linux-wallpaperengine").unwrap();
        assert_eq!(pids, vec![100, 300, 500]);
    }

    #[test]
    fn list_pids_in_proc_does_not_match_cwd_shell() {
        // The original pgrep -f bug: if the operator's cwd contained
        // "linux-wallpaperengine", pgrep would match the shell itself.
        // Our /proc/<pid>/cmdline walk never inspects the shell's argv
        // unless the shell really runs the pattern.
        let tmp = tempfile::tempdir().unwrap();
        let proc = tmp.path();
        write_cmdline(proc, 1000, &["/bin/zsh"]);
        let pids = list_pids_in_proc(proc, "linux-wallpaperengine").unwrap();
        assert!(pids.is_empty());
    }

    /// Real-process SIGSTOP/SIGCONT end-to-end: spawn `sleep`,
    /// freeze, inspect `/proc/<pid>/status`, thaw, re-inspect.
    ///
    /// This is the closest thing to "smoke test" the signal code
    /// without a real LWE instance. It uses the real
    /// `nix::sys::signal::kill` + the real `/proc` filesystem.
    #[test]
    fn real_sigstop_sigcont_round_trip() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;

        // Give the scheduler a moment so /proc reflects the new PID.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let backend = LweBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let state_running = rt.block_on(backend.state(pid)).unwrap();
        assert_eq!(
            state_running,
            BackendState::Running,
            "sleep should start running"
        );

        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGSTOP,
        )
        .expect("SIGSTOP");

        // Yield to scheduler so the kernel processes the signal.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let state_paused = rt.block_on(backend.state(pid)).unwrap();
        assert_eq!(
            state_paused,
            BackendState::Paused,
            "sleep should report paused after SIGSTOP"
        );

        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGCONT,
        )
        .expect("SIGCONT");

        std::thread::sleep(std::time::Duration::from_millis(50));
        let state_resumed = rt.block_on(backend.state(pid)).unwrap();
        assert_eq!(
            state_resumed,
            BackendState::Running,
            "sleep should report running after SIGCONT"
        );

        child.kill().expect("cleanup kill");
        let _ = child.wait();
    }

    #[test]
    fn state_for_nonexistent_pid_returns_not_running() {
        let backend = LweBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        // 0 is reserved for the kernel scheduler; /proc/0/status does
        // not exist on a normal box.
        let s = rt.block_on(backend.state(0)).unwrap();
        assert_eq!(s, BackendState::NotRunning);
    }

    // ---- SwwwBackend tests ----

    #[test]
    fn swww_backend_kind() {
        let b = SwwwBackend::new();
        assert_eq!(b.kind(), BackendKind::SwwwDaemon);
    }

    #[test]
    fn swww_pattern_matches_daemon() {
        assert_eq!(BackendKind::SwwwDaemon.process_pattern(), "swww-daemon");
    }

    #[test]
    fn swww_supports_only_loose_images() {
        use crate::inventory::{WallpaperEntry, WallpaperKind};
        let b = SwwwBackend::new();
        let mk = |kind: WallpaperKind| WallpaperEntry {
            path: PathBuf::from("/dummy"),
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind,
            title: None,
            workshop_id: None,
        };
        assert!(b.supports(&mk(WallpaperKind::LooseImage)));
        assert!(!b.supports(&mk(WallpaperKind::LooseVideo)));
        assert!(!b.supports(&mk(WallpaperKind::WorkshopScene)));
    }

    #[test]
    fn swww_binary_resolution_default() {
        let b = SwwwBackend::new();
        assert_eq!(b.cli(), "swww");
    }

    #[test]
    fn swww_binary_resolution_explicit() {
        let b = SwwwBackend::with_binaries("/opt/swww/swww", "/opt/swww/swww-daemon");
        assert_eq!(b.cli(), "/opt/swww/swww");
    }

    #[test]
    fn swww_pause_returns_backend_failure() {
        let b = SwwwBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(b.pause()).unwrap_err();
        assert!(
            matches!(err, Error::BackendFailure { .. }),
            "swww.pause() must refuse, got {err:?}"
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("does not support pause"),
            "error message must explain the limitation, got: {msg}"
        );
    }

    #[test]
    fn swww_resume_returns_backend_failure() {
        let b = SwwwBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(b.resume()).unwrap_err();
        assert!(
            matches!(err, Error::BackendFailure { .. }),
            "swww.resume() must refuse, got {err:?}"
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("does not support pause"),
            "error message must explain the limitation, got: {msg}"
        );
    }

    #[test]
    fn swww_set_rejects_missing_path() {
        let b = SwwwBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(b.set(Path::new("/does/not/exist.png"), Some("DP-1")))
            .unwrap_err();
        // Should refuse before spawning the CLI.
        assert!(
            matches!(err, Error::BackendUnreachable { .. }),
            "missing image must be caught before spawn, got {err:?}"
        );
    }

    #[test]
    fn list_pids_finds_swww_daemon_too() {
        // Confirm the same /proc walker picks up `swww-daemon`
        // instances — proves the BackendKind dispatch works for both
        // backends with no extra logic.
        let tmp = tempfile::tempdir().unwrap();
        let proc = tmp.path();
        write_cmdline(proc, 700, &["/usr/bin/swww-daemon"]);
        write_cmdline(proc, 800, &["/usr/bin/swww", "img", "/wall.jpg"]);

        let lwe_pids = list_pids_in_proc(proc, "linux-wallpaperengine").unwrap();
        assert!(lwe_pids.is_empty());
        let swww_pids = list_pids_in_proc(proc, "swww-daemon").unwrap();
        assert_eq!(swww_pids, vec![700]);
    }

    #[test]
    fn lwe_and_swww_are_distinct_patterns() {
        // The two backends must not collide on /proc walks.
        assert_ne!(
            BackendKind::LinuxWallpaperEngine.process_pattern(),
            BackendKind::SwwwDaemon.process_pattern()
        );
    }
}
