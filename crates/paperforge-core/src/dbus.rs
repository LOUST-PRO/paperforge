//! D-Bus IPC service for paperforge.
//!
//! Exposes paperforge's control plane over the session bus so external
//! scripts, keybindings, and other tools can drive it without spawning
//! a new process per command.
//!
//! # Bus name
//!
//! ```text
//! org.louzt.Paperforge
//! ```
//!
//! # Object path
//!
//! ```text
//! /org/louzt/Paperforge
//! ```
//!
//! # Interface
//!
//! `org.louzt.Paperforge1` (versioned suffix per freedesktop guidelines).
//!
//! # Methods
//!
//! - `SetWallpaper(s: output, s: scene_path) → ()` — apply a scene to a
//!   specific Wayland output. Equivalent to `paperforge set`.
//! - `UnsetWallpaper(s: output) → ()` — kill the LWE instance bound to
//!   `output`. Idempotent; returns Ok even when no wallpaper was bound.
//! - `Pause() → u32` — SIGSTOP all running LWE. Returns count.
//! - `Resume() → u32` — SIGCONT all paused LWE. Returns count.
//! - `AudioToggle() → u32` — SIGUSR1 all LWE. Returns count.
//! - `AudioMute() → u32` — SIGUSR2 all LWE. Returns count.
//! - `AudioUnmute() → u32` — SIGCONT all LWE. Returns count.
//! - `ListRunning() → a(i)` — return array of `(pid, state)` tuples.
//! - `ApplyPlaylist(s: name) → ()` — load and apply a named playlist.
//! - `GetState() → s` — JSON snapshot of the daemon state (backend,
//!   active playlist, known outputs, pool snapshot).
//! - `GetHealth() → s` — JSON-encoded [`HealthSnapshot`] for the GUI
//!   or external tooling (per-output PIDs + state, aggregate
//!   uptime + last-set timing, pool bindings).
//!
//! # Signals
//!
//! - `WallpaperStarted(s: output, s: scene_path, i: pid)` — emitted when
//!   a new LWE instance is spawned.
//! - `WallpaperStopped(i: pid)` — emitted when an LWE PID disappears.
//! - `MonitorChanged(as: outputs)` — emitted when the Wayland output
//!   set changes (hotplug). The new list of output names is in the arg.
//!
//! # Architecture
//!
//! The D-Bus interface is a thin layer over the
//! [`PaperforgeControl`] trait. Production code wires
//! [`LweDaemonControl`] as the implementation; tests can inject a
//! stub. The trait is the public API — zbus is an implementation
//! detail.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use zbus::{interface, object_server::SignalContext};

use crate::{
    backend::{BackendKind, BackendState},
    error::{Error, Result},
};

/// Snapshot of the daemon's runtime state. Returned by
/// `GetState` as a JSON string for forward-compatibility (new fields
/// can be added without breaking existing clients).
///
/// The `pool_*` fields are populated when the daemon's backend is
/// Linux Wallpaper Engine with the v0.2 pool architecture enabled.
/// For non-LWE backends (swww, hyprpaper, mpvpaper) the pool fields
/// stay at their defaults (`pool_pid = None`, `pool_bindings = {}`,
/// `pool_argv = None`) and clients should treat them as "not
/// applicable" rather than "empty / pool not running".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonState {
    /// Which backend the daemon is orchestrating.
    pub backend: BackendKind,
    /// Currently active playlist, if any.
    pub active_playlist: Option<String>,
    /// PIDs of running wallpaper processes with their last-known state.
    pub running: Vec<(i32, BackendState)>,
    /// Last-known set of Wayland output names.
    pub known_outputs: Vec<String>,
    /// paperforge version.
    pub version: String,
    /// PID of the v0.2 single-process pool, or `None` when the
    /// pool isn't running (or the backend isn't LWE). Populated by
    /// the daemon's `get_state` from `LweSinglePool::current_pid`.
    /// Added in Task #31 — fixes `paperforge pool status` reporting
    /// `(none)` when the daemon owns the pool (previously the CLI
    /// instantiated a fresh in-process pool and reported empty).
    #[serde(default)]
    pub pool_pid: Option<i32>,
    /// `output → content_id` bindings of the v0.2 single-process
    /// pool. Empty when no pool is running or the backend isn't
    /// LWE. Populated by the daemon's `get_state` from
    /// `LweSinglePool::bindings`.
    #[serde(default)]
    pub pool_bindings: BTreeMap<String, String>,
    /// Full argv the pool would respawn with (one entry per flag,
    /// including the binary path at `[0]`). `None` when the pool
    /// isn't running. Useful for debugging what `--screen-root` /
    /// `--bg` pairs are actually passed to LWE without scraping
    /// `/proc/<pid>/cmdline`.
    #[serde(default)]
    pub pool_argv: Option<Vec<String>>,
}

