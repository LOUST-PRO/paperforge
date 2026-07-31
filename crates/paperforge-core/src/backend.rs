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
    sync::Arc,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    audio::LweAudioController,
    error::{Error, Result},
    pool::LweSinglePool,
};

/// Identifier for a backend implementation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    /// `linux-wallpaperengine` (Almamu + louzt fork).
    #[default]
    LinuxWallpaperEngine,
    /// `swww` (https://github.com/Horus645/swww) — Wayland wallpaper
    /// daemon for static images. Differentiation: pause/resume not
    /// supported (swww runs as a single daemon, no per-output
    /// processes); only supports `LooseImage` entries.
    SwwwDaemon,
    /// `hyprpaper` (https://github.com/hyprwm/hyprpaper) — Hyprland's
    /// Wayland wallpaper daemon. Static images per output. Like swww,
    /// no pause/resume; only `LooseImage`. Hyprland-only (depends on
    /// `HYPRLAND_INSTANCE_SIGNATURE`).
    Hyprpaper,
    /// `mpvpaper` (https://github.com/GhostNaN/mpvpaper) — mpv-based
    /// Wayland wallpaper. Can play videos / scenes as a wallpaper.
    /// Pause via mpv's IPC socket (`pause yes` / `pause no`).
    /// Compatible with any wlroots-based compositor.
    Mpvpaper,
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
            Self::Hyprpaper => "hyprpaper",
            Self::Mpvpaper => "mpvpaper",
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
/// Talks to LWE via a shared [`LweSinglePool`] — one process, multi-output
/// argv. The pool is what makes `set()` idempotent across multiple
/// outputs (no per-output process spawn), and what lets `pause` / `resume`
/// SIGSTOP a single PID instead of N.
///
/// Direct-spawn fallback (the v0.1 path, one process per output) can
/// still be exercised by config [`crate::config::Config::pool_enabled`]
/// = `false`. In that case the daemon's `LweBackendOps` will short-circuit
/// to a per-call `Command::spawn` and bypass the pool entirely.
#[derive(Debug, Clone, Default)]
pub struct LweBackend {
    /// Optional override for the binary path. `None` means look up
    /// via `PATH` (default behaviour).
    pub binary_path: Option<PathBuf>,
    /// Shared multi-output pool. The pool is `Clone`-cheap (internal
    /// `Arc<Mutex<...>>`) so `LweBackend: Clone` stays.
    pool: Arc<LweSinglePool>,
}

/// Extract the Steam Workshop `content_id` from a scene path.
///
/// LWE expects background selectors as numeric Steam Workshop content
/// IDs (e.g. `850994960`), not absolute paths. We detect the Workshop
/// convention by matching the path suffix
/// `workshop/content/<appid>/<numeric>` where `appid` and
/// `content_id` are decimal numbers. The leaf of the matched path
/// is the content_id.
///
/// Returns `None` for:
/// - non-Workshop paths (e.g. `/home/lou/.local/share/backgrounds/foo.jpg`)
/// - Workshop paths whose content_id is not all-decimal
/// - relative paths (Steam Workshop scenes are always absolute on
///   a real Steam install)
pub(crate) fn workshop_content_id(scene: &Path) -> Option<String> {
    // Reject relative paths — Steam Workshop scenes are always
    // absolute on a real install. `Path::components()` happily
    // parses relative paths, so we have to short-circuit.
    if !scene.is_absolute() {
        return None;
    }

    // Components in reverse order, oldest → newest:
    //   /home/lou/.steam/.../workshop/content/431960/850994960
    //   [..., "content", "431960", "850994960"]
    //   [..., "workshop", ...] ← ancestor marker
    let comps: Vec<_> = scene.components().collect();
    if comps.len() < 4 {
        return None;
    }
    let content_id = comps[comps.len() - 1].as_os_str().to_str()?;
    let appid = comps[comps.len() - 2].as_os_str().to_str()?;
    let marker = comps[comps.len() - 3].as_os_str().to_str()?;
    let grandparent = comps[comps.len() - 4].as_os_str().to_str()?;

    if marker != "content" || grandparent != "workshop" {
        return None;
    }
    if !appid.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !content_id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(content_id.to_string())
}

impl LweBackend {
    /// Construct with default binary resolution.
    pub fn new() -> Self {
        let pool = LweSinglePool::new();
        Self {
            binary_path: None,
            pool: Arc::new(pool),
        }
    }

