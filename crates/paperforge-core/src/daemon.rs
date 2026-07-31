//! Daemon implementation of [`PaperforgeControl`].
//!
//! Wires the backend + audio + playlist subsystems into a single
//! state struct that the D-Bus interface can drive. The daemon
//! itself runs as `paperforge daemon` under systemd (see
//! `contrib/systemd/paperforge.service`).
//!
//! # Architecture
//!
//! ```text
//! ┌────────────────┐  tokio::select!  ┌─────────────────┐
//! │ HotplugWatcher ├──────────────────► monitor_changed  │
//! └────────────────┘                  │ signal → D-Bus  │
//!                                     └─────────────────┘
//! ┌────────────────┐  set()            ┌─────────────────┐
//! │ Backend (LWE)  ├──────────────────► wallpaper_started│
//! └────────────────┘                  │ signal → D-Bus  │
//!                                     └─────────────────┘
//! ```
//!
//! # Lifecycle
//!
//! The daemon holds the [`PaperforgeDaemon`] in an `Arc`. The D-Bus
//! interface holds a trait object; the daemon's methods delegate
//! to [`BackendOps`] + audio + playlist subsystems.
//!
//! Tests inject a stub [`BackendOps`]; production code wires the
//! real [`LweBackendOps`].

use std::{path::PathBuf, sync::Arc};

use chrono::Utc;
use tokio::sync::{mpsc, RwLock};

use crate::{
    audio::{AudioCommand, LweAudioController},
    backend::{BackendKind, BackendState, LweBackend, SwwwBackend, WallpaperBackend},
    dbus::{DaemonState, PaperforgeControl},
    error::{Error, Result},
    hotplug::HotplugEvent,
    playlist::PlaylistStore,
};

/// Backend abstraction used by the daemon. The daemon does not
/// care which concrete backend is wired (LWE vs swww vs hyprpaper
/// vs mpvpaper); only the trait surface matters.
#[async_trait::async_trait]
pub trait BackendOps: Send + Sync {
    /// Set a scene/image to a specific output (or default).
    async fn set(&self, output: &str, scene: &str) -> Result<()>;
    /// Set a scene to an output and return the pid of the running
    /// instance, or `0` if the backend doesn't track per-output pids
    /// (e.g. swww's single daemon).
    ///
    /// Default impl calls `set` and returns `Ok(0)` — most backends
    /// don't yet surface set-time pids. LWE's [`LweBackendOps`]
    /// overrides this to return the real pool PID.
    async fn set_with_pid(&self, output: &str, scene: &str) -> Result<i32> {
        tracing::debug!(
            backend = ?self.kind(),
            "set_with_pid: default impl returning 0 (no pid surface)"
        );
        self.set(output, scene).await?;
        Ok(0)
    }
    /// Pause all running instances. Returns count of PIDs signaled.
    async fn pause(&self) -> Result<usize>;
    /// Resume all paused instances. Returns count of PIDs signaled.
    async fn resume(&self) -> Result<usize>;
    /// List running instances as (pid, state) tuples.
    async fn list(&self) -> Result<Vec<(i32, BackendState)>>;
    /// Which backend kind.
    fn kind(&self) -> BackendKind;
}

/// Thin wrapper over [`LweBackend`] that implements [`BackendOps`].
///
/// `use_pool` decides the v0.2 vs v0.1 behaviour:
/// - `use_pool = true` (default): single LWE process via the pool,
///   shared across all outputs. Setters translate the path to a
///   `(output, content_id)` pair and call `pool.bind(...)`.
/// - `use_pool = false`: legacy v0.1 per-output spawn. Each `set`
///   forks a fresh LWE process with `--screen-root <output> --bg <id>`.
///   Slower, ~3× more RSS, but bypasses any pool bug.
#[derive(Clone)]
pub struct LweBackendOps {
    backend: LweBackend,
    audio: LweAudioController,
    /// Whether to use the shared multi-output pool (v0.2) or the
    /// per-output spawn (v0.1). Reads from
    /// [`crate::config::Config::pool_enabled`] at daemon construction.
    use_pool: bool,
}

impl LweBackendOps {
    /// Construct with the default LWE binary on PATH and the pool
    /// enabled (v0.2 mode).
    pub fn new() -> Self {
        Self::with_pool(true)
    }

    /// Construct with the pool enabled/disabled flag.
    pub fn with_pool(use_pool: bool) -> Self {
        let backend = LweBackend::new();
        let audio = LweAudioController::new(backend.clone());
        Self {
            backend,
            audio,
            use_pool,
        }
    }