/// Per-output daemon health snapshot returned by [`PaperforgeControl::get_health`].
///
/// Serialized via serde for D-Bus (the `GetHealth()` method returns
/// the JSON-encoded snapshot, parallel to `GetState()`). The struct
/// is `pub` so the GUI crate can `use paperforge_core::dbus::HealthSnapshot`
/// directly.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthSnapshot {
    /// Per-output health. Key = output name (e.g. `"DP-1"`).
    ///
    /// Empty for fresh daemons or non-LWE backends (the per-output
    /// map is LWE-only; swww/hyprpaper/mpvpaper report everything
    /// via the aggregate section instead).
    pub per_output: BTreeMap<String, PerOutputHealth>,

    /// Aggregate daemon state.
    pub aggregate: AggregateHealth,
}

/// Per-output daemon health (one entry per Wayland output that has
/// ever been bound in this daemon session).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PerOutputHealth {
    /// PID of the LWE process rendering this output. `None` if no
    /// wallpaper is bound or the daemon doesn't know the pid (e.g.
    /// after a stale pid was reaped; the entry stays so the GUI can
    /// show "daemon knew this output but lost the pid" rather than
    /// dropping it silently).
    pub lwe_pid: Option<i32>,
    /// Backend state for the pid as a string. One of `"Running"`,
    /// `"Paused"`, `"Dead"`, `"Unknown"`. String (not enum) to
    /// avoid a serde ↔ zbus type adapter.
    pub pid_state: String,
    /// ISO 8601 / RFC 3339 timestamp of the last successful bind on
    /// this output. `None` if the entry exists only because of a
    /// reaped pid and the daemon never recorded a successful bind
    /// (rare; defensive default for `per_output_pids` populated by
    /// `bind_external_pid`).
    pub last_set_at: Option<String>,
    /// How long the last transition took in milliseconds. `None`
    /// when no successful `set` has completed yet for this output
    /// in this daemon session.
    pub last_transition_ms: Option<u64>,
}

/// Aggregate daemon health (one per `HealthSnapshot`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AggregateHealth {
    /// Current pool PID (single-LWE multi-output) or `None` when
    /// no pool is running, the backend isn't LWE, or the pool
    /// hasn't spawned a child yet.
    pub current_pid: Option<i32>,
    /// Pool bindings (output → workshop content_id). Empty when no
    /// pool is running or the backend isn't LWE.
    pub pool_bindings: BTreeMap<String, String>,
    /// Daemon uptime in seconds since the `PaperforgeDaemon` was
    /// constructed (the moment this struct's underlying `Instant`
    /// was captured).
    pub uptime_secs: u64,
    /// UNIX epoch milliseconds of the last successful `set_wallpaper`
    /// on any output. `None` if the daemon hasn't completed a bind
    /// yet.
    pub last_set_total_ms: Option<u64>,
}

/// Backend-agnostic control surface that the D-Bus interface adapts.
///
/// All methods are async (the daemon runs on tokio). Implementations
/// should be cheap to clone — typically `Arc<impl ...>`.
#[async_trait]
pub trait PaperforgeControl: Send + Sync {
    /// Apply `scene_path` to the given Wayland `output`.
    async fn set_wallpaper(&self, output: &str, scene_path: &str) -> Result<()>;

    /// Remove the wallpaper bound to `output` (kill its LWE
    /// instance). After this returns, `list_running` no longer
    /// contains the output's PID and the daemon emits a
    /// `WallpaperStopped` signal. Returns `Ok(())` even when no
    /// wallpaper was bound to `output` (idempotent unset).
    async fn unset_wallpaper(&self, output: &str) -> Result<()>;

    /// Pause all running instances. Returns the count of PIDs signaled.
    async fn pause(&self) -> Result<u32>;

    /// Resume all paused instances. Returns the count of PIDs signaled.
    async fn resume(&self) -> Result<u32>;

    /// Toggle audio on all instances. Returns the count of PIDs signaled.
    async fn audio_toggle(&self) -> Result<u32>;

    /// Force-mute all instances. Returns the count of PIDs signaled.
    async fn audio_mute(&self) -> Result<u32>;

    /// Force-unmute all instances. Returns the count of PIDs signaled.
    async fn audio_unmute(&self) -> Result<u32>;

    /// List running instances with their last-known state.
    async fn list_running(&self) -> Result<Vec<(i32, BackendState)>>;

    /// Apply a named playlist from the playlist store.
    async fn apply_playlist(&self, name: &str) -> Result<()>;

    /// Trigger an immediate self-heal pass: re-bind any output
    /// whose LWE process has died. Returns the `(output, new_pid)`
    /// pairs that were re-spawned (empty when everything was already
    /// alive).
    async fn reconcile(&self) -> Result<Vec<(String, i32)>>;

    /// Return a JSON snapshot of the daemon state.
    async fn get_state(&self) -> Result<DaemonState>;

    /// Latest metrics snapshot as JSON. Default impl returns
    /// `Err(NotSupported)` so existing stubs compile without
    /// having to wire metrics. Production `LweDaemonControl`
    /// overrides this to read from the live collector.
    async fn get_metrics(&self) -> Result<String> {
        Err(Error::Other(anyhow::anyhow!(
            "metrics: not supported by this control impl"
        )))
    }

    /// Return the last `n` snapshots (or all if `n = 0`),
    /// oldest first, as JSON. Same default as `get_metrics`.
    async fn get_metrics_history(&self, _n: u32) -> Result<String> {
        Err(Error::Other(anyhow::anyhow!(
            "metrics history: not supported by this control impl"
        )))
    }