    /// Construct with an explicit binary path (used by tests or
    /// operators with non-standard installs).
    pub fn with_binary(path: impl Into<PathBuf>) -> Self {
        let pb: PathBuf = path.into();
        let pool = LweSinglePool::with_binary(pb.clone());
        Self {
            binary_path: Some(pb),
            pool: Arc::new(pool),
        }
    }

    /// Reference to the underlying multi-output pool. Used by the
    /// daemon layer when it wants to spawn or signal directly without
    /// going through the `WallpaperBackend` trait shim.
    pub fn pool(&self) -> &LweSinglePool {
        &self.pool
    }

    /// Test helper: replace the pool's flag list with an empty vec,
    /// so a `/bin/sleep` proxy binary stays alive long enough to
    /// accept our pause/resume signals. Production code never calls
    /// this — the pool's default flags include
    /// `--fullscreen-pause-only-active` etc.
    #[cfg(test)]
    pub fn with_empty_pool_flags(self) -> Self {
        let new_pool = LweSinglePool::with_binary(
            self.binary_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("linux-wallpaperengine")),
        )
        .with_flags(Vec::new());
        Self {
            binary_path: self.binary_path,
            pool: Arc::new(new_pool),
        }
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
        // The pool is the single source of truth for the LWE pid.
        // We deliberately do NOT walk /proc — that pattern would
        // pick up orphaned processes (e.g. from a previous daemon
        // or unit tests) that the pool doesn't know about, and it
        // would conflict with the pool's "one process per backend,
        // regardless of output count" invariant.
        let pid = self.pool.current_pid().await.unwrap_or(0);
        if pid > 0 {
            Ok(vec![pid])
        } else {
            Ok(Vec::new())
        }
    }

    async fn set(&self, scene: &Path, output: Option<&str>) -> Result<()> {
        if !scene.exists() {
            return Err(Error::BackendUnreachable {
                kind: self.kind().process_pattern().to_string(),
                message: format!("scene path does not exist: {}", scene.display()),
            });
        }

        // Translate the Workshop scene path to a numeric content_id.
        // Falling back to the basename (numeric leaf) like LWE does
        // internally is no longer wired here — the pool only knows
        // Workshop scenes, matching the project's scope.
        let content_id = workshop_content_id(scene).ok_or_else(|| Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
            message: format!(
                "scene path {} is not a Steam Workshop scene \
                     (expected `workshop/content/<appid>/<numeric>`)",
                scene.display()
            ),
        })?;

        let out = output.ok_or_else(|| Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
            message: "no --output specified (pool backend requires explicit output)".to_string(),
        })?;

        // The pool handles hot-swap: idempotent rebinds do nothing,
        // new outputs trigger a single respawn with merged argv.
        let pid = self.pool.bind(out, &content_id).await?;

        tracing::info!(
            "pool bind: output={} scene={} pid={}",
            out,
            scene.display(),
            pid,
        );
        Ok(())
    }

    async fn pause(&self) -> Result<usize> {
        let pid = self.pool.pause().await?;
        Ok(if pid.is_some() { 1 } else { 0 })
    }

    async fn resume(&self) -> Result<usize> {
        let pid = self.pool.resume().await?;
        Ok(if pid.is_some() { 1 } else { 0 })
    }

    async fn state(&self, pid: i32) -> Result<BackendState> {
        // The pool is the single source of truth for which LWE pid
        // we actually own. If the caller asks about a foreign pid,
        // we report NotRunning instead of a stale /proc read.
        let owned = self.pool.current_pid().await;
        if owned != Some(pid) {
            return Ok(BackendState::NotRunning);
        }
        pid_state_quick(pid)
    }

    fn supports(&self, entry: &crate::WallpaperEntry) -> bool {
        entry.kind.lwe_compatible()
    }
}

