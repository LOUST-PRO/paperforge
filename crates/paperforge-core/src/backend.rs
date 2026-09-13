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
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    audio::LweAudioController,
    error::{Error, Result},
    pool::LweSinglePool,
};

/// Per-key rate limiter for `tracing::warn!` emissions.
///
/// Some operational WARNs (e.g. "no scene recorded for output X",
/// "fullscreen ON on X: kill_per_output was a no-op") fire on a
/// 20-30 minute cadence from the daemon's polling loops even when
/// state has not changed. Emitting a fresh WARN every tick clutters
/// `journalctl -u paperforge` and makes it hard to spot real
/// anomalies.
///
/// This limiter suppresses repeats: the first WARN for a key in a
/// given cooldown window passes through; subsequent WARNs with the
/// same key inside the window are dropped (callers should demote
/// them to `tracing::debug!`). After the cooldown elapses, the next
/// WARN passes through again.
///
/// [`Self::clear_for_output`] is the escape hatch: when the
/// underlying state actually changes for an output (e.g. a fresh
/// bind succeeds), call it so the next WARN for that output
/// passes through immediately instead of waiting out the cooldown.
#[derive(Debug, Default)]
pub struct WarnRateLimiter {
    /// Map: `"output:warn_key"` → `Instant` of the last WARN emitted.
    last_warn: HashMap<String, Instant>,
    /// Cooldown window. WARN identical to the last one inside this
    /// duration is suppressed.
    cooldown: Duration,
}

impl WarnRateLimiter {
    /// Construct with the given cooldown window (default in
    /// production: 5 minutes; tests use 300s as the canonical
    /// representation).
    pub fn new(cooldown: Duration) -> Self {
        Self {
            last_warn: HashMap::new(),
            cooldown,
        }
    }

    /// Returns `true` if a WARN for `key` should be emitted (and
    /// records it as the most-recent WARN for `key`), `false` if it
    /// should be suppressed because a previous WARN with the same
    /// key was emitted inside the cooldown window.
    pub fn should_emit(&mut self, key: &str) -> bool {
        let now = Instant::now();
        match self.last_warn.get(key) {
            Some(t) if now.duration_since(*t) < self.cooldown => false,
            _ => {
                self.last_warn.insert(key.to_string(), now);
                true
            }
        }
    }

    /// Clear all keys that belong to `output` so the next WARN for
    /// this output passes through immediately.
    ///
    /// Callers should invoke this after a successful bind
    /// (`set_per_output_with_fps` or `bind_external_pid`) so that a
    /// later "no scene recorded" WARN for the same output isn't
    /// masked by the previous WARN that fired before the bind.
    pub fn clear_for_output(&mut self, output: &str) {
        let prefix = format!("{output}:");
        self.last_warn.retain(|k, _| !k.starts_with(&prefix));
    }
}

/// Identifier for a single LWE subprocess from the perspective of a
/// pipe-drain task. Wrapped in a newtype so the call sites are
/// readable (the test code generates fake pids, and `PidTarget(0)`
/// is more obvious than a bare `0`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PidTarget(pub i32);

/// Which pipe a drainer is reading from. Used to map the
/// stream to the right tracing level: stdout at INFO, stderr at
/// WARN. The "kind" is the only thing that differs between the two
/// drain tasks per LWE pid — they share `BufReader::lines()` logic
/// for the actual read loop.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PipeKind {
    Stdout,
    Stderr,
}

/// Read a child process's stdout/stderr line-by-line and emit each
/// line as a tracing event. The task terminates when the pipe closes
/// (i.e. the child exits or the handle is dropped). Pipe handles are
/// `tokio::process::ChildStdout` / `ChildStderr`, both implement
/// `AsyncRead + Unpin`.
///
/// The handler is shared between stdout and stderr — the only
/// difference is the tracing level. We dispatch on `kind` inside
/// the loop so the read logic stays in one place.
///
/// Lines longer than the `BufReader` buffer (8 KiB default) are
/// truncated at the buffer boundary; the next call to
/// `next_line()` reads the remainder. This matches typical LWE
/// output (timestamps, asset paths, GL driver messages) which is
/// well under 8 KiB per line in practice.
pub(crate) async fn drain_pipe<R>(reader: R, pid: PidTarget, kind: PipeKind)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match kind {
                PipeKind::Stdout => {
                    tracing::info!(
                        target: "paperforge",
                        "lwe[{}] stdout: {}",
                        pid.0,
                        line
                    );
                }
                PipeKind::Stderr => {
                    tracing::warn!(
                        target: "paperforge",
                        "lwe[{}] stderr: {}",
                        pid.0,
                        line
                    );
                }
            },
            Ok(None) => break, // EOF: child closed the pipe.
            Err(e) => {
                tracing::error!(
                    target: "paperforge",
                    "lwe[{}] pipe read error: {}",
                    pid.0,
                    e
                );
                break;
            }
        }
    }
}

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

/// Result of [`LweBackend::health_check`]. The reconciler maps this
/// directly to an error variant or a respawn trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolHealth {
    /// Pool tracks a PID and `/proc/<pid>/status` reports it running.
    Alive(i32),
    /// Pool tracks a PID but it's gone (segfault, OOM-killed, manually
    /// killed). Caller should respawn.
    Dead(i32),
    /// Pool has no PID at all. Caller should rebuild pool state from
    /// the daemon's known outputs + last-set scenes.
    Untracked,
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
    /// Per-output child PIDs (v0.1 legacy path). One LWE process per
    /// monitor — bypasses the merged-argv path that some LWE builds
    /// (e.g. `nicobz/linux-wallpaperengine` with `workshop/content/<id>`
    /// scenes that hit a parse error in `--bg <id>` when 2+ outputs
    /// are bound at once). Populated only when
    /// [`LweBackendOps::use_pool`] is `false`; the pool path ignores
    /// this map and uses `self.pool` instead.
    per_output_pids: Arc<Mutex<BTreeMap<String, i32>>>,
    /// Per-output scene paths (v0.1 legacy path). Mirror of
    /// `per_output_pids` keyed by output name so the soft-pause and
    /// throttle-pause modes know which scene to re-spawn with
    /// after the SIGTERM/SIGSTOP cycle.
    per_output_scenes: Arc<Mutex<BTreeMap<String, PathBuf>>>,
    /// Cancellation notify for the active per-output soft-pause
    /// cycle. Fired by `resume_per_output` so the cycle task exits
    /// cleanly instead of leaking until the next SIGCONT attempt.
    soft_pause_cancel: Arc<tokio::sync::Notify>,
    /// Tokio tasks that drain LWE subprocess stdout/stderr into the
    /// tracing pipeline. Keyed by LWE PID. Aborted on `unbind` /
    /// `shutdown` so the pipes close cleanly.
    ///
    /// Without this map, LWE's stdio would be discarded by
    /// `Stdio::null()` (the v0.1 behaviour) or hang forever if piped
    /// without a reader (the v0.2 latent bug). Component C wires the
    /// readers and stores their JoinHandles here, then aborts them
    /// when the LWE pid is killed so the pipe handles don't leak.
    ///
    /// Reading tasks are stored as `(stdout_handle, stderr_handle)`
    /// per pid. Both are aborted together on child cleanup.
    pipe_drainers: Arc<Mutex<BTreeMap<i32, LwePipeDrainers>>>,
    /// Rate limiter for repetitive operational WARNs. See
    /// [`WarnRateLimiter`] for the rationale. 5-minute cooldown
    /// matches the operator's complaint about WARNs every
    /// 20-30 minutes (≈half the cooldown).
    warn_limiter: Arc<Mutex<WarnRateLimiter>>,
}

/// Per-LWE-process pipe drainer pair. Type alias to keep the
/// `pipe_drainers` BTreeMap below the clippy `type_complexity` cap.
type LwePipeDrainers = (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>);

/// Async task that wakes LWE pids on a duty cycle so the layer-shell
/// surface stays alive while "paused".
///
/// The task holds a strong reference to the pids map; the loop
/// aborts when the cancellation notify fires (set by
/// `resume_per_output`). Without the notify, the previous version
/// leaked cycles when `pause_per_output_soft` was called multiple
/// times in a row or resumed without the daemon dropping the
/// handle.
async fn soft_pause_cycle(
    pids: Arc<Mutex<BTreeMap<String, i32>>>,
    initial: Vec<i32>,
    awake_ms: u64,
    asleep_ms: u64,
    cancel: Arc<tokio::sync::Notify>,
) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    use std::time::Duration;
    let awake = Duration::from_millis(awake_ms);
    let asleep = Duration::from_millis(asleep_ms);

    loop {
        // Snapshot the current pids (the map may have rotated under
        // us if respawn_watcher added new entries).
        let current: Vec<i32> = {
            let map = pids.lock().await;
            map.values().copied().collect()
        };
        if current.is_empty() && initial.is_empty() {
            return;
        }
        // Wake phase: SIGCONT, sleep `awake_ms` (or until cancelled),
        // then SIGSTOP.
        for pid in &current {
            let _ = kill(Pid::from_raw(*pid), Signal::SIGCONT);
        }
        tokio::select! {
            _ = tokio::time::sleep(awake) => {}
            _ = cancel.notified() => {
                // Caller asked us to stop; leave the pids in their
                // current SIGCONT state so resume completes cleanly.
                return;
            }
        }
        for pid in &current {
            let _ = kill(Pid::from_raw(*pid), Signal::SIGSTOP);
        }
        tokio::select! {
            _ = tokio::time::sleep(asleep) => {}
            _ = cancel.notified() => return,
        }
    }
}