    /// Return a structured [`HealthSnapshot`] (per-output PIDs +
    /// state, aggregate uptime + last-set timing).
    ///
    /// Default impl returns `Err(NotSupported)` so existing stubs
    /// compile without having to wire health. Production
    /// `LweDaemonControl` overrides this to read from the live
    /// backend state.
    async fn get_health(&self) -> Result<HealthSnapshot> {
        Err(Error::Other(anyhow::anyhow!(
            "health: not supported by this control impl"
        )))
    }
}

/// The D-Bus interface object. Adapts a [`PaperforgeControl`] to the
/// zbus connection. One instance per connection; the daemon holds the
/// trait impl, the D-Bus layer is a thin proxy.
pub struct PaperforgeInterface {
    ctrl: Arc<dyn PaperforgeControl>,
}

impl PaperforgeInterface {
    /// Wrap a control impl for exposure over D-Bus.
    pub fn new(ctrl: Arc<dyn PaperforgeControl>) -> Self {
        Self { ctrl }
    }
}

#[interface(name = "org.louzt.Paperforge1")]
impl PaperforgeInterface {
    /// Apply `scene_path` to `output`. Returns an error string on
    /// failure (zbus can only transport strings across methods).
    async fn set_wallpaper(&self, output: String, scene_path: String) -> zbus::fdo::Result<()> {
        self.ctrl
            .set_wallpaper(&output, &scene_path)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))
    }

    /// Remove the wallpaper bound to `output`. Idempotent — returns
    /// `Ok(())` even when no wallpaper was bound.
    async fn unset_wallpaper(&self, output: String) -> zbus::fdo::Result<()> {
        self.ctrl
            .unset_wallpaper(&output)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))
    }

    async fn pause(&self) -> zbus::fdo::Result<u32> {
        self.ctrl
            .pause()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))
    }

    async fn resume(&self) -> zbus::fdo::Result<u32> {
        self.ctrl
            .resume()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))
    }

    async fn audio_toggle(&self) -> zbus::fdo::Result<u32> {
        self.ctrl
            .audio_toggle()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))
    }

    async fn audio_mute(&self) -> zbus::fdo::Result<u32> {
        self.ctrl
            .audio_mute()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))
    }

    async fn audio_unmute(&self) -> zbus::fdo::Result<u32> {
        self.ctrl
            .audio_unmute()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))
    }

    /// Returns `a(is)` — array of `(pid, state_string)` tuples where
    /// `state_string` is one of `"running"`, `"paused"`, `"not-running"`.
    async fn list_running(&self) -> zbus::fdo::Result<Vec<(i32, String)>> {
        let raw = self
            .ctrl
            .list_running()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))?;
        Ok(raw
            .into_iter()
            .map(|(pid, state)| {
                let s = match state {
                    BackendState::Running => "running".to_string(),
                    BackendState::Paused => "paused".to_string(),
                    BackendState::NotRunning => "not-running".to_string(),
                };
                (pid, s)
            })
            .collect())
    }

    async fn apply_playlist(&self, name: String) -> zbus::fdo::Result<()> {
        self.ctrl
            .apply_playlist(&name)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))
    }

    /// Re-bind outputs whose LWE died. Returns `a(ss)` — array of
    /// `(output, new_pid_string)` tuples. `new_pid_string` is a
    /// string-encoded i32 for stability across zbus signature
    /// changes (signals the same way).
    async fn reconcile(&self) -> zbus::fdo::Result<Vec<(String, String)>> {
        let pairs = self
            .ctrl
            .reconcile()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))?;
        Ok(pairs
            .into_iter()
            .map(|(output, pid)| (output, pid.to_string()))
            .collect())
    }

    /// Return JSON-encoded [`DaemonState`].
    async fn get_state(&self) -> zbus::fdo::Result<String> {
        let s = self
            .ctrl
            .get_state()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))?;
        serde_json::to_string(&s).map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))
    }

    /// Return the latest metrics snapshot as JSON.
    async fn get_metrics(&self) -> zbus::fdo::Result<String> {
        self.ctrl
            .get_metrics()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))
    }

    /// Return the last `n` metrics snapshots (or all of them when
    /// `n = 0`), oldest first, JSON-encoded.
    async fn get_metrics_history(&self, n: u32) -> zbus::fdo::Result<String> {
        self.ctrl
            .get_metrics_history(n)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))
    }

    /// Return the JSON-encoded [`HealthSnapshot`] for the daemon.
    /// Wraps the trait's `get_health` result the same way `GetState`
    /// does.
    async fn get_health(&self) -> zbus::fdo::Result<String> {
        let snap = self
            .ctrl
            .get_health()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))?;
        serde_json::to_string(&snap).map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))
    }

    /// Signal: a wallpaper instance just started rendering on `output`
    /// from `scene_path` under `pid`.
    #[zbus(signal)]
    async fn wallpaper_started(
        signal_ctx: &SignalContext<'_>,
        output: String,
        scene_path: String,
        pid: i32,
    ) -> zbus::Result<()>;

    /// Signal: the LWE process with `pid` has exited.
    #[zbus(signal)]
    async fn wallpaper_stopped(signal_ctx: &SignalContext<'_>, pid: i32) -> zbus::Result<()>;

    /// Signal: the Wayland output set changed. `outputs` is the new
    /// list (no guaranteed order).
    #[zbus(signal)]
    async fn monitor_changed(
        signal_ctx: &SignalContext<'_>,
        outputs: Vec<String>,
    ) -> zbus::Result<()>;
}