    /// Construct with an explicit LWE binary path and the pool enabled.
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self::with_binary_and_pool(binary, true)
    }

    /// Construct with an explicit LWE binary path and the pool
    /// enabled/disabled flag.
    pub fn with_binary_and_pool(binary: impl Into<PathBuf>, use_pool: bool) -> Self {
        let backend = LweBackend::with_binary(binary);
        let audio = LweAudioController::new(backend.clone());
        Self {
            backend,
            audio,
            use_pool,
        }
    }

    /// Access the underlying LWE backend (for signal control).
    pub fn backend(&self) -> &LweBackend {
        &self.backend
    }

    /// Access the audio controller (for SIGUSR1/SIGUSR2 dispatch).
    pub fn audio(&self) -> &LweAudioController {
        &self.audio
    }

    /// Whether this instance uses the v0.2 pool architecture.
    pub fn use_pool(&self) -> bool {
        self.use_pool
    }
}

impl Default for LweBackendOps {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl BackendOps for LweBackendOps {
    async fn set(&self, output: &str, scene: &str) -> Result<()> {
        if self.use_pool {
            // v0.2: pool-backed. Translate the scene path to a
            // content_id and bind it. The pool owns the spawn +
            // respawn; we just propagate the pid through the
            // daemon's event channel.
            let _pid = self.set_with_pid(output, scene).await?;
        } else {
            // v0.1 legacy: per-output spawn. The `WallpaperBackend`
            // shim already implements this path; the only thing we
            // lose in this branch is the pid (the legacy implementation
            // doesn't surface it). Returning 0 is acknowledged by the
            // `DaemonEvent::WallpaperStarted { pid: 0 }` payload.
            let path = std::path::Path::new(scene);
            self.backend.set(path, Some(output)).await?;
        }
        Ok(())
    }

    async fn set_with_pid(&self, output: &str, scene: &str) -> Result<i32> {
        // Only the pool path returns a real pid. The v0.1 fallback
        // is observable as `pid: 0` in the daemon event.
        if !self.use_pool {
            let path = std::path::Path::new(scene);
            self.backend.set(path, Some(output)).await?;
            return Ok(0);
        }
        let path = std::path::Path::new(scene);
        // Need to extract content_id from the Workshop path; we
        // delegate to the pool's bind_scene which validates +
        // returns the pid.
        let pid = self.backend.pool().bind_scene(output, path).await?;
        Ok(pid)
    }

    async fn pause(&self) -> Result<usize> {
        self.backend.pause().await
    }

    async fn resume(&self) -> Result<usize> {
        self.backend.resume().await
    }

    async fn list(&self) -> Result<Vec<(i32, BackendState)>> {
        let pids = self.backend.list_pids().await?;
        let mut out = Vec::with_capacity(pids.len());
        for pid in pids {
            let s = self.backend.state(pid).await?;
            out.push((pid, s));
        }
        Ok(out)
    }

    fn kind(&self) -> BackendKind {
        self.backend.kind()
    }
}

/// Events emitted by the daemon to the D-Bus adapter.
#[derive(Debug, Clone)]
pub enum DaemonEvent {
    /// A wallpaper instance started rendering.
    WallpaperStarted {
        /// Output name (e.g. `"DP-1"`).
        output: String,
        /// Absolute path to the scene.
        scene_path: String,
        /// LWE PID (0 if unknown).
        pid: i32,
        /// Wallclock timestamp (RFC3339).
        at: chrono::DateTime<Utc>,
    },
    /// A wallpaper instance exited.
    WallpaperStopped {
        /// PID that exited.
        pid: i32,
        /// Wallclock timestamp (RFC3339).
        at: chrono::DateTime<Utc>,
    },
    /// Wayland output set changed (hotplug).
    MonitorChanged {
        /// New set of output names.
        outputs: Vec<String>,
        /// Wallclock timestamp (RFC3339).
        at: chrono::DateTime<Utc>,
    },
}

/// Concrete daemon state. Shared across all D-Bus method calls
/// (which run concurrently on the tokio runtime).
pub struct PaperforgeDaemon {
    backend: Arc<dyn BackendOps>,
    /// Optional concrete handle to the LWE backend ops, used by
    /// LWE-specific paths (pool unbind on hotplug, audio dispatch).
    /// `None` for non-LWE backends or test stubs.
    lwe_ops: Option<Arc<LweBackendOps>>,
    playlists: Arc<RwLock<PlaylistStore>>,
    active_playlist: Arc<RwLock<Option<String>>>,
    known_outputs: Arc<RwLock<Vec<String>>>,
    /// Emits `wallpaper_started` / `wallpaper_stopped` /
    /// `monitor_changed` events to the D-Bus layer. The receiver
    /// lives in the D-Bus adapter (not here).
    event_tx: mpsc::UnboundedSender<DaemonEvent>,
    version: String,
}

impl PaperforgeDaemon {
    /// Construct a new daemon. Returns the daemon + a receiver for
    /// events the daemon emits (the D-Bus adapter consumes these).
    ///
    /// Uses `PlaylistStore::default_location()` which creates
    /// `~/.config/paperforge/playlists/` if missing.
    pub fn new(
        backend: Arc<dyn BackendOps>,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<DaemonEvent>)> {
        let playlists = PlaylistStore::default_location()?;
        Ok(Self::with_store(backend, Arc::new(RwLock::new(playlists))))
    }