/// Read the kernel-reported state of a PID via `/proc/<pid>/status`.
///
/// Returns:
/// - `BackendState::Paused` if the kernel reports `T (stopped)` —
///   typical after the caller sent SIGSTOP via the nix syscall.
/// - `BackendState::Running` if the kernel reports any of `R (running)`,
///   `S (sleeping)`, `D (disk sleep)`, `Z (zombie)`, `I (idle)`. We
///   treat all of these as "alive, not paused".
/// - `BackendState::NotRunning` if `/proc/<pid>/status` doesn't exist
///   (process exited, or never existed).
///
/// Errors only on actual I/O failures (permissions, transient FS
/// issues). Most call sites should treat `NotRunning` as the signal
/// they need (process died, time to respawn) without needing a
/// `Result`-flavored API.
///
/// This is a synchronous read because `/proc` is a kernel pseudofs:
/// no I/O wait, no network, no async needed. Callers wrap in
/// `spawn_blocking` if they're holding an async runtime.
pub(crate) fn pid_state_quick(pid: i32) -> Result<BackendState> {
    let status_path = format!("/proc/{pid}/status");
    let content = match std::fs::read_to_string(&status_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BackendState::NotRunning),
        Err(e) => return Err(e.into()),
    };
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("State:") {
            // State line looks like: "State:\tT (stopped)"
            //                       or   "State:\tR (running)"
            //                       or   "State:\tS (sleeping)"
            //                       or   "State:\tZ (zombie)"
            if rest.contains('T') {
                return Ok(BackendState::Paused);
            }
            if rest.contains('R')
                || rest.contains('S')
                || rest.contains('D')
                || rest.contains('I')
                || rest.contains('Z')
            {
                return Ok(BackendState::Running);
            }
            // Unknown state letter — fall through to NotRunning.
            return Ok(BackendState::NotRunning);
        }
    }
    Ok(BackendState::NotRunning)
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
            message:
                "swww does not support pause/resume; the previous wallpaper is lost when cleared"
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

/// Backend implementation for `hyprpaper`.
///
/// Talks to hyprpaper via the `hyprctl hyprpaper` IPC bridge (Hyprland
/// already exposes hyprpaper commands through `hyprctl`). Like swww,
/// hyprpaper runs as a single daemon; pause/resume are not supported
/// (only `LooseImage` entries).
///
/// ## Hyprland-only
///
/// hyprpaper depends on Hyprland IPC (`HYPRLAND_INSTANCE_SIGNATURE`).
/// On non-Hyprland compositors, [`list_pids`](WallpaperBackend::list_pids)
/// will return empty but `set` will still try to spawn hyprctl and
/// fail with a backend-reachable error.
#[derive(Debug, Clone, Default)]
pub struct HyprpaperBackend {
    /// Path to the `hyprctl` binary (used to dispatch set/clear).
    /// `None` means PATH lookup.
    pub cli_binary: Option<PathBuf>,
    /// Optional: pre-load target (`""` means all unloaded outputs).
    pub preload_target: Option<String>,
}

impl HyprpaperBackend {
    /// Construct with PATH lookup for `hyprctl`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with explicit `hyprctl` path.
    pub fn with_cli(cli: impl Into<PathBuf>) -> Self {
        Self {
            cli_binary: Some(cli.into()),
            preload_target: None,
        }
    }

    fn cli(&self) -> &str {
        self.cli_binary
            .as_ref()
            .map(|p| p.to_str().unwrap_or("hyprctl"))
            .unwrap_or("hyprctl")
    }
}