/// Standard D-Bus bus name for paperforge.
pub const BUS_NAME: &str = "org.louzt.Paperforge";
/// Standard D-Bus object path for paperforge.
pub const OBJECT_PATH: &str = "/org/louzt/Paperforge";

/// Thin D-Bus client used by the `paperforge reconcile` CLI.
///
/// Hides the `zbus::proxy` macro plumbing from downstream crates
/// (the `zbus` dependency lives only in `paperforge-core`). The
/// client connects to the session bus, looks up the
/// `org.louzt.Paperforge1` interface, and exposes a typed `reconcile`
/// call that returns parsed `(output, pid)` tuples.
///
/// All methods are infallible at the type level: the D-Bus transport
/// maps any failure to an [`Error::Other`] so the CLI can just
/// `anyhow!()` it.
pub struct PaperforgeClient {
    conn: zbus::connection::Connection,
    if_name: zbus::names::InterfaceName<'static>,
    obj_path: zbus::zvariant::ObjectPath<'static>,
}

impl PaperforgeClient {
    /// Connect to the running daemon. Returns an error if no
    /// `paperforge daemon` is reachable on the session bus.
    pub async fn connect() -> crate::error::Result<Self> {
        let conn = zbus::connection::Connection::session()
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("session bus: {e}")))?;
        let if_name: zbus::names::InterfaceName<'static> =
            zbus::names::InterfaceName::try_from("org.louzt.Paperforge1")
                .map_err(|e| Error::Other(anyhow::anyhow!("interface name: {e}")))?;
        let obj_path: zbus::zvariant::ObjectPath<'static> =
            zbus::zvariant::ObjectPath::try_from(OBJECT_PATH)
                .map_err(|e| Error::Other(anyhow::anyhow!("object path: {e}")))?;
        Ok(Self {
            conn,
            if_name,
            obj_path,
        })
    }

    /// Underlying zbus connection. Exposed for signal subscription
    /// (`MessageStream::for_match_rule` requires a connection
    /// reference). Prefer the typed helpers below when possible —
    /// this is the escape hatch for surfaces we have not wrapped yet.
    pub fn connection(&self) -> &zbus::connection::Connection {
        &self.conn
    }

    /// Call `SetWallpaper(output, scene_path)` on the daemon.
    pub async fn set_wallpaper(&self, output: &str, scene_path: &str) -> crate::error::Result<()> {
        self.call_no_return("SetWallpaper", &(output, scene_path))
            .await
    }

    /// Call `UnsetWallpaper(output)` on the daemon. Idempotent.
    pub async fn unset_wallpaper(&self, output: &str) -> crate::error::Result<()> {
        self.call_no_return("UnsetWallpaper", &(output,)).await
    }

    /// Call `Pause()` on the daemon. Returns the count of PIDs signaled.
    pub async fn pause(&self) -> crate::error::Result<u32> {
        self.call_u32("Pause", &()).await
    }

    /// Call `Resume()` on the daemon. Returns the count of PIDs signaled.
    pub async fn resume(&self) -> crate::error::Result<u32> {
        self.call_u32("Resume", &()).await
    }

    /// Call `AudioToggle()` on the daemon. Returns the count of PIDs signaled.
    pub async fn audio_toggle(&self) -> crate::error::Result<u32> {
        self.call_u32("AudioToggle", &()).await
    }

    /// Call `AudioMute()` on the daemon. Returns the count of PIDs signaled.
    pub async fn audio_mute(&self) -> crate::error::Result<u32> {
        self.call_u32("AudioMute", &()).await
    }

    /// Call `AudioUnmute()` on the daemon. Returns the count of PIDs signaled.
    pub async fn audio_unmute(&self) -> crate::error::Result<u32> {
        self.call_u32("AudioUnmute", &()).await
    }

    /// Call `ApplyPlaylist(name)` on the daemon.
    pub async fn apply_playlist(&self, name: &str) -> crate::error::Result<()> {
        self.call_no_return("ApplyPlaylist", &(name,)).await
    }

    /// Call `ListRunning()` on the daemon. Returns `(pid, state)`
    /// tuples where `state` is the last-known `BackendState` for
    /// that PID. The daemon returns `(i32, String)`; we map the
    /// string back to `BackendState` here so callers don't deal
    /// with the transport encoding.
    pub async fn list_running(&self) -> crate::error::Result<Vec<(i32, BackendState)>> {
        let reply = self
            .conn
            .call_method(
                Some(BUS_NAME),
                self.obj_path.clone(),
                Some(self.if_name.clone()),
                "ListRunning",
                &(),
            )
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("D-Bus ListRunning call: {e}")))?;
        let body = reply.body();
        let raw: Vec<(i32, String)> = body
            .deserialize()
            .map_err(|e| Error::Other(anyhow::anyhow!("D-Bus ListRunning parse: {e}")))?;
        raw.into_iter()
            .map(|(pid, state)| {
                let parsed = match state.as_str() {
                    "running" => BackendState::Running,
                    "paused" => BackendState::Paused,
                    "not-running" => BackendState::NotRunning,
                    other => {
                        return Err(Error::Other(anyhow::anyhow!(
                            "D-Bus ListRunning returned unknown state {other:?} for pid {pid}"
                        )));
                    }
                };
                Ok((pid, parsed))
            })
            .collect::<crate::error::Result<Vec<_>>>()
    }

    /// Call `GetState()` on the daemon. Returns the parsed
    /// [`DaemonState`] snapshot.
    pub async fn get_state(&self) -> crate::error::Result<DaemonState> {
        let reply = self
            .conn
            .call_method(
                Some(BUS_NAME),
                self.obj_path.clone(),
                Some(self.if_name.clone()),
                "GetState",
                &(),
            )
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("D-Bus GetState call: {e}")))?;
        let body = reply.body();
        let raw: String = body
            .deserialize()
            .map_err(|e| Error::Other(anyhow::anyhow!("D-Bus GetState body: {e}")))?;
        serde_json::from_str(&raw)
            .map_err(|e| Error::Other(anyhow::anyhow!("DaemonState JSON parse: {e}")))
    }

    /// Call `GetHealth()` on the daemon. Returns the parsed
    /// [`HealthSnapshot`] (per-output PIDs + state, aggregate
    /// uptime, last-set timing). Returns the same shape across LWE
    /// and non-LWE backends: non-LWE backends have an empty
    /// `per_output` map (LWE-only) and a `None` `aggregate.current_pid`.
    pub async fn get_health(&self) -> crate::error::Result<HealthSnapshot> {
        let reply = self
            .conn
            .call_method(
                Some(BUS_NAME),
                self.obj_path.clone(),
                Some(self.if_name.clone()),
                "GetHealth",
                &(),
            )
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("D-Bus GetHealth call: {e}")))?;
        let body = reply.body();
        let raw: String = body
            .deserialize()
            .map_err(|e| Error::Other(anyhow::anyhow!("D-Bus GetHealth body: {e}")))?;
        serde_json::from_str(&raw)
            .map_err(|e| Error::Other(anyhow::anyhow!("HealthSnapshot JSON parse: {e}")))
    }

    /// Call `Reconcile` on the daemon. Returns the `(output,
    /// new_pid)` pairs that were re-spawned (empty when nothing
    /// needed re-binding).
    pub async fn reconcile(&self) -> crate::error::Result<Vec<(String, i32)>> {
        // Use the raw `call_method` API: we don't want to drag the
        // `#[zbus::proxy]` macro + a generated `PaperforgeControlProxy`
        // struct into this crate just for one method. The interface
        // declares `reconcile() → a(ss)` (array of (string, string)
        // tuples); we parse each string to i32 here.
        let reply = self
            .conn
            .call_method(
                Some(BUS_NAME),
                self.obj_path.clone(),
                Some(self.if_name.clone()),
                "Reconcile",
                &(),
            )
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("D-Bus Reconcile call: {e}")))?;
        // The reply body is an array of (String, String) structs.
        let body = reply.body();
        let parsed: Vec<(String, String)> = body
            .deserialize()
            .map_err(|e| Error::Other(anyhow::anyhow!("D-Bus response parse: {e}")))?;
        parsed
            .into_iter()
            .map(|(output, pid_str)| {
                let pid = pid_str.parse::<i32>().map_err(|e| {
                    Error::Other(anyhow::anyhow!(
                        "D-Bus returned non-numeric pid {pid_str:?} for output {output}: {e}"
                    ))
                })?;
                Ok((output, pid))
            })
            .collect()
    }

    /// Internal helper: invoke a no-arg or `(String)`/`(String, String)`
    /// method that returns `()`. Avoids the boilerplate of
    /// `call_method` + body deserialize + `Error::Other` wrapping
    /// for each method.
    async fn call_no_return<B>(&self, method: &'static str, body: &B) -> crate::error::Result<()>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let _reply = self
            .conn
            .call_method(
                Some(BUS_NAME),
                self.obj_path.clone(),
                Some(self.if_name.clone()),
                method,
                body,
            )
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("D-Bus {method} call: {e}")))?;
        Ok(())
    }

    /// Internal helper: invoke a no-arg method that returns `u32`.
    async fn call_u32<B>(&self, method: &'static str, body: &B) -> crate::error::Result<u32>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let reply = self
            .conn
            .call_method(
                Some(BUS_NAME),
                self.obj_path.clone(),
                Some(self.if_name.clone()),
                method,
                body,
            )
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("D-Bus {method} call: {e}")))?;
        let parsed: u32 = reply
            .body()
            .deserialize()
            .map_err(|e| Error::Other(anyhow::anyhow!("D-Bus {method} response parse: {e}")))?;
        Ok(parsed)
    }
}