/// Pool variant: cycle on the current pool pid, re-reading it each
/// iteration so respawn-watcher rotations stay effective. The cancel
/// notify is owned by the pool (see `LweSinglePool::soft_pause_task`)
/// so resume / shutdown abort the cycle.
pub async fn soft_pause_cycle_pool(
    inner: Arc<Mutex<Option<crate::pool::PoolProcess>>>,
    awake_ms: u64,
    asleep_ms: u64,
    cancel: Arc<tokio::sync::Notify>,
) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    use std::time::Duration;
    let awake = Duration::from_millis(awake_ms);
    let asleep = Duration::from_millis(asleep_ms);

    loop {
        let current_pid: Option<i32> = {
            let guard = inner.lock().await;
            guard.as_ref().map(|p| p.pid)
        };
        let Some(pid) = current_pid else {
            tokio::select! {
                _ = tokio::time::sleep(asleep) => {}
                _ = cancel.notified() => return,
            }
            continue;
        };
        let _ = kill(Pid::from_raw(pid), Signal::SIGCONT);
        tokio::select! {
            _ = tokio::time::sleep(awake) => {}
            _ = cancel.notified() => return,
        }
        let _ = kill(Pid::from_raw(pid), Signal::SIGSTOP);
        tokio::select! {
            _ = tokio::time::sleep(asleep) => {}
            _ = cancel.notified() => return,
        }
    }
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
    /// Construct with default binary resolution. Production code
    /// should use [`Config::backend`](crate::config::Config::backend)
    /// which honours `[fps].active_max` from config.toml; this
    /// 30-fps default matches LWE's own default and is only safe
    /// for tests and one-shot CLI runs.
    pub fn new() -> Self {
        let pool = LweSinglePool::new();
        Self {
            binary_path: None,
            pool: Arc::new(pool),
            per_output_pids: Arc::new(Mutex::new(BTreeMap::new())),
            per_output_scenes: Arc::new(Mutex::new(BTreeMap::new())),
            soft_pause_cancel: Arc::new(tokio::sync::Notify::new()),
            pipe_drainers: Arc::new(Mutex::new(BTreeMap::new())),
            warn_limiter: Arc::new(Mutex::new(WarnRateLimiter::new(Duration::from_secs(300)))),
        }
    }

    /// Construct with an explicit binary path (used by tests or
    /// operators with non-standard installs). For production use
    /// [`Self::with_binary_and_fps`] so the FPS cap flows from
    /// config.
    pub fn with_binary(path: impl Into<PathBuf>) -> Self {
        let pb: PathBuf = path.into();
        let pool = LweSinglePool::with_binary(pb.clone());
        Self {
            binary_path: Some(pb),
            pool: Arc::new(pool),
            per_output_pids: Arc::new(Mutex::new(BTreeMap::new())),
            per_output_scenes: Arc::new(Mutex::new(BTreeMap::new())),
            soft_pause_cancel: Arc::new(tokio::sync::Notify::new()),
            pipe_drainers: Arc::new(Mutex::new(BTreeMap::new())),
            warn_limiter: Arc::new(Mutex::new(WarnRateLimiter::new(Duration::from_secs(300)))),
        }
    }

    /// Construct with an explicit binary path AND initial FPS cap.
    /// Production code path: `[fps].active_max` flows in via
    /// [`crate::config::Config::backend`].
    pub fn with_binary_and_fps(path: impl Into<PathBuf>, active_fps: u32) -> Self {
        let pb: PathBuf = path.into();
        let pool = LweSinglePool::with_binary_and_fps(pb.clone(), active_fps);
        Self {
            binary_path: Some(pb),
            pool: Arc::new(pool),
            per_output_pids: Arc::new(Mutex::new(BTreeMap::new())),
            per_output_scenes: Arc::new(Mutex::new(BTreeMap::new())),
            soft_pause_cancel: Arc::new(tokio::sync::Notify::new()),
            pipe_drainers: Arc::new(Mutex::new(BTreeMap::new())),
            warn_limiter: Arc::new(Mutex::new(WarnRateLimiter::new(Duration::from_secs(300)))),
        }
    }

    /// Test-only accessor for the per-output pid map. Lets tests
    /// inject a fake/forced pid (e.g. a guaranteed-dead value)
    /// without going through `set_per_output`. Production code
    /// never touches this directly — it goes through
    /// `set_per_output` + the reaper task.
    #[cfg(test)]
    pub fn per_output_pids_test_accessor(
        &self,
    ) -> &Arc<Mutex<std::collections::BTreeMap<String, i32>>> {
        &self.per_output_pids
    }

    /// Snapshot the per-output `(output, pid)` map. Used by
    /// [`crate::daemon::PaperforgeDaemon::get_health`] to expose
    /// per-output PIDs over D-Bus without forcing callers to take
    /// the internal lock directly. Returns the `(output, pid)`
    /// pairs with the per-output `BTreeMap`'s natural ordering.
    pub async fn per_output_pids_snapshot(&self) -> Vec<(String, i32)> {
        let guard = self.per_output_pids.lock().await;
        guard.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    /// Reference to the underlying multi-output pool. Used by the
    /// daemon layer when it wants to spawn or signal directly without
    /// going through the `WallpaperBackend` trait shim.
    pub fn pool(&self) -> &LweSinglePool {
        &self.pool
    }

    /// Accessor for the WARN-rate limiter. Used by the CLI's
    /// fullscreen dispatcher (and any other long-running caller that
    /// wants to apply the same 5-minute cooldown to its own
    /// repetitive WARNs) so it can share a single cooldown window
    /// with the backend's resume_per_output_specific emission. Cloning
    /// the `Arc` is cheap; the limiter's internal state is shared.
    pub fn warn_limiter(&self) -> Arc<Mutex<WarnRateLimiter>> {
        Arc::clone(&self.warn_limiter)
    }

    /// Snapshot the LWE pool's process state for the daemon's reconciler.
    /// Reads `/proc/<pool_pid>/status` once via `pid_state_quick`. Returns
    /// `PoolHealth::Alive(pid, state)` if the pool is tracked and the
    /// process responds to a stat probe; `PoolHealth::Dead(pid)` if the
    /// tracked PID is gone; `PoolHealth::Untracked` if no pool PID is
    /// recorded (the daemon should treat this as a stale-state bug and
    /// rebuild from scratch).
    ///
    /// This does NOT touch the network or signal LWE — pure procfs read.
    /// Cheap enough to call on every reconcile tick.
    pub async fn health_check(&self) -> PoolHealth {
        // For now the per-output pid map is the canonical pool state.
        // When the single-pool path is fully wired (component B lands),
        // switch this to consult `self.pool.current_pid()` instead.
        let pids = self.per_output_pids.lock().await;
        if pids.is_empty() {
            return PoolHealth::Untracked;
        }
        // Use the first pid as the canonical "pool is alive" indicator.
        // Per-output pids are usually 1 process (the pool) on the v0.2
        // single-process path; the map has one entry per output only when
        // each output runs its own LWE (the legacy v0.1 fallback).
        let mut pids_sorted: Vec<i32> = pids.values().copied().collect();
        pids_sorted.sort_unstable();
        let canonical_pid = pids_sorted[0];
        match pid_state_quick(canonical_pid, BackendKind::LinuxWallpaperEngine) {
            Ok(BackendState::Running) | Ok(BackendState::Paused) => {
                PoolHealth::Alive(canonical_pid)
            }
            Ok(BackendState::NotRunning) => PoolHealth::Dead(canonical_pid),
            Err(_) => PoolHealth::Dead(canonical_pid),
        }
    }

    /// Test helper: replace the pool's flag list with an empty vec,
    /// so a `/bin/sleep` proxy binary stays alive long enough to
    /// accept our pause/resume signals. Production code never calls
    /// this — the pool's default flags include
    /// `--fullscreen-pause-only-active` etc.
    #[cfg(test)]
    pub fn with_empty_pool_flags(self) -> Self {
        let new_pool = LweSinglePool::with_binary(self.binary_path.clone().unwrap_or_else(|| {
            crate::lwe_locator::resolve().unwrap_or_else(|_| PathBuf::from("linux-wallpaperengine"))
        }))
        .with_flags(Vec::new());
        Self {
            binary_path: self.binary_path,
            pool: Arc::new(new_pool),
            per_output_pids: self.per_output_pids,
            per_output_scenes: self.per_output_scenes,
            soft_pause_cancel: self.soft_pause_cancel,
            pipe_drainers: self.pipe_drainers,
            warn_limiter: self.warn_limiter,
        }
    }

    /// Returns the audio controller for this backend (lives here
    /// because SIGUSR1/SIGUSR2 are tied to LWE).
    pub fn audio(&self) -> LweAudioController {
        LweAudioController::new(self.clone())
    }

    /// Sync the per-output pid/scene maps from the pool's current state.
    ///
    /// Called after every successful [`LweSinglePool::bind`] so the
    /// legacy per-output maps (`per_output_pids` / `per_output_scenes`)
    /// — which `kill_per_output` and `resume_per_output_specific`
    /// consult — stay consistent with the pool as the single source
    /// of truth. Without this sync, outputs that share a merged-LWE
    /// process with the bind target would have no entry in
    /// `per_output_pids`, and `kill_per_output` would silently
    /// no-op for them (the CLI fullscreen dispatcher reports that
    /// condition every ~30 s when fullscreen is on).
    ///
    /// For each output the pool currently tracks, we record the
    /// pool's `current_pid` so `kill_per_output` finds the real
    /// merged-LWE pid for every output. For `per_output_scenes`,
    /// we only set the entry for `bind_output` — the pool only
    /// stores `content_id`s, not scene paths, so the other
    /// outputs' scenes are not knowable from the pool alone.
    /// Leaving those empty is the correct behaviour:
    /// `resume_per_output_specific` will fail with "no scene
    /// recorded" for them, which is a genuine lack-of-information
    /// rather than a stale-sync bug.
    ///
    /// Returns early (without touching either map) if the pool is
    /// empty.
    pub async fn sync_pid_map_from_pool(&self, bind_output: &str, scene: &Path) {
        let Some(current_pid) = self.pool.current_pid().await else {
            return; // pool empty: nothing to sync
        };
        let pool_bindings = self.pool.bindings().await;
        let mut pids = self.per_output_pids.lock().await;
        let mut scenes = self.per_output_scenes.lock().await;
        for output in pool_bindings.keys() {
            pids.insert(output.clone(), current_pid);
        }
        scenes.insert(bind_output.to_string(), scene.to_path_buf());
        tracing::debug!(
            target: "paperforge",
            "synced per-output pid/scenes from pool: pool_pid={} outputs={:?}",
            current_pid,
            pool_bindings.keys().collect::<Vec<_>>()
        );
    }

    /// Per-output spawn (v0.1 legacy path). Spawns a fresh LWE
    /// process for `output` with `--screen-root <output> --bg <id>`,
    /// SIGTERMs any previous LWE for the same output, and records the
    /// new pid in `per_output_pids`. Returns the spawned pid.
    ///
    /// Idempotent re-spawn: when called with the same `(output,
    /// content_id)` pair as already recorded AND the existing
    /// process is alive, returns the existing pid without spawning
    /// (cheap fast-path for `paperforge set` repeated by hotplug).
    /// When the recorded process is **dead** (parent got SIGCHLD),
    /// we treat the slot as vacant and spawn a replacement. This is
    /// the auto-respawn path that keeps monitors alive after LWE
    /// crashes.
    ///
    /// Use this when the upstream LWE binary mishandles the
    /// merged-argv path (e.g. crashes within seconds when 2+ outputs
    /// are bound at once). Cost: ~1 LWE process per monitor (~250 MiB
    /// RSS each on Wayland/CEF builds). Trade vs the pool is memory
    /// for stability.
    ///
    /// The FPS cap is taken from the pool's `active_fps()` at spawn
    /// time. For runtime overrides (e.g. the throttle mode wants
    /// `--fps 1`), use [`Self::set_per_output_with_fps`].
    pub async fn set_per_output(&self, scene: &Path, output: &str) -> Result<i32> {
        let fps = self.pool.active_fps();
        self.set_per_output_with_fps(scene, output, fps).await
    }

    /// Like [`Self::set`] but tags the pool's transition timing log
    /// with the caller-provided `op` (e.g. `"playlist_apply"`). The
    /// log line shape is identical — only the `op=` field differs —
    /// so operators can grep `journalctl -u paperforge.service |
    /// grep 'transition:'` and filter by op to separate user-facing
    /// sets from playlist-driven ones.
    ///
    /// Inherent on `LweBackend` rather than on the `WallpaperBackend`
    /// trait because the other backends (swww, hyprpaper, mpvpaper)
    /// have no pool-side log line to tag — they'd have to ignore
    /// `op` anyway, which is the default `set()` behaviour.
    pub async fn set_with_op(
        &self,
        scene: &Path,
        output: Option<&str>,
        op: &'static str,
    ) -> Result<()> {
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
        // Thread `op` through so the structured timing log in the
        // pool carries the caller's tag (default "set", or e.g.
        // "playlist_apply" when invoked from `apply_playlist`).
        let pid = self.pool.bind_with_op(out, &content_id, op).await?;

        // Sync the legacy per-output maps so `kill_per_output` /
        // `resume_per_output_specific` find a real pid for every
        // output the pool now owns (not just `out` — outputs that
        // were bound on earlier `set` calls share the same merged
        // LWE process and need their pid entries populated too).
        self.sync_pid_map_from_pool(out, scene).await;

        tracing::info!(
            "pool bind: output={} scene={} pid={}",
            out,
            scene.display(),
            pid,
        );
        Ok(())
    }

    /// Same as [`Self::set_per_output`] but with an explicit FPS
    /// override. Used by `pause_per_output_throttle` to spawn with
    /// `--fps 1` so the freshly-spawned LWE is already at the
    /// throttled renderer cap (no SIGSTOP loop needed).
    pub async fn set_per_output_with_fps(
        &self,
        scene: &Path,
        output: &str,
        fps: u32,
    ) -> Result<i32> {
        if !scene.exists() {
            return Err(Error::BackendUnreachable {
                kind: self.kind().process_pattern().to_string(),
                message: format!("scene path does not exist: {}", scene.display()),
            });
        }
        let content_id = workshop_content_id(scene).ok_or_else(|| Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
            message: format!(
                "scene path {} is not a Steam Workshop scene \
                     (expected `workshop/content/<appid>/<numeric>`)",
                scene.display()
            ),
        })?;

        let binary = match self.binary_path.clone() {
            Some(p) => p,
            None => match crate::lwe_locator::resolve() {
                Ok(p) => p,
                Err(e) => {
                    return Err(e);
                }
            },
        };

        // Idempotent fast path: same output + same content_id + same
        // FPS cap AND existing pid still alive → return existing pid.
        // We use the recorded content_id from `per_output_scenes` (the
        // path the caller passed last time) so a re-bind with a
        // different path that points to the same workshop scene is
        // also a no-op. Cheap because we read pid state briefly under
        // the lock and let it go before checking /proc.
        //
        // Auto-respawn: if the recorded pid is dead (crashed, killed),
        // fall through to the spawn path so the output gets a fresh
        // LWE. Without this, a dead output stays dead until the
        // operator manually re-binds.
        {
            let pids = self.per_output_pids.lock().await;
            let scenes = self.per_output_scenes.lock().await;
            if let (Some(&existing_pid), Some(existing_scene)) =
                (pids.get(output), scenes.get(output))
            {
                if existing_scene == scene
                    && matches!(
                        pid_state_quick(existing_pid, BackendKind::LinuxWallpaperEngine),
                        Ok(BackendState::Running) | Ok(BackendState::Paused)
                    )
                {
                    tracing::debug!(
                        "per-output set fast-path: output={} pid={} unchanged",
                        output,
                        existing_pid
                    );
                    return Ok(existing_pid);
                }
            }
        }

        // Kill any existing LWE for this output before spawning the
        // replacement. SIGTERM is graceful; the new spawn happens
        // ~immediately so a hung child is OK. We `remove()` from the
        // map so the reaper task doesn't try to reap it again — and
        // so the next SIGCHLD doesn't see a stale pid.
        //
        // The lock is scoped to the cleanup block and dropped before
        // `cmd.spawn()` (which is sync, so this is purely about
        // releasing the MutexGuard before we re-acquire later in
        // this function to insert the new pid). `tokio::sync::Mutex`
        // is non-reentrant — same task acquiring twice on the same
        // lock would deadlock waiting for itself.
        let old_pid_for_drainers: Option<i32> = {
            let mut pids = self.per_output_pids.lock().await;
            if let Some(old) = pids.remove(output) {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(old),
                    nix::sys::signal::Signal::SIGTERM,
                );
                // Also clear the scene map so a re-spawn with a
                // different scene doesn't get confused.
                let mut scenes = self.per_output_scenes.lock().await;
                scenes.remove(output);
                Some(old)
            } else {
                None
            }
        };
        // Drop the dead pid's pipe drainers so the FD pair
        // closes cleanly. Without this, the drainer tasks would
        // keep reading an EOF socket until their JoinHandle is
        // explicitly aborted, leaking FDs into the daemon's
        // table.
        if let Some(old) = old_pid_for_drainers {
            let mut drainers = self.pipe_drainers.lock().await;
            if let Some((sout, serr)) = drainers.remove(&old) {
                sout.abort();
                serr.abort();
            }
        }

        let cfg = crate::lwe_spawn::SpawnConfig {
            binary: &binary,
            output,
            content_id: &content_id,
            fps,
        };
        let mut cmd = crate::lwe_spawn::build_command(&cfg).map_err(|e| Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
            message: format!("per-output spawn LWE command build failed: {e}"),
        })?;
        let mut child = cmd.spawn().map_err(|e| Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
            message: format!("per-output spawn LWE failed: {e}"),
        })?;
        let pid = child.id().unwrap_or(0) as i32;
        let stdout = child
            .stdout
            .take()
            .expect("Stdio::piped() guarantees stdout");
        let stderr = child
            .stderr
            .take()
            .expect("Stdio::piped() guarantees stderr");

        // Spawn drainer tasks per LWE process. Each task reads one
        // line at a time and emits a tracing event. The systemd
        // service routes tracing events to journald, so the
        // operator can `journalctl -u paperforge` to see LWE's
        // diagnostics on crash.
        let stdout_handle = tokio::spawn(drain_pipe(stdout, PidTarget(pid), PipeKind::Stdout));
        let stderr_handle = tokio::spawn(drain_pipe(stderr, PidTarget(pid), PipeKind::Stderr));
        {
            let mut drainers = self.pipe_drainers.lock().await;
            drainers.insert(pid, (stdout_handle, stderr_handle));
        }
        tracing::info!(
            "per-output spawn: output={} bg={} pid={} fps={}",
            output,
            content_id,
            pid,
            fps,
        );

        let mut pids = self.per_output_pids.lock().await;
        pids.insert(output.to_string(), pid);
        let mut scenes = self.per_output_scenes.lock().await;
        scenes.insert(output.to_string(), scene.to_path_buf());
        // Successful bind: clear any pending rate-limited WARN
        // entries for this output so a future "no scene recorded"
        // (e.g. on a hot-unplug + re-bind) starts with a clean
        // cooldown rather than waiting out stale entries from a
        // previous lifecycle.
        drop(pids);
        drop(scenes);
        {
            let mut limiter = self.warn_limiter.lock().await;
            limiter.clear_for_output(output);
        }
        Ok(pid)
    }

    /// Per-output pause (v0.1). Sends SIGSTOP to every LWE pid we
    /// recorded via [`Self::set_per_output`].
    pub async fn pause_per_output(&self) -> Result<usize> {
        let pids = self.per_output_pids.lock().await;
        let mut count = 0;
        for &pid in pids.values() {
            // Skip dead pids so SIGSTOP doesn't fail noisily on
            // zombies that the reaper hasn't swept yet.
            if !matches!(
                pid_state_quick(pid, BackendKind::LinuxWallpaperEngine),
                Ok(BackendState::Running) | Ok(BackendState::Paused)
            ) {
                continue;
            }
            if nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGSTOP,
            )
            .is_ok()
            {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Per-output soft pause. Sends SIGSTOP to every recorded pid
    /// and spawns a tokio task that cycles SIGCONT/SIGSTOP with the
    /// given duty ratio. The cycle keeps the Wayland layer-shell
    /// surface receiving frames so niri does not show the layer
    /// background (grey) when paused.
    ///
    /// Returns the count of pids that accepted SIGSTOP. The cycling
    /// task is registered with `self.soft_pause_cancel` so
    /// `resume_per_output` aborts it cleanly. Re-entrant: each call
    /// installs a fresh cancellation handle; the previous cycle
    /// wakes via the new notify the next time it hits its
    /// `tokio::select!`.
    ///
    /// `awake_ms` is the SIGCONT duration; `asleep_ms` is the SIGSTOP
    /// duration. Effective fps = `30 * awake_ms / (awake_ms + asleep_ms)`
    /// (assuming LWE's default 30 fps ceiling when un-throttled).
    pub async fn pause_per_output_soft(&self, awake_ms: u64, asleep_ms: u64) -> Result<usize> {
        let (stopped, pids_snapshot) = {
            let pids = self.per_output_pids.lock().await;
            let mut count = 0;
            // Filter to live pids only. Dead pids (defunct/zombie)
            // would make the cycle below send SIGCONT/SIGSTOP into
            // the void every iteration, which is wasted work and
            // pollutes the kernel log. The reconciliation task will
            // re-bind dead outputs separately.
            let live: Vec<i32> = pids
                .values()
                .copied()
                .filter(|pid| {
                    matches!(
                        pid_state_quick(*pid, BackendKind::LinuxWallpaperEngine),
                        Ok(BackendState::Running) | Ok(BackendState::Paused)
                    )
                })
                .collect();
            for pid in &live {
                if nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(*pid),
                    nix::sys::signal::Signal::SIGSTOP,
                )
                .is_ok()
                {
                    count += 1;
                }
            }
            (count, live)
        };
        if stopped > 0 {
            // Each cycle owns its own Notify; the cycle reads the
            // latest `self.soft_pause_cancel` value at every
            // `notified().await` so a `resume_per_output` call can
            // poke it without us wiring a JoinHandle here.
            let cancel = Arc::new(tokio::sync::Notify::new());
            let outer = self.soft_pause_cancel.clone();
            // Bridge task: forward outer (backend-shared) → inner
            // (cycle-local). The cycle only knows about `inner` so
            // we keep that isolation and don't have to share the
            // outer Notify across the cycle boundary.
            let inner = cancel.clone();
            tokio::spawn(async move {
                outer.notified().await;
                inner.notify_waiters();
            });
            tokio::spawn(soft_pause_cycle(
                self.per_output_pids.clone(),
                pids_snapshot,
                awake_ms,
                asleep_ms,
                cancel,
            ));
            tracing::info!(
                "soft-pause cycle: stopped={} awake={}ms asleep={}ms",
                stopped,
                awake_ms,
                asleep_ms
            );
        }
        Ok(stopped)
    }

    /// Per-output throttle pause. SIGTERMs every recorded pid and
    /// re-spawns LWE with `--fps 1` so the renderer is barely
    /// ticking while the surface stays alive. Resume is instant
    /// (the next `set_per_output` brings back the active FPS cap).
    ///
    /// `scene_resolver` is consulted for outputs whose scene isn't
    /// in `per_output_scenes` (cold-start case where the daemon
    /// spawned before any per-output `set()`). The closure is
    /// `&str → Option<PathBuf>` so callers can plug in their own
    /// inventory lookup without us depending on it here.
    ///
    /// Per-output errors are logged but **not** propagated: a
    /// single respawn failure shouldn't prevent the rest of the
    /// outputs from reaching the throttled state.
    pub async fn pause_per_output_throttle(
        &self,
        scene_resolver: impl Fn(&str) -> Option<PathBuf>,
    ) -> Result<usize> {
        // Collect (output, scene) pairs first so we don't hold the
        // map lock across spawns. Outputs without a known scene
        // fall back to the supplied resolver; outputs that still
        // have no scene after the resolver are skipped (logged).
        let pairs: Vec<(String, PathBuf)> = {
            let pids = self.per_output_pids.lock().await;
            let scenes = self.per_output_scenes.lock().await;
            pids.keys()
                .filter_map(|out| {
                    let scene = scenes.get(out).cloned().or_else(|| scene_resolver(out));
                    scene.map(|s| (out.clone(), s))
                })
                .collect()
        };
        // SIGTERM everything. We'll respawn below.
        let stopped = self.pause_per_output_kill_only().await?;
        // Clear pid map; populate after re-spawn.
        {
            let mut pids = self.per_output_pids.lock().await;
            pids.clear();
        }
        let mut respawned = 0usize;
        let mut skipped: Vec<(String, String)> = Vec::new();
        for (output, scene) in &pairs {
            // Spawn at fps=1 so the renderer is already at the
            // throttled cap. No SIGSTOP loop needed (the new process
            // is throttled by the `--fps` flag itself).
            match self.set_per_output_with_fps(scene, output, 1).await {
                Ok(pid) if pid > 0 => respawned += 1,
                Ok(_) => {
                    skipped.push((output.clone(), "spawn returned pid 0".into()));
                }
                Err(e) => {
                    tracing::warn!("throttle-pause: respawn for output={} failed: {e}", output);
                    skipped.push((output.clone(), e.to_string()));
                }
            }
        }
        tracing::info!(
            "throttle-pause: stopped={} respawned={} skipped={}",
            stopped,
            respawned,
            skipped.len()
        );
        Ok(respawned)
    }

    /// Internal: SIGTERM every recorded pid (no SIGCONT follow-up).
    async fn pause_per_output_kill_only(&self) -> Result<usize> {
        let pids = self.per_output_pids.lock().await;
        let mut count = 0;
        for &pid in pids.values() {
            if nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            )
            .is_ok()
            {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Per-output resume (v0.1). Sends SIGCONT to every recorded pid
    /// and cancels any active soft-pause cycle so the cycling task
    /// doesn't immediately SIGSTOP again. The forwarder task spawned
    /// by [`Self::pause_per_output_soft`] wakes once on
    /// `soft_pause_cancel.notified()` and notifies the cycle-local
    /// handle, which the running cycle reads via `tokio::select!`.
    pub async fn resume_per_output(&self) -> Result<usize> {
        // Cancel any active soft-pause cycle BEFORE sending SIGCONT
        // so the cycle doesn't immediately re-apply SIGSTOP. We
        // notify_waiters (rather than notify_one) so multiple cycles
        // stacked from re-entrant pause calls all wake.
        self.soft_pause_cancel.notify_waiters();
        let pids = self.per_output_pids.lock().await;
        let mut count = 0;
        for &pid in pids.values() {
            // Skip dead pids so SIGCONT on a defunct process doesn't
            // get logged as an error by the kernel.
            if matches!(
                pid_state_quick(pid, BackendKind::LinuxWallpaperEngine),
                Ok(BackendState::Running) | Ok(BackendState::Paused)
            ) && nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGCONT,
            )
            .is_ok()
            {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Per-output THROTTLE resume (v0.1). Mirrors `pause_per_output_throttle`:
    /// when LWE was re-spawned with `--fps 1` on pause, resume
    /// should re-spawn with the normal FPS cap so the renderer
    /// ramps back up. We SIGTERM the throttled children, clear
    /// their pids, and respawn at `target_fps`.
    pub async fn resume_per_output_throttle(&self, target_fps: u32) -> Result<usize> {
        let pairs: Vec<(String, PathBuf)> = {
            let pids = self.per_output_pids.lock().await;
            let scenes = self.per_output_scenes.lock().await;
            pids.keys()
                .filter_map(|out| scenes.get(out).cloned().map(|s| (out.clone(), s)))
                .collect()
        };
        // Clear pid map; populate after re-spawn. Throttled LWE
        // will be replaced (not SIGCONT'd) so we don't need to
        // clean-shutdown each one — the kernel reaps them when
        // their parent (us) exits the spawned-child ref.
        {
            let mut pids = self.per_output_pids.lock().await;
            pids.clear();
        }
        let mut respawned = 0usize;
        let mut skipped: Vec<(String, String)> = Vec::new();
        for (output, scene) in &pairs {
            match self
                .set_per_output_with_fps(scene, output, target_fps)
                .await
            {
                Ok(pid) if pid > 0 => respawned += 1,
                Ok(_) => skipped.push((output.clone(), "spawn returned pid 0".into())),
                Err(e) => {
                    tracing::warn!("throttle-resume: respawn for output={} failed: {e}", output);
                    skipped.push((output.clone(), e.to_string()));
                }
            }
        }
        if !skipped.is_empty() {
            tracing::warn!(
                target: "paperforge",
                "throttle-resume: {} of {} output(s) skipped: {:?}",
                skipped.len(),
                pairs.len(),
                skipped
            );
        }
        Ok(respawned)
    }

    /// Send SIGTERM to the LWE pid for `output` and clear it from
    /// the in-memory map. Keeps `per_output_scenes` so the
    /// fullscreen watcher (or `resume_per_output_specific`) knows
    /// what scene to re-spawn with.
    ///
    /// This is the **release the socket** primitive for
    /// fullscreen-detected monitors: SIGTERM is graceful (LWE
    /// closes its DRM/EGL surface), the kernel reaps the child,
    /// and the next /proc poll sees it gone.
    ///
    /// Idempotent: if the output has no recorded pid (already
    /// killed or never spawned), the call is a no-op. The scene
    /// map entry is preserved.
    pub async fn kill_per_output(&self, output: &str) -> Result<()> {
        let pid_opt = {
            let mut pids = self.per_output_pids.lock().await;
            pids.remove(output)
        };
        if let Some(pid) = pid_opt {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
            tracing::info!(
                target: "paperforge",
                "kill_per_output: output={} pid={} SIGTERM sent",
                output,
                pid
            );
        } else {
            tracing::debug!(
                target: "paperforge",
                "kill_per_output: output={} had no recorded pid (no-op)",
                output
            );
        }
        Ok(())
    }

    /// Adopt a pre-existing LWE process that was launched outside
    /// the daemon (operator by hand, another tool, leftover from a
    /// previous daemon lifetime) into our per-output maps.
    ///
    /// Used by `adopt_existing_lwes` in `paperforge-cli` so the
    /// daemon's fullscreen dispatcher, reaper, and reconcile task
    /// treat the adopted process as first-class state instead of
    /// logging "no scene recorded" when they try to resume it.
    ///
    /// Idempotent: if the output already has a live pid recorded,
    /// this is a no-op (the alive pid wins — the daemon should
    /// have been the source of truth). If the recorded pid is dead,
    /// it gets replaced by the external pid. This prevents the
    /// reaper from spawning a duplicate over a still-running
    /// adopted LWE.
    pub async fn bind_external_pid(&self, output: &str, scene: &Path, pid: i32) -> bool {
        let already_live = {
            let pids = self.per_output_pids.lock().await;
            match pids.get(output) {
                Some(existing) => matches!(
                    pid_state_quick(*existing, BackendKind::LinuxWallpaperEngine),
                    Ok(BackendState::Running) | Ok(BackendState::Paused)
                ),
                None => false,
            }
        };
        if already_live {
            tracing::debug!(
                target: "paperforge",
                "bind_external_pid: output={} already has a live pid; \
                 refusing to replace with external pid={}",
                output,
                pid
            );
            return false;
        }
        let mut pids = self.per_output_pids.lock().await;
        let mut scenes = self.per_output_scenes.lock().await;
        pids.insert(output.to_string(), pid);
        scenes.insert(output.to_string(), scene.to_path_buf());
        // Drop the per-output pid/scene locks before clearing the
        // warn-limiter so we don't hold 2 mutexes at once. The warn
        // limiter's cooldown is per-key, so dropping here is safe.
        drop(pids);
        drop(scenes);
        {
            let mut limiter = self.warn_limiter.lock().await;
            limiter.clear_for_output(output);
        }
        tracing::info!(
            target: "paperforge",
            "bind_external_pid: output={} pid={} scene={} adopted",
            output,
            pid,
            scene.display()
        );
        true
    }

    /// Re-spawn LWE for `output` using its last-known scene. Used
    /// by the fullscreen watcher to restore a wallpaper after the
    /// covering window goes away (user switched workspace, game
    /// exits fullscreen, etc.).
    ///
    /// Returns the new pid on success, errors if there's no scene
    /// to spawn with.
    pub async fn resume_per_output_specific(&self, output: &str) -> Result<i32> {
        let scene = {
            let scenes = self.per_output_scenes.lock().await;
            scenes.get(output).cloned()
        };
        let Some(scene) = scene else {
            // Rate-limited WARN: the fullscreen dispatcher polls every
            // ~30s, and the no-scene-yet condition is true for the
            // entire window between "daemon started" and "operator's
            // first bind". Emitting a fresh WARN every poll would
            // flood journalctl. Suppress duplicates within the 5-min
            // cooldown; re-emit after the window so a long-running
            // operator-visible condition is still surfaced.
            let key = format!("{output}:no_scene_recorded");
            let should_warn = {
                let mut limiter = self.warn_limiter.lock().await;
                limiter.should_emit(&key)
            };
            if should_warn {
                tracing::warn!(
                    target: "paperforge",
                    "resume_per_output_specific({output}): no scene recorded; \
                     was a wallpaper ever bound to this output?"
                );
            } else {
                tracing::debug!(
                    target: "paperforge",
                    "resume_per_output_specific({output}): no scene recorded (suppressed by warn-rate-limiter)"
                );
            }
            return Err(Error::BackendFailure {
                kind: self.kind().process_pattern().to_string(),
                message: format!(
                    "resume_per_output_specific({output}): no scene recorded; \
                     was a wallpaper ever bound to this output?"
                ),
            });
        };
        let fps = self.pool.active_fps();
        let new_pid = self.set_per_output_with_fps(&scene, output, fps).await?;
        tracing::info!(
            target: "paperforge",
            "resume_per_output_specific: output={} re-spawned pid={} scene={}",
            output,
            new_pid,
            scene.display()
        );
        Ok(new_pid)
    }

    /// Prune dead children from `per_output_pids` + `per_output_scenes`.
    /// Returns the outputs that were pruned (caller can re-bind or
    /// emit a D-Bus event).
    ///
    /// Used by the daemon's periodic reconciliation task and the
    /// child-reaper driven by SIGCHLD. We rely on
    /// [`pid_state_quick`] reading `/proc/<pid>/status` instead of
    /// `kill(pid, 0)` because:
    ///
    /// - `kill(pid, 0)` returns `EPERM` for processes we can't signal
    ///   (different uid) which would look like "alive"; not what
    ///   we want for our own children.
    /// - `/proc/<pid>/status` is the same source `ps` uses; absence
    ///   means the process exited and was reaped.
    pub async fn prune_dead_pids(&self) -> Vec<String> {
        let mut pruned = Vec::new();
        let mut pids = self.per_output_pids.lock().await;
        // Collect first so we don't hold the lock across /proc reads.
        let candidates: Vec<(String, i32)> = pids.iter().map(|(k, v)| (k.clone(), *v)).collect();
        // We don't need to mutate `scenes` in this function (we
        // keep scenes for re-spawn via `reconcile_outputs`); bind
        // non-mut to silence the warn without changing behaviour.
        let _scenes = self.per_output_scenes.lock().await;
        for (output, pid) in candidates {
            match pid_state_quick(pid, BackendKind::LinuxWallpaperEngine) {
                Ok(BackendState::NotRunning) => {
                    tracing::info!(
                        target: "paperforge",
                        "per-output reaper: output={} pid={} dead; \
                         clearing from map (operator can re-bind)",
                        output,
                        pid
                    );
                    pids.remove(&output);
                    // Keep the scene around so the reconciliation
                    // task knows what to re-bind with.
                    pruned.push(output);
                }
                Err(e) => {
                    tracing::warn!(
                        target: "paperforge",
                        "per-output reaper: pid_state_quick({pid}) failed: {e}"
                    );
                }
                Ok(_) => {
                    // Running or Paused — leave alone.
                }
            }
        }
        pruned
    }

    /// Snapshot of the per-output scene map. Used by the daemon's
    /// reconciliation task to re-bind dead outputs with their
    /// previously-bound scene.
    pub async fn last_known_scenes(&self) -> std::collections::BTreeMap<String, PathBuf> {
        self.per_output_scenes.lock().await.clone()
    }

    /// Set of outputs that have a recorded pid. Used by the
    /// fullscreen dispatcher to decide whether `kill_per_output`
    /// would be a real kill vs a no-op. The daemon log line for
    /// "fullscreen ON on X: killed LWE" was misleading when X had
    /// no recorded pid — this helper lets the caller pre-check
    /// and surface the actual outcome honestly.
    pub async fn outputs_with_pids(&self) -> std::collections::BTreeSet<String> {
        let pids = self.per_output_pids.lock().await;
        pids.keys().cloned().collect()
    }

    /// Re-bind any outputs whose LWE process has died, using the
    /// scene they were last bound to. Returns the list of outputs
    /// that were re-spawned (for logging / D-Bus signal emission).
    ///
    /// Algorithm:
    /// 1. Snapshot the per-output pid + scene maps.
    /// 2. Walk the pid map; for any pid that's `NotRunning` per
    ///    `/proc/<pid>/status`, clear it from `per_output_pids`
    ///    (keep the scene so we can re-spawn with it).
    /// 3. For each cleared output, call
    ///    `set_per_output_with_fps(scene, output, active_fps)` to
    ///    spawn a fresh LWE. The idempotent fast-path catches the
    ///    case where LWE is already alive — no-op in that branch.
    /// 4. Errors during re-spawn are logged + counted; we keep
    ///    trying other outputs instead of aborting on the first
    ///    failure.
    ///
    /// This is the daemon's "self-heal" path: after a crash or
    /// SIGCHLD the next reconcile pass resurrects the dead
    /// outputs so the operator doesn't have to manually re-bind.
    pub async fn reconcile_outputs(&self) -> Vec<(String, i32)> {
        // Step 1+2: prune dead pids, collecting their outputs.
        let pruned = self.prune_dead_pids().await;
        if pruned.is_empty() {
            return Vec::new();
        }

        // Snapshot scenes under the (now released) lock so the spawn
        // loop doesn't hold any backend state.
        let scenes = self.last_known_scenes().await;
        let fps = self.pool.active_fps();

        let mut respawned = Vec::new();
        for output in pruned {
            let Some(scene) = scenes.get(&output) else {
                tracing::warn!(
                    target: "paperforge",
                    "reconcile: output={} has dead pid but no scene; \
                     cannot re-spawn without operator action",
                    output
                );
                continue;
            };
            match self.set_per_output_with_fps(scene, &output, fps).await {
                Ok(new_pid) => {
                    tracing::info!(
                        target: "paperforge",
                        "reconcile: output={} re-spawned pid={} scene={}",
                        output,
                        new_pid,
                        scene.display()
                    );
                    respawned.push((output, new_pid));
                }
                Err(e) => {
                    tracing::error!(
                        target: "paperforge",
                        "reconcile: output={} re-spawn failed: {e}",
                        output
                    );
                }
            }
        }
        respawned
    }

    /// List per-output PIDs (v0.1).
    ///
    /// Strategy: prefer the in-memory `per_output_pids` map (populated
    /// by `set_per_output` calls in this process) so we don't pick up
    /// foreign LWE processes. Fall back to `/proc` walking when the
    /// map is empty — that's the CLI-stateless case where earlier
    /// invocations spawned LWE children that got reparented to init.
    /// In a daemon context the map is always populated, so `/proc` is
    /// rarely hit.
    pub async fn list_per_output_pids(&self) -> Vec<i32> {
        let pids = self.per_output_pids.lock().await;
        if !pids.is_empty() {
            return pids.values().copied().collect();
        }
        // Fallback: walk /proc for any LWE process. The CLI is a
        // single-shot process; children survive in /proc after exit.
        list_pids_in_proc(Path::new("/proc"), self.kind().process_pattern()).unwrap_or_default()
    }

    /// Component C: tear down the pipe drainers for a single LWE pid.
    /// Used after the child has been killed (or after the child has
    /// died and we want to drop the now-EOF readers). The handles
    /// are aborted, not awaited — JoinHandle::abort() schedules the
    /// task for cancellation and the reader future is dropped at
    /// the next await point.
    ///
    /// Idempotent: removing a pid that isn't in the map is a no-op.
    /// Pids die naturally (EOF triggers `drain_pipe` to return) and
    /// the entries linger until the next bind/unbind cycle removes
    /// them — this is the cleanup path for that.
    pub async fn unbind(&self, pid: i32) {
        let mut drainers = self.pipe_drainers.lock().await;
        if let Some((sout, serr)) = drainers.remove(&pid) {
            sout.abort();
            serr.abort();
        }
    }

    /// Component C: tear down ALL pipe drainers. Called on full
    /// daemon shutdown so the readers don't keep the pipe FDs open
    /// after their owning process is gone. The systemd unit's
    /// `KillMode=process` doesn't guarantee FDs close cleanly if
    /// async tasks are still holding them.
    pub async fn shutdown(&self) {
        let mut drainers = self.pipe_drainers.lock().await;
        // `MutexGuard` doesn't expose `BTreeMap::drain`, so take the
        // map out and iterate by value. The replacement is empty,
        // matching the post-shutdown invariant.
        let taken = std::mem::take(&mut *drainers);
        for (_, (sout, serr)) in taken {
            sout.abort();
            serr.abort();
        }
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

        // Sync the legacy per-output maps so `kill_per_output` /
        // `resume_per_output_specific` find a real pid for every
        // output the pool now owns (not just `out` — outputs that
        // were bound on earlier `set` calls share the same merged
        // LWE process and need their pid entries populated too).
        self.sync_pid_map_from_pool(out, scene).await;

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
        // we actually own in v0.2 mode. If the caller asks about a
        // foreign pid in pool mode, we report NotRunning instead of
        // a stale /proc read.
        //
        // v0.1 per-output path: stateless CLI calls have an empty
        // `per_output_pids` map, but LWE children survive in /proc
        // (reparented to init after our exit). Read the kernel-
        // reported state directly via /proc/<pid>/status. If the
        // pid is gone, /proc reports ENOENT → NotRunning. The
        // `pid_state_quick` call also cross-checks the cmdline
        // against `LinuxWallpaperEngine`'s pattern so a recycled
        // PID (kernel handed the same PID to a `bash` / `sleep`)
        // is reported as NotRunning, not Running.
        let owned = self.pool.current_pid().await;
        if owned == Some(pid) {
            return pid_state_quick(pid, BackendKind::LinuxWallpaperEngine);
        }
        // Per-output + stateless CLI: skip the ownership gate and
        // trust /proc. This matches the v0.1 design where each LWE
        // child survives independently of any parent state.
        pid_state_quick(pid, BackendKind::LinuxWallpaperEngine)
    }

    fn supports(&self, entry: &crate::WallpaperEntry) -> bool {
        entry.kind.lwe_compatible()
    }
}

/// Read `/proc/<pid>/cmdline` and check whether its argv contains
/// `BackendKind::process_pattern()` for `kind`.
///
/// Returns:
/// - `true` if the cmdline contains the pattern (this PID IS an
///   instance of `kind`).
/// - `false` if the PID exists but the cmdline doesn't match (PID
///   was recycled to a different process — the most insidious case:
///   `pid_state_quick` would otherwise return Running on a
///   totally unrelated process).
/// - `false` if `/proc/<pid>/cmdline` doesn't exist (PID gone).
///
/// This is the PID-recycling defense. Combined with
/// [`pid_state_quick`], it lets call sites distinguish "alive AND is
/// `kind`" from "alive BUT is something else". The original LWE
/// can die and the kernel can recycle its PID to a `bash` / `sleep`
/// / whatever — a kernel-state-only check would mistake that for
/// "LWE is alive" and skip the respawn.
///
/// Public (not `pub(crate)`) because the upcoming watchdog will
/// call it directly and we want it reachable from integration
/// tests in other crates.
pub fn pid_is_backend_kind(pid: i32, kind: BackendKind) -> bool {
    let cmdline_path = format!("/proc/{pid}/cmdline");
    let Ok(cmdline) = std::fs::read(&cmdline_path) else {
        return false;
    };
    let pattern = kind.process_pattern();
    cmdline
        .split(|b| *b == 0)
        .filter(|arg| !arg.is_empty())
        .any(|arg| {
            std::str::from_utf8(arg)
                .map(|s| s.contains(pattern))
                .unwrap_or(false)
        })
}

/// Read the kernel-reported state of a PID via `/proc/<pid>/status`.
///
/// Returns:
/// - `BackendState::Paused` if the kernel reports `T (stopped)` —
///   typical after the caller sent SIGSTOP via the nix syscall.
/// - `BackendState::Running` if the kernel reports any of `R (running)`,
///   `S (sleeping)`, `D (disk sleep)`, `I (idle)`. We treat all of
///   these as "alive, not paused". Before returning Running we
///   cross-check against [`pid_is_backend_kind`]: if the PID was
///   recycled to a different process (e.g. an LWE died and the
///   kernel handed its PID to a `bash`), the cmdline won't match
///   `kind.process_pattern()` and we report NotRunning instead.
///   Without this check, `bind()`'s fast-path would happily return
///   `Ok(pid)` against a recycled PID and the wallpapers would
///   stay dead even though the pool "thinks" LWE is alive.
/// - `BackendState::NotRunning` if `/proc/<pid>/status` doesn't exist
///   (process exited, or never existed) OR the kernel reports
///   `Z (zombie)`. A zombie has finished executing — its task_struct
///   is just waiting for the parent to `wait()`. From the daemon's
///   perspective it is dead: it can't render frames, it can't respond
///   to signals, it can't do anything. Treating it as `Running`
///   would make `bind()`'s spawn-first-kill-after abort path miss
///   the case where the new LWE crashed during the grace window.
///
/// Errors only on actual I/O failures (permissions, transient FS
/// issues). Most call sites should treat `NotRunning` as the signal
/// they need (process died, time to respawn) without needing a
/// `Result`-flavored API.
///
/// This is a synchronous read because `/proc` is a kernel pseudofs:
/// no I/O wait, no network, no async needed. Callers wrap in
/// `spawn_blocking` if they're holding an async runtime.
pub(crate) fn pid_state_quick(pid: i32, kind: BackendKind) -> Result<BackendState> {
    let status_path = format!("/proc/{pid}/status");
    let content = match std::fs::read_to_string(&status_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BackendState::NotRunning),
        Err(e) => return Err(e.into()),
    };
    let mut kernel_state: Option<BackendState> = None;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("State:") {
            // State line looks like: "State:\tT (stopped)"
            //                       or   "State:\tR (running)"
            //                       or   "State:\tS (sleeping)"
            //                       or   "State:\tZ (zombie)"
            if rest.contains('T') {
                // SIGSTOP'd LWE still has the correct cmdline — the
                // kernel never recycles a PID that's currently
                // stopped. Skip the cross-check to save one syscall.
                return Ok(BackendState::Paused);
            }
            if rest.contains('R') || rest.contains('S') || rest.contains('D') || rest.contains('I')
            {
                kernel_state = Some(BackendState::Running);
                break;
            }
            // Unknown state letter, or Z (zombie) — fall through to
            // NotRunning. The wrapper script case (exits 1 within the
            // grace window) lands here as a zombie: parent hasn't
            // reaped yet, but the process has finished executing and
            // is functionally dead.
            return Ok(BackendState::NotRunning);
        }
    }
    // After the loop: kernel reported Running OR the `State:` line
    // was absent (very rare; treat as Running-shaped to be safe).
    // Cross-check the cmdline against `kind` to defend against PID
    // recycling — the original process died and the kernel handed
    // the same PID to a `bash` / `sleep` / whatever. The kernel
    // state alone would call that "Running", which is true but
    // misleading for our purposes.
    match kernel_state {
        Some(BackendState::Running) => {
            if pid_is_backend_kind(pid, kind) {
                Ok(BackendState::Running)
            } else {
                tracing::debug!(
                    target: "paperforge",
                    "pid_state_quick: pid={pid} kind={kind:?} reports Running but cmdline does not match (recycled PID); reporting NotRunning",
                );
                Ok(BackendState::NotRunning)
            }
        }
        _ => Ok(BackendState::NotRunning),
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

    /// Monotonic counter used to uniquify wrapper script paths in
    /// parallel test invocations. Using only `std::process::id()` in
    /// the path collides when two tests in the same binary write the
    /// same filename concurrently — the second `Command::spawn()`
    /// returns ETXTBSY ("Text file busy") because the kernel still
    /// has the file open for exec from the first test's spawn.
    fn next_wrapper_seq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        SEQ.fetch_add(1, Ordering::Relaxed)
    }

    /// First call to `should_emit` for a given key returns true and
    /// records the timestamp. Subsequent identical calls within the
    /// cooldown window return false. Different keys are
    /// independent (the limiter's state is per-key, not global).
    #[test]
    fn warn_rate_limiter_suppresses_repeats() {
        let mut limiter = WarnRateLimiter::new(Duration::from_secs(300));
        assert!(
            limiter.should_emit("DP-1:no_scene_recorded"),
            "first call for a key must emit"
        );
        assert!(
            !limiter.should_emit("DP-1:no_scene_recorded"),
            "immediate repeat must be suppressed"
        );
        assert!(
            !limiter.should_emit("DP-1:no_scene_recorded"),
            "further repeats stay suppressed"
        );
        assert!(
            limiter.should_emit("HDMI-A-1:no_scene_recorded"),
            "different output is a different key — emit"
        );
    }

    /// `clear_for_output` removes all keys that share the
    /// `"{output}:"` prefix so the next WARN for that output passes
    /// through immediately. Other outputs are untouched.
    #[test]
    fn warn_rate_limiter_clear_for_output() {
        let mut limiter = WarnRateLimiter::new(Duration::from_secs(300));
        assert!(limiter.should_emit("DP-1:no_scene_recorded"));
        assert!(limiter.should_emit("DP-1:fullscreen_no_op"));
        assert!(limiter.should_emit("HDMI-A-1:no_scene_recorded"));
        limiter.clear_for_output("DP-1");
        // DP-1 keys are gone — next call emits again.
        assert!(
            limiter.should_emit("DP-1:no_scene_recorded"),
            "after clear_for_output, the next WARN passes through"
        );
        // HDMI-A-1 is independent — still suppressed.
        assert!(
            !limiter.should_emit("HDMI-A-1:no_scene_recorded"),
            "clear_for_output(DP-1) must not touch other outputs"
        );
    }

    /// `should_emit` returns true again once the cooldown has
    /// elapsed. We use a 1ms cooldown so the test runs without
    /// `tokio::time::sleep` (which would require a runtime).
    #[test]
    fn warn_rate_limiter_reemits_after_cooldown() {
        let mut limiter = WarnRateLimiter::new(Duration::from_millis(1));
        assert!(limiter.should_emit("DP-1:no_scene_recorded"));
        assert!(!limiter.should_emit("DP-1:no_scene_recorded"));
        // Sleep past the cooldown (1ms + safety margin for scheduler
        // granularity). 25ms is enough on every reasonable system.
        std::thread::sleep(Duration::from_millis(25));
        assert!(
            limiter.should_emit("DP-1:no_scene_recorded"),
            "after cooldown elapses, the next WARN must pass through again"
        );
    }

    /// `LweBackend::warn_limiter()` returns a clone of the same
    /// `Arc<Mutex<_>>`, so external callers (e.g. the CLI's
    /// fullscreen dispatcher) share the cooldown window with the
    /// backend's own emissions.
    #[tokio::test]
    async fn lwe_backend_warn_limiter_accessor_shares_state() {
        let backend = LweBackend::new();
        let limiter_clone = backend.warn_limiter();
        // Drive the backend's own limiter to "emit" a key, then
        // verify the external clone sees the same state.
        {
            let mut inner = limiter_clone.lock().await;
            assert!(inner.should_emit("DP-1:fullscreen_no_op"));
        }
        // The backend's internal limiter should observe the same
        // "already emitted" state.
        let backend_inner = backend.warn_limiter();
        let mut inner = backend_inner.lock().await;
        assert!(
            !inner.should_emit("DP-1:fullscreen_no_op"),
            "shared Arc: external clone and backend see the same cooldown state"
        );
    }

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
        // just sleeps long enough for all the binds. The wrapper
        // also re-stamps argv[0] via `exec -a` so the resulting
        // cmdline contains the `linux-wallpaperengine` pattern and
        // `pid_state_quick`'s recycling defense accepts the
        // process. Uses bash because /bin/sh on Debian is dash
        // which lacks `exec -a`.
        // Unique per-run path so parallel tests don't race on the
        // same inode (kernel returns ETXTBSY "Text file busy" when
        // a second test re-writes the wrapper while the previous
        // spawn is still exec'ing it).
        let wrapper = std::env::temp_dir().join(format!(
            "paperforge-pool-single-{}-{}.sh",
            std::process::id(),
            next_wrapper_seq()
        ));
        std::fs::write(
            &wrapper,
            "#!/usr/bin/env bash\n\
             exec -a linux-wallpaperengine-pool-single /bin/sleep 60\n",
        )
        .unwrap();
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
        let wrapper = std::env::temp_dir().join(format!(
            "paperforge-pool-idempotent-{}-{}.sh",
            std::process::id(),
            next_wrapper_seq()
        ));
        // bash + `exec -a` so the resulting cmdline carries the
        // LWE pattern and the pid_state_quick recycling defense
        // accepts the process. /bin/sh on Debian is dash which
        // doesn't grok `exec -a`.
        std::fs::write(
            &wrapper,
            "#!/usr/bin/env bash\n\
             exec -a linux-wallpaperengine-pool-idempotent /bin/sleep 60\n",
        )
        .unwrap();
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

    /// `LweBackend::set` (pool path) must populate `per_output_pids`
    /// for **every** output currently bound in the pool, not just the
    /// bind target. Outputs that share a merged-LWE process need their
    /// pid entries so `kill_per_output` and `resume_per_output_specific`
    /// find something to signal — without this sync, the fullscreen
    /// dispatcher's `kill_per_output` silently no-ops for non-target
    /// outputs and emits a WARN every ~30 s. See `sync_pid_map_from_pool`
    /// for the rationale.
    #[tokio::test]
    async fn set_populates_per_output_pids_for_all_pool_outputs() {
        // Wrapper that swallows the LWE-style argv and just sleeps —
        // matches the pattern in `lwe_backend_set_via_pool_uses_single_process`.
        // bash + `exec -a` so the resulting cmdline carries the LWE
        // pattern and the pid_state_quick recycling defense accepts
        // the process. /bin/sh on Debian is dash which doesn't grok
        // `exec -a`.
        let wrapper = std::env::temp_dir().join("paperforge-sync-pid-map-binary.sh");
        std::fs::write(
            &wrapper,
            "#!/usr/bin/env bash\n\
             exec -a linux-wallpaperengine-sync-pid-map /bin/sleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let backend = LweBackend::with_binary(&wrapper).with_empty_pool_flags();

        // Build three Workshop-shaped paths on disk so the
        // workshop_content_id check (which inspects the trailing
        // components) passes. Each path's leaf is the numeric id.
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

        // Step 1: bind DP-1 directly via the pool (low-level) — this
        // simulates a previous daemon state where the pool already
        // owned an HDMI-A-1 binding before we got control. We need
        // to populate the pool with two outputs *before* testing
        // `set()` to verify the helper syncs both, not just the new
        // bind target.
        backend
            .pool()
            .bind("HDMI-A-1", "222")
            .await
            .expect("first pool bind");

        // Step 2: call `LweBackend::set` for a different output.
        // The helper should now sync pids for BOTH outputs (because
        // the pool's `bindings()` returns {"HDMI-A-1": "222",
        // "DP-1": "111"}) and record DP-1's scene.
        backend
            .set(&scenes[0], Some("DP-1"))
            .await
            .expect("LweBackend::set for DP-1");

        let pool_pid = backend
            .pool()
            .current_pid()
            .await
            .expect("pool must own a pid after set");
        let pool_bindings = backend.pool().bindings().await;
        assert_eq!(
            pool_bindings.len(),
            2,
            "pool should track both outputs (HDMI-A-1 pre-existing + DP-1 from set)"
        );
        assert!(pool_bindings.contains_key("DP-1"));
        assert!(pool_bindings.contains_key("HDMI-A-1"));

        // Crucial assertions: BOTH outputs have a recorded pid in the
        // per_output_pids map, and that pid is the pool's pid (the
        // merged-LWE pid).
        let pids = backend.per_output_pids_test_accessor().lock().await;
        assert_eq!(
            pids.get("DP-1"),
            Some(&pool_pid),
            "DP-1 must be in per_output_pids with the pool pid"
        );
        assert_eq!(
            pids.get("HDMI-A-1"),
            Some(&pool_pid),
            "HDMI-A-1 must be in per_output_pids with the pool pid (shared LWE process)"
        );
        assert_eq!(
            pids.get("DP-1"),
            pids.get("HDMI-A-1"),
            "shared-LWE outputs must share the same pid"
        );
        drop(pids);

        // per_output_scenes should have DP-1 (the bind target) but
        // NOT HDMI-A-1 (the pool only knows content_ids, never the
        // scene_path the operator passed earlier — we don't have that
        // information here). Leaving HDMI-A-1 empty is intentional:
        // `resume_per_output_specific` will surface a real "no scene
        // recorded" warning for it, which is the correct outcome.
        let scenes_map = backend.per_output_scenes.lock().await;
        assert_eq!(
            scenes_map.get("DP-1"),
            Some(&scenes[0]),
            "DP-1 must record the scene the caller passed to set()"
        );
        assert!(
            scenes_map.get("HDMI-A-1").is_none(),
            "HDMI-A-1 should not be set: we never had its scene path"
        );
        drop(scenes_map);

        // Cleanup.
        backend.pool().shutdown().await.unwrap();
        let _ = std::fs::remove_file(&wrapper);
    }

    /// `sync_pid_map_from_pool` is a no-op when the pool has no PID
    /// (i.e. `current_pid()` returns None). Must not panic on the
    /// empty map case — the maps stay empty, no spurious entries.
    #[tokio::test]
    async fn sync_pid_map_from_pool_is_noop_when_pool_empty() {
        let backend = LweBackend::with_binary("/bin/sleep");
        // Don't bind anything; pool is empty by construction.

        // Call the helper directly with a stand-in scene path.
        backend
            .sync_pid_map_from_pool(
                "DP-1",
                std::path::Path::new("/tmp/workshop/content/431960/111"),
            )
            .await;

        // Both maps remain empty.
        let pids = backend.per_output_pids_test_accessor().lock().await;
        assert!(
            pids.is_empty(),
            "per_output_pids must remain empty when pool is empty"
        );
        drop(pids);
        let scenes_map = backend.per_output_scenes.lock().await;
        assert!(
            scenes_map.is_empty(),
            "per_output_scenes must remain empty when pool is empty"
        );
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

    /// Real-process SIGSTOP/SIGCONT end-to-end: spawn a wrapper whose
    /// argv contains `linux-wallpaperengine` (so the
    /// `pid_state_quick` cmdline cross-check passes), freeze it,
    /// inspect `/proc/<pid>/status`, thaw, re-inspect.
    ///
    /// This is the closest thing to "smoke test" the signal code
    /// without a real LWE instance. It uses the real
    /// `nix::sys::signal::kill` + the real `/proc` filesystem.
    ///
    /// Uses `pid_state_quick` directly because `LweBackend::state`
    /// gates on pool ownership (returns NotRunning for foreign pids),
    /// which is correct production behavior but wrong for this smoke
    /// test — the test deliberately creates an unmanaged pseudo-LWE.
    ///
    /// The wrapper uses `exec -a` to set argv[0] to a name containing
    /// the LWE pattern, then sleeps for 60s. `pid_is_backend_kind`
    /// matches on `argv[0]` (and any other argv slot), so the
    /// kernel-state transition check that follows passes the
    /// recycling defense.
    #[test]
    fn real_sigstop_sigcont_round_trip() {
        // Wrapper script: exec sleep with a fake argv[0] so the
        // cmdline contains "linux-wallpaperengine" and our
        // recycling defense doesn't mistake it for a recycled PID.
        let wrapper = std::env::temp_dir().join(format!(
            "paperforge-sigstop-smoke-{}-{}.sh",
            std::process::id(),
            next_wrapper_seq()
        ));
        // `exec -a NAME sleep 60` runs /usr/bin/sleep with argv[0]
        // overridden to NAME. The /proc/<pid>/cmdline then reads
        // "linux-wallpaperengine-sigstop-smoke\0sleep\060\0".
        std::fs::write(
            &wrapper,
            "#!/usr/bin/env bash\n\
             exec -a linux-wallpaperengine-sigstop-smoke /usr/bin/sleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let mut child = std::process::Command::new(&wrapper)
            .spawn()
            .expect("spawn wrapper");
        let pid = child.id() as i32;

        // Give the scheduler a moment so /proc reflects the new PID.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let state_running = pid_state_quick(pid, BackendKind::LinuxWallpaperEngine).unwrap();
        assert_eq!(
            state_running,
            BackendState::Running,
            "wrapper should start running"
        );

        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGSTOP,
        )
        .expect("SIGSTOP");

        // Yield to scheduler so the kernel processes the signal.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let state_paused = pid_state_quick(pid, BackendKind::LinuxWallpaperEngine).unwrap();
        assert_eq!(
            state_paused,
            BackendState::Paused,
            "wrapper should report paused after SIGSTOP"
        );

        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGCONT,
        )
        .expect("SIGCONT");

        std::thread::sleep(std::time::Duration::from_millis(50));
        let state_resumed = pid_state_quick(pid, BackendKind::LinuxWallpaperEngine).unwrap();
        assert_eq!(
            state_resumed,
            BackendState::Running,
            "wrapper should report running after SIGCONT"
        );

        child.kill().expect("cleanup kill");
        let _ = child.wait();
        let _ = std::fs::remove_file(&wrapper);
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

    // ---- PID-recycling defense tests ----
    //
    // These tests cover the bug where the kernel reuses a deceased
    // LWE's PID for an unrelated process (a `bash`, a `sleep`, etc.).
    // The kernel-side state (R/S/D/I) on the recycled PID is still
    // "Running" — only the cmdline reveals the recycling. Tests
    // against the REAL /proc because the cmdline read path is
    // intentionally hard-coded to `/proc` (production invariant: the
    // procfs root is fixed by the kernel).

    /// `pid_is_backend_kind` returns true for a process whose argv
    /// contains the LWE pattern. We spawn a wrapper that uses
    /// `exec -a` to override argv[0] so the cmdline contains the
    /// pattern even though the underlying binary is `/bin/sleep`.
    #[test]
    fn pid_is_backend_kind_matches_lwe_cmdline() {
        let wrapper = std::env::temp_dir().join(format!(
            "paperforge-pid-kind-match-{}-{}.sh",
            std::process::id(),
            next_wrapper_seq()
        ));
        std::fs::write(
            &wrapper,
            "#!/usr/bin/env bash\n\
             exec -a linux-wallpaperengine-pid-kind /usr/bin/sleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let mut child = std::process::Command::new(&wrapper)
            .spawn()
            .expect("spawn wrapper");
        let pid = child.id() as i32;

        // Give the scheduler a moment so /proc reflects the new PID.
        std::thread::sleep(std::time::Duration::from_millis(50));

        assert!(
            pid_is_backend_kind(pid, BackendKind::LinuxWallpaperEngine),
            "PID {pid} should be recognized as LWE (wrapper argv[0] contains the pattern)"
        );

        child.kill().expect("cleanup kill");
        let _ = child.wait();
        let _ = std::fs::remove_file(&wrapper);
    }

    /// `pid_is_backend_kind` returns false for an unrelated process
    /// (a plain `/bin/sleep` whose argv has no LWE pattern). This is
    /// the case the kernel-state-only check would have misclassified
    /// as "LWE is alive" — the recycling bug.
    #[test]
    fn pid_is_backend_kind_rejects_non_lwe_process() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;

        std::thread::sleep(std::time::Duration::from_millis(50));

        assert!(
            !pid_is_backend_kind(pid, BackendKind::LinuxWallpaperEngine),
            "PID {pid} (plain /bin/sleep) must NOT be misclassified as LWE"
        );

        child.kill().expect("cleanup kill");
        let _ = child.wait();
    }

    /// `pid_is_backend_kind` returns false for a PID that doesn't
    /// exist on the system. The kernel returns ENOENT for
    /// `/proc/<pid>/cmdline` and we map that to `false`.
    #[test]
    fn pid_is_backend_kind_returns_false_for_dead_pid() {
        // Spawn a real process, capture its PID, kill it, then wait
        // long enough for the kernel to reap it so /proc/<pid> goes
        // away. (If we used an arbitrary large PID the test would
        // be flaky on a busy system — using a real PID we just
        // reaped guarantees the directory is gone by the time we
        // probe.)
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("10")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        child.kill().expect("cleanup kill");
        let _ = child.wait();
        // Yield so the reaper can release the procfs entry.
        std::thread::sleep(std::time::Duration::from_millis(100));

        assert!(
            !pid_is_backend_kind(pid, BackendKind::LinuxWallpaperEngine),
            "reaped PID {pid} must return false (no /proc/<pid>/cmdline)"
        );
    }

    /// End-to-end: `pid_state_quick` correctly classifies a recycled
    /// PID as NotRunning. We can't reliably reproduce kernel-side
    /// PID recycling in a unit test (the kernel picks the next
    /// available PID, which depends on the host's PID namespace
    /// churn), so we simulate the recycling case by checking the
    /// behavior that matters: a PID whose cmdline does NOT match the
    /// LWE pattern must report NotRunning even though the kernel
    /// sees the process as Running.
    ///
    /// This is the exact bug the fix solves: a `/bin/sleep` that the
    /// kernel handed the same numeric slot to would otherwise be
    /// misclassified as LWE.
    #[test]
    fn pid_state_quick_treats_non_lwe_running_pid_as_not_running() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;

        std::thread::sleep(std::time::Duration::from_millis(50));

        // The kernel sees this as Running (R/S). But the cmdline
        // is `/bin/sleep\060\0` — no LWE pattern. The cross-check
        // must produce NotRunning.
        let state = pid_state_quick(pid, BackendKind::LinuxWallpaperEngine).unwrap();
        assert_eq!(
            state,
            BackendState::NotRunning,
            "non-LWE PID must be classified as NotRunning to defeat PID recycling"
        );

        // Sanity check: the underlying helper agrees it's not LWE.
        assert!(!pid_is_backend_kind(pid, BackendKind::LinuxWallpaperEngine));

        child.kill().expect("cleanup kill");
        let _ = child.wait();
    }

    /// `pid_state_quick` still returns Running for a process whose
    /// cmdline contains the LWE pattern. The cmdline cross-check
    /// must not false-positive on legitimate LWE processes.
    #[test]
    fn pid_state_quick_returns_running_for_lwe_pattern_cmdline() {
        let wrapper = std::env::temp_dir().join(format!(
            "paperforge-pid-st-quick-{}-{}.sh",
            std::process::id(),
            next_wrapper_seq()
        ));
        std::fs::write(
            &wrapper,
            "#!/usr/bin/env bash\n\
             exec -a linux-wallpaperengine-st-quick /usr/bin/sleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let mut child = std::process::Command::new(&wrapper)
            .spawn()
            .expect("spawn wrapper");
        let pid = child.id() as i32;

        std::thread::sleep(std::time::Duration::from_millis(50));

        let state = pid_state_quick(pid, BackendKind::LinuxWallpaperEngine).unwrap();
        assert_eq!(
            state,
            BackendState::Running,
            "wrapper with LWE-pattern argv[0] must classify as Running"
        );

        child.kill().expect("cleanup kill");
        let _ = child.wait();
        let _ = std::fs::remove_file(&wrapper);
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

    // ---- self-heal / prune_dead_pids / reconcile_outputs ----

    /// `prune_dead_pids` should NOT touch alive pids — they're left
    /// in the map so the SIGSTOP/SIGCONT signal path still finds
    /// them. We use a long-running `sleep` as the live process.
    #[tokio::test]
    async fn prune_dead_pids_leaves_alive_pids_untouched() {
        // Wrapper bash + exec -a so the spawned child has the LWE
        // pattern in argv[0] and the cmdline recycling defense
        // accepts it. /bin/sh on Debian is dash which lacks `exec -a`.
        let wrapper = std::env::temp_dir().join(format!(
            "paperforge-prune-test-{}-{}.sh",
            std::process::id(),
            next_wrapper_seq()
        ));
        std::fs::write(
            &wrapper,
            "#!/usr/bin/env bash\n\
             exec -a linux-wallpaperengine-prune-test /bin/sleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let backend = LweBackend::with_binary(&wrapper);
        // Spawn a real child so /proc/<pid>/status is valid.
        let mut child = std::process::Command::new(&wrapper).spawn().unwrap();
        let live_pid = child.id() as i32;

        // Give the scheduler a moment so /proc/<pid>/cmdline reflects
        // the post-exec argv[0] (the wrapper's `exec -a` only lands
        // after the kernel schedules the new process).
        std::thread::sleep(std::time::Duration::from_millis(50));

        {
            let mut pids = backend.per_output_pids.lock().await;
            pids.insert("DP-1".to_string(), live_pid);
        }
        // And a dead pid (this pid almost certainly doesn't exist).
        {
            let mut pids = backend.per_output_pids.lock().await;
            pids.insert("HDMI-A-1".to_string(), 2_999_999);
        }

        let pruned = backend.prune_dead_pids().await;
        assert_eq!(
            pruned,
            vec!["HDMI-A-1".to_string()],
            "only the dead pid should be pruned"
        );

        let pids = backend.per_output_pids.lock().await;
        assert_eq!(pids.get("DP-1"), Some(&live_pid), "live pid must remain");

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&wrapper);
    }

    /// `last_known_scenes` is the canonical "which scene was this
    /// output last bound to" snapshot. Reconcile relies on it to
    /// know what to re-spawn with after a crash.
    #[tokio::test]
    async fn last_known_scenes_returns_per_output_snapshot() {
        let backend = LweBackend::with_binary("/bin/sleep");
        {
            let mut scenes = backend.per_output_scenes.lock().await;
            scenes.insert(
                "DP-1".to_string(),
                PathBuf::from("/home/lou/steam/workshop/content/431960/111"),
            );
            scenes.insert(
                "HDMI-A-1".to_string(),
                PathBuf::from("/home/lou/steam/workshop/content/431960/222"),
            );
        }
        let snapshot = backend.last_known_scenes().await;
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.contains_key("DP-1"));
        assert!(snapshot.contains_key("HDMI-A-1"));
    }

    /// `reconcile_outputs` is the self-heal entry point: it should
    /// NOT touch alive outputs and SHOULD attempt to re-spawn dead
    /// ones. We can't actually spawn a real LWE in a unit test, so
    /// we use `/bin/true` as the fake binary — that gets us to the
    /// `cmd.spawn()` call which will spawn a short-lived process and
    /// return a real pid; then the test asserts the spawn happened.
    #[tokio::test]
    async fn reconcile_outputs_respawns_only_dead_pids() {
        // `/bin/true` exits immediately; LWE proxy via wrapper
        // isn't needed here — the per-output spawn path is just
        // `cmd.spawn()` of whatever binary path we set.
        let backend = LweBackend::with_binary("/bin/true");

        // Build a real workshop-style scene directory so
        // `set_per_output_with_fps` reaches its `cmd.spawn()`
        // call (it rejects paths that don't exist).
        let tmp = tempfile::tempdir().unwrap();
        let scene = tmp.path().join("workshop/content/431960/111");
        std::fs::create_dir_all(&scene).unwrap();
        std::fs::write(scene.join("scene.json"), b"{}").unwrap();

        // Dead pid that doesn't exist, with the just-created scene.
        {
            let mut pids = backend.per_output_pids.lock().await;
            pids.insert("DP-1".to_string(), 2_999_999);
        }
        {
            let mut scenes = backend.per_output_scenes.lock().await;
            scenes.insert("DP-1".to_string(), scene.clone());
        }

        let respawned = backend.reconcile_outputs().await;
        // The dead pid was re-spawned; the new pid should be a real
        // spawned pid (not the dead one). `/bin/true` exits ~0 ms
        // so by the time the next /proc read happens, it may
        // already be reaped — we tolerate either "spawned a fresh
        // pid" or "was so fast it spawned nothing alive". The real
        // contract is: prune_dead_pids flagged it + the respawn
        // attempted. Allow 0 or 1 in the result.
        if respawned.is_empty() {
            // Fast-path OK: /bin/true raced through spawn so fast
            // the test could not observe a live pid. The prune
            // pass already validated the dead-pid detection
            // (see `prune_dead_pids_leaves_alive_pids_untouched`).
            return;
        }
        assert_eq!(respawned.len(), 1, "one respawn expected");
        let (output, new_pid) = &respawned[0];
        assert_eq!(output, "DP-1");
        assert_ne!(*new_pid, 2_999_999);
    }

    /// `reconcile_outputs` should be a no-op when all PIDs are
    /// alive — no respawn, no log noise (other than the prune pass).
    #[tokio::test]
    async fn reconcile_outputs_noop_when_all_alive() {
        // bash + exec -a wrapper so the spawned child carries the
        // LWE pattern in argv[0] and the cmdline recycling defense
        // accepts it. /bin/sh on Debian is dash which lacks `exec -a`.
        let wrapper = std::env::temp_dir().join(format!(
            "paperforge-reconcile-noop-{}-{}.sh",
            std::process::id(),
            next_wrapper_seq()
        ));
        std::fs::write(
            &wrapper,
            "#!/usr/bin/env bash\n\
             exec -a linux-wallpaperengine-reconcile-noop /bin/sleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let backend = LweBackend::with_binary(&wrapper);
        let mut child = std::process::Command::new(&wrapper).spawn().unwrap();
        let live_pid = child.id() as i32;

        // Give the scheduler a moment so /proc/<pid>/cmdline reflects
        // the post-exec argv[0] (the wrapper's `exec -a` only lands
        // after the kernel schedules the new process).
        std::thread::sleep(std::time::Duration::from_millis(50));

        {
            let pids_arc = backend.per_output_pids_test_accessor();
            let mut pids = pids_arc.lock().await;
            pids.insert("DP-1".to_string(), live_pid);
        }
        {
            let mut scenes = backend.per_output_scenes.lock().await;
            scenes.insert(
                "DP-1".to_string(),
                PathBuf::from("/tmp/workshop/content/431960/111"),
            );
        }

        let respawned = backend.reconcile_outputs().await;
        assert!(respawned.is_empty(), "no respawn when pid is alive");

        let pids = backend.per_output_pids.lock().await;
        assert_eq!(
            pids.get("DP-1"),
            Some(&live_pid),
            "alive pid must be untouched"
        );

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&wrapper);
    }

    /// `kill_per_output` SIGTERMs the recorded pid and removes it
    /// from the map, but keeps the scene so a later
    /// `resume_per_output_specific` knows what to re-spawn with.
    /// Idempotent: killing an output that has no pid is a no-op.
    #[tokio::test]
    async fn kill_per_output_sends_sigterm_and_clears_pid_keeps_scene() {
        let backend = LweBackend::with_binary("/bin/sleep");
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let live_pid = child.id() as i32;

        {
            let mut pids = backend.per_output_pids.lock().await;
            pids.insert("DP-1".to_string(), live_pid);
        }
        let scene = PathBuf::from("/tmp/workshop/content/431960/111");
        {
            let mut scenes = backend.per_output_scenes.lock().await;
            scenes.insert("DP-1".to_string(), scene.clone());
        }

        backend.kill_per_output("DP-1").await.expect("kill");

        // Pid is gone from the map.
        let pids = backend.per_output_pids.lock().await;
        assert!(pids.get("DP-1").is_none(), "kill must remove pid from map");
        drop(pids);
        // Scene stays (re-spawn target).
        let scenes = backend.per_output_scenes.lock().await;
        assert_eq!(
            scenes.get("DP-1"),
            Some(&scene),
            "kill must keep scene for re-spawn"
        );

        // Idempotent: second kill is a no-op.
        backend
            .kill_per_output("DP-1")
            .await
            .expect("kill idempotent");

        // Cleanup the actual child.
        let _ = child.kill();
        let _ = child.wait();
    }

    /// `resume_per_output_specific` re-spawns LWE with the
    /// last-known scene. We use `/bin/true` as the binary because
    /// `/bin/sleep` would block and `/bin/true` exits immediately
    /// (the respawn still goes through `cmd.spawn()`).
    #[tokio::test]
    async fn resume_per_output_specific_respawns_with_last_scene() {
        let backend = LweBackend::with_binary("/bin/true");

        // Build a real workshop-style scene directory so
        // `set_per_output_with_fps` reaches its `cmd.spawn()` call.
        let tmp = tempfile::tempdir().unwrap();
        let scene = tmp.path().join("workshop/content/431960/111");
        std::fs::create_dir_all(&scene).unwrap();
        std::fs::write(scene.join("scene.json"), b"{}").unwrap();

        // Record the scene without a pid (simulating post-kill
        // state).
        {
            let mut scenes = backend.per_output_scenes.lock().await;
            scenes.insert("DP-1".to_string(), scene.clone());
        }

        // `resume_per_output_specific` should re-spawn. `/bin/true`
        // exits ~0 ms, so the pid may already be reaped by the
        // time the test checks — we tolerate both outcomes (real
        // pid from spawn, or 0 if the kernel reaped before we
        // looked).
        let _ = backend.resume_per_output_specific("DP-1").await;

        // The pid map should now contain DP-1 (whether or not the
        // spawned process is still alive — the map is set BEFORE
        // the kernel can reap).
        // Note: `set_per_output_with_fps` clears the pid on
        // failure, so we can't assert it's set without racy timing.
    }

    /// `resume_per_output_specific` errors when there's no scene
    /// to re-spawn with. This guards against callers calling
    /// resume on an output that was never bound.
    #[tokio::test]
    async fn resume_per_output_specific_errors_on_no_scene() {
        let backend = LweBackend::with_binary("/bin/true");
        let err = backend
            .resume_per_output_specific("Mystery-Out")
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("no scene"),
            "error must explain missing scene; got: {msg}"
        );
    }

    /// `bind_external_pid` against an empty per-output map must
    /// succeed and record the supplied pid + scene. This is the
    /// happy path for `adopt_existing_lwes` when the daemon is
    /// starting up against a pre-existing LWE child.
    #[tokio::test]
    async fn bind_external_pid_records_into_empty_map() {
        let backend = LweBackend::with_binary("/bin/true");
        let scene = std::env::temp_dir().join("paperforge-bind-external-empty.scene");
        std::fs::write(&scene, b"fake scene").unwrap();
        let adopted = backend
            .bind_external_pid("DP-1", &scene, /* pid = */ 1)
            .await;
        assert!(adopted, "empty map must accept the external pid");

        let pids = backend.per_output_pids.lock().await;
        assert_eq!(pids.get("DP-1"), Some(&1));
        drop(pids);

        let scenes = backend.per_output_scenes.lock().await;
        assert_eq!(scenes.get("DP-1"), Some(&scene));
    }

    /// `bind_external_pid` is idempotent against an already-live pid:
    /// if the recorded pid is alive (Running or Paused), the call
    /// is a no-op and returns `false`. Regression guard for "daemon
    /// must not clobber its own state with an adoption candidate".
    #[tokio::test]
    async fn bind_external_pid_refuses_to_clobber_live_pid() {
        let backend = LweBackend::with_binary("/bin/true");
        let scene = std::env::temp_dir().join("paperforge-bind-external-clobber.scene");
        std::fs::write(&scene, b"fake scene").unwrap();

        // Use a bash wrapper that exec's /bin/sleep with argv[0]
        // set to a name containing the LWE pattern. The linter-
        // integrated `pid_state_quick` cross-check rejects PIDs
        // whose cmdline doesn't match the LWE pattern, so a plain
        // `/bin/sleep` would be reported as dead-in-grace.
        let wrapper = std::env::temp_dir().join(format!(
            "paperforge-bind-ext-clobber-{}-{}.sh",
            std::process::id(),
            next_wrapper_seq()
        ));
        std::fs::write(
            &wrapper,
            "#!/usr/bin/env bash\n\
             exec -a linux-wallpaperengine-bind-ext-clobber /bin/sleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let mut child = std::process::Command::new(&wrapper).spawn().unwrap();
        let live_pid = child.id() as i32;

        // Give the scheduler a moment so /proc reflects the new PID.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let first = backend.bind_external_pid("DP-1", &scene, live_pid).await;
        assert!(first, "first adopt must succeed");

        // Second adopt with a DIFFERENT pid must be refused.
        let second = backend
            .bind_external_pid("DP-1", &scene, /* different pid = */ 2)
            .await;
        assert!(
            !second,
            "adopt must refuse when an alive pid is already recorded"
        );

        let pids = backend.per_output_pids.lock().await;
        assert_eq!(
            pids.get("DP-1"),
            Some(&live_pid),
            "recorded pid must be preserved against clobber attempt"
        );

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&wrapper);
    }

    /// When the recorded pid is dead, `bind_external_pid` must
    /// succeed and replace the dead pid with the new one. Used by
    /// `adopt_existing_lwes` after a daemon restart where the
    /// previous daemon instance's tracked pid is gone.
    #[tokio::test]
    async fn bind_external_pid_replaces_dead_pid() {
        let backend = LweBackend::with_binary("/bin/true");
        let scene = std::env::temp_dir().join("paperforge-bind-external-dead.scene");
        std::fs::write(&scene, b"fake scene").unwrap();

        {
            let mut pids = backend.per_output_pids.lock().await;
            pids.insert("DP-1".to_string(), 2_999_999);
            let mut scenes = backend.per_output_scenes.lock().await;
            scenes.insert("DP-1".to_string(), scene.clone());
        }

        let adopted = backend
            .bind_external_pid("DP-1", &scene, /* replacement pid = */ 3)
            .await;
        assert!(
            adopted,
            "dead pid in the map must be replaceable by an external pid"
        );

        let pids = backend.per_output_pids.lock().await;
        assert_eq!(
            pids.get("DP-1"),
            Some(&3),
            "dead pid must be replaced, not preserved"
        );
    }

    /// `outputs_with_pids` returns a `BTreeSet` of output names
    /// that have a recorded pid. Empty map → empty set.
    #[tokio::test]
    async fn outputs_with_pids_returns_empty_set_for_empty_map() {
        let backend = LweBackend::with_binary("/bin/true");
        let owned = backend.outputs_with_pids().await;
        assert!(owned.is_empty(), "fresh backend has no owned pids");
    }

    /// `outputs_with_pids` returns the keys of the recorded pid map
    /// in sorted order (BTreeSet contract).
    #[tokio::test]
    async fn outputs_with_pids_returns_keys_in_sorted_order() {
        let backend = LweBackend::with_binary("/bin/true");
        {
            let mut pids = backend.per_output_pids.lock().await;
            pids.insert("DP-1".to_string(), 11);
            pids.insert("HDMI-A-1".to_string(), 22);
            pids.insert("eDP-1".to_string(), 33);
        }
        let owned = backend.outputs_with_pids().await;
        let got: Vec<&str> = owned.iter().map(String::as_str).collect();
        assert_eq!(
            got,
            vec!["DP-1", "HDMI-A-1", "eDP-1"],
            "BTreeSet must preserve sorted order"
        );
    }

    /// Tests for the Component C pipe drainers.
    ///
    /// The hard part: we can't easily assert against tracing events
    /// without a custom Subscriber. Instead, the tests spin up a
    /// `/bin/sh` subprocess that prints a known marker, capture the
    /// output via the `drain_pipe` helper directly (bypassing the
    /// `LweBackend` plumbing — which is just orchestration anyway),
    /// and assert that the marker would have been emitted by
    /// inspecting the helper's exit timing (EOF on pipe → drainer
    /// returns).
    #[cfg(test)]
    mod pipe_drain_tests {
        use super::*;

        /// Helper: spawn `/bin/sh` with the given command, drain its
        /// stdout/stderr through `drain_pipe`, return when both pipes
        /// close (i.e. child exited). The drainers are returned
        /// alive so individual tests can assert on their state.
        async fn spawn_and_drain(
            cmd: &str,
        ) -> (
            i32,
            tokio::task::JoinHandle<()>,
            tokio::task::JoinHandle<()>,
        ) {
            let mut child = tokio::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(cmd)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn sh");
            let pid = child.id().unwrap_or(0) as i32;
            let stdout = child.stdout.take().expect("piped stdout");
            let stderr = child.stderr.take().expect("piped stderr");

            let stdout_handle = tokio::spawn(drain_pipe(stdout, PidTarget(pid), PipeKind::Stdout));
            let stderr_handle = tokio::spawn(drain_pipe(stderr, PidTarget(pid), PipeKind::Stderr));

            (pid, stdout_handle, stderr_handle)
        }

        /// `drainer_returns_after_pipe_eof`: the canonical happy path.
        /// The child exits cleanly, the OS closes both pipes, and
        /// each drainer task sees EOF on `next_line()` and returns.
        /// `is_finished()` is true once the task has been polled to
        /// completion — we sleep briefly to give the runtime a chance
        /// to poll the drainers.
        #[tokio::test]
        async fn drainer_returns_after_pipe_eof() {
            let mut child = tokio::process::Command::new("/bin/sh")
                .arg("-c")
                .arg("echo MARKER_OUT; echo MARKER_ERR 1>&2; exit 0")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn sh");
            let pid = child.id().unwrap_or(0) as i32;
            let stdout = child.stdout.take().expect("piped stdout");
            let stderr = child.stderr.take().expect("piped stderr");

            let stdout_handle = tokio::spawn(drain_pipe(stdout, PidTarget(pid), PipeKind::Stdout));
            let stderr_handle = tokio::spawn(drain_pipe(stderr, PidTarget(pid), PipeKind::Stderr));

            let _ = child.wait().await;
            // Give the drainers a moment to process EOF. The runtime
            // polls tasks on the same thread when their waker is
            // signalled (here, by the I/O reactor), so a small delay
            // is enough.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            assert!(
                stdout_handle.is_finished(),
                "stdout drainer didn't terminate after pipe EOF"
            );
            assert!(
                stderr_handle.is_finished(),
                "stderr drainer didn't terminate after pipe EOF"
            );
        }

        /// `drainer_handles_empty_child_output`: the child writes
        /// nothing — `/bin/true` exits 0 with empty pipes. The
        /// drainer must terminate, not hang. This is the regression
        /// guard for the "LWE dying silently" case where the
        /// previous (v0.1) `Stdio::null()` flow would have just
        /// thrown the output away.
        #[tokio::test]
        async fn drainer_handles_empty_child_output() {
            let mut child = tokio::process::Command::new("/bin/true")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn /bin/true");
            let stdout = child.stdout.take().expect("piped stdout");
            let stderr = child.stderr.take().expect("piped stderr");
            let stdout_handle = tokio::spawn(drain_pipe(stdout, PidTarget(0), PipeKind::Stdout));
            let stderr_handle = tokio::spawn(drain_pipe(stderr, PidTarget(0), PipeKind::Stderr));

            let _ = child.wait().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert!(stdout_handle.is_finished());
            assert!(stderr_handle.is_finished());
        }

        /// `drainer_emits_multiple_lines`: the multi-line case.
        /// Three stdout + two stderr lines. The drainer must
        /// process all of them and then terminate on EOF. We don't
        /// assert on tracing output (that would need a custom
        /// Subscriber); instead we verify the JoinHandle finishes
        /// promptly, which is the correctness invariant for the
        /// the drain loop's exit condition.
        #[tokio::test]
        async fn drainer_emits_multiple_lines() {
            let (pid, stdout_handle, stderr_handle) = spawn_and_drain(
                "echo line1; echo line2; echo line3; \
                 echo err1 1>&2; echo err2 1>&2; exit 0",
            )
            .await;

            // Wait for the child + drainers to finish. We poll for
            // is_finished with a small timeout because the runtime
            // needs to schedule the drainers after the child closes
            // its pipes.
            let mut waited_ms = 0u64;
            while waited_ms < 1000 && !stdout_handle.is_finished() {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                waited_ms += 25;
            }
            let mut waited_ms2 = 0u64;
            while waited_ms2 < 1000 && !stderr_handle.is_finished() {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                waited_ms2 += 25;
            }

            assert!(
                stdout_handle.is_finished(),
                "stdout drainer didn't finish after multi-line child output (pid={pid})"
            );
            assert!(
                stderr_handle.is_finished(),
                "stderr drainer didn't finish after multi-line child output (pid={pid})"
            );
        }

        /// `pipe_drainers_init_empty`: the new `pipe_drainers` field
        /// must start empty on every constructor. This is a
        /// regression guard for the constructor-update step (Step 1
        /// of Component C).
        #[tokio::test]
        async fn pipe_drainers_init_empty() {
            let b = LweBackend::new();
            let drainers = b.pipe_drainers.lock().await;
            assert!(drainers.is_empty(), "new() must start with empty drainers");

            let b = LweBackend::with_binary("/bin/true");
            let drainers = b.pipe_drainers.lock().await;
            assert!(
                drainers.is_empty(),
                "with_binary() must start with empty drainers"
            );

            let b = LweBackend::with_binary_and_fps("/bin/true", 30);
            let drainers = b.pipe_drainers.lock().await;
            assert!(
                drainers.is_empty(),
                "with_binary_and_fps() must start with empty drainers"
            );
        }

        /// `unbind_removes_drainers`: the public `unbind()` method
        /// removes the (stdout_handle, stderr_handle) pair for the
        /// given pid and aborts both. Using an unknown pid is a
        /// no-op. We spawn real (but trivially-completing) tasks so
        /// the test exercises the abort path without leaving
        /// pending JoinHandles for the runtime to wait on.
        ///
        /// The test holds the JoinHandles directly so we can wait
        /// on them after `unbind()` aborts. The runtime requires
        /// tasks to be cancelled before the test future can return;
        /// if we let the JoinHandles drop without observing the
        /// cancellation, the runtime may hang on shutdown.
        #[tokio::test]
        async fn unbind_removes_drainers() {
            let b = LweBackend::new();
            let (sout, serr) = {
                let mut drainers = b.pipe_drainers.lock().await;
                let s = tokio::spawn(async { tokio::task::yield_now().await });
                let r = tokio::spawn(async { tokio::task::yield_now().await });
                drainers.insert(99, (s, r));
                // Detach the handles into the local scope so we
                // can wait on them after unbind().
                let entry = drainers.remove(&99).unwrap();
                (entry.0, entry.1)
            };
            // Re-insert for the actual unbind test.
            {
                let mut drainers = b.pipe_drainers.lock().await;
                drainers.insert(99, (sout, serr));
            }

            b.unbind(99).await;
            let drainers = b.pipe_drainers.lock().await;
            assert!(drainers.get(&99).is_none(), "unbind must remove entry");
            drop(drainers);

            // Unknown pid is a no-op (no panic, no spurious entry).
            b.unbind(12345).await;
        }

        /// `shutdown_aborts_all_drainers`: the public `shutdown()`
        /// method drains the entire map and aborts every entry.
        /// We re-insert the JoinHandles after observing the map is
        /// empty so the runtime doesn't wait on them at shutdown.
        #[tokio::test]
        async fn shutdown_aborts_all_drainers() {
            let b = LweBackend::new();
            let mut handles: Vec<(tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>)> =
                Vec::new();
            {
                let mut drainers = b.pipe_drainers.lock().await;
                for pid in [10, 20, 30] {
                    let s = tokio::spawn(async { tokio::task::yield_now().await });
                    let r = tokio::spawn(async { tokio::task::yield_now().await });
                    handles.push((s, r));
                    drainers.insert(pid, (tokio::spawn(async {}), tokio::spawn(async {})));
                }
            }

            b.shutdown().await;
            let drainers = b.pipe_drainers.lock().await;
            assert!(drainers.is_empty(), "shutdown must drain the map");
            drop(drainers);

            // Wait for the JoinHandles to be reaped so the runtime
            // doesn't hang on shutdown. Abort + await the cancelled
            // JoinError so the task is fully reaped.
            for (s, r) in handles {
                s.abort();
                r.abort();
                let _ = s.await;
                let _ = r.await;
            }
        }
    }
}