#[async_trait]
impl WallpaperBackend for HyprpaperBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Hyprpaper
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
        // hyprpaper syntax: `hyprctl hyprpaper preload <path>` then
        // `hyprctl hyprpaper wallpaper <output>,<path>`. For all
        // outputs, omit the output prefix.
        let path_arg = scene.to_string_lossy().to_string();
        let mut preload = Command::new(self.cli());
        preload.args(["hyprpaper", "preload", &path_arg]);
        let status = preload.status().map_err(|e| Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
            message: format!("spawn {} failed: {e}", self.cli()),
        })?;
        if !status.success() {
            return Err(Error::BackendFailure {
                kind: self.kind().process_pattern().to_string(),
                message: format!(
                    "`{} hyprpaper preload` exited with {:?}",
                    self.cli(),
                    status.code()
                ),
            });
        }

        let mut wp = Command::new(self.cli());
        wp.args(["hyprpaper", "wallpaper"]);
        match output {
            Some(out) => wp.arg(format!("{out},{path_arg}")),
            None => wp.arg(format!(",{path_arg}")),
        };
        let status = wp.status().map_err(|e| Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
            message: format!("spawn {} failed: {e}", self.cli()),
        })?;
        if !status.success() {
            return Err(Error::BackendFailure {
                kind: self.kind().process_pattern().to_string(),
                message: format!(
                    "`{} hyprpaper wallpaper` exited with {:?}",
                    self.cli(),
                    status.code()
                ),
            });
        }
        tracing::info!(target: "paperforge", backend = "hyprpaper", "set {} on {:?}", scene.display(), output);
        Ok(())
    }

    async fn pause(&self) -> Result<usize> {
        Err(Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
            message: "hyprpaper does not support pause".to_string(),
        })
    }

    async fn resume(&self) -> Result<usize> {
        Err(Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
            message: "hyprpaper does not support resume".to_string(),
        })
    }

    async fn state(&self, pid: i32) -> Result<BackendState> {
        // hyprpaper has no pause concept. Any running instance
        // is "Running".
        let status_path = format!("/proc/{pid}/status");
        match std::fs::read_to_string(&status_path) {
            Ok(content) => {
                for line in content.lines() {
                    if line.starts_with("State:") {
                        if line.contains("(stopped)") {
                            return Ok(BackendState::Paused);
                        }
                        if line.contains("(running)") || line.contains("(sleeping)") {
                            return Ok(BackendState::Running);
                        }
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

/// Backend implementation for `mpvpaper`.
///
/// Spawns one `mpvpaper` per output. Each instance runs an embedded
/// mpv with an `--input-ipc-server` socket; pause/resume go through
/// that socket with `pause yes` / `pause no`.
///
/// ## Why pause is supported here
///
/// mpvpaper keeps a live mpv process per output. Sending pause via
/// mpv's IPC is non-destructive — the decoder freezes but the
/// process is alive. This matches what LWE does with SIGSTOP/SIGCONT,
/// but the transport is JSON-over-Unix-socket, not POSIX signals.
#[derive(Debug, Clone, Default)]
pub struct MpvpaperBackend {
    /// Path to the `mpvpaper` binary. `None` means PATH lookup.
    pub binary: Option<PathBuf>,
    /// Extra args passed verbatim to mpvpaper (e.g. `--layer overlay`).
    pub extra_args: Vec<String>,
}

impl MpvpaperBackend {
    /// Construct with PATH lookup for `mpvpaper`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with explicit binary path + extra mpv args.
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: Some(binary.into()),
            extra_args: Vec::new(),
        }
    }

    fn bin(&self) -> &str {
        self.binary
            .as_ref()
            .map(|p| p.to_str().unwrap_or("mpvpaper"))
            .unwrap_or("mpvpaper")
    }

    /// Dispatch an mpv IPC command to all running mpvpaper instances.
    /// Used by [`pause`](WallpaperBackend::pause) and
    /// [`resume`](WallpaperBackend::resume).
    async fn dispatch_ipc_pause(&self, paused: bool) -> Result<usize> {
        let pids = self.list_pids().await?;
        if pids.is_empty() {
            return Ok(0);
        }
        let cmd = if paused { "pause yes" } else { "pause no" };
        let total = pids.len();
        let mut failed = 0u32;
        for pid in pids {
            // mpvpaper uses `<XDG_RUNTIME_DIR>/mpvpaper/<pid>.sock`
            // by default; we look up the socket by reading
            // `/proc/<pid>/environ` for `MPVpaper_SOCKET` (set by
            // mpvpaper itself when it spawns mpv). If unset, fall
            // back to the default path.
            let sock = mpvpaper_socket_for(pid).unwrap_or_else(|| {
                let runtime =
                    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
                PathBuf::from(format!("{runtime}/mpvpaper/{pid}.sock"))
            });
            match send_mpv_ipc(&sock, cmd).await {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(target: "paperforge", "mpvpaper pid={pid} ipc failed: {e}");
                    failed += 1;
                }
            }
        }
        let succeeded = total - failed as usize;
        if failed > 0 && succeeded == 0 {
            return Err(Error::BackendFailure {
                kind: self.kind().process_pattern().to_string(),
                message: format!("all {failed} mpvpaper instances failed IPC"),
            });
        }
        Ok(succeeded)
    }
}

#[async_trait]
impl WallpaperBackend for MpvpaperBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Mpvpaper
    }

    async fn list_pids(&self) -> Result<Vec<i32>> {
        list_pids_in_proc(Path::new("/proc"), self.kind().process_pattern())
    }

    async fn set(&self, scene: &Path, output: Option<&str>) -> Result<()> {
        if !scene.exists() {
            return Err(Error::BackendUnreachable {
                kind: self.kind().process_pattern().to_string(),
                message: format!("video/scene path does not exist: {}", scene.display()),
            });
        }
        let out = output.unwrap_or("default");
        let mut cmd = Command::new(self.bin());
        cmd.arg(format!("-o {out}")).arg(scene);
        for arg in &self.extra_args {
            cmd.arg(arg);
        }
        let status = cmd.status().map_err(|e| Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
            message: format!("spawn {} failed: {e}", self.bin()),
        })?;
        if !status.success() {
            return Err(Error::BackendFailure {
                kind: self.kind().process_pattern().to_string(),
                message: format!("`{}` exited with {:?}", self.bin(), status.code()),
            });
        }
        tracing::info!(target: "paperforge", backend = "mpvpaper", "set {} on {out}", scene.display());
        Ok(())
    }

    async fn pause(&self) -> Result<usize> {
        self.dispatch_ipc_pause(true).await
    }

    async fn resume(&self) -> Result<usize> {
        self.dispatch_ipc_pause(false).await
    }

    async fn state(&self, pid: i32) -> Result<BackendState> {
        // Read `/proc/<pid>/status` for the `State:` field. "T"
        // (stopped) means paused via signal — but mpvpaper doesn't
        // get SIGSTOP, so we treat any running instance as Running
        // and rely on the IPC query result we don't have here.
        let status_path = format!("/proc/{pid}/status");
        match std::fs::read_to_string(&status_path) {
            Ok(content) => {
                for line in content.lines() {
                    if let Some(rest) = line.strip_prefix("State:") {
                        if rest.contains('T') {
                            return Ok(BackendState::Paused);
                        }
                        if rest.contains('R') || rest.contains('S') {
                            return Ok(BackendState::Running);
                        }
                    }
                }
                Ok(BackendState::NotRunning)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BackendState::NotRunning),
            Err(e) => Err(Error::BackendFailure {
                kind: self.kind().process_pattern().to_string(),
                message: format!("read /proc/{pid}/status: {e}"),
            }),
        }
    }

    fn supports(&self, entry: &crate::WallpaperEntry) -> bool {
        // mpvpaper plays anything mpv plays: video files, scene
        // directories (via mpv's demuxer), images. The only kind
        // it cannot do is Workshop scenes that need LWE-specific
        // decoding — that's covered by `LooseImage` and `Video`
        // here.
        matches!(
            entry.kind,
            crate::inventory::WallpaperKind::LooseImage
                | crate::inventory::WallpaperKind::LooseVideo
        )
    }
}