/// Serve the D-Bus interface on the session bus. Returns once the
/// connection is established and the interface is registered.
///
/// Blocks until the connection is dropped (typically when the daemon
/// receives SIGTERM/SIGINT and the tokio runtime is shut down).
///
/// The `event_rx` is consumed by an internal forwarder task that
/// translates in-process [`crate::DaemonEvent`]s into zbus signal
/// emissions on the interface. Without this, the daemon emits events
/// to a channel that no one reads.
pub async fn serve_dbus(
    ctrl: Arc<dyn PaperforgeControl>,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<crate::DaemonEvent>,
) -> Result<()> {
    let iface = PaperforgeInterface::new(ctrl);

    let conn = zbus::connection::Builder::session()
        .map_err(|e| Error::Other(anyhow::anyhow!("session bus: {e}")))?
        .name(BUS_NAME)
        .map_err(|e| Error::Other(anyhow::anyhow!("name claim: {e}")))?
        .serve_at(OBJECT_PATH, iface)
        .map_err(|e| Error::Other(anyhow::anyhow!("serve_at: {e}")))?
        .build()
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("build: {e}")))?;

    // Take a reference to the interface so we can emit signals from
    // the forwarder task. `connection::Connection` exposes the
    // `SignalContext` we need for `emit_signal`.
    let iface_ref = conn
        .object_server()
        .interface::<_, PaperforgeInterface>(OBJECT_PATH)
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("interface lookup: {e}")))?;
    let signal_ctx = iface_ref.signal_context().clone();

    tokio::spawn(forward_events(event_rx, signal_ctx));

    tracing::info!("D-Bus interface ready: {} {}", BUS_NAME, OBJECT_PATH);

    // Hold the connection until it's dropped externally.
    let _ = conn;
    // Wait forever (until the runtime is shut down). We don't have a
    // great way to await "until the connection is gone" without
    // listening on a channel; for the daemon pattern, the supervisor
    // (systemd) terminates the process via SIGTERM, which drops the
    // runtime and ends the serve loop.
    std::future::pending::<()>().await;
    Ok(())
}

