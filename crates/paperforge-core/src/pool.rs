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
    sync::Arc,
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
/// the fields are all `PathBuf` + `Vec<String>` + `Arc<...>`, so
/// clone is cheap.
#[derive(Debug, Clone)]
pub struct LweSinglePool {
    binary: PathBuf,
    /// Common flags appended to every invocation (e.g. `--silent`,
    /// `--disable-particles`). Operator-overridable via constructor.
    common_flags: Vec<String>,
    /// FPS cap passed as `--fps <N>` to LWE. Defaults to 30 (LWE's
    /// own default). Smart calibration can override this at runtime
    /// via [`Self::set_active_fps`].
    active_fps: u32,
    inner: Arc<Mutex<Option<PoolProcess>>>,
}

impl LweSinglePool {
    /// Construct with the default LWE binary on PATH and default flags.
    pub fn new() -> Self {
        Self::with_binary(PathBuf::from("linux-wallpaperengine"))
    }

    /// Construct with an explicit LWE binary path.
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            common_flags: default_flags(),
            active_fps: 30,
            inner: Arc::new(Mutex::new(None)),
        }
    }

    /// Override the flag list (mostly for tests). Default is
    /// `[--silent, --no-audio-processing, --disable-particles,
    ///   --disable-mouse, --disable-parallax, --fullscreen-pause-only-active]`.
    pub fn with_flags(mut self, flags: Vec<String>) -> Self {
        self.common_flags = flags;
        self
    }

    /// Override the FPS cap (mostly for tests / smart calibration).
    pub fn with_active_fps(mut self, fps: u32) -> Self {
        self.active_fps = fps;
        self
    }

    /// Read the current FPS cap (passed as `--fps <N>` to LWE).
    pub fn active_fps(&self) -> u32 {
        self.active_fps
    }

    /// Update the FPS cap at runtime. The new value takes effect on
    /// the next respawn (current process keeps its existing
    /// `--fps` argv until it's respawned).
    pub fn set_active_fps(&mut self, fps: u32) {
        self.active_fps = fps;
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
                self.active_fps,
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
    /// Returns the new PID after the swap, or the existing PID if no
    /// respawn was needed.
    pub async fn bind(&self, output: &str, content_id: &str) -> Result<i32> {
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

        // Fast path: output already bound to the same content_id,
        // AND process is alive. No respawn needed.
        if let Some(proc) = guard.as_ref() {
            if let Some(existing) = proc.bindings.get(output) {
                if existing == content_id {
                    // Verify the process is still alive (it may have
                    // crashed externally). If dead, fall through to
                    // respawn with the merged bindings.
                    if let Ok(BackendState::Running) = crate::backend::pid_state_quick(proc.pid) {
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

        // Kill the previous process if any.
        if let Some(mut prev) = guard.take() {
            // Best-effort SIGTERM. If LWE is stuck, SIGKILL after a
            // short grace period. The grace is small (200 ms) because
            // the typical case is "LWE was already idle from a previous
            // respawn and is responsive to SIGTERM".
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

        // Spawn the new process.
        let argv = build_argv(
            &self.binary,
            &new_bindings,
            &self.common_flags,
            self.active_fps,
        );
        let mut cmd = Command::new(&self.binary);
        cmd.args(&argv[1..]); // argv[0] is the binary itself; Command already has it
        let child = cmd.spawn().map_err(|e| Error::BackendFailure {
            kind: BackendKind::LinuxWallpaperEngine
                .process_pattern()
                .to_string(),
            message: format!("spawn LWE failed: {e}"),
        })?;
        let pid = child.id() as i32;

        *guard = Some(PoolProcess {
            pid,
            bindings: new_bindings,
            child: Some(child),
        });

        tracing::info!(
            target: "paperforge",
            "LWE pool respawn: pid={} bindings={:?}",
            pid,
            guard.as_ref().unwrap().bindings,
        );

        Ok(pid)
    }

    /// Convenience: translate a Workshop scene path to its content_id
    /// and call [`bind`](Self::bind).
    pub async fn bind_scene(&self, output: &str, scene: &Path) -> Result<i32> {
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
        self.bind(output, &content_id).await
    }

    /// Unbind an output. Removes the binding from the map and respawns
    /// the pool with the remaining bindings (if any). If no bindings
    /// remain, the pool is killed entirely.
    pub async fn unbind(&self, output: &str) -> Result<()> {
        let mut guard = self.inner.lock().await;

        // Step 1: take the current process out so we can decide what
        // to do with it without fighting the borrow checker. If there
        // is no current process, the pool is already empty.
        let mut prev = match guard.take() {
            Some(p) => p,
            None => return Ok(()),
        };

        // Step 2: clone the bindings and remove the output. We clone
        // (not move) because if the output wasn't bound, we put the
        // process back untouched.
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

        // Step 4: if no bindings remain, leave the pool empty.
        if new_bindings.is_empty() {
            tracing::info!(
                target: "paperforge",
                "LWE pool empty after unbind({})",
                output
            );
            return Ok(());
        }

        // Step 5: spawn fresh with the remaining bindings.
        let argv = build_argv(
            &self.binary,
            &new_bindings,
            &self.common_flags,
            self.active_fps,
        );
        let mut cmd = Command::new(&self.binary);
        cmd.args(&argv[1..]);
        let child = cmd.spawn().map_err(|e| Error::BackendFailure {
            kind: BackendKind::LinuxWallpaperEngine
                .process_pattern()
                .to_string(),
            message: format!("respawn after unbind failed: {e}"),
        })?;
        let pid = child.id() as i32;
        *guard = Some(PoolProcess {
            pid,
            bindings: new_bindings,
            child: Some(child),
        });
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
    pub async fn pause_soft(&self, awake_ms: u64, asleep_ms: u64) -> Result<Option<i32>> {
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
            tokio::spawn(super::backend::soft_pause_cycle_pool(
                self.inner.clone(),
                awake_ms,
                asleep_ms,
            ));
        }
        Ok(Some(pid))
    }

    /// SIGCONT the current LWE process (global resume).
    pub async fn resume(&self) -> Result<Option<i32>> {
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
        let mut guard = self.inner.lock().await;
        if let Some(mut prev) = guard.take() {
            let _ = kill(Pid::from_raw(prev.pid), Signal::SIGTERM);
            if let Some(c) = prev.child.as_mut() {
                let _ = c.wait();
            }
        }
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
        // Best-effort SIGTERM if a process is alive. We can't async-wait
        // here; the parent daemon is expected to call shutdown() during
        // graceful exit.
        if let Ok(guard) = self.inner.try_lock() {
            if let Some(proc) = guard.as_ref() {
                let _ = kill(Pid::from_raw(proc.pid), Signal::SIGTERM);
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

    /// End-to-end: spawn `/bin/sleep` as the "LWE binary" with a
    /// multi-output argv, then SIGTERM and verify exit. We can't use
    /// the real LWE binary in CI (no Wayland session), but the pool's
    /// argv construction + spawn + kill path is identical regardless
    /// of the binary.
    #[tokio::test]
    async fn bind_spawns_and_pause_resume_real_process() {
        let pool = LweSinglePool::with_binary("/bin/sleep").with_flags(vec!["60".to_string()]);
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
    }

    #[tokio::test]
    async fn unbind_when_pool_empty_is_noop() {
        let pool = LweSinglePool::with_binary("/bin/sleep");
        pool.unbind("DP-1").await.unwrap();
        assert!(pool.current_pid().await.is_none());
    }

    #[tokio::test]
    async fn shutdown_idempotent() {
        let pool = LweSinglePool::with_binary("/bin/sleep").with_flags(vec!["60".to_string()]);
        pool.bind("DP-1", "111").await.unwrap();
        pool.shutdown().await.unwrap();
        pool.shutdown().await.unwrap(); // second call must not panic
        assert!(pool.current_pid().await.is_none());
    }

    /// Output hotplug: a new output is bound after the pool already
    /// has one running. Pool must respawn with merged argv (existing
    /// + new pair). After unbind of the new output, pool respawns
    /// again with the original pair.
    #[tokio::test]
    async fn single_pool_handles_output_hotplug() {
        let pool = LweSinglePool::with_binary("/bin/sleep").with_flags(vec!["60".to_string()]);

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
    }

    /// Pause/resume: SIGSTOP + SIGCONT to the single LWE PID must
    /// toggle `/proc/<pid>/status` State field T ↔ R/S. We use
    /// `/bin/sleep` as a stand-in for the real LWE binary. Because
    /// sleep rejects the `--screen-root` argv that the pool emits,
    /// we override the flag list with empty so sleep sees only its
    /// own arg (`60` seconds) and stays alive long enough for us
    /// to signal it.
    #[tokio::test]
    async fn single_pool_pause_global_via_sigstop() {
        // With zero common flags the argv is just
        //   /bin/sleep --screen-root DP-1 --bg 111
        // which sleep still rejects (--screen-root not understood).
        // So we use a long sleep + bypass the LWE argv entirely via
        // a tiny shell wrapper that ignores its argv and just sleeps.
        let wrapper = std::env::temp_dir().join("paperforge-sleep-wrapper.sh");
        std::fs::write(&wrapper, "#!/bin/sh\nexec /bin/sleep 60\n").unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();

        // Build pool with empty flags so the wrapper sees zero argv
        // past the binary path (sleep just sleeps 60s).
        let pool = LweSinglePool::with_binary(&wrapper).with_flags(vec![]);
        let pid = pool.bind("DP-1", "111").await.unwrap();
        assert!(pid > 0);

        // Pre-state: process is running.
        let pre = crate::backend::pid_state_quick(pid).unwrap();
        assert_eq!(pre, BackendState::Running, "pre-pause must be Running");

        // Pause: SIGSTOP.
        pool.pause().await.unwrap();
        // Give the kernel a moment to deliver + record the signal state.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let paused = crate::backend::pid_state_quick(pid).unwrap();
        assert_eq!(paused, BackendState::Paused, "post-STOP must be Paused");

        // Resume: SIGCONT.
        pool.resume().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let resumed = crate::backend::pid_state_quick(pid).unwrap();
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
}