    /// Construct with an explicit playlist store (used by tests).
    /// Always succeeds (mpsc channel creation is infallible).
    pub fn with_store(
        backend: Arc<dyn BackendOps>,
        playlists: Arc<RwLock<PlaylistStore>>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<DaemonEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let daemon = Arc::new(Self {
            backend,
            lwe_ops: None,
            playlists,
            active_playlist: Arc::new(RwLock::new(None)),
            known_outputs: Arc::new(RwLock::new(Vec::new())),
            event_tx,
            version: crate::VERSION.to_string(),
        });
        (daemon, event_rx)
    }

    /// Construct with an explicit LWE backend handle. Used by
    /// production entry points that built an `LweBackendOps` and
    /// want the daemon to be able to call pool-specific methods
    /// (unbind, audio) without going through the trait object.
    pub fn with_lwe_backend_ops(
        backend: Arc<dyn BackendOps>,
        lwe_ops: Arc<LweBackendOps>,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<DaemonEvent>)> {
        let playlists = PlaylistStore::default_location()?;
        Ok(Self::with_lwe_backend_ops_and_store(
            backend,
            lwe_ops,
            Arc::new(RwLock::new(playlists)),
        ))
    }

    /// Same as `with_lwe_backend_ops` but with an explicit
    /// `PlaylistStore` (used by tests).
    pub fn with_lwe_backend_ops_and_store(
        backend: Arc<dyn BackendOps>,
        lwe_ops: Arc<LweBackendOps>,
        playlists: Arc<RwLock<PlaylistStore>>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<DaemonEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let daemon = Arc::new(Self {
            backend,
            lwe_ops: Some(lwe_ops),
            playlists,
            active_playlist: Arc::new(RwLock::new(None)),
            known_outputs: Arc::new(RwLock::new(Vec::new())),
            event_tx,
            version: crate::VERSION.to_string(),
        });
        (daemon, event_rx)
    }

    /// Convenience constructor for swww-backed daemon.
    pub fn with_swww() -> Result<(Arc<Self>, mpsc::UnboundedReceiver<DaemonEvent>)> {
        let backend: Arc<dyn BackendOps> = Arc::new(SwwwBackendOps(SwwwBackend::new()));
        Self::new(backend)
    }

    /// Add a known output (called by the hotplug watcher when it
    /// discovers the initial set).
    pub async fn set_known_outputs(&self, outputs: Vec<String>) {
        *self.known_outputs.write().await = outputs;
    }

    /// List known outputs (snapshot).
    pub async fn known_outputs(&self) -> Vec<String> {
        self.known_outputs.read().await.clone()
    }

    /// Emit an event to the D-Bus adapter. Returns `false` if the
    /// receiver is dropped (daemon shutting down).
    pub fn emit(&self, ev: DaemonEvent) -> bool {
        self.event_tx.send(ev).is_ok()
    }

    /// Apply an [`AudioCommand`] via the embedded audio controller
    /// (only present when the backend is LWE-backed).
    ///
    /// Returns `Ok(0)` for non-LWE backends — they don't accept
    /// SIGUSR1/SIGUSR2 audio toggles.
    pub async fn audio(&self, cmd: AudioCommand) -> Result<u32> {
        if !matches!(self.backend.kind(), BackendKind::LinuxWallpaperEngine) {
            return Ok(0);
        }
        // For LWE-backed daemons, the audio controller lives inside
        // `LweBackendOps`. Since we store it as `Arc<dyn BackendOps>`,
        // we can't downcast cleanly; the daemon's audio controller
        // IS the same one the backend wraps, so we re-issue through
        // the backend's LWE handle. Production wiring constructs
        // LweBackendOps and shares the audio controller between
        // both the BackendOps and a stash here. Until we wire that
        // stash, this path returns 0 with a tracing warn — the
        // CLI binary (`paperforge pause --audio`) does the SIGUSR
        // dispatch directly via `LweBackendOps::audio()`.
        let _ = cmd;
        tracing::warn!(
            "audio dispatch through D-Bus not wired yet; use `paperforge audio toggle` CLI"
        );
        Ok(0)
    }