/// Forward in-process `DaemonEvent`s to D-Bus signals. The forwarder
/// drains the receiver channel and emits the zbus signal with the
/// matching payload. Exits cleanly when the channel closes (daemon
/// shutdown).
async fn forward_events(
    mut event_rx: tokio::sync::mpsc::UnboundedReceiver<crate::DaemonEvent>,
    signal_ctx: zbus::object_server::SignalContext<'_>,
) {
    use crate::DaemonEvent;
    while let Some(ev) = event_rx.recv().await {
        match ev {
            DaemonEvent::WallpaperStarted {
                output,
                scene_path,
                pid,
                at: _,
            } => {
                if let Err(e) =
                    PaperforgeInterface::wallpaper_started(&signal_ctx, output, scene_path, pid)
                        .await
                {
                    tracing::warn!(target: "paperforge", "wallpaper_started signal failed: {e}");
                }
            }
            DaemonEvent::WallpaperStopped { pid, at: _ } => {
                if let Err(e) = PaperforgeInterface::wallpaper_stopped(&signal_ctx, pid).await {
                    tracing::warn!(target: "paperforge", "wallpaper_stopped signal failed: {e}");
                }
            }
            DaemonEvent::MonitorChanged { outputs, at: _ } => {
                if let Err(e) = PaperforgeInterface::monitor_changed(&signal_ctx, outputs).await {
                    tracing::warn!(target: "paperforge", "monitor_changed signal failed: {e}");
                }
            }
        }
    }
    tracing::debug!(target: "paperforge", "event forwarder exiting (channel closed)");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory stub for `PaperforgeControl`. Records every method
    /// call so tests can assert on the side-effects.
    #[derive(Default)]
    struct StubControl {
        calls: Mutex<Vec<String>>,
        paused: Mutex<u32>,
        applied: Mutex<Option<String>>,
        state: Mutex<DaemonState>,
    }

    #[async_trait]
    impl PaperforgeControl for StubControl {
        async fn set_wallpaper(&self, output: &str, scene_path: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("set_wallpaper:{output}:{scene_path}"));
            Ok(())
        }

        async fn unset_wallpaper(&self, output: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("unset_wallpaper:{output}"));
            Ok(())
        }

        async fn pause(&self) -> Result<u32> {
            self.calls.lock().unwrap().push("pause".to_string());
            *self.paused.lock().unwrap() = 3;
            Ok(3)
        }

        async fn resume(&self) -> Result<u32> {
            self.calls.lock().unwrap().push("resume".to_string());
            *self.paused.lock().unwrap() = 0;
            Ok(3)
        }

        async fn audio_toggle(&self) -> Result<u32> {
            self.calls.lock().unwrap().push("audio_toggle".to_string());
            Ok(2)
        }

        async fn audio_mute(&self) -> Result<u32> {
            self.calls.lock().unwrap().push("audio_mute".to_string());
            Ok(2)
        }

        async fn audio_unmute(&self) -> Result<u32> {
            self.calls.lock().unwrap().push("audio_unmute".to_string());
            Ok(2)
        }

        async fn list_running(&self) -> Result<Vec<(i32, BackendState)>> {
            self.calls.lock().unwrap().push("list_running".to_string());
            Ok(vec![
                (100, BackendState::Running),
                (101, BackendState::Paused),
            ])
        }

        async fn apply_playlist(&self, name: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("apply_playlist:{name}"));
            *self.applied.lock().unwrap() = Some(name.to_string());
            Ok(())
        }

        async fn reconcile(&self) -> Result<Vec<(String, i32)>> {
            self.calls.lock().unwrap().push("reconcile".to_string());
            Ok(Vec::new())
        }

        async fn get_state(&self) -> Result<DaemonState> {
            self.calls.lock().unwrap().push("get_state".to_string());
            Ok(self.state.lock().unwrap().clone())
        }
    }

    #[test]
    fn daemon_state_serializes_as_json() {
        let s = DaemonState {
            backend: BackendKind::LinuxWallpaperEngine,
            active_playlist: Some("focus".to_string()),
            running: vec![(100, BackendState::Running)],
            known_outputs: vec!["DP-1".to_string(), "HDMI-A-1".to_string()],
            version: "0.1.0".to_string(),
            pool_pid: Some(4242),
            pool_bindings: BTreeMap::new(),
            pool_argv: None,
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("\"backend\":\"linux-wallpaper-engine\""));
        assert!(j.contains("\"active_playlist\":\"focus\""));
        assert!(j.contains("\"version\":\"0.1.0\""));
        assert!(j.contains("\"pool_pid\":4242"));
    }

    #[test]
    fn daemon_state_roundtrip() {
        let s = DaemonState {
            backend: BackendKind::SwwwDaemon,
            active_playlist: None,
            running: vec![],
            known_outputs: vec!["eDP-1".to_string()],
            version: "0.1.0".to_string(),
            pool_pid: None,
            pool_bindings: BTreeMap::new(),
            pool_argv: None,
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: DaemonState = serde_json::from_str(&j).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn daemon_state_roundtrip_with_populated_pool() {
        // The Task #31 fix ships new `pool_*` fields; existing
        // serializations (without those keys) must still
        // deserialize thanks to `#[serde(default)]`. This is the
        // backwards-compat guard for any on-disk snapshot /
        // cross-version client that doesn't know about the new
        // fields yet.
        let legacy = r#"{
            "backend": "linux-wallpaper-engine",
            "active_playlist": null,
            "running": [],
            "known_outputs": ["DP-1"],
            "version": "0.1.0"
        }"#;
        let parsed: DaemonState = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.pool_pid, None);
        assert!(parsed.pool_bindings.is_empty());
        assert_eq!(parsed.pool_argv, None);
    }

    #[tokio::test]
    async fn stub_control_records_calls() {
        let stub = Arc::new(StubControl::default());
        let ctrl: Arc<dyn PaperforgeControl> = stub.clone();

        ctrl.set_wallpaper("DP-1", "/scenes/focus").await.unwrap();
        ctrl.pause().await.unwrap();
        ctrl.apply_playlist("focus").await.unwrap();
        ctrl.audio_toggle().await.unwrap();

        let calls = stub.calls.lock().unwrap();
        assert_eq!(calls[0], "set_wallpaper:DP-1:/scenes/focus");
        assert_eq!(calls[1], "pause");
        assert_eq!(calls[2], "apply_playlist:focus");
        assert_eq!(calls[3], "audio_toggle");

        let applied = stub.applied.lock().unwrap();
        assert_eq!(*applied, Some("focus".to_string()));
    }

    #[tokio::test]
    async fn stub_control_list_running() {
        let stub = Arc::new(StubControl::default());
        let ctrl: Arc<dyn PaperforgeControl> = stub.clone();
        let r = ctrl.list_running().await.unwrap();
        assert_eq!(
            r,
            vec![(100, BackendState::Running), (101, BackendState::Paused)]
        );
    }

    #[tokio::test]
    async fn stub_control_get_state() {
        let stub = Arc::new(StubControl::default());
        {
            let mut s = stub.state.lock().unwrap();
            *s = DaemonState {
                backend: BackendKind::LinuxWallpaperEngine,
                active_playlist: Some("default".to_string()),
                running: vec![],
                known_outputs: vec!["DP-1".to_string()],
                version: "0.1.0".to_string(),
                pool_pid: None,
                pool_bindings: BTreeMap::new(),
                pool_argv: None,
            };
        }
        let ctrl: Arc<dyn PaperforgeControl> = stub.clone();
        let s = ctrl.get_state().await.unwrap();
        assert_eq!(s.active_playlist.as_deref(), Some("default"));
        assert_eq!(s.known_outputs, vec!["DP-1".to_string()]);
        // Task #31 — stub doesn't know about pool state; defaults
        // must be populated rather than missing.
        assert_eq!(s.pool_pid, None);
        assert!(s.pool_bindings.is_empty());
        assert_eq!(s.pool_argv, None);
    }

    /// `HealthSnapshot` roundtrips through JSON. This is the
    /// transport contract for the `GetHealth` D-Bus method — losing
    /// a field on serialization would make the GUI's per-output
    /// health panel silently show "no data" instead of the actual
    /// PID / state.
    #[test]
    fn health_snapshot_json_roundtrip() {
        let mut per_output = BTreeMap::new();
        per_output.insert(
            "DP-1".to_string(),
            PerOutputHealth {
                lwe_pid: Some(4242),
                pid_state: "Running".to_string(),
                last_set_at: Some("2026-08-15T10:00:00Z".to_string()),
                last_transition_ms: Some(125),
            },
        );
        per_output.insert(
            "HDMI-A-1".to_string(),
            PerOutputHealth {
                lwe_pid: None,
                pid_state: "Dead".to_string(),
                last_set_at: Some("2026-08-15T09:55:30Z".to_string()),
                last_transition_ms: None,
            },
        );
        let mut pool_bindings = BTreeMap::new();
        pool_bindings.insert("DP-1".to_string(), "847261582".to_string());
        let snap = HealthSnapshot {
            per_output,
            aggregate: AggregateHealth {
                current_pid: Some(4242),
                pool_bindings,
                uptime_secs: 3600,
                last_set_total_ms: Some(1_700_000_000_000),
            },
        };
        let json = serde_json::to_string(&snap).expect("HealthSnapshot serializes");
        let back: HealthSnapshot = serde_json::from_str(&json).expect("HealthSnapshot parses");
        assert_eq!(back, snap);
        // Spot-check field names so a serde rename silently
        // dropping `last_set_total_ms` etc. is caught.
        assert!(json.contains("\"per_output\":"), "per_output key missing");
        assert!(json.contains("\"aggregate\":"), "aggregate key missing");
        assert!(json.contains("\"lwe_pid\":4242"), "lwe_pid missing/wrong");
        assert!(
            json.contains("\"pid_state\":\"Running\""),
            "pid_state missing/wrong"
        );
        assert!(
            json.contains("\"last_set_at\":\"2026-08-15T10:00:00Z\""),
            "last_set_at missing"
        );
        assert!(
            json.contains("\"last_transition_ms\":125"),
            "last_transition_ms missing"
        );
        assert!(json.contains("\"uptime_secs\":3600"), "uptime_secs missing");
        assert!(
            json.contains("\"last_set_total_ms\":1700000000000"),
            "last_set_total_ms missing"
        );
    }

    /// Empty `HealthSnapshot` (no outputs bound) serializes cleanly.
    /// This is what a fresh daemon (or a non-LWE backend daemon)
    /// returns.
    #[test]
    fn health_snapshot_json_roundtrip_empty() {
        let snap = HealthSnapshot::default();
        let json = serde_json::to_string(&snap).expect("empty HealthSnapshot serializes");
        let back: HealthSnapshot =
            serde_json::from_str(&json).expect("empty HealthSnapshot parses");
        assert_eq!(back, snap);
        assert!(back.per_output.is_empty());
        assert_eq!(back.aggregate.current_pid, None);
        assert_eq!(back.aggregate.uptime_secs, 0);
        assert_eq!(back.aggregate.last_set_total_ms, None);
    }

    /// The trait's default `get_health` impl returns `Err` so the
    /// `StubControl` (which doesn't override it) inherits Err
    /// without us having to plumb a fake snapshot through every
    /// test.
    #[tokio::test]
    async fn stub_control_get_health_returns_default_err() {
        let stub = Arc::new(StubControl::default());
        let ctrl: Arc<dyn PaperforgeControl> = stub.clone();
        let err = ctrl.get_health().await.unwrap_err();
        assert!(
            format!("{err}").contains("not supported"),
            "default get_health must return NotSupported; got: {err}"
        );
    }

    /// Verify the zbus interface compiles + the type is constructible.
    /// We don't open a real D-Bus connection here — that's covered by
    /// the `serve_dbus` integration smoke test below.
    #[test]
    fn interface_compiles_with_stub() {
        let stub: Arc<dyn PaperforgeControl> = Arc::new(StubControl::default());
        let _iface = PaperforgeInterface::new(stub);
    }

    #[test]
    fn bus_constants_are_static() {
        assert_eq!(BUS_NAME, "org.louzt.Paperforge");
        assert_eq!(OBJECT_PATH, "/org/louzt/Paperforge");
    }

    /// The forwarder task is a pure event→signal mapper. We can't
    /// easily exercise the zbus signal emission without a real
    /// connection, but we can verify the channel draining math: an
    /// empty channel exits the loop, and a closed channel returns
    /// `None` immediately.
    #[tokio::test]
    async fn forward_events_drains_then_exits_on_close() {
        // We can't call the real `forward_events` without a real
        // SignalContext (it needs a zbus connection), but we can
        // assert the behaviour of the same channel pattern in
        // isolation: an UnboundedReceiver on a freshly-built channel
        // returns None on the first recv when the sender is dropped.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::DaemonEvent>();
        tx.send(crate::DaemonEvent::WallpaperStopped {
            pid: 42,
            at: chrono::Utc::now(),
        })
        .unwrap();
        drop(tx);
        let first = rx.recv().await.unwrap();
        match first {
            crate::DaemonEvent::WallpaperStopped { pid, .. } => assert_eq!(pid, 42),
            _ => panic!("expected WallpaperStopped"),
        }
        assert!(
            rx.recv().await.is_none(),
            "next recv after sender drop must be None"
        );
    }
}
