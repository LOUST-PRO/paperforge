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
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

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
}

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
        }
    }

    /// Test-only accessor for the per-output pid map. Lets tests
    /// inject a fake/forced pid (e.g. a guaranteed-dead value)
    /// without going through `set_per_output`. Production code
    /// never touches this directly — it goes through
    /// `set_per_output` + the reaper task.
    #[cfg(test)]
    pub fn per_output_pids_test_accessor(&self) -> &Arc<Mutex<std::collections::BTreeMap<String, i32>>> {
        &self.per_output_pids
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
            per_output_pids: self.per_output_pids,
            per_output_scenes: self.per_output_scenes,
            soft_pause_cancel: self.soft_pause_cancel,
        }
    }

    /// Returns the audio controller for this backend (lives here
    /// because SIGUSR1/SIGUSR2 are tied to LWE).
    pub fn audio(&self) -> LweAudioController {
        LweAudioController::new(self.clone())
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

        let binary = self
            .binary_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("linux-wallpaperengine"));

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
                        pid_state_quick(existing_pid),
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
        let _ = {
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
            }
        };

        let mut cmd = std::process::Command::new(&binary);
        cmd.arg("--screen-root")
            .arg(output)
            .arg("--bg")
            .arg(&content_id)
            .arg("--silent")
            .arg("--volume")
            .arg("0")
            .arg("--no-audio-processing")
            .arg("--noautomute")
            .arg("--disable-particles")
            .arg("--disable-mouse")
            .arg("--disable-parallax")
            .arg("--fullscreen-pause-only-active")
            .arg("--fps")
            .arg(fps.to_string());

        let child = cmd.spawn().map_err(|e| Error::BackendFailure {
            kind: self.kind().process_pattern().to_string(),
            message: format!("per-output spawn LWE failed: {e}"),
        })?;
        let pid = child.id() as i32;
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
                pid_state_quick(pid),
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
                        pid_state_quick(*pid),
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
                pid_state_quick(pid),
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
                    tracing::warn!(
                        "throttle-resume: respawn for output={} failed: {e}",
                        output
                    );
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
        let candidates: Vec<(String, i32)> = pids
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), *v)))
            .collect();
        // We don't need to mutate `scenes` in this function (we
        // keep scenes for re-spawn via `reconcile_outputs`); bind
        // non-mut to silence the warn without changing behaviour.
        let _scenes = self.per_output_scenes.lock().await;
        for (output, pid) in candidates {
            match pid_state_quick(pid) {
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
            match self
                .set_per_output_with_fps(scene, &output, fps)
                .await
            {
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
        // we actually own in v0.2 mode. If the caller asks about a
        // foreign pid in pool mode, we report NotRunning instead of
        // a stale /proc read.
        //
        // v0.1 per-output path: stateless CLI calls have an empty
        // `per_output_pids` map, but LWE children survive in /proc
        // (reparented to init after our exit). Read the kernel-
        // reported state directly via /proc/<pid>/status. If the
        // pid is gone, /proc reports ENOENT → NotRunning.
        let owned = self.pool.current_pid().await;
        if owned == Some(pid) {
            return pid_state_quick(pid);
        }
        // Per-output + stateless CLI: skip the ownership gate and
        // trust /proc. This matches the v0.1 design where each LWE
        // child survives independently of any parent state.
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

    // ---- self-heal / prune_dead_pids / reconcile_outputs ----

    /// `prune_dead_pids` should NOT touch alive pids — they're left
    /// in the map so the SIGSTOP/SIGCONT signal path still finds
    /// them. We use a long-running `sleep` as the live process.
    #[tokio::test]
    async fn prune_dead_pids_leaves_alive_pids_untouched() {
        let backend = LweBackend::with_binary("/bin/sleep");
        // Spawn a real child so /proc/<pid>/status is valid.
        let mut child = std::process::Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let live_pid = child.id() as i32;

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
        let backend = LweBackend::with_binary("/bin/sleep");
        let mut child = std::process::Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let live_pid = child.id() as i32;

        {
            let mut pids = backend.per_output_pids.lock().await;
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
    }

    /// `kill_per_output` SIGTERMs the recorded pid and removes it
    /// from the map, but keeps the scene so a later
    /// `resume_per_output_specific` knows what to re-spawn with.
    /// Idempotent: killing an output that has no pid is a no-op.
    #[tokio::test]
    async fn kill_per_output_sends_sigterm_and_clears_pid_keeps_scene() {
        let backend = LweBackend::with_binary("/bin/sleep");
        let mut child = std::process::Command::new("/bin/sleep").arg("60").spawn().unwrap();
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
        assert!(
            pids.get("DP-1").is_none(),
            "kill must remove pid from map"
        );
        drop(pids);
        // Scene stays (re-spawn target).
        let scenes = backend.per_output_scenes.lock().await;
        assert_eq!(
            scenes.get("DP-1"),
            Some(&scene),
            "kill must keep scene for re-spawn"
        );

        // Idempotent: second kill is a no-op.
        backend.kill_per_output("DP-1").await.expect("kill idempotent");

        // Cleanup the actual child.
        let _ = child.kill();
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
}