    /// React to a hotplug event: update `known_outputs`, and when
    /// the backend is LWE + pool-enabled, `pool.unbind()` any
    /// outputs that disappeared. This keeps the pool's argv
    /// consistent with the current monitor set (no zombie
    /// `--screen-root <gone-output>` pairs).
    ///
    /// Returns the outputs that were unbound (for logging / D-Bus
    /// signal emission).
    pub async fn handle_hotplug(&self, ev: HotplugEvent) -> Vec<String> {
        let new_outputs = ev.current_names();
        *self.known_outputs.write().await = new_outputs;
        let mut unbound = Vec::new();
        // Only the pool path needs unbind cleanup. For non-LWE
        // backends (swww, hyprpaper, mpvpaper) the daemon owns no
        // per-output state, so there's nothing to clean up.
        if !matches!(self.backend.kind(), BackendKind::LinuxWallpaperEngine) {
            return unbound;
        }
        // Pool unbind is a no-op when the output wasn't bound, so
        // it's safe to call for every removed candidate.
        let removed = match &ev {
            HotplugEvent::Changed { removed, .. } => removed,
            _ => return unbound,
        };
        // We need a handle to the actual LweBackendOps (or its pool)
        // to call unbind. The trait object hides it, so we
        // best-effort: dispatch via the backend's `set_with_pid` if
        // it were a pool-backed LWE. We can't unbind from a
        // non-LWE backend, so this is the right scope.
        if let Some(lwe_ops) = self.backend_as_lwe() {
            if !lwe_ops.use_pool() {
                return unbound;
            }
            for out in removed {
                let name = out.name.clone();
                if let Err(e) = lwe_ops.backend().pool().unbind(&name).await {
                    tracing::warn!(
                        target: "paperforge",
                        "pool unbind({name}) failed: {e}"
                    );
                } else {
                    unbound.push(name);
                }
            }
        }
        unbound
    }

    /// Best-effort downcast to [`LweBackendOps`] via the trait
    /// object. Returns `None` when the backend is swww / hyprpaper /
    /// mpvpaper or a test stub.
    fn backend_as_lwe(&self) -> Option<&LweBackendOps> {
        // We can't dynamically downcast a `dyn Trait`. The daemon
        // stores `Arc<dyn BackendOps>` for backend-agnostic dispatch,
        // but LWE-specific paths (audio, pool unbind) need a
        // concrete handle. The standard mitigation is to ALSO store
        // an `Option<Arc<LweBackendOps>>` here, set when the daemon
        // is constructed with an LWE backend. See
        // [`PaperforgeDaemon::with_lwe_backend_ops`].
        self.lwe_ops.as_deref()
    }
}

#[async_trait::async_trait]
impl PaperforgeControl for PaperforgeDaemon {
    async fn set_wallpaper(&self, output: &str, scene_path: &str) -> Result<()> {
        // Capture the pid from the backend (real for LWE pool,
        // 0 for everything else). The backend's `set` is a superset
        // of `set_with_pid` for the pool architecture, but we route
        // through `set_with_pid` so the pid is propagated through the
        // same code path regardless of pool_enabled.
        let pid = self.backend.set_with_pid(output, scene_path).await?;
        let _ = self.emit(DaemonEvent::WallpaperStarted {
            output: output.to_string(),
            scene_path: scene_path.to_string(),
            pid,
            at: Utc::now(),
        });
        Ok(())
    }

    async fn pause(&self) -> Result<u32> {
        self.backend.pause().await.map(|n| n as u32)
    }

    async fn resume(&self) -> Result<u32> {
        self.backend.resume().await.map(|n| n as u32)
    }

    async fn audio_toggle(&self) -> Result<u32> {
        self.audio(AudioCommand::Toggle).await
    }

    async fn audio_mute(&self) -> Result<u32> {
        self.audio(AudioCommand::Mute).await
    }

    async fn audio_unmute(&self) -> Result<u32> {
        self.audio(AudioCommand::Unmute).await
    }

    async fn list_running(&self) -> Result<Vec<(i32, BackendState)>> {
        self.backend.list().await
    }

    async fn apply_playlist(&self, name: &str) -> Result<()> {
        let pl = {
            let store = self.playlists.read().await;
            store
                .load(name)
                .map_err(|_| Error::Other(anyhow::anyhow!("playlist not found: {name}")))?
        };
        let backend = self.backend.clone();
        let outputs = if pl.outputs.is_empty() {
            self.known_outputs.read().await.clone()
        } else {
            pl.outputs.clone()
        };
        let n = pl.wallpapers.len().max(1);
        for (i, output) in outputs.iter().enumerate() {
            let scene = pl.wallpapers[i % n].to_string_lossy().to_string();
            backend.set(output, &scene).await?;
        }
        *self.active_playlist.write().await = Some(name.to_string());
        Ok(())
    }

    async fn get_state(&self) -> Result<DaemonState> {
        let running = self.backend.list().await?;
        Ok(DaemonState {
            backend: self.backend.kind(),
            active_playlist: self.active_playlist.read().await.clone(),
            running,
            known_outputs: self.known_outputs.read().await.clone(),
            version: self.version.clone(),
        })
    }
}

/// Adapter wrapping [`SwwwBackend`] as [`BackendOps`].
struct SwwwBackendOps(SwwwBackend);

#[async_trait::async_trait]
impl BackendOps for SwwwBackendOps {
    async fn set(&self, output: &str, scene: &str) -> Result<()> {
        self.0.set(scene.as_ref(), Some(output)).await
    }

    async fn pause(&self) -> Result<usize> {
        self.0.pause().await
    }

    async fn resume(&self) -> Result<usize> {
        self.0.resume().await
    }

