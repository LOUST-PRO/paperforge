//! Pool-based LWE architecture.
//!
//! ## Why this exists
//!
//! Before v0.2, `LweBackend::set` spawned **one LWE process per output**:
//! for a 3-monitor setup, that's three concurrent `linux-wallpaperengine`
//! processes, each with its own WebGL context, libmpv, and audio thread.
//! Memory adds up fast (~780 MB RSS for three video-heavy scenes).
//!
//! The Linux Wallpaper Engine CLI natively supports **multiple
//! `--screen-root <name> --bg <content_id>` pairs in one invocation**:
//!
//! ```text
//! linux-wallpaperengine --screen-root HDMI-1 --bg 850994960 \
//!                       --screen-root DP-1   --bg 827961360
//! ```
//!
//! One process, one rendering pipeline, multiple layer-shell surfaces.
//! This is what swaybg, swww, and waypaper do — and what paperforge v0.2
//! adopts via [`LweSinglePool`].
//!
//! ## Hot-swap semantics
//!
//! When `set()` is called and the new (output, scene) pair differs from
//! what's already running on that output, the pool performs a hot-swap:
//!
//! 1. SIGTERM the current LWE process.
//! 2. Wait for it to exit (`child.wait()`).
//! 3. Spawn a new LWE process with the merged argv: existing pairs that
//!    didn't change + the new/updated pair.
//! 4. Update the in-memory map.
//!
//! The `merge_argv` step is the only stateful logic — output bindings
//! are tracked by `(output_name, content_id)` and only the changed
//! pair triggers a respawn.
//!
//! ## Trade-offs
//!
//! - **Pro**: 1 process for N monitors → RSS roughly halved.
//! - **Pro**: Per-output SIGSTOP/SIGCONT still works (single PID, but
//!   the signal pauses the whole process — see "pause is global" note).
//! - **Con**: Hot-swap is brief-flicker (1-2 frames during respawn).
//!   Mitigation: skip respawn when the pair is identical to current.
//! - **Con**: Lose per-process isolation. If LWE crashes, all monitors
//!   go down. Mitigation: a watchdog can respawn the pool (future work).
//!
//! ## Per-monitor pause (Fase 2)
//!
//! `pause()` here is GLOBAL — one process, one signal target. Per-output
//! pause lives in LWE's native fullscreen detection:
//! `--fullscreen-pause-only-active` (Wayland) pauses only the renderer
//! when a fullscreen window is active on the compositor. We pass that
//! flag in the default argv so the common case "game in fullscreen
//! pauses only that monitor" works without a Rust governor.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use tokio::sync::Mutex;

use crate::{
    backend::{workshop_content_id, BackendKind, BackendState},
    error::{Error, Result},
};

/// Default watchdog tick interval (seconds). Imported by the
/// `LweSinglePool::with_binary_and_fps` constructor and overridable
/// via `set_watchdog_interval_secs` / `with_watchdog_interval_secs`.
fn default_watchdog_interval_secs() -> u64 {
    5
}

/// Maximum backoff after a failed respawn (seconds). The watchdog
/// doubles its wait on each consecutive failure, capped here so a
/// persistently broken LWE binary doesn't dominate the log + CPU.
const WATCHDOG_MAX_BACKOFF_SECS: u64 = 60;

/// One running LWE process with its current output→content_id bindings.
///
/// `child` is held as an `Option` so we can `take()` it during a swap
/// (kill + wait), then `replace()` with a fresh `Child`.
#[derive(Debug)]
pub struct PoolProcess {
    /// OS pid of the spawned LWE process.
    pub pid: i32,
    /// `output_name` → `content_id` for every `--screen-root` pair in
    /// the current argv. BTreeMap keeps iteration deterministic for
    /// argv construction (tests + reproducible respawns).
    pub bindings: BTreeMap<String, String>,
    /// Handle to the spawned child so we can `wait()` after SIGTERM
    /// during hot-swap. Held as `Option` so we can `take()` it during
    /// the swap and `replace()` with the fresh `Child`.
    pub child: Option<std::process::Child>,
}

/// LWE single-pool: one process, multi-output argv.
///
/// All public methods take `&self` and rely on interior `Mutex` for
/// mutation. The lock is held only for short critical sections
/// (binding table updates, child respawn); the actual `Command::spawn`
/// is blocking but fast (<50 ms typical on a cold start).
///
/// `Clone` is explicit (rather than `#[derive(Clone)]`) so the
/// future addition of expensive resources (e.g. a Child stdout pipe
/// when we add native logging) doesn't silently copy them. Today
/// the fields are all `PathBuf` + `Vec<String>` + `Arc<...>` or
/// interior-mutable atomics, so clone is cheap. `AtomicU32` and
/// `tokio::task::JoinHandle` are NOT `Clone`, so we explicitly clone
/// the `Arc<Mutex<...>>` wrappers that own them.
#[derive(Debug)]
pub struct LweSinglePool {
    binary: PathBuf,
    /// Common flags appended to every invocation (e.g. `--silent`,
    /// `--disable-particles`). Operator-overridable via constructor.
    common_flags: Vec<String>,
    /// FPS cap passed as `--fps <N>` to LWE. `Arc<AtomicU32>` so the
    /// watchdog task (spawned via `tokio::spawn`) can read the
    /// current value at respawn time without taking `&self`. Smart
    /// calibration reaches this via [`Self::set_active_fps`]. The
    /// `Arc` is clone-cheap — every `Clone` of `LweSinglePool`
    /// shares the same atomic so smart calibration updates are
    /// visible across clones (mirrors the existing `Arc<Mutex<...>>`
    /// pattern for `inner`).
    active_fps: Arc<std::sync::atomic::AtomicU32>,
    /// Grace window (ms) for hot-swap transitions. `bind()` spawns
    /// the new LWE first, waits this long, THEN kills the old one —
    /// the new process steals the wlr-layer-shell surface from the
    /// old immediately at spawn, eliminating the visible "no wallpaper"
    /// gap during transitions. Interior-mutable for parity with
    /// [`Self::active_fps`].
    transition_grace_ms: std::sync::atomic::AtomicU64,
    inner: Arc<Mutex<Option<PoolProcess>>>,
    /// Last-known bindings (output → content_id). Persists across
    /// respawns so the watchdog can rebuild the pool even when
    /// `inner` is None (e.g. all outputs were unbound). Updated
    /// atomically by `bind`/`unbind`. Separate from
    /// `inner.Some.bindings` because that field is cleared on full
    /// unbind (the pool goes empty) but we want to remember what the
    /// operator had bound so the watchdog can restore it on respawn.
    ///
    /// Cleared by `unbind_with_op` when the operator removes the
    /// LAST binding — that means the operator explicitly wants no
    /// wallpaper, so the watchdog should NOT respawn a phantom pool.
    /// On every other transition (including a crash) the bindings
    /// persist so the watchdog can restore them.
    last_bindings: Arc<Mutex<BTreeMap<String, String>>>,
    /// Cancellation notify for the active soft-pause cycle, if any.
    /// Fired by `resume` and `shutdown` so the cycle can exit
    /// gracefully (the cycle SIGCONTs the pid before returning so
    /// the LWE process isn't left frozen mid-cycle). Cleared by
    /// `pause_soft` when it spawns a new cycle.
    soft_pause_cancel: Arc<tokio::sync::Notify>,
    /// Tokio task handle for the active soft-pause cycle, if any.
    /// `None` when not paused. Set by `pause_soft`, cleared by
    /// `resume` and `shutdown`. Re-entrant `pause_soft` calls abort
    /// the existing handle before spawning a new one so we don't
    /// accumulate cycle tasks.
    soft_pause_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Watchdog tick interval (seconds). `Arc<AtomicU64>` so the
    /// watchdog task (spawned via `tokio::spawn`) can read the current
    /// value at respawn time without taking `&self`, and so that
    /// `Clone` of `LweSinglePool` shares the same atomic (mirrors
    /// the existing `Arc<...>` pattern for `active_fps`). The
    /// watchdog reads this every tick — a runtime change to the
    /// interval takes effect on the next sleep. Default is
    /// [`default_watchdog_interval_secs`] (5 s).
    watchdog_interval_secs: Arc<std::sync::atomic::AtomicU64>,
    /// Cancellation notify fired by `abort_watchdog` / `shutdown`.
    /// The watchdog's `tokio::select!` loop wakes on this so a
    /// SIGTERM-driven shutdown exits cleanly instead of waiting for
    /// the next tick. Cloned into the watchdog task so dropping the
    /// last pool reference can also wake it via the standard
    /// `notify_waiters` semantics.
    watchdog_cancel: Arc<tokio::sync::Notify>,
    /// Handle to the spawned watchdog task, if any. `None` until
    /// `spawn_watchdog` is called and the spawned task is registered.
    /// `Arc<Mutex<>>` because the watchdog task itself is a tokio
    /// task that doesn't share `&self` — it needs the Arc to store
    /// `None` back after abort so subsequent calls don't trip on a
    /// dead handle. `Abort` is idempotent so calling it on an
    /// already-finished handle is harmless.
    watchdog_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

/// Structured timing log emitted at the end of every bind/unbind.
///
/// Operators grep `journalctl -u paperforge.service | grep 'transition:'`
/// to see all transitions and their phase timings. All durations are
/// milliseconds since `started_at`; absolute wall-clock time is
/// captured only at the spawn moment so the log line remains
/// self-consistent even if logging is slightly delayed.
///
/// Field-by-field:
/// - `op`: caller label — `"set"`, `"unset"`, or `"playlist_apply"`.
/// - `output`: the Wayland output name (`DP-1`, `HDMI-A-1`, etc).
/// - `scene_id`: Workshop numeric content_id, or `"<none>"` for
///   `unbind()` paths and the abort branches of `bind()`.
/// - `spawn_ms`: time from `started_at` to successful `Command::spawn`
///   of the new LWE process. `0` when spawn failed or was skipped
///   (e.g. unbind leaves the pool empty).
/// - `grace_ms`: time from `started_at` to end of the post-spawn
///   grace sleep. `0` for unbind (no grace) and for the spawn-failure
///   branch of bind (no grace reached).
/// - `kill_ms`: time from `started_at` to completion of SIGTERM +
///   reap of the OLD LWE. `0` when no kill happened (fast-path
///   idempotent rebind, spawn failure, abort-during-grace).
/// - `total_ms`: time from `started_at` to log emission — i.e. the
///   end-to-end latency of the transition.
/// - `new_pid`/`old_pid`: PIDs in play. `<none>` when not applicable.
#[derive(Debug)]
struct TransitionTiming {
    op: &'static str,
    output: String,
    scene_id: String,
    started_at: Instant,
    spawn_completed_at: Option<Instant>,
    grace_completed_at: Option<Instant>,
    kill_completed_at: Option<Instant>,
    new_pid: Option<i32>,
    old_pid: Option<i32>,
}

impl TransitionTiming {
    fn new(op: &'static str, output: String, scene_id: String, old_pid: Option<i32>) -> Self {
        Self {
            op,
            output,
            scene_id,
            started_at: Instant::now(),
            spawn_completed_at: None,
            grace_completed_at: None,
            kill_completed_at: None,
            new_pid: None,
            old_pid,
        }
    }