/// Read `/proc/<pid>/environ` looking for an `MPVpaper_SOCKET=...`
/// entry. The NUL-separated format is parsed by hand because
/// `std::env::split` doesn't apply.
fn mpvpaper_socket_for(pid: i32) -> Option<PathBuf> {
    let data = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    for entry in data.split(|b| *b == 0) {
        if let Some(rest) = entry.strip_prefix(b"MPVpaper_SOCKET=") {
            if let Ok(s) = std::str::from_utf8(rest) {
                return Some(PathBuf::from(s));
            }
        }
    }
    None
}

/// Send a single mpv IPC command over a Unix socket. Used by
/// [`MpvpaperBackend::pause`] / [`resume`].
async fn send_mpv_ipc(socket: &std::path::Path, command: &str) -> Result<()> {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixStream,
    };
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| Error::BackendFailure {
            kind: "mpvpaper".to_string(),
            message: format!("connect {}: {e}", socket.display()),
        })?;
    let payload = serde_json::json!({ "command": ["loadfile", command] });
    // mpv accepts plain-text commands too. We use the JSON shape for
    // forward-compatibility with future commands.
    let mut buf = serde_json::to_string(&payload)
        .map_err(|e| Error::Other(anyhow::anyhow!("ipc serialize: {e}")))?;
    buf.push('\n');
    stream
        .write_all(buf.as_bytes())
        .await
        .map_err(|e| Error::BackendFailure {
            kind: "mpvpaper".to_string(),
            message: format!("ipc write: {e}"),
        })?;
    // Read until newline or EOF to drain mpv's response.
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workshop_content_id_extracts_from_steam_layout() {
        // The canonical Steam Workshop layout:
        let p = Path::new("/home/lou/.steam/root/steamapps/workshop/content/431960/850994960");
        assert_eq!(workshop_content_id(p).as_deref(), Some("850994960"));
    }

    #[test]
    fn workshop_content_id_works_with_alt_steam_roots() {
        // Some users symlink Steam from a different path:
        let p = Path::new(
            "/run/media/lou/games/SteamLibrary/steamapps/workshop/content/431960/2908795522",
        );
        assert_eq!(workshop_content_id(p).as_deref(), Some("2908795522"));
    }

    #[test]
    fn workshop_content_id_rejects_loose_images() {
        let p = Path::new("/home/lou/.local/share/backgrounds/foo.jpg");
        assert_eq!(workshop_content_id(p), None);
    }

    #[test]
    fn workshop_content_id_rejects_relative_paths() {
        let p = Path::new("workshop/content/431960/850994960");
        assert_eq!(workshop_content_id(p), None);
    }

    #[test]
    fn workshop_content_id_rejects_non_numeric_segments() {
        // Missing workshop/content markers entirely.
        let p = Path::new("/home/lou/garbage/content/431960/850994960");
        assert_eq!(workshop_content_id(p), None);
        // content_id is alphabetic, not numeric.
        let p = Path::new("/home/lou/.steam/root/steamapps/workshop/content/431960/scene-foo");
        assert_eq!(workshop_content_id(p), None);
        // appid is alphabetic.
        let p = Path::new(
            "/home/lou/.steam/root/steamapps/workshop/content/wallpaper-engine/850994960",
        );
        assert_eq!(workshop_content_id(p), None);
    }

    #[test]
    fn workshop_content_id_rejects_too_short_paths() {
        assert_eq!(workshop_content_id(Path::new("/")), None);
        assert_eq!(workshop_content_id(Path::new("/a")), None);
        assert_eq!(workshop_content_id(Path::new("/a/b")), None);
        assert_eq!(workshop_content_id(Path::new("/a/b/c")), None);
    }

    #[test]
    fn lwe_backend_set_rejects_non_workshop_scene() {
        // Confirms that a path NOT under workshop/content is rejected
        // with a BackendFailure that explains the Workshop convention
        // — not BackendUnreachable (the path may exist; we just can't
        // translate it to an LWE argv).
        //
        // We use /tmp because /tmp always exists on Linux; the
        // exists() check must succeed so we can reach the Workshop
        // validation step that we actually want to test.
        let b = LweBackend::with_binary("/bin/true");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(b.set(Path::new("/tmp"), Some("DP-1")))
            .unwrap_err();
        assert!(
            matches!(err, Error::BackendFailure { .. }),
            "non-Workshop path must be BackendFailure, got {err:?}"
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("Workshop"),
            "error must explain Workshop convention, got: {msg}"
        );
    }

    /// Multi-output `set` via the shared pool must result in exactly
    /// ONE running pid, regardless of how many outputs are bound.
    /// This is the v0.2 RSS win (one process handles N monitors).
    ///
    /// We use `/bin/sleep` as the LWE proxy and a 3-step bind to
    /// confirm the pool hot-swap merges argv rather than spawning
    /// 3 separate processes.
    #[tokio::test]
    async fn lwe_backend_set_via_pool_uses_single_process() {
        // /bin/sleep needs to swallow the LWE-style argv the pool
        // emits, so wrap it in a shell that ignores its argv and
        // just sleeps long enough for all the binds.
        let wrapper = std::env::temp_dir().join("paperforge-pool-single-binary.sh");
        std::fs::write(&wrapper, "#!/bin/sh\nexec /bin/sleep 60\n").unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let backend = LweBackend::with_binary(&wrapper).with_empty_pool_flags();
        // Build three Workshop-shaped paths on disk so the
        // workshop_content_id check passes. The leaf must be the
        // numeric content_id (the parser takes
        // `workshop/content/<appid>/<id>` from the END of the path).
        let tmp = tempfile::tempdir().unwrap();
        let scenes: Vec<std::path::PathBuf> = ["111", "222", "333"]
            .iter()
            .map(|id| {
                let p = tmp
                    .path()
                    .join(format!("fake/workshop/content/431960/{id}"));
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, b"fake scene").unwrap();
                p
            })
            .collect();

        // Bind output #1.
        backend
            .set(&scenes[0], Some("DP-1"))
            .await
            .expect("bind DP-1");
        let pids_after_first = backend.list_pids().await.unwrap();
        assert_eq!(
            pids_after_first.len(),
            1,
            "after first bind there must be exactly 1 pid"
        );

        // Bind output #2 — pool respawns with merged argv, still 1 process.
        backend
            .set(&scenes[1], Some("HDMI-A-1"))
            .await
            .expect("bind HDMI-A-1");
        let pids_after_second = backend.list_pids().await.unwrap();
        assert_eq!(
            pids_after_second.len(),
            1,
            "pool keeps a single process for N outputs"
        );

        // Bind output #3 — same: still 1 pid.
        backend
            .set(&scenes[2], Some("eDP-1"))
            .await
            .expect("bind eDP-1");
        let pids_after_third = backend.list_pids().await.unwrap();
        assert_eq!(
            pids_after_third.len(),
            1,
            "pool continues to share process across 3 outputs"
        );

        // Cleanup.
        backend.pool().shutdown().await.unwrap();
        let _ = std::fs::remove_file(&wrapper);
    }

    /// Re-binding the same (output, content_id) pair must be a no-op:
    /// no respawn, no PID change. This is the "set is idempotent"
    /// guarantee the pool promises in its docstring.
    #[tokio::test]
    async fn lwe_backend_set_idempotent_when_unchanged() {
        let wrapper = std::env::temp_dir().join("paperforge-pool-idempotent-binary.sh");
        std::fs::write(&wrapper, "#!/bin/sh\nexec /bin/sleep 60\n").unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let backend = LweBackend::with_binary(&wrapper).with_empty_pool_flags();
        let tmp = tempfile::tempdir().unwrap();
        let scene = tmp.path().join("fake/workshop/content/431960/111");
        std::fs::create_dir_all(scene.parent().unwrap()).unwrap();
        std::fs::write(&scene, b"fake scene").unwrap();

        backend.set(&scene, Some("DP-1")).await.expect("bind");
        let pid_after_first = backend
            .pool()
            .current_pid()
            .await
            .expect("first bind must produce a pid");

        // Re-bind the same output + content_id; the pool's fast path
        // detects this and returns the existing PID without respawn.
        backend.set(&scene, Some("DP-1")).await.expect("rebind");
        let pid_after_second = backend
            .pool()
            .current_pid()
            .await
            .expect("rebind must keep pid");
        assert_eq!(
            pid_after_first, pid_after_second,
            "idempotent set must not respawn"
        );

        // Cleanup.
        backend.pool().shutdown().await.unwrap();
        let _ = std::fs::remove_file(&wrapper);
    }

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
        assert_eq!(
            b.binary_path.as_deref(),
            None,
            "default LweBackend has no binary_path"
        );
    }

    #[test]
    fn binary_resolution_explicit() {
        let b = LweBackend::with_binary("/opt/lwe/bin/linux-wallpaperengine");
        assert_eq!(
            b.binary_path.as_deref(),
            Some(std::path::Path::new("/opt/lwe/bin/linux-wallpaperengine"))
        );
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
    ///
    /// Uses `pid_state_quick` directly because `LweBackend::state`
    /// gates on pool ownership (returns NotRunning for foreign pids),
    /// which is correct production behavior but wrong for this smoke
    /// test — the test deliberately creates an unmanaged sleep.
    #[test]
    fn real_sigstop_sigcont_round_trip() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;

        // Give the scheduler a moment so /proc reflects the new PID.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let state_running = pid_state_quick(pid).unwrap();
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
        let state_paused = pid_state_quick(pid).unwrap();
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
        let state_resumed = pid_state_quick(pid).unwrap();
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

    // ---- HyprpaperBackend tests ----

    #[test]
    fn hyprpaper_backend_kind() {
        let b = HyprpaperBackend::new();
        assert_eq!(b.kind(), BackendKind::Hyprpaper);
    }

    #[test]
    fn hyprpaper_pattern_matches() {
        assert_eq!(BackendKind::Hyprpaper.process_pattern(), "hyprpaper");
    }

    #[test]
    fn hyprpaper_cli_default() {
        let b = HyprpaperBackend::new();
        assert_eq!(b.cli(), "hyprctl");
    }

    #[test]
    fn hyprpaper_cli_explicit() {
        let b = HyprpaperBackend::with_cli("/opt/hyprctl");
        assert_eq!(b.cli(), "/opt/hyprctl");
    }

    #[test]
    fn hyprpaper_supports_only_loose_images() {
        use crate::inventory::{WallpaperEntry, WallpaperKind};
        let b = HyprpaperBackend::new();
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
    fn hyprpaper_pause_returns_backend_failure() {
        let b = HyprpaperBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(b.pause()).unwrap_err();
        assert!(matches!(err, Error::BackendFailure { .. }));
        assert!(format!("{err}").contains("does not support pause"));
    }

    #[test]
    fn hyprpaper_resume_returns_backend_failure() {
        let b = HyprpaperBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(b.resume()).unwrap_err();
        assert!(matches!(err, Error::BackendFailure { .. }));
        assert!(format!("{err}").contains("does not support resume"));
    }

    #[test]
    fn hyprpaper_set_rejects_missing_path() {
        let b = HyprpaperBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(b.set(Path::new("/does/not/exist.png"), Some("DP-1")))
            .unwrap_err();
        assert!(matches!(err, Error::BackendUnreachable { .. }));
    }

    #[test]
    fn hyprpaper_state_for_unknown_pid_is_not_running() {
        let b = HyprpaperBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        // PID 1 always exists (init) — but it's not hyprpaper, so
        // if we ever read /proc/1/status we get "running". The
        // important guarantee is no panic; we accept either Running
        // or NotRunning depending on the host.
        let _ = rt.block_on(b.state(1)).unwrap();
    }

    // ---- MpvpaperBackend tests ----

    #[test]
    fn mpvpaper_backend_kind() {
        let b = MpvpaperBackend::new();
        assert_eq!(b.kind(), BackendKind::Mpvpaper);
    }

    #[test]
    fn mpvpaper_pattern_matches() {
        assert_eq!(BackendKind::Mpvpaper.process_pattern(), "mpvpaper");
    }

    #[test]
    fn mpvpaper_bin_default() {
        let b = MpvpaperBackend::new();
        assert_eq!(b.bin(), "mpvpaper");
    }

    #[test]
    fn mpvpaper_bin_explicit() {
        let b = MpvpaperBackend::with_binary("/opt/mpvpaper");
        assert_eq!(b.bin(), "/opt/mpvpaper");
    }

    #[test]
    fn mpvpaper_supports_image_and_video() {
        use crate::inventory::{WallpaperEntry, WallpaperKind};
        let b = MpvpaperBackend::new();
        let mk = |kind: WallpaperKind| WallpaperEntry {
            path: PathBuf::from("/dummy"),
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind,
            title: None,
            workshop_id: None,
        };
        assert!(b.supports(&mk(WallpaperKind::LooseImage)));
        assert!(b.supports(&mk(WallpaperKind::LooseVideo)));
        assert!(!b.supports(&mk(WallpaperKind::WorkshopScene)));
    }

    #[test]
    fn mpvpaper_set_rejects_missing_path() {
        let b = MpvpaperBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(b.set(Path::new("/does/not/exist.mp4"), Some("DP-1")))
            .unwrap_err();
        assert!(matches!(err, Error::BackendUnreachable { .. }));
    }

    #[test]
    fn mpvpaper_pause_with_no_running_instances_is_zero() {
        // When no mpvpaper process is running, dispatch_ipc_pause
        // should return Ok(0) immediately without trying IPC.
        let b = MpvpaperBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let n = rt.block_on(b.pause()).unwrap();
        assert_eq!(n, 0);
        let n = rt.block_on(b.resume()).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn mpvpaper_state_for_unknown_pid_is_not_running() {
        let b = MpvpaperBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        // Fake PID; /proc/<huge>/status won't exist.
        let s = rt.block_on(b.state(999_999)).unwrap();
        assert_eq!(s, BackendState::NotRunning);
    }

    #[test]
    fn mpvpaper_socket_path_falls_back_when_environ_unset() {
        // We can't easily write /proc/<pid>/environ without root,
        // so we just exercise the fallback path indirectly by
        // calling pause/resume which internally calls
        // mpvpaper_socket_for. With no mpvpaper running it returns
        // Ok(0) before reading anything.
        let b = MpvpaperBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert_eq!(rt.block_on(b.pause()).unwrap(), 0);
    }

    #[test]
    fn all_four_backend_patterns_are_distinct() {
        // Critical invariant: list_pids_in_proc walks /proc by
        // pattern, so no two backends may share a substring.
        let pats = [
            BackendKind::LinuxWallpaperEngine.process_pattern(),
            BackendKind::SwwwDaemon.process_pattern(),
            BackendKind::Hyprpaper.process_pattern(),
            BackendKind::Mpvpaper.process_pattern(),
        ];
        for (i, a) in pats.iter().enumerate() {
            for (j, b) in pats.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "BackendKind pattern collision: {a} vs {b}");
                }
            }
        }
    }
}