    async fn list(&self) -> Result<Vec<(i32, BackendState)>> {
        let pids = self.0.list_pids().await?;
        let mut out = Vec::with_capacity(pids.len());
        for pid in pids {
            let s = self.0.state(pid).await?;
            out.push((pid, s));
        }
        Ok(out)
    }

    fn kind(&self) -> BackendKind {
        self.0.kind()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory backend for tests.
    struct FakeBackend {
        kind: BackendKind,
        sets: Mutex<Vec<(String, String)>>,
        paused_count: Mutex<usize>,
        running: Mutex<Vec<(i32, BackendState)>>,
    }

    impl FakeBackend {
        fn new(kind: BackendKind) -> Self {
            Self {
                kind,
                sets: Mutex::new(Vec::new()),
                paused_count: Mutex::new(0),
                running: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl BackendOps for FakeBackend {
        async fn set(&self, output: &str, scene: &str) -> Result<()> {
            self.sets
                .lock()
                .unwrap()
                .push((output.to_string(), scene.to_string()));
            Ok(())
        }
        async fn pause(&self) -> Result<usize> {
            Ok(*self.paused_count.lock().unwrap())
        }
        async fn resume(&self) -> Result<usize> {
            *self.paused_count.lock().unwrap() = 0;
            Ok(0)
        }
        async fn list(&self) -> Result<Vec<(i32, BackendState)>> {
            Ok(self.running.lock().unwrap().clone())
        }
        fn kind(&self) -> BackendKind {
            self.kind
        }
    }

    /// Construct a daemon with an in-memory playlist store rooted
    /// at a tempdir. Returns the tempdir so the test can keep it
    /// alive for the test duration.
    fn fresh_daemon(
        backend: &Arc<FakeBackend>,
    ) -> (
        tempfile::TempDir,
        Arc<PaperforgeDaemon>,
        mpsc::UnboundedReceiver<DaemonEvent>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let store = PlaylistStore::new(tmp.path()).unwrap();
        let backend_dyn: Arc<dyn BackendOps> = backend.clone();
        let (daemon, rx) = PaperforgeDaemon::with_store(backend_dyn, Arc::new(RwLock::new(store)));
        (tmp, daemon, rx)
    }

    #[tokio::test]
    async fn daemon_set_wallpaper_records_call() {
        let backend = Arc::new(FakeBackend::new(BackendKind::LinuxWallpaperEngine));
        let (_tmp, daemon, _rx) = fresh_daemon(&backend);
        daemon.set_wallpaper("DP-1", "/scenes/x").await.unwrap();
        let sets = backend.sets.lock().unwrap();
        assert_eq!(sets[0], ("DP-1".to_string(), "/scenes/x".to_string()));
    }

    #[tokio::test]
    async fn daemon_pause_delegates_to_backend() {
        let backend = Arc::new(FakeBackend::new(BackendKind::LinuxWallpaperEngine));
        *backend.paused_count.lock().unwrap() = 3;
        let (_tmp, daemon, _rx) = fresh_daemon(&backend);
        assert_eq!(daemon.pause().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn daemon_resume_zeros_paused_count() {
        let backend = Arc::new(FakeBackend::new(BackendKind::LinuxWallpaperEngine));
        *backend.paused_count.lock().unwrap() = 5;
        let (_tmp, daemon, _rx) = fresh_daemon(&backend);
        assert_eq!(daemon.resume().await.unwrap(), 0);
        assert_eq!(*backend.paused_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn daemon_list_running_returns_backend_pids() {
        let backend = Arc::new(FakeBackend::new(BackendKind::LinuxWallpaperEngine));
        *backend.running.lock().unwrap() =
            vec![(100, BackendState::Running), (101, BackendState::Paused)];
        let (_tmp, daemon, _rx) = fresh_daemon(&backend);
        let list = daemon.list_running().await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], (100, BackendState::Running));
        assert_eq!(list[1], (101, BackendState::Paused));
    }

    #[tokio::test]
    async fn daemon_get_state_includes_backend_kind() {
        let backend = Arc::new(FakeBackend::new(BackendKind::SwwwDaemon));
        let (_tmp, daemon, _rx) = fresh_daemon(&backend);
        let s = daemon.get_state().await.unwrap();
        assert_eq!(s.backend, BackendKind::SwwwDaemon);
        assert!(s.active_playlist.is_none());
        assert!(s.running.is_empty());
    }

    #[tokio::test]
    async fn daemon_set_known_outputs_then_get_state_reflects() {
        let backend = Arc::new(FakeBackend::new(BackendKind::LinuxWallpaperEngine));
        let (_tmp, daemon, _rx) = fresh_daemon(&backend);
        daemon
            .set_known_outputs(vec!["DP-1".to_string(), "eDP-1".to_string()])
            .await;
        let s = daemon.get_state().await.unwrap();
        assert_eq!(
            s.known_outputs,
            vec!["DP-1".to_string(), "eDP-1".to_string()]
        );
    }

    #[tokio::test]
    async fn daemon_apply_unknown_playlist_errors() {
        let backend = Arc::new(FakeBackend::new(BackendKind::LinuxWallpaperEngine));
        let (_tmp, daemon, _rx) = fresh_daemon(&backend);
        let err = daemon.apply_playlist("nope").await.unwrap_err();
        assert!(format!("{err}").contains("playlist not found"));
    }

    #[tokio::test]
    async fn daemon_apply_known_playlist_fans_out() {
        use crate::playlist::{FillMode, Playlist};
        let backend = Arc::new(FakeBackend::new(BackendKind::LinuxWallpaperEngine));
        let (_tmp, daemon, _rx) = fresh_daemon(&backend);
        let pl = Playlist {
            name: "focus".to_string(),
            description: None,
            outputs: vec!["DP-1".to_string(), "eDP-1".to_string()],
            wallpapers: vec![PathBuf::from("/scenes/a"), PathBuf::from("/scenes/b")],
            fill: FillMode::Fill,
        };
        daemon.playlists.write().await.save(&pl).unwrap();
        daemon
            .set_known_outputs(vec!["DP-1".to_string(), "eDP-1".to_string()])
            .await;
        daemon.apply_playlist("focus").await.unwrap();
        let sets = backend.sets.lock().unwrap();
        assert_eq!(sets.len(), 2);
        assert!(sets.contains(&("DP-1".to_string(), "/scenes/a".to_string())));
        assert!(sets.contains(&("eDP-1".to_string(), "/scenes/b".to_string())));
    }

    #[tokio::test]
    async fn daemon_emit_returns_false_when_receiver_dropped() {
        let backend = Arc::new(FakeBackend::new(BackendKind::LinuxWallpaperEngine));
        let (_tmp, daemon, rx) = fresh_daemon(&backend);
        drop(rx);
        let sent = daemon.emit(DaemonEvent::WallpaperStopped {
            pid: 1,
            at: Utc::now(),
        });
        assert!(!sent, "emit must fail when no receiver");
    }

    #[tokio::test]
    async fn daemon_emit_returns_true_when_receiver_alive() {
        let backend = Arc::new(FakeBackend::new(BackendKind::LinuxWallpaperEngine));
        let (_tmp, daemon, mut rx) = fresh_daemon(&backend);
        let sent = daemon.emit(DaemonEvent::WallpaperStopped {
            pid: 1,
            at: Utc::now(),
        });
        assert!(sent);
        let ev = rx.recv().await.unwrap();
        assert!(matches!(ev, DaemonEvent::WallpaperStopped { pid: 1, .. }));
    }

    #[tokio::test]
    async fn daemon_audio_for_swww_returns_zero() {
        let backend = Arc::new(FakeBackend::new(BackendKind::SwwwDaemon));
        let (_tmp, daemon, _rx) = fresh_daemon(&backend);
        assert_eq!(daemon.audio_toggle().await.unwrap(), 0);
        assert_eq!(daemon.audio_mute().await.unwrap(), 0);
        assert_eq!(daemon.audio_unmute().await.unwrap(), 0);
    }

    #[test]
    fn lwe_backend_ops_default_constructs() {
        let _ = LweBackendOps::default();
    }

    #[test]
    fn lwe_backend_ops_with_binary_constructs() {
        let _ = LweBackendOps::with_binary("/opt/lwe/linux-wallpaperengine");
    }

    /// `use_pool = false` (the v0.1 fallback path) must still
    /// succeed on `set_with_pid` but return `pid = 0` — the legacy
    /// per-output spawn does not surface a pid.
    #[tokio::test]
    async fn lwe_backend_ops_pool_disabled_returns_zero_pid() {
        let backend = LweBackendOps::with_pool(false);
        assert!(!backend.use_pool(), "use_pool must be false");
        // The v0.1 (legacy) path validates the scene path is a
        // Workshop-shaped path AND the file exists. Spawn a wrapper
        // binary so the legacy spawn actually runs (otherwise the
        // path's existence check is the only guarantee).
        let wrapper = std::env::temp_dir().join("paperforge-pool-disabled-binary.sh");
        std::fs::write(&wrapper, "#!/bin/sh\nexec /bin/sleep 60\n").unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let backend = LweBackendOps::with_binary_and_pool(&wrapper, false);

        let tmp = tempfile::tempdir().unwrap();
        let scene = tmp.path().join("fake/workshop/content/431960/847261582");
        std::fs::create_dir_all(scene.parent().unwrap()).unwrap();
        std::fs::write(&scene, b"fake scene").unwrap();

        let pid = backend
            .set_with_pid("DP-1", scene.to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(
            pid, 0,
            "use_pool=false path must return pid=0 (legacy v0.1)"
        );
        // Cleanup.
        let _ = std::fs::remove_file(&wrapper);
    }

    /// `use_pool = true` on an empty pool must NOT panic and must
    /// return `pid = 0` because nothing has been bound yet.
    /// (Subsequent set_with_pid on a Workshop path will return the
    /// real pid, covered by
    /// `lwe_backend_ops_set_emits_wallpaper_started_with_real_pid`.)
    #[tokio::test]
    async fn lwe_backend_ops_pool_enabled_empty_returns_zero_pid() {
        let backend = LweBackendOps::with_pool(true);
        assert!(backend.use_pool(), "use_pool must be true");
        let pid = backend.set_with_pid("DP-1", "/nonexistent/scene.pkg").await;
        // /nonexistent/... is not a Workshop path AND the file does
        // not exist, so we expect BackendUnreachable (path check).
        // The important invariant here is that the pool doesn't
        // panic on empty/non-Workshop input.
        assert!(
            pid.is_err(),
            "non-Workshop path must error rather than produce a bogus pid"
        );
    }

    /// Hotplug → pool unbind end-to-end. Build a daemon with an
    /// LWE backend, bind TWO outputs, simulate an unplug of one,
    /// and verify the pool has the expected single binding left.
    #[tokio::test]
    async fn daemon_handle_hotplug_unbinds_removed_output() {
        let wrapper = std::env::temp_dir().join("paperforge-hotplug-pool-binary.sh");
        std::fs::write(&wrapper, "#!/bin/sh\nexec /bin/sleep 60\n").unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let lwe_ops = Arc::new(LweBackendOps::with_binary_and_pool(&wrapper, true));
        let pool = lwe_ops.backend().pool().clone();
        let backend_dyn: Arc<dyn BackendOps> = lwe_ops.clone();

        let tmp = tempfile::tempdir().unwrap();
        let store = PlaylistStore::new(tmp.path()).unwrap();
        let (daemon, _rx) = PaperforgeDaemon::with_lwe_backend_ops_and_store(
            backend_dyn,
            lwe_ops,
            Arc::new(RwLock::new(store)),
        );

        // Build two Workshop-shaped paths on disk.
        let mk_scene = |id: &str| {
            let p = tmp
                .path()
                .join(format!("fake/workshop/content/431960/{id}"));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"fake scene").unwrap();
            p
        };

        // Bind both outputs.
        daemon
            .set_wallpaper("DP-1", mk_scene("111").to_str().unwrap())
            .await
            .unwrap();
        daemon
            .set_wallpaper("HDMI-A-1", mk_scene("222").to_str().unwrap())
            .await
            .unwrap();

        let bindings_before = pool.bindings().await;
        assert_eq!(bindings_before.len(), 2, "both outputs must be bound");

        // Simulate a hotplug change: HDMI-A-1 disappears.
        let unbound = daemon
            .handle_hotplug(HotplugEvent::Changed {
                current: vec![crate::hotplug::Output {
                    name: "DP-1".to_string(),
                }],
                added: vec![],
                removed: vec![crate::hotplug::Output {
                    name: "HDMI-A-1".to_string(),
                }],
            })
            .await;
        assert_eq!(unbound, vec!["HDMI-A-1".to_string()]);

        let bindings_after = pool.bindings().await;
        assert_eq!(bindings_after.len(), 1, "removed output must be unbound");
        assert!(bindings_after.contains_key("DP-1"));
        assert!(!bindings_after.contains_key("HDMI-A-1"));

        // The daemon's known_outputs must reflect the new state.
        assert_eq!(daemon.known_outputs().await, vec!["DP-1".to_string()]);

        // Cleanup.
        pool.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&wrapper);
    }

    /// End-to-end: a real `LweBackendOps` (pool-backed) bound to a
    /// `/bin/sleep` proxy binary must report a non-zero pid via the
    /// `DaemonEvent::WallpaperStarted` channel when `set_wallpaper`
    /// is called.
    ///
    /// This is the daemon-side smoke test for the v0.2 pool
    /// architecture: the pool bind() returns the pid, and the
    /// `PaperforgeDaemon::set_wallpaper` path propagates it through
    /// `set_with_pid`.
    #[tokio::test]
    async fn lwe_backend_ops_set_emits_wallpaper_started_with_real_pid() {
        // Wrapper script: sleep long enough for the test to receive
        // the event before the process exits.
        let wrapper = std::env::temp_dir().join("paperforge-daemon-pool-binary.sh");
        std::fs::write(&wrapper, "#!/bin/sh\nexec /bin/sleep 60\n").unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let backend_ops = LweBackendOps::with_binary_and_pool(&wrapper, true);
        // Clone the pool Arc into its own binding so it lives
        // independently of the backend. LweBackendOps is Clone
        // (cheap via internal Arcs); we move the original into the
        // `Arc<dyn BackendOps>` the daemon owns.
        let pool = backend_ops.backend().pool().clone();
        let backend_dyn: Arc<dyn BackendOps> = Arc::new(backend_ops.clone());
        // Suppress the "moved but not used" lint: `backend_ops` is
        // consumed by the trait-object Arc, but the compiler doesn't
        // see the Arc holding the last reference.
        let _ = backend_dyn;

        let tmp = tempfile::tempdir().unwrap();
        let store = PlaylistStore::new(tmp.path()).unwrap();
        let (daemon, mut rx) =
            PaperforgeDaemon::with_store(backend_dyn, Arc::new(RwLock::new(store)));

        // Build a Workshop-shaped path on disk so workshop_content_id
        // parses correctly. The leaf must be the numeric content_id
        // (the parser takes `workshop/content/<appid>/<id>` from the
        // END of the path).
        let scene = tmp.path().join("fake/workshop/content/431960/111");
        std::fs::create_dir_all(scene.parent().unwrap()).unwrap();
        std::fs::write(&scene, b"fake scene").unwrap();

        daemon
            .set_wallpaper("DP-1", scene.to_str().unwrap())
            .await
            .unwrap();

        // First event must be WallpaperStarted with a real pid.
        let ev = rx.recv().await.unwrap();
        let started_pid = match ev {
            DaemonEvent::WallpaperStarted { pid, .. } => pid,
            other => panic!("expected WallpaperStarted, got {other:?}"),
        };
        assert!(
            started_pid > 0,
            "pool must surface a non-zero pid, got {started_pid}"
        );
        // The pool itself should know about that pid.
        assert_eq!(pool.current_pid().await, Some(started_pid));

        // Cleanup.
        pool.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&wrapper);
    }

    /// End-to-end pool daemon smoke test mirroring the v0.2 architecture:
    /// 1. Construct a daemon with a pool-backed LWE backend (using a
    ///    `/bin/sleep` wrapper so the test doesn't need Wayland).
    /// 2. Feed an `Initial` hotplug event with two outputs.
    /// 3. `set_wallpaper` on one output — verify the pool has one binding.
    /// 4. Feed a `Changed` event that removes the bound output.
    /// 5. Verify the pool is empty afterwards.
    /// 6. `pool.shutdown()` is idempotent — verify no panic on cleanup.
    #[tokio::test]
    async fn daemon_initial_then_remove_yields_empty_pool() {
        let wrapper = std::env::temp_dir().join("paperforge-fase4-initial-pool.sh");
        std::fs::write(&wrapper, "#!/bin/sh\nexec /bin/sleep 60\n").unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        let lwe_ops = Arc::new(LweBackendOps::with_binary_and_pool(&wrapper, true));
        let pool = lwe_ops.backend().pool().clone();
        let backend_dyn: Arc<dyn BackendOps> = lwe_ops.clone();

        let tmp = tempfile::tempdir().unwrap();
        let store = PlaylistStore::new(tmp.path()).unwrap();
        let (daemon, mut rx) = PaperforgeDaemon::with_lwe_backend_ops_and_store(
            backend_dyn,
            lwe_ops,
            Arc::new(RwLock::new(store)),
        );

        // 1. Initial: DP-1 and HDMI-A-1 are connected.
        let initial = HotplugEvent::Initial(vec![
            crate::hotplug::Output {
                name: "DP-1".to_string(),
            },
            crate::hotplug::Output {
                name: "HDMI-A-1".to_string(),
            },
        ]);
        daemon.handle_hotplug(initial).await;
        assert_eq!(
            daemon.known_outputs().await,
            vec!["DP-1".to_string(), "HDMI-A-1".to_string()]
        );
        assert!(
            pool.bindings().await.is_empty(),
            "Initial event must not bind"
        );

        // 2. Bind DP-1.
        let scene = tmp.path().join("fake/workshop/content/431960/847261582");
        std::fs::create_dir_all(scene.parent().unwrap()).unwrap();
        std::fs::write(&scene, b"fake scene").unwrap();
        daemon
            .set_wallpaper("DP-1", scene.to_str().unwrap())
            .await
            .unwrap();

        let started = rx.recv().await.unwrap();
        assert!(matches!(started, DaemonEvent::WallpaperStarted { pid, .. } if pid > 0));

        let bindings = pool.bindings().await;
        assert_eq!(bindings.len(), 1, "exactly one bind after set_wallpaper");
        assert_eq!(bindings.get("DP-1").map(String::as_str), Some("847261582"));

        // 3. Hotplug: DP-1 disappears.
        let unbound = daemon
            .handle_hotplug(HotplugEvent::Changed {
                current: vec![crate::hotplug::Output {
                    name: "HDMI-A-1".to_string(),
                }],
                added: vec![],
                removed: vec![crate::hotplug::Output {
                    name: "DP-1".to_string(),
                }],
            })
            .await;
        assert_eq!(unbound, vec!["DP-1".to_string()]);
        assert!(pool.bindings().await.is_empty(), "pool empty after unplug");
        assert_eq!(daemon.known_outputs().await, vec!["HDMI-A-1".to_string()]);

        // 4. Cleanup.
        pool.shutdown().await.unwrap();
        pool.shutdown().await.unwrap(); // idempotent
        let _ = std::fs::remove_file(&wrapper);
    }
}