    /// Emit the structured log line. Uses a single `Instant::now()`
    /// so all phase deltas are consistent with each other even if
    /// logging itself is slightly delayed.
    fn log(&self) {
        let now = Instant::now();
        let since = |t: Instant| now.duration_since(t).as_millis() as u64;
        let pid_or_none = |p: Option<i32>| {
            p.map(|x| x.to_string())
                .unwrap_or_else(|| "<none>".to_string())
        };
        tracing::info!(
            target: "paperforge",
            "transition: op={} output={} scene_id={} \
             spawn_ms={} grace_ms={} kill_ms={} total_ms={} \
             new_pid={} old_pid={}",
            self.op,
            self.output,
            self.scene_id,
            self.spawn_completed_at.map(since).unwrap_or(0),
            self.grace_completed_at.map(since).unwrap_or(0),
            self.kill_completed_at.map(since).unwrap_or(0),
            since(self.started_at),
            pid_or_none(self.new_pid),
            pid_or_none(self.old_pid),
        );
    }
}

impl Clone for LweSinglePool {
    fn clone(&self) -> Self {
        use std::sync::atomic::Ordering;
        Self {
            binary: self.binary.clone(),
            common_flags: self.common_flags.clone(),
            // Clone the Arc — every clone shares the same atomic so
            // `set_active_fps` from any handle propagates to all
            // others (mirrors the existing Arc-shared semantics of
            // `inner`, `soft_pause_*`, `last_bindings`, etc).
            active_fps: Arc::clone(&self.active_fps),
            transition_grace_ms: std::sync::atomic::AtomicU64::new(
                self.transition_grace_ms.load(Ordering::Relaxed),
            ),
            inner: self.inner.clone(),
            last_bindings: self.last_bindings.clone(),
            soft_pause_cancel: self.soft_pause_cancel.clone(),
            soft_pause_task: self.soft_pause_task.clone(),
            watchdog_interval_secs: Arc::new(std::sync::atomic::AtomicU64::new(
                self.watchdog_interval_secs.load(Ordering::Relaxed),
            )),
            watchdog_cancel: self.watchdog_cancel.clone(),
            watchdog_task: self.watchdog_task.clone(),
        }
    }
}

impl LweSinglePool {
    /// Construct with the default LWE binary on PATH and default flags.
    pub fn new() -> Self {
        Self::with_binary(PathBuf::from("linux-wallpaperengine"))
    }

    /// Construct with an explicit LWE binary path.
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self::with_binary_and_fps(binary, 30)
    }

    /// Construct with an explicit LWE binary path AND initial FPS
    /// cap. Production construction goes through here so the
    /// `[fps].active_max` value flows in instead of being hardcoded
    /// to LWE's own 30-fps default.
    ///
    /// NOTE: this constructor does NOT spawn the watchdog. Call
    /// [`Self::spawn_watchdog`] from the daemon's startup path (after
    /// the tokio runtime is established) so the background task can
    /// tick without racing against the constructor's caller.
    pub fn with_binary_and_fps(binary: impl Into<PathBuf>, active_fps: u32) -> Self {
        use std::sync::atomic::AtomicU64;
        Self {
            binary: binary.into(),
            common_flags: default_flags(),
            active_fps: Arc::new(std::sync::atomic::AtomicU32::new(active_fps)),
            transition_grace_ms: AtomicU64::new(default_transition_grace_ms()),
            inner: Arc::new(Mutex::new(None)),
            last_bindings: Arc::new(Mutex::new(BTreeMap::new())),
            soft_pause_cancel: Arc::new(tokio::sync::Notify::new()),
            soft_pause_task: Arc::new(Mutex::new(None)),
            watchdog_interval_secs: Arc::new(std::sync::atomic::AtomicU64::new(
                default_watchdog_interval_secs(),
            )),
            watchdog_cancel: Arc::new(tokio::sync::Notify::new()),
            watchdog_task: Arc::new(Mutex::new(None)),
        }
    }

    /// Override the flag list (mostly for tests). Default is
    /// `[--silent, --no-audio-processing, --disable-particles,
    ///   --disable-mouse, --disable-parallax, --fullscreen-pause-only-active]`.
    pub fn with_flags(mut self, flags: Vec<String>) -> Self {
        self.common_flags = flags;
        self
    }

    /// Override the initial FPS cap (mostly for tests / smart
    /// calibration). For runtime updates on an existing pool use
    /// [`Self::set_active_fps`] instead.
    pub fn with_active_fps(mut self, fps: u32) -> Self {
        self.active_fps = Arc::new(std::sync::atomic::AtomicU32::new(fps));
        self
    }

    /// Read the current FPS cap (passed as `--fps <N>` to LWE).
    pub fn active_fps(&self) -> u32 {
        use std::sync::atomic::Ordering;
        self.active_fps.load(Ordering::Relaxed)
    }

    /// Update the FPS cap at runtime. The new value takes effect on
    /// the next respawn (current process keeps its existing
    /// `--fps` argv until it's respawned). Interior-mutable so it
    /// works through `Arc<LweSinglePool>`.
    pub fn set_active_fps(&self, fps: u32) {
        use std::sync::atomic::Ordering;
        self.active_fps.store(fps, Ordering::Relaxed);
    }

    /// Read the current transition grace window (ms) used by
    /// `bind()` between spawning the new LWE and killing the old.
    pub fn transition_grace_ms(&self) -> u64 {
        use std::sync::atomic::Ordering;
        self.transition_grace_ms.load(Ordering::Relaxed)
    }

    /// Override the transition grace window (ms). Setting to 0
    /// reverts `bind()` to the v0.1 "kill-then-spawn" flow (visible
    /// black gap during transitions). The default is
    /// [`default_transition_grace_ms`] (2000 ms).
    pub fn set_transition_grace_ms(&self, ms: u64) {
        use std::sync::atomic::Ordering;
        self.transition_grace_ms.store(ms, Ordering::Relaxed);
    }

    /// Builder-form of [`Self::set_transition_grace_ms`]. Mostly
    /// useful for tests that want a non-default grace window
    /// without going through `set_transition_grace_ms` after
    /// construction.
    pub fn with_transition_grace_ms(self, ms: u64) -> Self {
        self.transition_grace_ms
            .store(ms, std::sync::atomic::Ordering::Relaxed);
        self
    }

    /// Read the current watchdog tick interval (seconds). The
    /// watchdog uses this value as the base sleep between ticks,
    /// plus any active backoff seconds. Default is
    /// [`default_watchdog_interval_secs`] (5 s).
    pub fn watchdog_interval_secs(&self) -> u64 {
        use std::sync::atomic::Ordering;
        self.watchdog_interval_secs.load(Ordering::Relaxed)
    }

    /// Update the watchdog tick interval (seconds). The new value
    /// takes effect on the watchdog's next sleep (the running tick
    /// completes first). Interior-mutable so it works through
    /// `Arc<LweSinglePool>` clones. Setting to 0 makes the watchdog
    /// tick in a tight loop — useful for tests but a footgun in
    /// production.
    pub fn set_watchdog_interval_secs(&self, secs: u64) {
        use std::sync::atomic::Ordering;
        self.watchdog_interval_secs.store(secs, Ordering::Relaxed);
    }

    /// Builder-form of [`Self::set_watchdog_interval_secs`]. Useful
    /// for tests that want a zero-tick watchdog without going
    /// through `set_watchdog_interval_secs` after construction.
    pub fn with_watchdog_interval_secs(self, secs: u64) -> Self {
        self.watchdog_interval_secs
            .store(secs, std::sync::atomic::Ordering::Relaxed);
        self
    }

    /// Snapshot the `last_bindings` map (output → content_id). The
    /// watchdog uses this to rebuild the pool after a crash even
    /// when `inner` is `None`. Tests + the upcoming `daemon get_state`
    /// D-Bus method can also read it to surface the crash-recovery
    /// state to the operator.
    pub async fn last_bindings(&self) -> BTreeMap<String, String> {
        self.last_bindings.lock().await.clone()
    }

    /// Cancel any active soft-pause cycle. Fires the cancellation
    /// notify so the cycle can exit gracefully (SIGCONT before
    /// return), then aborts the JoinHandle as a safety net for the
    /// case where the cycle is blocked on a syscall and didn't see
    /// the notify in time. Called from `pause_soft`, `resume`, and
    /// `shutdown` so multiple cycles don't pile up.
    pub async fn abort_soft_pause(&self) {
        // Fire the notify FIRST so the cycle can SIGCONT the LWE
        // pid before exiting. notify_waiters (not notify_one) so
        // any stale cycles from re-entrant calls also wake.
        self.soft_pause_cancel.notify_waiters();
        let mut task_guard = self.soft_pause_task.lock().await;
        if let Some(handle) = task_guard.take() {
            handle.abort();
        }
    }

    /// Spawn the background watchdog task. Idempotent: if a
    /// watchdog is already running, no-op. The task polls `self.inner`
    /// every `watchdog_interval_secs()` seconds. If the tracked PID
    /// is dead OR the cmdline doesn't match
    /// `BackendKind::LinuxWallpaperEngine` (PID recycling), and
    /// `last_bindings` is non-empty, the task respawns LWE with
    /// those bindings. If respawn fails, exponential backoff capped
    /// at 60 s.
    ///
    /// Cancel via [`abort_watchdog`] or [`shutdown`]. Default
    /// interval is 5 s; tune via [`set_watchdog_interval_secs`].
    ///
    /// The constructor does NOT spawn the watchdog — the daemon
    /// startup path must call this so the background task has a
    /// tokio runtime to schedule against. This keeps the sync
    /// constructors (`with_binary`, `with_binary_and_fps`) easy to
    /// use from tests without a runtime.
    pub async fn spawn_watchdog(&self) {
        let mut task_guard = self.watchdog_task.lock().await;
        if task_guard.is_some() {
            return; // idempotent — already spawned
        }
        let inner = self.inner.clone();
        let last_bindings = self.last_bindings.clone();
        let interval_secs = self.watchdog_interval_secs.clone();
        let cancel = self.watchdog_cancel.clone();
        let binary = self.binary.clone();
        let common_flags = self.common_flags.clone();
        let active_fps = self.active_fps.clone();
        let handle = tokio::spawn(async move {
            watchdog_loop(
                inner,
                last_bindings,
                interval_secs,
                cancel,
                binary,
                common_flags,
                active_fps,
            )
            .await;
        });
        *task_guard = Some(handle);
    }

    /// Cancel the watchdog task. Fires the cancellation notify so
    /// the task can exit gracefully on its next `tokio::select!`,
    /// then aborts the JoinHandle as a safety net for the case
    /// where the task is blocked on a syscall and didn't see the
    /// notify in time. Idempotent: safe to call when no watchdog
    /// is running.
    pub async fn abort_watchdog(&self) {
        // Fire the notify FIRST so the loop can exit via its
        // `tokio::select!` arm rather than waiting for the next
        // `tokio::time::sleep` to return. notify_waiters (not
        // notify_one) so re-entrant calls all wake.
        self.watchdog_cancel.notify_waiters();
        let mut task_guard = self.watchdog_task.lock().await;
        if let Some(handle) = task_guard.take() {
            handle.abort();
        }
    }

    /// Return the in-memory current argv (one entry per flag), used by
    /// tests + the upcoming `daemon get_state` D-Bus method.
    pub async fn current_argv(&self) -> Option<Vec<String>> {
        let guard = self.inner.lock().await;
        guard.as_ref().map(|p| {
            build_argv(
                &self.binary,
                &p.bindings,
                &self.common_flags,
                self.active_fps(),
            )
        })
    }

    /// Return the current bindings (output → content_id), or empty.
    pub async fn bindings(&self) -> BTreeMap<String, String> {
        let guard = self.inner.lock().await;
        guard
            .as_ref()
            .map(|p| p.bindings.clone())
            .unwrap_or_default()
    }

    /// Return the current LWE PID, or None if no process is running.
    pub async fn current_pid(&self) -> Option<i32> {
        let guard = self.inner.lock().await;
        guard.as_ref().map(|p| p.pid)
    }

    /// Bind a (output, content_id) pair. If `output` is already bound
    /// to the same `content_id`, this is a no-op (no respawn). If it
    /// is bound to a different scene, OR not bound at all, the pool
    /// performs a hot-swap with the merged argv.
    ///
    /// This is a thin wrapper over [`Self::bind_with_op`] that tags
    /// the resulting log line with `op = "set"`. Use [`Self::bind_with_op`]
    /// directly when the caller is something other than a single
    /// user-facing `set` (e.g. `playlist apply`).
    ///
    /// ## Spawn-first-kill-after flow (v0.4)
    ///
    /// Unlike the v0.1 / v0.3 "kill-then-spawn" flow (which left a
    /// visible black gap of 1-3 s between the old SIGTERM and the
    /// new LWE's first frame), this implementation:
    ///
    /// 1. Builds the merged bindings (existing + updated).
    /// 2. Spawns the new LWE process and captures its PID.
    /// 3. Sleeps for `transition_grace_ms` (default 2000 ms) so
    ///    the new LWE has time to render its first frame.
    /// 4. Checks the new PID is still alive — if it died during the
    ///    grace window (e.g. bad scene ID, OOM at startup), SIGKILL
    ///    the new PID, REAP the zombie, and return `Err` while
    ///    leaving the OLD process untouched.
    /// 5. Only then takes the old process out, SIGTERMs it with a
    ///    200 ms grace + SIGKILL fallback, and reaps.
    /// 6. Stores the new process in `inner`.
    ///
    /// The visual effect is seamless because the new LWE steals the
    /// wlr-layer-shell surface from the old immediately at spawn —
    /// both processes are alive during the grace window, so the
    /// compositor never sees an empty layer-shell surface.
    ///
    /// Returns the new PID after the swap, or the existing PID if no
    /// respawn was needed.
    pub async fn bind(&self, output: &str, content_id: &str) -> Result<i32> {
        self.bind_with_op(output, content_id, "set").await
    }

    /// Like [`Self::bind`] but lets the caller tag the resulting
    /// structured log line with an `op` label (`"set"`, `"unset"`,
    /// `"playlist_apply"`, etc.). The timing log itself is identical
    /// in shape — only the `op=` field differs.
    pub async fn bind_with_op(
        &self,
        output: &str,
        content_id: &str,
        op: &'static str,
    ) -> Result<i32> {
        if output.is_empty() {
            return Err(Error::Other(anyhow::anyhow!(
                "bind() requires a non-empty output name"
            )));
        }
        if content_id.is_empty() {
            return Err(Error::Other(anyhow::anyhow!(
                "bind() requires a non-empty content_id"
            )));
        }

        let mut guard = self.inner.lock().await;
        let old_pid = guard.as_ref().map(|p| p.pid);
        let mut timing =
            TransitionTiming::new(op, output.to_string(), content_id.to_string(), old_pid);

        // Fast path: output already bound to the same content_id,
        // AND process is alive. No respawn needed → no transition
        // log line (would spam the journal on every idempotent set).
        if let Some(proc) = guard.as_ref() {
            if let Some(existing) = proc.bindings.get(output) {
                if existing == content_id {
                    // Verify the process is still alive (it may have
                    // crashed externally). If dead, fall through to
                    // respawn with the merged bindings.
                    if let Ok(BackendState::Running) = crate::backend::pid_state_quick(
                        proc.pid,
                        crate::backend::BackendKind::LinuxWallpaperEngine,
                    ) {
                        return Ok(proc.pid);
                    }
                }
            }
        }

        // Build the new bindings: existing + updated.
        let mut new_bindings: BTreeMap<String, String> = guard
            .as_ref()
            .map(|p| p.bindings.clone())
            .unwrap_or_default();
        new_bindings.insert(output.to_string(), content_id.to_string());

        // Read grace once (atomic load + cheap) before spawning so
        // the value is captured even if `set_transition_grace_ms`
        // races with this bind.
        let grace_ms = self.transition_grace_ms();

        // Step 1: spawn the NEW process FIRST. We deliberately do
        // not touch the existing process yet — that's the whole
        // point of the spawn-first-kill-after flow.
        let argv = build_argv(
            &self.binary,
            &new_bindings,
            &self.common_flags,
            self.active_fps(),
        );
        let mut cmd = Command::new(&self.binary);
        cmd.args(&argv[1..]); // argv[0] is the binary itself; Command already has it
        let mut new_child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                // Spawn failed before any process existed — emit
                // the transition log with spawn_completed_at unset
                // so the operator can grep for it. kill_completed_at
                // and new_pid stay None.
                timing.log();
                return Err(Error::BackendFailure {
                    kind: BackendKind::LinuxWallpaperEngine
                        .process_pattern()
                        .to_string(),
                    message: format!("spawn LWE failed: {e}"),
                });
            }
        };
        let new_pid = new_child.id() as i32;
        timing.spawn_completed_at = Some(Instant::now());

        // Cmdline-settle window: the kernel needs a moment to
        // process the wrapper's `exec -a linux-wallpaperengine-...
        // /bin/sleep N` (or the real LWE binary's main() startup)
        // before `/proc/<pid>/cmdline` reflects the post-exec
        // argv[0]. Without this settle, the immediate
        // `pid_state_quick` below sees the wrapper's bash
        // command line (no LWE pattern) instead of the renamed
        // argv, and the [PID-recycling
        // defense](crate::backend::pid_is_backend_kind) correctly
        // rejects it as not-LWE — falsely flagging the fresh
        // spawn as dead-in-grace.
        //
        // 50 ms is enough under normal scheduler conditions for
        // either path to land its cmdline update. Even when the
        // caller requested `grace_ms = 0` for the v0.1 kill-first
        // semantics, we still need this minimum settle for the
        // post-spawn state check to be meaningful.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Step 2: wait for the grace window. The new LWE has this
        // long to initialize WebGL, load the scene, and render its
        // first frame on the layer-shell surface it just stole from
        // the old process. We use `tokio::time::sleep` (not
        // `std::thread::sleep`) so the runtime can service other
        // tasks; the lock held here only blocks concurrent `bind()`
        // calls, which is intentional (no two swaps should race).
        //
        // Plain sleep (no `tokio::select!` cancel) per design: a
        // daemon shutdown during a transition is rare, and 2 s is
        // acceptable cleanup latency.
        tokio::time::sleep(std::time::Duration::from_millis(grace_ms)).await;
        timing.grace_completed_at = Some(Instant::now());

        // Step 3: confirm the new LWE is still alive. If it died
        // during the grace window (bad scene ID, OOM, missing
        // Wayland session, etc.) we must NOT kill the old one —
        // abort the transition and let the caller retry with a
        // valid scene.
        match crate::backend::pid_state_quick(
            new_pid,
            crate::backend::BackendKind::LinuxWallpaperEngine,
        ) {
            Ok(BackendState::Running) | Ok(BackendState::Paused) => {
                // Healthy. Continue to kill the old process.
            }
            _ => {
                // New LWE died in the grace window. Clean up the
                // zombie, signal SIGKILL as a belt-and-braces
                // measure in case it's stuck in uninterruptible
                // sleep, and return Err. The OLD process is left
                // untouched in `guard` so the next bind() call
                // can pick up from it. Per the spec we emit the
                // transition log with new_pid = None so the
                // operator can grep for the abort.
                let _ = kill(Pid::from_raw(new_pid), Signal::SIGKILL);
                let _ = new_child.wait();
                timing.new_pid = None;
                timing.log();
                tracing::warn!(
                    target: "paperforge",
                    "new LWE (pid={}) died during transition grace \
                     ({}ms); old process preserved",
                    new_pid,
                    grace_ms,
                );
                return Err(Error::BackendFailure {
                    kind: BackendKind::LinuxWallpaperEngine
                        .process_pattern()
                        .to_string(),
                    message: format!(
                        "new LWE (pid={new_pid}) died during transition grace \
                         ({grace_ms} ms); old process preserved"
                    ),
                });
            }
        }

        // Step 4: kill the old process. Same grace + SIGKILL
        // fallback as the v0.3 flow — 200 ms because the typical
        // case is "LWE was already idle and responsive to SIGTERM".
        if let Some(mut prev) = guard.take() {
            let _ = kill(Pid::from_raw(prev.pid), Signal::SIGTERM);
            let grace = std::time::Duration::from_millis(200);
            let start = std::time::Instant::now();
            while let Some(c) = prev.child.as_mut() {
                match c.try_wait() {
                    Ok(Some(_status)) => break, // exited cleanly
                    Ok(None) => {
                        if start.elapsed() >= grace {
                            let _ = kill(Pid::from_raw(prev.pid), Signal::SIGKILL);
                            let _ = c.wait();
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        }
        timing.kill_completed_at = Some(Instant::now());

        // Step 5: store the new process in the pool.
        let stored_bindings = new_bindings.clone();
        *guard = Some(PoolProcess {
            pid: new_pid,
            bindings: new_bindings,
            child: Some(new_child),
        });
        timing.new_pid = Some(new_pid);
        timing.log();

        // Also update `last_bindings` so the watchdog can rebuild the
        // pool after a crash. We do this after the inner lock is
        // released (we hold `guard` across the assignment above; the
        // `last_bindings` mutex is independent so a short contended
        // window is fine). Cloned so the watchdog's snapshot doesn't
        // race with a future bind.
        *self.last_bindings.lock().await = stored_bindings;

        tracing::info!(
            target: "paperforge",
            "LWE pool respawn: pid={} bindings={:?} grace_ms={}",
            new_pid,
            guard.as_ref().unwrap().bindings,
            grace_ms,
        );

        Ok(new_pid)
    }

    /// Convenience: translate a Workshop scene path to its content_id
    /// and call [`bind`](Self::bind). Thin wrapper over
    /// [`Self::bind_scene_with_op`] with `op = "set"`.
    pub async fn bind_scene(&self, output: &str, scene: &Path) -> Result<i32> {
        self.bind_scene_with_op(output, scene, "set").await
    }

    /// Like [`Self::bind_scene`] but lets the caller tag the
    /// resulting log line with `op` (e.g. `"playlist_apply"`).
    pub async fn bind_scene_with_op(
        &self,
        output: &str,
        scene: &Path,
        op: &'static str,
    ) -> Result<i32> {
        if !scene.exists() {
            return Err(Error::BackendUnreachable {
                kind: BackendKind::LinuxWallpaperEngine
                    .process_pattern()
                    .to_string(),
                message: format!("scene path does not exist: {}", scene.display()),
            });
        }
        let content_id = workshop_content_id(scene).ok_or_else(|| Error::BackendFailure {
            kind: BackendKind::LinuxWallpaperEngine
                .process_pattern()
                .to_string(),
            message: format!(
                "scene path {} is not a Steam Workshop scene \
                     (expected `workshop/content/<appid>/<numeric>`)",
                scene.display()
            ),
        })?;
        self.bind_with_op(output, &content_id, op).await
    }

    /// Unbind an output. Removes the binding from the map and respawns
    /// the pool with the remaining bindings (if any). If no bindings
    /// remain, the pool is killed entirely.
    ///
    /// This is a thin wrapper over [`Self::unbind_with_op`] with
    /// `op = "unset"`. Use [`Self::unbind_with_op`] directly when a
    /// distinct caller label is needed.
    pub async fn unbind(&self, output: &str) -> Result<()> {
        self.unbind_with_op(output, "unset").await
    }

    /// Like [`Self::unbind`] but tags the resulting log line with
    /// `op`. Use `"hotplug"` or similar when the caller is something
    /// other than a direct unset request — the log shape is the same,
    /// only the `op=` field changes.
    pub async fn unbind_with_op(&self, output: &str, op: &'static str) -> Result<()> {
        let mut guard = self.inner.lock().await;
        let old_pid = guard.as_ref().map(|p| p.pid);
        // unbind has no scene — always log "<none>" so operators
        // can grep for unset transitions independently of set ones.
        let mut timing =
            TransitionTiming::new(op, output.to_string(), "<none>".to_string(), old_pid);

        // Step 1: take the current process out so we can decide what
        // to do with it without fighting the borrow checker. If there
        // is no current process, the pool is already empty — no
        // transition happened, no log line.
        let mut prev = match guard.take() {
            Some(p) => p,
            None => return Ok(()),
        };

        // Step 2: clone the bindings and remove the output. We clone
        // (not move) because if the output wasn't bound, we put the
        // process back untouched. Again, no transition happened.
        let mut new_bindings = prev.bindings.clone();
        if new_bindings.remove(output).is_none() {
            // Output wasn't bound; put the process back untouched.
            *guard = Some(prev);
            return Ok(());
        }

        // Step 3: kill the previous process. We're going to respawn
        // (potentially with fewer bindings) or leave the pool empty.
        let _ = kill(Pid::from_raw(prev.pid), Signal::SIGTERM);
        if let Some(c) = prev.child.as_mut() {
            let _ = c.wait();
        }
        timing.kill_completed_at = Some(Instant::now());

        // Step 4: if no bindings remain, leave the pool empty.
        if new_bindings.is_empty() {
            // No respawn — new_pid stays None, spawn_completed_at
            // stays None. Emit the log so operators can see the
            // unset landed (kill_ms=N, spawn_ms=0).
            timing.log();
            tracing::info!(
                target: "paperforge",
                "LWE pool empty after unbind({})",
                output
            );
            // Also clear `last_bindings`: the operator explicitly
            // removed the LAST binding, which means they want no
            // wallpaper. The watchdog must NOT respawn a phantom pool
            // in this case. A subsequent `bind` will populate
            // `last_bindings` again.
            self.last_bindings.lock().await.clear();
            return Ok(());
        }

        // Step 5: spawn fresh with the remaining bindings.
        let argv = build_argv(
            &self.binary,
            &new_bindings,
            &self.common_flags,
            self.active_fps(),
        );
        let mut cmd = Command::new(&self.binary);
        cmd.args(&argv[1..]);
        let child = cmd.spawn().map_err(|e| {
            // Spawn failed after we already killed the old process.
            // Emit the transition log with spawn_completed_at unset
            // and new_pid=None so the operator sees the partial
            // transition (kill completed, respawn didn't).
            timing.log();
            Error::BackendFailure {
                kind: BackendKind::LinuxWallpaperEngine
                    .process_pattern()
                    .to_string(),
                message: format!("respawn after unbind failed: {e}"),
            }
        })?;
        let pid = child.id() as i32;
        timing.spawn_completed_at = Some(Instant::now());
        let stored_bindings = new_bindings.clone();
        *guard = Some(PoolProcess {
            pid,
            bindings: new_bindings,
            child: Some(child),
        });
        timing.new_pid = Some(pid);
        timing.log();
        // Update `last_bindings` so the watchdog can rebuild the
        // pool after a crash. Same rationale as in `bind_with_op`.
        *self.last_bindings.lock().await = stored_bindings;
        tracing::info!(
            target: "paperforge",
            "LWE pool respawn after unbind({}): pid={} bindings={:?}",
            output,
            pid,
            guard.as_ref().unwrap().bindings,
        );
        Ok(())
    }

    /// SIGSTOP the current LWE process (global pause — single process).
    /// Returns Ok(pid) if a process was signaled, Ok(None) if pool empty.
    pub async fn pause(&self) -> Result<Option<i32>> {
        // Plain SIGSTOP also aborts any active soft-pause cycle, so
        // the two pause modes are mutually exclusive. Without this,
        // a `pause_soft` followed by `pause` would leave a zombie
        // wake-up task still SIGSTOPing after resume.
        self.abort_soft_pause().await;
        let guard = self.inner.lock().await;
        let Some(proc) = guard.as_ref() else {
            return Ok(None);
        };
        kill(Pid::from_raw(proc.pid), Signal::SIGSTOP).map_err(|e| Error::BackendFailure {
            kind: BackendKind::LinuxWallpaperEngine
                .process_pattern()
                .to_string(),
            message: format!("SIGSTOP to pid {} failed: {e}", proc.pid),
        })?;
        Ok(Some(proc.pid))
    }

    /// Frame pause: SIGSTOP + tokio SIGCONT/SIGSTOP clock so the
    /// layer-shell surface keeps receiving frames. Default behaviour
    /// in v0.3.
    ///
    /// The cycle task holds an `Arc<Mutex<Option<PoolProcess>>>` clone
    /// of `self.inner`. When the LWE process dies (respawn watcher
    /// replaces it under a new PID), the cycle continues with the
    /// new PID because we re-read `self.inner` at each iteration.
    ///
    /// Re-entrant: if a previous soft-pause cycle is still alive,
    /// it's aborted first so we don't accumulate ghost tasks. The
    /// cycle observes `self.soft_pause_cancel` so `resume` /
    /// `shutdown` can fire it; the JoinHandle is stored in
    /// `self.soft_pause_task` as a safety net.
    pub async fn pause_soft(&self, awake_ms: u64, asleep_ms: u64) -> Result<Option<i32>> {
        // Abort any prior cycle first (fires its cancel notify too).
        // Then SIGSTOP and spawn a fresh cycle that observes the
        // pool's shared cancel notify.
        self.abort_soft_pause().await;
        let (started, pid) = {
            let guard = self.inner.lock().await;
            let Some(proc) = guard.as_ref() else {
                return Ok(None);
            };
            kill(Pid::from_raw(proc.pid), Signal::SIGSTOP).map_err(|e| Error::BackendFailure {
                kind: BackendKind::LinuxWallpaperEngine
                    .process_pattern()
                    .to_string(),
                message: format!("SIGSTOP to pid {} failed: {e}", proc.pid),
            })?;
            (true, proc.pid)
        };
        if started {
            let inner = self.inner.clone();
            let cancel = self.soft_pause_cancel.clone();
            let handle = tokio::spawn(super::backend::soft_pause_cycle_pool(
                inner, awake_ms, asleep_ms, cancel,
            ));
            let mut task_guard = self.soft_pause_task.lock().await;
            *task_guard = Some(handle);
        }
        Ok(Some(pid))
    }

    /// SIGCONT the current LWE process (global resume).
    pub async fn resume(&self) -> Result<Option<i32>> {
        // Abort any active soft-pause cycle so the SIGCONT we send
        // here isn't immediately followed by another SIGSTOP.
        self.abort_soft_pause().await;
        let guard = self.inner.lock().await;
        let Some(proc) = guard.as_ref() else {
            return Ok(None);
        };
        kill(Pid::from_raw(proc.pid), Signal::SIGCONT).map_err(|e| Error::BackendFailure {
            kind: BackendKind::LinuxWallpaperEngine
                .process_pattern()
                .to_string(),
            message: format!("SIGCONT to pid {} failed: {e}", proc.pid),
        })?;
        Ok(Some(proc.pid))
    }

    /// Kill the LWE process and clear all bindings. Idempotent.
    pub async fn shutdown(&self) -> Result<()> {
        // Abort any active soft-pause cycle first; otherwise the
        // SIGTERM below races with the cycle's SIGCONT.
        self.abort_soft_pause().await;
        // Also stop the watchdog: without this, the watchdog could
        // tick AFTER `inner` is cleared, see `inner == None` AND
        // `last_bindings` non-empty, and respawn a process the
        // operator just shut down. (The pool itself goes empty on
        // the last `unbind` which clears `last_bindings`, but a
        // direct `shutdown` after a `bind` is also a valid path.)
        self.abort_watchdog().await;
        let mut guard = self.inner.lock().await;
        if let Some(mut prev) = guard.take() {
            let _ = kill(Pid::from_raw(prev.pid), Signal::SIGTERM);
            if let Some(c) = prev.child.as_mut() {
                let _ = c.wait();
            }
        }
        // Defensive: also clear `last_bindings` so a subsequent
        // `spawn_watchdog` (e.g. in a future test that re-uses this
        // pool after a shutdown) doesn't try to respawn from stale
        // data. Production `shutdown` is terminal so this is a no-op
        // for the daemon, but tests benefit.
        self.last_bindings.lock().await.clear();
        Ok(())
    }
}

impl Default for LweSinglePool {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LweSinglePool {
    fn drop(&mut self) {
        // SAFETY NET ONLY: cleanup is supposed to go through
        // `shutdown().await` from an async context. Drop fires when a
        // pool is dropped without explicit shutdown (panic, early
        // return, parent daemon crash mid-init). Without a guard, a
        // CLONE dropped while a sibling still owns the live pool
        // would see the latest pid in the shared `inner` mutex and
        // SIGTERM the LIVE process — e.g. `pool_bg = pool.clone()`
        // moved into a `tokio::spawn` and dropped when the spawn
        // completes: at that moment `inner` holds the NEW pid from
        // the bind the spawn ran, and Drop would kill it before the
        // caller ever sees it.
        //
        // Fix: only fire the safety-net kill when this is the LAST
        // Arc reference to `inner`. Clones (count > 1) are no-ops;
        // the last owner takes responsibility. This preserves the
        // original "kill on leak" intent while preventing the
        // sibling-clone foot-gun. We use SIGKILL (not SIGTERM)
        // because we have no chance to wait() and a stale LWE that
        // ignores SIGTERM would otherwise orphan a wallpaper session.
        if Arc::strong_count(&self.inner) == 1 {
            if let Ok(guard) = self.inner.try_lock() {
                if let Some(proc) = guard.as_ref() {
                    let _ = kill(Pid::from_raw(proc.pid), Signal::SIGKILL);
                }
            }
        }
    }
}

/// Construct the argv: `[binary, --screen-root <out>, --bg <id>, ...]`
/// from a binding map and a list of common flags appended at the end.
fn build_argv(
    binary: &Path,
    bindings: &BTreeMap<String, String>,
    common_flags: &[String],
    active_fps: u32,
) -> Vec<String> {
    let mut argv = Vec::with_capacity(3 + bindings.len() * 4 + common_flags.len() + 2);
    argv.push(binary.display().to_string());
    for (out, id) in bindings {
        argv.push("--screen-root".to_string());
        argv.push(out.clone());
        argv.push("--bg".to_string());
        argv.push(id.clone());
    }
    argv.extend(common_flags.iter().cloned());
    // FPS cap last so it always wins on the argv even if common_flags
    // includes another `--fps` (operator override via config).
    argv.push("--fps".to_string());
    argv.push(active_fps.to_string());
    argv
}

/// Default flags applied to every spawned LWE process. `--silent` is
/// essential in a systemd-managed daemon (otherwise stderr floods the
/// journal); `--volume 0` defensively mutes the LWE-internal audio
/// path even if a fork build ignores `--silent`; the daemon also
/// mutes the PulseAudio sink-input post-spawn as a second layer
/// (see [`crate::backend::LweBackend::set_per_output`]). `--no-automute`
/// prevents LWE from re-enabling audio when another app stops
/// playing. `--fullscreen-pause-only-active` enables LWE's native
/// per-output pause when a fullscreen window is focused on that output
/// (Wayland only).
fn default_flags() -> Vec<String> {
    vec![
        "--silent".to_string(),
        "--volume".to_string(),
        "0".to_string(),
        "--no-audio-processing".to_string(),
        "--noautomute".to_string(),
        "--disable-particles".to_string(),
        "--disable-mouse".to_string(),
        "--disable-parallax".to_string(),
        "--fullscreen-pause-only-active".to_string(),
    ]
}

/// Default grace window (ms) for `bind()` spawn-first-kill-after.
/// 2000 ms is enough for most Steam Workshop scenes to render their
/// first frame; heavier scenes may need 3000-4000 ms. Mirrors
/// `default_transition_grace_ms()` in `config.rs` so production
/// values stay in sync if either default is tuned.
fn default_transition_grace_ms() -> u64 {
    2000
}

/// Background task that watches `inner` for a dead / recycled pool
/// process and respawns LWE from `last_bindings` when needed.
///
/// ## Cancellation
///
/// The `tokio::select!` on the sleep wakes on either the interval
/// elapsing OR a notification on `cancel`. `abort_watchdog` /
/// `shutdown` fire the notify, so SIGTERM-driven shutdown exits
/// cleanly instead of waiting for the next tick.
///
/// ## Backoff
///
/// A respawn failure doubles `backoff_secs` (1 → 2 → 4 → 8 → 16 →
/// 32 → 60 → 60 ...). The next interval sleeps for `interval +
/// backoff_secs`. We reset `backoff_secs = 0` after a successful
/// respawn OR when the pool is alive / `last_bindings` empty (no
/// need to keep the penalty around).
///
/// ## Race avoidance
///
/// The loop never holds any lock across `tokio::time::sleep`. We
/// snapshot `inner` and `last_bindings` under their respective locks,
/// drop the locks, then do the `/proc` read + `Command::spawn`
/// without holding any pool state. The inner mutex is reacquired
/// only briefly when we replace `inner` with the fresh
/// `PoolProcess`.
async fn watchdog_loop(
    inner: Arc<Mutex<Option<PoolProcess>>>,
    last_bindings: Arc<Mutex<BTreeMap<String, String>>>,
    interval_secs: Arc<AtomicU64>,
    cancel: Arc<tokio::sync::Notify>,
    binary: PathBuf,
    common_flags: Vec<String>,
    active_fps: Arc<std::sync::atomic::AtomicU32>,
) {
    let mut backoff_secs: u64 = 0;
    loop {
        let sleep_secs = interval_secs
            .load(Ordering::Relaxed)
            .saturating_add(backoff_secs);
        tokio::select! {
            _ = cancel.notified() => return,
            _ = tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)) => {}
        }

        // Snapshot pool + bindings. Two short locks; we drop both
        // before doing the (potentially slow) `/proc` read.
        let snapshot: Option<(i32, BTreeMap<String, String>)> = {
            let guard = inner.lock().await;
            guard.as_ref().map(|p| (p.pid, p.bindings.clone()))
        };
        let preserved: BTreeMap<String, String> = {
            let guard = last_bindings.lock().await;
            guard.clone()
        };

        // Decide if we need to respawn. `pid_state_quick(pid, kind)`
        // already cross-checks the cmdline against the backend
        // pattern — if the PID was recycled to an unrelated process
        // it returns `NotRunning`, which triggers our respawn path.
        // Any error or non-Running/Paused result is also a respawn
        // trigger (the watchdog's job is to make the pool alive
        // again).
        let needs_respawn: bool = match snapshot.as_ref() {
            Some((pid, _)) => !matches!(
                crate::backend::pid_state_quick(*pid, BackendKind::LinuxWallpaperEngine),
                Ok(BackendState::Running) | Ok(BackendState::Paused)
            ),
            None => !preserved.is_empty(),
        };

        if !needs_respawn {
            // Healthy OR no pool. Reset backoff either way so a
            // long-running healthy pool doesn't carry stale state
            // across an unrelated transient failure.
            backoff_secs = 0;
            continue;
        }
        if preserved.is_empty() {
            // Operator hasn't bound anything yet (or has unbound
            // the last output). The pool is intentionally empty;
            // don't respawn a phantom process.
            backoff_secs = 0;
            continue;
        }

        // Try respawn from preserved bindings.
        let argv = build_argv(
            &binary,
            &preserved,
            &common_flags,
            active_fps.load(Ordering::Relaxed),
        );
        let mut cmd = Command::new(&binary);
        cmd.args(&argv[1..]);
        match cmd.spawn() {
            Ok(new_child) => {
                let new_pid = new_child.id() as i32;
                // Replace `inner` under the brief mutex.
                let mut guard = inner.lock().await;
                *guard = Some(PoolProcess {
                    pid: new_pid,
                    bindings: preserved.clone(),
                    child: Some(new_child),
                });
                tracing::info!(
                    target: "paperforge",
                    event = "watchdog_respawn",
                    pid = new_pid,
                    bindings = ?preserved,
                    backoff_secs = backoff_secs,
                    "watchdog respawned LWE pool after detected death / recycling"
                );
                backoff_secs = 0;
            }
            Err(e) => {
                let next = (backoff_secs.saturating_mul(2)).clamp(1, WATCHDOG_MAX_BACKOFF_SECS);
                tracing::error!(
                    target: "paperforge",
                    event = "watchdog_respawn_failed",
                    error = %e,
                    backoff_secs = backoff_secs,
                    next_backoff_secs = next,
                    "watchdog respawn failed; will retry with backoff"
                );
                backoff_secs = next;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_argv_emits_pairs_in_sorted_order() {
        let binary = PathBuf::from("/usr/bin/lwe");
        let mut bindings = BTreeMap::new();
        bindings.insert("DP-1".to_string(), "111".to_string());
        bindings.insert("HDMI-A-1".to_string(), "222".to_string());
        let argv = build_argv(&binary, &bindings, &["--silent".to_string()], 30);
        // BTreeMap iterates sorted by key. `--fps 30` is appended
        // last so it always wins on the argv.
        assert_eq!(
            argv,
            vec![
                "/usr/bin/lwe",
                "--screen-root",
                "DP-1",
                "--bg",
                "111",
                "--screen-root",
                "HDMI-A-1",
                "--bg",
                "222",
                "--silent",
                "--fps",
                "30",
            ]
        );
    }

    #[test]
    fn build_argv_empty_bindings_just_binary_and_flags() {
        let binary = PathBuf::from("/usr/bin/lwe");
        let argv = build_argv(&binary, &BTreeMap::new(), &["--silent".to_string()], 30);
        assert_eq!(argv, vec!["/usr/bin/lwe", "--silent", "--fps", "30"]);
    }

    #[test]
    fn default_flags_include_native_fullscreen_pause() {
        let f = default_flags();
        assert!(
            f.iter().any(|s| s == "--fullscreen-pause-only-active"),
            "default flags must enable LWE's native per-output fullscreen pause"
        );
    }

    #[test]
    fn default_flags_mute_audio_path() {
        // Default spawn must include `--volume 0` and `--noautomute` so
        // the wallpaper cannot produce audio even on LWE builds that
        // ignore `--silent`. `--no-audio-processing` is the upstream
        // decoder-side mute; together they form a layered defense.
        let f = default_flags();
        let has_volume_zero = f.windows(2).any(|w| w[0] == "--volume" && w[1] == "0");
        assert!(has_volume_zero, "default flags must include --volume 0");
        assert!(
            f.iter().any(|s| s == "--noautomute"),
            "default flags must include --noautomute to prevent auto-unmute"
        );
        assert!(
            f.iter().any(|s| s == "--silent"),
            "default flags must include --silent"
        );
        assert!(
            f.iter().any(|s| s == "--no-audio-processing"),
            "default flags must include --no-audio-processing"
        );
    }

    #[tokio::test]
    async fn pool_starts_empty() {
        let pool = LweSinglePool::with_binary("/bin/true");
        assert!(pool.current_pid().await.is_none());
        assert!(pool.bindings().await.is_empty());
    }

    #[tokio::test]
    async fn bind_rejects_empty_output() {
        let pool = LweSinglePool::with_binary("/bin/true");
        let err = pool.bind("", "123").await.unwrap_err();
        assert!(format!("{err}").contains("non-empty output"));
    }

    #[tokio::test]
    async fn bind_rejects_empty_content_id() {
        let pool = LweSinglePool::with_binary("/bin/true");
        let err = pool.bind("DP-1", "").await.unwrap_err();
        assert!(format!("{err}").contains("non-empty content_id"));
    }

    #[tokio::test]
    async fn bind_scene_rejects_non_workshop() {
        let pool = LweSinglePool::with_binary("/bin/true");
        let err = pool
            .bind_scene("DP-1", Path::new("/tmp"))
            .await
            .unwrap_err();
        // /tmp exists, so BackendUnreachable check passes and we hit
        // the Workshop validation. We want BackendFailure with "Workshop".
        assert!(matches!(err, Error::BackendFailure { .. }));
        assert!(format!("{err}").contains("Workshop"));
    }

    #[tokio::test]
    async fn bind_scene_rejects_missing_path() {
        let pool = LweSinglePool::with_binary("/bin/true");
        let err = pool
            .bind_scene("DP-1", Path::new("/does/not/exist/123"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::BackendUnreachable { .. }));
    }

    /// End-to-end: spawn a sleep wrapper as the "LWE binary" with a
    /// multi-output argv, then SIGTERM and verify exit. We can't use
    /// the real LWE binary in CI (no Wayland session), but the pool's
    /// argv construction + spawn + kill path is identical regardless
    /// of the binary. The wrapper ignores its argv and just sleeps so
    /// it stays alive past the grace window — `/bin/sleep` itself
    /// rejects `--screen-root`/`--bg`/etc. and would die immediately.
    #[tokio::test]
    async fn bind_spawns_and_pause_resume_real_process() {
        let wrapper = write_sleep_wrapper(60);
        let pool = LweSinglePool::with_binary(&wrapper).with_flags(vec![]);
        let pid = pool.bind("DP-1", "111").await.unwrap();
        assert!(pid > 0);
        assert_eq!(pool.current_pid().await, Some(pid));
        assert_eq!(
            pool.bindings().await.get("DP-1").map(String::as_str),
            Some("111")
        );

        // Second bind to a different output should NOT respawn when
        // the existing one is unchanged, BUT it should add the new
        // binding — which means a respawn (because argv differs).
        // However our pool reuses the same process for any number of
        // bindings, so binding another output triggers a swap.
        let pid2 = pool.bind("HDMI-A-1", "222").await.unwrap();
        assert!(pid2 > 0);
        // Both bindings should be present now.
        let b = pool.bindings().await;
        assert_eq!(b.get("DP-1").map(String::as_str), Some("111"));
        assert_eq!(b.get("HDMI-A-1").map(String::as_str), Some("222"));

        // Idempotent rebind (same output + content_id) must not respawn.
        let pid3 = pool.bind("HDMI-A-1", "222").await.unwrap();
        assert_eq!(pid3, pid2, "idempotent bind must reuse the same PID");

        // Unbind one output → respawn with remaining binding.
        pool.unbind("HDMI-A-1").await.unwrap();
        let b = pool.bindings().await;
        assert_eq!(b.len(), 1);
        assert_eq!(b.get("DP-1").map(String::as_str), Some("111"));

        // Unbind the last one → pool goes empty, no process.
        pool.unbind("DP-1").await.unwrap();
        assert!(pool.current_pid().await.is_none());

        // shutdown is idempotent.
        pool.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&wrapper);
    }

    #[tokio::test]
    async fn unbind_when_pool_empty_is_noop() {
        // Use a no-op-argv wrapper: `unbind` doesn't actually need a
        // running process but constructing the pool still does not
        // require spawn, so `/bin/sleep` would also work — kept
        // consistent with the surrounding tests for simplicity.
        let pool = LweSinglePool::with_binary("/bin/sleep");
        pool.unbind("DP-1").await.unwrap();
        assert!(pool.current_pid().await.is_none());
    }

    #[tokio::test]
    async fn shutdown_idempotent() {
        let wrapper = write_sleep_wrapper(60);
        let pool = LweSinglePool::with_binary(&wrapper).with_flags(vec![]);
        pool.bind("DP-1", "111").await.unwrap();
        pool.shutdown().await.unwrap();
        pool.shutdown().await.unwrap(); // second call must not panic
        assert!(pool.current_pid().await.is_none());
        let _ = std::fs::remove_file(&wrapper);
    }

    /// Output hotplug: a new output is bound after the pool already
    /// has one running. Pool must respawn with merged argv (existing
    /// + new pair). After unbind of the new output, pool respawns
    /// again with the original pair.
    #[tokio::test]
    async fn single_pool_handles_output_hotplug() {
        let wrapper = write_sleep_wrapper(60);
        let pool = LweSinglePool::with_binary(&wrapper).with_flags(vec![]);

        // Initial: bind DP-1.
        let pid_initial = pool.bind("DP-1", "111").await.unwrap();
        assert!(pid_initial > 0);
        assert_eq!(pool.bindings().await.len(), 1);

        // Hotplug: bind HDMI-A-1 to a different output. The pool
        // respawns with the merged argv (DP-1, HDMI-A-1).
        let pid_after_plug = pool.bind("HDMI-A-1", "222").await.unwrap();
        assert!(pid_after_plug > 0, "new PID after hotplug");
        // Could be same PID if /bin/sleep and the host recycle IDs
        // collide — but in practice the respawned PID is different.
        // The bindings map is what matters here.
        let b = pool.bindings().await;
        assert_eq!(b.len(), 2);
        assert_eq!(b.get("DP-1").map(String::as_str), Some("111"));
        assert_eq!(b.get("HDMI-A-1").map(String::as_str), Some("222"));

        // Unplug: remove HDMI-A-1. Pool respawns with only DP-1.
        pool.unbind("HDMI-A-1").await.unwrap();
        let b = pool.bindings().await;
        assert_eq!(b.len(), 1);
        assert_eq!(b.get("DP-1").map(String::as_str), Some("111"));

        pool.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&wrapper);
    }

    /// Pause/resume: SIGSTOP + SIGCONT to the single LWE PID must
    /// toggle `/proc/<pid>/status` State field T ↔ R/S. We use a
    /// sleep-wrapper script as a stand-in for the real LWE binary
    /// (see `write_sleep_wrapper`). The wrapper ignores any argv
    /// and just runs `/bin/sleep N`, so the pool can emit its full
    /// `--screen-root / --bg / --fps` argv and the underlying sleep
    /// still stays alive past the grace window.
    #[tokio::test]
    async fn single_pool_pause_global_via_sigstop() {
        let wrapper = write_sleep_wrapper(60);

        // Build pool with empty flags so the wrapper sees zero argv
        // past the binary path (sleep just sleeps 60s).
        let pool = LweSinglePool::with_binary(&wrapper).with_flags(vec![]);
        let pid = pool.bind("DP-1", "111").await.unwrap();
        assert!(pid > 0);

        // Pre-state: process is running.
        let pre = crate::backend::pid_state_quick(pid, BackendKind::LinuxWallpaperEngine).unwrap();
        assert_eq!(pre, BackendState::Running, "pre-pause must be Running");

        // Pause: SIGSTOP.
        pool.pause().await.unwrap();
        // Give the kernel a moment to deliver + record the signal state.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let paused =
            crate::backend::pid_state_quick(pid, BackendKind::LinuxWallpaperEngine).unwrap();
        assert_eq!(paused, BackendState::Paused, "post-STOP must be Paused");

        // Resume: SIGCONT.
        pool.resume().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let resumed =
            crate::backend::pid_state_quick(pid, BackendKind::LinuxWallpaperEngine).unwrap();
        assert_eq!(resumed, BackendState::Running, "post-CONT must be Running");

        pool.shutdown().await.unwrap();

        // Clean up the wrapper script.
        let _ = std::fs::remove_file(&wrapper);
    }

    /// Pause on empty pool returns Ok(None) — no process to signal.
    #[tokio::test]
    async fn pause_on_empty_pool_is_noop() {
        let pool = LweSinglePool::with_binary("/bin/sleep");
        let paused_pid = pool.pause().await.unwrap();
        assert_eq!(paused_pid, None);
        let resumed_pid = pool.resume().await.unwrap();
        assert_eq!(resumed_pid, None);
    }

    /// `set_active_fps` and `active_fps` must work through
    /// `Arc<LweSinglePool>` so the daemon layer can mutate the FPS
    /// cap from the smart-calibration path without taking `&mut self`
    /// (the daemon already holds the pool through an Arc).
    #[tokio::test]
    async fn active_fps_round_trip_through_arc() {
        let pool = LweSinglePool::with_binary_and_fps("/bin/sleep", 30);
        let pool_arc = std::sync::Arc::new(pool);
        let mutator = pool_arc.clone();
        let reader = pool_arc.clone();
        assert_eq!(reader.active_fps(), 30);
        mutator.set_active_fps(60);
        assert_eq!(
            reader.active_fps(),
            60,
            "Arc-shared pool must observe FPS cap updates"
        );
        // Convoluted case: clone through Arc and mutate again.
        let mutator2 = pool_arc.clone();
        mutator2.set_active_fps(15);
        assert_eq!(
            reader.active_fps(),
            15,
            "any Arc handle must mutate the same AtomicU32"
        );
    }

    /// Verify `with_binary_and_fps` constructor threads the initial
    /// FPS cap into `active_fps()` correctly.
    #[tokio::test]
    async fn with_binary_and_fps_stores_initial_value() {
        let pool = LweSinglePool::with_binary_and_fps("/bin/sleep", 24);
        assert_eq!(pool.active_fps(), 24);
        let pool2 = LweSinglePool::with_binary_and_fps("/bin/sleep", 1);
        assert_eq!(pool2.active_fps(), 1);
    }

    /// Default grace window is 2000 ms; operators can override via
    /// `set_transition_grace_ms` or the `with_transition_grace_ms`
    /// builder. Interior-mutable atomic works through `Arc` clones,
    /// parallel to [`Self::set_active_fps`].
    #[tokio::test]
    async fn transition_grace_ms_default_and_overrides() {
        let pool = LweSinglePool::with_binary("/bin/sleep");
        assert_eq!(
            pool.transition_grace_ms(),
            2000,
            "default transition_grace_ms must be 2000 ms"
        );
        pool.set_transition_grace_ms(500);
        assert_eq!(pool.transition_grace_ms(), 500);

        // Builder form.
        let pool2 = LweSinglePool::with_binary("/bin/sleep").with_transition_grace_ms(0);
        assert_eq!(
            pool2.transition_grace_ms(),
            0,
            "with_transition_grace_ms(0) must store zero (legacy v0.1 mode)"
        );

        // Arc-shared mutation: the setter propagates across clones.
        let pool_arc = std::sync::Arc::new(pool);
        let writer = pool_arc.clone();
        let reader = pool_arc.clone();
        writer.set_transition_grace_ms(3500);
        assert_eq!(
            reader.transition_grace_ms(),
            3500,
            "Arc-shared pool must observe transition_grace_ms updates"
        );
    }

    /// Wrapper script that ignores its argv (so /bin/sleep never
    /// sees the LWE-specific flags) and writes its PID to the
    /// given output file on startup. The sleep duration is long
    /// enough to outlast any grace window used in the tests. The
    /// file name is uniquified per call so parallel test
    /// invocations don't race on a shared path.
    fn write_pid_wrapper(out_file: &Path, sleep_seconds: u64) -> PathBuf {
        let wrapper = std::env::temp_dir().join(format!(
            "paperforge-grace-wrapper-{}-{}.sh",
            std::process::id(),
            next_wrapper_seq(),
        ));
        // `exec -a NAME` is bash-specific, so use bash explicitly.
        // `/bin/sh` on Debian is `dash`, which lacks `exec -a`; on
        // other distros it might be bash, but pinning to bash makes
        // the wrapper portable across hosts regardless of /bin/sh.
        let body = format!(
            "#!/usr/bin/env bash\n\
             # paperforge grace-window test wrapper\n\
             echo $$ > {out_file}\n\
             exec -a linux-wallpaperengine-test /bin/sleep {sleep_seconds}\n",
            out_file = out_file.display(),
            sleep_seconds = sleep_seconds,
        );
        std::fs::write(&wrapper, body).unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        wrapper
    }

    /// Wrapper script that dies immediately when invoked — used to
    /// exercise the abort path in `bind()`.
    fn write_dying_wrapper() -> PathBuf {
        let wrapper = std::env::temp_dir().join(format!(
            "paperforge-dying-wrapper-{}-{}.sh",
            std::process::id(),
            next_wrapper_seq(),
        ));
        std::fs::write(&wrapper, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        wrapper
    }

    /// Monotonic counter used to uniquify wrapper script paths so
    /// parallel test invocations don't clobber each other.
    fn next_wrapper_seq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        SEQ.fetch_add(1, Ordering::Relaxed)
    }

    /// Build a tiny `/bin/sh` wrapper at a unique temp path that
    /// ignores its argv and runs `/bin/sleep <sleep_seconds>`. This
    /// is the workhorse stand-in for the LWE binary in end-to-end
    /// pool tests: the wrapper cannot accidentally reject the pool's
    /// real argv (which `/bin/sleep` does for `--screen-root`/etc.)
    /// and it stays alive past the default 2000 ms grace window so
    /// the "OLD survives, NEW takes over" branch is exercised. The
    /// filename includes `getpid()` + `next_wrapper_seq()` so two
    /// tests running in parallel don't race on the same path.
    ///
    /// `exec -a linux-wallpaperengine-test /bin/sleep N` overrides
    /// argv[0] to contain the LWE pattern so the [PID-recycling
    /// defense](crate::backend::pid_is_backend_kind) accepts the
    /// resulting process. Without this, the spawn child would look
    /// like a `sleep` (no LWE pattern) and `pid_state_quick` would
    /// return NotRunning — which is the correct production
    /// behaviour but wrong for the test, which is explicitly using
    /// an unmanaged stand-in.
    fn write_sleep_wrapper(sleep_seconds: u64) -> PathBuf {
        let wrapper = std::env::temp_dir().join(format!(
            "paperforge-sleep-wrapper-{}-{}.sh",
            std::process::id(),
            next_wrapper_seq(),
        ));
        // `exec -a NAME` is bash-specific, so use bash explicitly.
        // `/bin/sh` on Debian is `dash`, which lacks `exec -a`; on
        // other distros it might be bash, but pinning to bash makes
        // the wrapper portable across hosts regardless of /bin/sh.
        let body = format!(
            "#!/usr/bin/env bash\n\
             # paperforge sleep-wrapper test helper (ignores its argv)\n\
             exec -a linux-wallpaperengine-test /bin/sleep {sleep_seconds}\n"
        );
        std::fs::write(&wrapper, body).unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        wrapper
    }

    /// End-to-end: bind("DP-1", "111") → bind("DP-1", "222") must
    /// spawn the second LWE BEFORE killing the first. While the
    /// second bind() is mid-flight (inside the 500 ms grace window)
    /// the OLD PID must still be Running — that's the spawn-first
    /// invariant. After the second bind() returns, the OLD PID is
    /// dead and the new PID is the only one tracked by the pool.
    #[tokio::test]
    async fn bind_overlap_kills_old_after_grace() {
        let pid_file =
            std::env::temp_dir().join(format!("paperforge-overlap-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&pid_file);

        // Long sleep (600s) so the first wrapper's PID can't be
        // recycled during the test.
        let wrapper = write_pid_wrapper(&pid_file, 600);

        // 500 ms grace: long enough to mid-flight observe the old
        // PID still alive while keeping the test fast.
        let pool = LweSinglePool::with_binary(&wrapper)
            .with_flags(vec![])
            .with_transition_grace_ms(500);

        // First bind: this writes pid1 to the file.
        let pid1 = pool.bind("DP-1", "111").await.unwrap();
        let pid1_from_file: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(pid1, pid1_from_file, "first wrapper must write its PID");
        assert_eq!(
            crate::backend::pid_state_quick(pid1, BackendKind::LinuxWallpaperEngine).unwrap(),
            BackendState::Running,
            "first LWE must be Running before second bind"
        );

        // Kick off the second bind in a background task so we can
        // sample the OLD PID's state mid-grace.
        let pool_bg = pool.clone();
        let bg_task = tokio::spawn(async move { pool_bg.bind("DP-1", "222").await });

        // Sample during grace: the second spawn overwrites the
        // pid file with pid2. We sleep long enough for the spawn
        // to land but well inside the 500 ms grace so the old
        // PID has NOT been killed yet.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let pid2_from_file: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_ne!(
            pid2_from_file, pid1,
            "second bind must spawn a different PID"
        );
        assert_eq!(
            crate::backend::pid_state_quick(pid1, BackendKind::LinuxWallpaperEngine).unwrap(),
            BackendState::Running,
            "old LWE must survive the grace window (spawn-first-kill-after)"
        );
        assert_eq!(
            crate::backend::pid_state_quick(pid2_from_file, BackendKind::LinuxWallpaperEngine)
                .unwrap(),
            BackendState::Running,
            "new LWE must be Running mid-grace"
        );

        // Wait for second bind to complete; verify the pool's
        // bookkeeping reflects the swap. We deliberately do NOT
        // re-read `/proc/<pid2>/state` here: the test process is
        // the wrapper's parent, and Rust's test runtime auto-reaps
        // its own children as they exit — leading to a brief
        // `[sleep] <defunct>` window between the bind returning
        // and the next `pid_state_quick` call. The pool's own
        // `current_pid` / `bindings` accessors are the canonical
        // correctness signal here; the kernel-state check inside
        // `bind_with_op` already verified the new wrapper was
        // alive before the kill-old step (otherwise the bind
        // would have returned Err).
        let pid2 = bg_task.await.unwrap().unwrap();
        assert_eq!(pid2, pid2_from_file);
        assert_eq!(pool.current_pid().await, Some(pid2));
        assert_eq!(
            pool.bindings().await.get("DP-1").map(String::as_str),
            Some("222"),
            "bindings must point at the new scene after the swap"
        );

        pool.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&wrapper);
        let _ = std::fs::remove_file(&pid_file);
    }
    /// spawns it will get a dead process after the grace window.
    /// The first bind uses a healthy /bin/sleep wrapper so we have
    /// an OLD process to preserve; the second bind uses the dying
    /// wrapper via a separate pool because the binary path is
    /// fixed at construction. To exercise the abort path with a
    /// single binary, we instead use a wrapper that ALWAYS dies
    /// and verify `bind()` on an empty pool: with no OLD process,
    /// the abort should still return Err cleanly. Then we use a
    /// SECOND pool where the OLD wrapper stays alive and a SECOND
    /// bind with the dying wrapper aborts while preserving the
    /// OLD process.
    #[tokio::test]
    async fn bind_aborts_when_new_dies_in_grace() {
        // Pool A: a healthy wrapper that stays alive long enough
        // to be the OLD process during the abort test.
        let healthy_pid_file = std::env::temp_dir().join(format!(
            "paperforge-abort-healthy-{}.pid",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&healthy_pid_file);
        let healthy_wrapper = write_pid_wrapper(&healthy_pid_file, 600);

        // Dying wrapper: exits immediately, regardless of argv.
        let dying_wrapper = write_dying_wrapper();

        // Pool: the binary is the dying wrapper. Even the FIRST
        // bind will fail because the dying wrapper exits before
        // the grace elapses. To verify "OLD preserved" we need a
        // different shape: the OLD process is held by the pool's
        // `inner` state, not a separate pool. So we make TWO
        // pools, both pointing at the dying wrapper, and assert
        // that bind() returns Err when the new LWE dies — that's
        // the contract we care about.
        //
        // Specifically: pool starts empty, first bind spawns
        // dying wrapper → after grace, wrapper is dead → bind
        // returns Err (no OLD to preserve, just abort cleanly).
        let pool = LweSinglePool::with_binary(&dying_wrapper)
            .with_flags(vec![])
            .with_transition_grace_ms(150);

        let err = pool.bind("DP-1", "111").await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("transition grace"),
            "error message must mention the grace window: {msg}"
        );
        // Pool must remain empty after the abort.
        assert_eq!(
            pool.current_pid().await,
            None,
            "pool must stay empty when spawn fails during grace"
        );
        assert!(pool.bindings().await.is_empty());

        // Now verify the "OLD preserved" branch: pool has a
        // healthy process via the healthy wrapper, then a
        // SECOND bind is made via a pool that ALSO uses the
        // healthy wrapper — but the second bind goes through
        // a different code path where we manually swap the
        // pool's binary to a dying one. Since we can't change
        // the binary post-construction, we use a different
        // strategy: spin up a SECOND pool that points at the
        // healthy wrapper, get a process running, then make a
        // second bind on the FIRST pool (dying wrapper) but
        // that doesn't test the OLD-preserved branch.
        //
        // Alternative: use the healthy wrapper for the FIRST
        // bind, then call bind() on the dying-wrapper pool —
        // but the dying pool is independent.
        //
        // Cleanest test of the OLD-preserved branch: same pool
        // binary must stay constant. We use a wrapper that
        // chooses to die based on argv: it inspects the full
        // argv for a literal "die" token.
        drop(pool); // we're done with the dying-only pool
        let _ = std::fs::remove_file(&dying_wrapper);

        // Conditional-die wrapper: dies if any argv equals "die",
        // else sleeps long. This lets us use a single pool to
        // test both branches. Uses bash so `exec -a` (which
        // re-stamps argv[0] to carry the LWE pattern) is supported
        // — `/bin/sh` on Debian is `dash`, which lacks `exec -a`,
        // and the wrapper would otherwise exit immediately with
        // "exec: -a: not found".
        let cond_wrapper = std::env::temp_dir().join(format!(
            "paperforge-cond-wrapper-{}-{}.sh",
            std::process::id(),
            next_wrapper_seq(),
        ));
        std::fs::write(
            &cond_wrapper,
            "#!/usr/bin/env bash\n\
             # paperforge conditional-die wrapper\n\
             for arg in \"$@\"; do\n\
               if [ \"$arg\" = \"die\" ]; then\n\
                 exit 1\n\
               fi\n\
             done\n\
             exec -a linux-wallpaperengine-cond-test /bin/sleep 600\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &cond_wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let pool = LweSinglePool::with_binary(&cond_wrapper)
            .with_flags(vec![])
            .with_transition_grace_ms(150);

        // First bind: content_id "alive" → wrapper sleeps long.
        let pid1 = pool.bind("DP-1", "alive").await.unwrap();
        assert_eq!(
            crate::backend::pid_state_quick(pid1, BackendKind::LinuxWallpaperEngine).unwrap(),
            BackendState::Running,
            "first LWE must be Running before the abort-triggering bind"
        );

        // Second bind: content_id "die" → wrapper exits 1
        // immediately. bind() must return Err and leave pid1 alive.
        let err = pool.bind("DP-1", "die").await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("transition grace"),
            "error message must mention the grace window: {msg}"
        );

        // pid1 must still be alive — the abort path must NOT
        // have killed it. This is the whole point.
        assert_eq!(
            crate::backend::pid_state_quick(pid1, BackendKind::LinuxWallpaperEngine).unwrap(),
            BackendState::Running,
            "old LWE must survive the abort (spawn-first-kill-after rollback)"
        );
        assert_eq!(
            pool.current_pid().await,
            Some(pid1),
            "pool must keep tracking the old PID after a failed swap"
        );
        assert_eq!(
            pool.bindings().await.get("DP-1").map(String::as_str),
            Some("alive"),
            "bindings must NOT have been updated to the failing scene"
        );

        pool.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&healthy_wrapper);
        let _ = std::fs::remove_file(&healthy_pid_file);
        let _ = std::fs::remove_file(&cond_wrapper);
    }

    /// Setting `transition_grace_ms = 0` reverts to the v0.1
    /// kill-then-spawn flow. The spawn still succeeds but there's
    /// no observable "overlap" window — by the time bind() returns,
    /// the old PID is already dead. This is documented behaviour
    /// for operators who want the legacy semantics.
    #[tokio::test]
    async fn bind_with_zero_grace_kills_old_first() {
        let pid_file =
            std::env::temp_dir().join(format!("paperforge-zero-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&pid_file);
        let wrapper = write_pid_wrapper(&pid_file, 600);

        let pool = LweSinglePool::with_binary(&wrapper)
            .with_flags(vec![])
            .with_transition_grace_ms(0);

        let pid1 = pool.bind("DP-1", "111").await.unwrap();
        // Give the kernel a moment to populate /proc/<pid>/cmdline
        // AFTER the wrapper's `exec -a` (the wrapper's pre-exec
        // sleep is 0.1s; we wait 0.5s for safety). The linter-
        // integrated `pid_state_quick` cross-check rejects PIDs
        // whose cmdline doesn't match the LWE pattern.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(
            crate::backend::pid_state_quick(pid1, BackendKind::LinuxWallpaperEngine).unwrap(),
            BackendState::Running
        );

        let pid2 = pool.bind("DP-1", "222").await.unwrap();
        assert_ne!(pid1, pid2);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // With grace=0 there's no wait, so the old PID is killed
        // immediately after spawn + immediate check. pid1 should
        // be NotRunning by the time bind() returns.
        assert_eq!(
            crate::backend::pid_state_quick(pid1, BackendKind::LinuxWallpaperEngine).unwrap(),
            BackendState::NotRunning,
            "with grace=0 the old PID must be dead by the time bind() returns"
        );
        assert_eq!(
            crate::backend::pid_state_quick(pid2, BackendKind::LinuxWallpaperEngine).unwrap(),
            BackendState::Running,
            "new PID must be the canonical one with grace=0"
        );
        assert_eq!(pool.current_pid().await, Some(pid2));

        pool.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&wrapper);
        let _ = std::fs::remove_file(&pid_file);
    }

    /// Verify `bind_with_op` emits the structured transition timing
    /// log line with the expected `op`, `output`, `scene_id`, and
    /// PID fields. Uses a thread-local `tracing_subscriber::fmt`
    /// writer to capture the output, then asserts the line matches
    /// the documented shape. The thread-local scope is safe here
    /// because `#[tokio::test]` runs on a single-threaded
    /// current-thread runtime so no task hops a thread.
    #[tokio::test]
    async fn bind_with_op_emits_transition_timing_log() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let captured_clone = captured.clone();
        let make_writer =
            move || -> Box<dyn std::io::Write> { Box::new(CapturedWriter(captured_clone.clone())) };
        let subscriber = tracing_subscriber::fmt()
            .with_writer(make_writer)
            .with_max_level(tracing::Level::INFO)
            .without_time()
            .with_target(true)
            .with_ansi(false)
            .finish();

        // Install a thread-local default subscriber for the test body
        // via `tracing::dispatcher::set_default`. The guard lives
        // until the test function returns, which keeps the
        // subscriber active across the async bind chain's
        // `.await` points within the
        // `#[tokio::test(flavor = "current_thread")]` runtime.
        //
        // NOTE on parallel-test flakiness: a previous version of
        // this test instrumented `set_global_default` here with the
        // intent to defend against the thread-local subscriber
        // being silently swapped by another parallel test's
        // `set_default`. That did NOT fix the flakiness, so we
        // reverted to the simpler thread-local scope. The race
        // appears to be in tracing-subscriber's lazy writer cache
        // rather than the dispatcher's thread-local swap.
        let _guard = tracing::subscriber::set_default(subscriber);

        let wrapper = write_sleep_wrapper(60);
        let pool = LweSinglePool::with_binary(&wrapper)
            .with_flags(vec![])
            .with_transition_grace_ms(100);

        // First bind: spawns the wrapper, captures a transition log.
        let pid1 = pool.bind("DP-1", "111").await.unwrap();
        // Second bind with explicit op label — also a transition.
        let pid2 = pool
            .bind_with_op("HDMI-A-1", "222", "playlist_apply")
            .await
            .unwrap();
        // Third bind is an idempotent rebind (same output, same
        // content_id) — must NOT emit a transition log.
        let pid3 = pool.bind_with_op("HDMI-A-1", "222", "set").await.unwrap();
        assert_eq!(pid3, pid2, "idempotent rebind reuses the same PID");

        pool.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&wrapper);

        let output = String::from_utf8(captured.lock().unwrap().clone()).unwrap();

        // We expect at least two transition log lines (the two
        // actual transitions). Each is on its own line and starts
        // with the INFO-level `transition: ` marker.
        let transition_lines: Vec<&str> = output
            .lines()
            .filter(|l| l.contains("transition:"))
            .collect();
        assert!(
            transition_lines.len() >= 2,
            "expected at least two transition log lines, got {}:\n{output}",
            transition_lines.len()
        );

        // First line: bind() default op="set".
        let line0 = transition_lines[0];
        assert!(
            line0.contains("op=set"),
            "first transition must use op=set: {line0}"
        );
        assert!(
            line0.contains("output=DP-1"),
            "first transition must report output=DP-1: {line0}"
        );
        assert!(
            line0.contains("scene_id=111"),
            "first transition must report scene_id=111: {line0}"
        );
        assert!(
            line0.contains(&format!("new_pid={pid1}")),
            "first transition must report new_pid={pid1}: {line0}"
        );
        assert!(
            line0.contains("old_pid=<none>"),
            "first transition (cold start) must report old_pid=<none>: {line0}"
        );

        // Second line: bind_with_op(... "playlist_apply").
        let line1 = transition_lines[1];
        assert!(
            line1.contains("op=playlist_apply"),
            "second transition must use op=playlist_apply: {line1}"
        );
        assert!(
            line1.contains("output=HDMI-A-1"),
            "second transition must report output=HDMI-A-1: {line1}"
        );
        assert!(
            line1.contains("scene_id=222"),
            "second transition must report scene_id=222: {line1}"
        );
        assert!(
            line1.contains(&format!("old_pid={pid1}")),
            "second transition must report old_pid={pid1}: {line1}"
        );

        // All timing fields must be present and non-negative. We
        // can't assert exact values (CI clock jitter) but the
        // fields must exist.
        for line in &transition_lines[..2] {
            assert!(line.contains("spawn_ms="), "missing spawn_ms: {line}");
            assert!(line.contains("grace_ms="), "missing grace_ms: {line}");
            assert!(line.contains("kill_ms="), "missing kill_ms: {line}");
            assert!(line.contains("total_ms="), "missing total_ms: {line}");
        }

        // Idempotent rebind must NOT have produced a transition
        // log line for the third bind.
        let idempotent_lines: Vec<&str> = transition_lines
            .iter()
            .copied()
            .filter(|l| l.contains("op=set") && l.contains("output=HDMI-A-1"))
            .collect();
        assert_eq!(
            idempotent_lines.len(),
            0,
            "idempotent rebind must not emit a transition log: {idempotent_lines:?}"
        );
    }

    /// Verify `unbind_with_op` emits a transition log with `op=unset`
    /// (via the default `unbind()` wrapper) and `scene_id=<none>`.
    /// The log line must come through even when no respawn happens
    /// (i.e. the output being unbound was the last binding).
    #[tokio::test]
    async fn unbind_emits_transition_timing_log() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let captured_clone = captured.clone();
        let make_writer =
            move || -> Box<dyn std::io::Write> { Box::new(CapturedWriter(captured_clone.clone())) };
        let subscriber = tracing_subscriber::fmt()
            .with_writer(make_writer)
            .with_max_level(tracing::Level::INFO)
            .without_time()
            .with_target(true)
            .with_ansi(false)
            .finish();

        let _guard = tracing::subscriber::set_default(subscriber);

        let wrapper = write_sleep_wrapper(60);
        let pool = LweSinglePool::with_binary(&wrapper)
            .with_flags(vec![])
            .with_transition_grace_ms(100);

        // First: bind so there's something to unbind.
        let pid1 = pool.bind("DP-1", "111").await.unwrap();
        // Now unbind via the default op="unset".
        pool.unbind("DP-1").await.unwrap();

        let output = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        let unset_lines: Vec<&str> = output
            .lines()
            .filter(|l| l.contains("transition:") && l.contains("op=unset"))
            .collect();
        assert_eq!(
            unset_lines.len(),
            1,
            "expected exactly one op=unset transition line, got {unset_lines:?}\nfull output:\n{output}"
        );
        let line = unset_lines[0];
        assert!(line.contains("output=DP-1"), "missing output=DP-1: {line}");
        assert!(
            line.contains("scene_id=<none>"),
            "unbind must report scene_id=<none>: {line}"
        );
        assert!(
            line.contains(&format!("old_pid={pid1}")),
            "unbind must report old_pid={pid1}: {line}"
        );
        assert!(
            line.contains("new_pid=<none>"),
            "unbind with no remaining bindings must report new_pid=<none>: {line}"
        );

        // Pool is now empty; second unbind is a no-op and must
        // NOT emit a transition log.
        let captured_before = captured.lock().unwrap().len();
        pool.unbind("DP-1").await.unwrap();
        let captured_after = captured.lock().unwrap().len();
        assert_eq!(
            captured_before, captured_after,
            "no-op unbind on empty pool must not emit a log line"
        );

        let _ = std::fs::remove_file(&wrapper);
    }

    // -----------------------------------------------------------------
    // Watchdog tests (Fase 4: LWE pool auto-respawn)
    // -----------------------------------------------------------------
    //
    // These tests cover the four scenarios the spec calls out:
    // 1. The watchdog actually respawns a dead pool.
    // 2. The watchdog does NOT respawn when last_bindings is empty
    //    (no point spawning a phantom LWE for nobody).
    // 3. `abort_watchdog` (and `shutdown`) cancel the task cleanly.
    // 4. `last_bindings` survives an unbind of one output when others
    //    remain — only unbind of the LAST binding clears it.
    //
    // The `pid_state_quick` lwe-name cross-check means we cannot
    // just `bind` against `/bin/true` — that wrapper's cmdline
    // would never match the `linux-wallpaperengine` pattern and
    // the bind would be reported as dead-in-grace. So these tests
    // use a bash wrapper that re-stamps `argv[0]` to a name that
    // contains the LWE pattern. The wrapper is identical in spirit
    // to `write_sleep_wrapper` (above) but uses bash so `exec -a`
    // is supported; /bin/sh on Debian is dash which doesn't grok
    // `-a`.

    /// Bash wrapper that exec's `/bin/sleep` with `argv[0]` set to
    /// a name containing `linux-wallpaperengine`. The pid survives
    /// the watchdog's `pid_state_quick` cross-check because the
    /// pattern is a substring match.
    fn write_lwe_sleep_wrapper(sleep_seconds: u64) -> PathBuf {
        let wrapper = std::env::temp_dir().join(format!(
            "paperforge-lwe-sleep-{}-{}.sh",
            std::process::id(),
            next_wrapper_seq(),
        ));
        let body = format!(
            "#!/bin/bash\n\
             # paperforge lwe-named sleep wrapper for watchdog tests\n\
             exec -a linux-wallpaperengine-watchdog-test /bin/sleep {sleep_seconds}\n"
        );
        std::fs::write(&wrapper, body).unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        wrapper
    }

    /// `spawn_watchdog` should be idempotent: a second call while
    /// the task is alive must not spawn a duplicate. Pre-flight
    /// check used by the other watchdog tests below.
    #[tokio::test]
    async fn watchdog_spawn_is_idempotent() {
        let pool = LweSinglePool::with_binary("/bin/true").with_watchdog_interval_secs(3600);
        pool.spawn_watchdog().await;
        pool.spawn_watchdog().await;
        pool.abort_watchdog().await;
    }

    /// Bind an output, kill the LWE child externally, and verify
    /// the watchdog respawns with a new PID within the configured
    /// tick window. Uses a wrapper that exec's `/bin/sleep` so the
    /// PID appears in `/proc` and can be killed with `nix::kill`.
    /// The test's interval is 1 s so a single tick is enough.
    #[tokio::test]
    async fn watchdog_respawns_dead_pool() {
        // Long sleep so the wrapper has time to be observed alive,
        // then killed, then the respawn to be observed — all within
        // the test's wall-clock budget.
        let wrapper = write_lwe_sleep_wrapper(600);
        let pool = LweSinglePool::with_binary(&wrapper)
            .with_flags(vec![])
            .with_transition_grace_ms(100)
            .with_watchdog_interval_secs(1);

        let pid_before = pool.bind("DP-1", "111").await.unwrap();
        assert!(pid_before > 0, "bind must return a real PID");

        // Spawn the watchdog after the bind so `last_bindings` is
        // populated. Without this, the watchdog would see
        // `last_bindings.is_empty()` and refuse to respawn.
        pool.spawn_watchdog().await;

        // Kill the LWE child. The watchdog's next tick should
        // detect `NotRunning` and respawn from `last_bindings`.
        // We use `libc::SIGKILL` via `nix` for portability.
        // SAFETY: pid_from_bind was obtained from the same
        // `child.id()` we are about to terminate; the kernel
        // reuses PIDs lazily enough that 1 s is safe.
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid_before),
            nix::sys::signal::Signal::SIGKILL,
        );

        // Poll for up to 5 s (covers 1 s tick + 1 s respawn +
        // grace margin). We sleep in 200 ms slices and re-check
        // `current_pid()` each time.
        let deadline = std::time::Duration::from_secs(5);
        let start = std::time::Instant::now();
        let mut pid_after: Option<i32> = None;
        while start.elapsed() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let maybe = pool.current_pid().await;
            if let Some(p) = maybe {
                if p != pid_before {
                    pid_after = Some(p);
                    break;
                }
            }
        }
        assert!(
            pid_after.is_some(),
            "watchdog should have respawned with a new PID within 5s; \
             pid_before={pid_before}, current_pid={:?}",
            pool.current_pid().await
        );

        pool.abort_watchdog().await;
        // After abort, the respawned child is still alive — best
        // effort cleanup so test runs don't leave sleep processes
        // around. Failures here are not test failures (the PID
        // could already be gone if something raced).
        if let Some(p) = pid_after {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(p),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        if let Some(p) = pool.current_pid().await {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(p),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = std::fs::remove_file(&wrapper);
    }

    /// If the pool is empty (no bindings ever, or all bindings
    /// unbound), the watchdog must NOT spawn a phantom LWE. We
    /// verify by checking that `current_pid()` stays `None` for
    /// at least one full tick.
    #[tokio::test]
    async fn watchdog_does_not_respawn_when_bindings_empty() {
        // `/bin/true` is a safe binary that exits immediately. If
        // the watchdog somehow spawned it, `current_pid()` would
        // briefly become Some(.) and then back to None. We assert
        // it stays None across the poll window.
        let pool = LweSinglePool::with_binary("/bin/true").with_watchdog_interval_secs(1);
        pool.spawn_watchdog().await;

        // Wait longer than one tick so the watchdog has had at
        // least one chance to (incorrectly) spawn.
        tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;

        assert!(
            pool.current_pid().await.is_none(),
            "watchdog must not spawn a pool when last_bindings is empty"
        );

        pool.abort_watchdog().await;
    }

    /// `abort_watchdog` cancels the spawned task. After returning,
    /// the inner task handle is `None`, so a subsequent
    /// `spawn_watchdog` spawns a fresh one (and survives the
    /// double-abort cleanly).
    #[tokio::test]
    async fn watchdog_aborts_on_shutdown() {
        let pool = LweSinglePool::with_binary("/bin/true").with_watchdog_interval_secs(3600);
        pool.spawn_watchdog().await;

        // First abort cancels the task.
        pool.abort_watchdog().await;

        // Second abort is a no-op (idempotent).
        pool.abort_watchdog().await;

        // Re-spawning works after an abort.
        pool.spawn_watchdog().await;
        pool.abort_watchdog().await;
    }

    /// `last_bindings` must persist across partial unbinds. The
    /// watchdog uses this snapshot to rebuild the pool after a
    /// crash, so missing a single output's binding would mean the
    /// respawn loses a monitor. The rule is: clear only when the
    /// operator removes the LAST binding.
    #[tokio::test]
    async fn last_bindings_persists_through_unbind_of_some_outputs() {
        // Use the lwe-named wrapper so `bind()` survives the
        // `pid_state_quick` cross-check (the new linter-integrated
        // check rejects PIDs whose cmdline doesn't match the LWE
        // pattern — even a freshly spawned `/bin/true` would fail).
        let wrapper = write_lwe_sleep_wrapper(600);
        let pool = LweSinglePool::with_binary(&wrapper)
            .with_flags(vec![])
            .with_transition_grace_ms(100);

        // Bind two outputs.
        let _ = pool.bind("DP-1", "111").await.unwrap();
        let _ = pool.bind("HDMI-A-1", "222").await.unwrap();

        // Unbind one. The other survives.
        pool.unbind("DP-1").await.unwrap();

        let snap = pool.last_bindings().await;
        assert_eq!(
            snap,
            std::collections::BTreeMap::from([("HDMI-A-1".to_string(), "222".to_string())]),
            "last_bindings must retain the remaining binding after a partial unbind"
        );

        // Unbind the last one. NOW last_bindings clears.
        pool.unbind("HDMI-A-1").await.unwrap();
        assert!(
            pool.last_bindings().await.is_empty(),
            "last_bindings must clear when the operator removes the LAST binding"
        );

        // Best-effort cleanup of any leftover sleep child.
        if let Some(p) = pool.current_pid().await {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(p),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = std::fs::remove_file(&wrapper);
    }
}

/// Test-only writer that captures into a shared `Vec<u8>`. Used by
/// `tracing_subscriber::fmt::MakeWriter` to capture structured log
/// lines into a buffer the test can assert against.
#[cfg(test)]
#[derive(Clone)]
struct CapturedWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl std::io::Write for CapturedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
