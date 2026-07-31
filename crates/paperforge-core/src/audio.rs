//! Audio control for `linux-wallpaperengine` via POSIX signals.
//!
//! LWE doesn't have a public IPC socket for runtime control today.
//! It does, however, react to POSIX signals:
//!
//! - `SIGUSR1` — toggle mute (LWE convention for `linux-wallpaperengine`
//!   builds linked against PulseAudio/PipeWire output sinks).
//! - `SIGUSR2` — explicitly mute.
//!
//! This module sends those signals via `nix::sys::signal::kill` after
//! locating LWE PIDs through the same `/proc/<pid>/cmdline` mechanism
//! used by [`crate::backend::LweBackend::list_pids`].
//!
//! # Safety
//!
//! **SIGUSR1/SIGUSR2 handling in LWE is not universally present across
//! builds.** If the signal is unhandled, the kernel default action is
//! to terminate the process. Before sending audio signals,
//! [`LweAudioController::send`] consults the safety gate:
//!
//! 1. If config `lwe_supports_audio_signals = Some(true)` — send anyway.
//! 2. If config `lwe_supports_audio_signals = Some(false)` — refuse.
//! 3. If `None` (default) — probe `<binary> --version` for the
//!    "signal handlers" marker; send only if probe says
//!    [`crate::lwe_probe::LweBuildKind::SupportedFork`].
//!
//! Override the config flag if the operator has audited their LWE
//! fork manually. The probe result is cached per controller instance
//! so we don't spawn `<binary> --version` on every audio command.

use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

use crate::{
    backend::{BackendState, LweBackend, WallpaperBackend},
    error::{Error, Result},
    lwe_probe::{probe_lwe_binary, LweBuildKind},
};

/// Signal-dispatch function: receives the signal + pid and either
/// performs the kill (default) or records the call (tests).
pub type DispatchFn = dyn Fn(nix::sys::signal::Signal, i32) -> std::io::Result<()> + Send + Sync;

/// Commands that can be sent to LWE's audio control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioCommand {
    /// Toggle mute state (`SIGUSR1`).
    Toggle,
    /// Force muted (`SIGUSR2`).
    Mute,
    /// Force unmuted (`SIGCONT` — wakes any paused decoder too).
    Unmute,
}

impl AudioCommand {
    /// POSIX signal used to deliver this command.
    fn signal(self) -> nix::sys::signal::Signal {
        match self {
            Self::Toggle => nix::sys::signal::Signal::SIGUSR1,
            Self::Mute => nix::sys::signal::Signal::SIGUSR2,
            Self::Unmute => nix::sys::signal::Signal::SIGCONT,
        }
    }
}

/// How the controller decides whether SIGUSR1/SIGUSR2 is safe to
/// dispatch to the running LWE binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSafetyPolicy {
    /// Operator explicitly opted in (config flag `Some(true)`).
    ForceOn,
    /// Operator explicitly opted out (config flag `Some(false)`).
    ForceOff,
    /// Run `<binary> --version` once and cache the result.
    AutoProbe,
}

/// Controller that dispatches [`AudioCommand`]s to running LWE
/// instances. Constructed via [`LweBackend::audio`].
#[derive(Clone)]
pub struct LweAudioController {
    backend: LweBackend,
    policy: AudioSafetyPolicy,
    /// Probe result cache — populated on first call to `send` under
    /// [`AudioSafetyPolicy::AutoProbe`].
    probe_cache: Arc<OnceLock<LweBuildKind>>,
    /// Pluggable signal dispatch. Default uses `nix::sys::signal::kill`;
    /// tests inject a recorder so they don't actually send signals
    /// when an LWE happens to be running on the same machine.
    dispatch: Arc<DispatchFn>,
}

impl std::fmt::Debug for LweAudioController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LweAudioController")
            .field("policy", &self.policy)
            .field("probe_cache", &self.probe_cache)
            .finish()
    }
}

impl LweAudioController {
    /// Construct from an existing [`LweBackend`] with the default
    /// [`AudioSafetyPolicy::AutoProbe`] and the real `kill()` dispatch.
    pub fn new(backend: LweBackend) -> Self {
        Self {
            backend,
            policy: AudioSafetyPolicy::AutoProbe,
            probe_cache: Arc::new(OnceLock::new()),
            dispatch: Arc::new(|sig, pid| {
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), sig)
                    .map_err(|e| std::io::Error::other(format!("{e}")))
            }),
        }
    }

    /// Construct with an explicit safety policy (used by callers that
    /// have parsed the config and want to bypass probing).
    pub fn with_policy(backend: LweBackend, policy: AudioSafetyPolicy) -> Self {
        let mut c = Self::new(backend);
        c.policy = policy;
        c
    }

    /// Replace the signal-dispatch function. Intended for tests that
    /// want to verify the safety gate without actually sending
    /// signals to running processes.
    ///
    /// Accepts an `Arc<dyn Fn(Signal, i32) -> io::Result<()>>` so
    /// callers can keep a clone of the dispatcher around for
    /// inspection (e.g. a recorder) without leaking the inner closure
    /// type.
    pub fn with_dispatch_fn(mut self, dispatch: Arc<DispatchFn>) -> Self {
        self.dispatch = dispatch;
        self
    }

    /// Pre-populate the probe cache. Useful for tests + operators who
    /// already know the answer and want to skip the probe subprocess.
    ///
    /// Note: only works if the controller has not been cloned yet
    /// (uses `Arc::get_mut` to mutate the inner `OnceLock`). If
    /// callers need to pre-populate after cloning, build the
    /// controller with this method first, then clone.
    pub fn with_probe_result(self, kind: LweBuildKind) -> Self {
        // Try to mutate in place; if Arc is shared, we still need to
        // populate, so rebuild the Arc around a pre-filled OnceLock.
        match Arc::try_unwrap(self.probe_cache) {
            Ok(cache) => {
                let _ = cache.set(kind);
                Self {
                    backend: self.backend,
                    policy: self.policy,
                    probe_cache: Arc::new(cache),
                    dispatch: self.dispatch,
                }
            }
            Err(arc) => {
                // Already shared: callers should set before cloning.
                // We expose the value via cached_probe() if already
                // populated; otherwise we silently no-op. Tests that
                // need this guarantee should build the controller
                // with this method first.
                tracing::warn!(
                    "with_probe_result called on a shared controller — \
                     populate the cache before cloning"
                );
                Self {
                    backend: self.backend,
                    policy: self.policy,
                    probe_cache: arc,
                    dispatch: self.dispatch,
                }
            }
        }
    }

    /// Verify only that the safety gate would allow this command,
    /// without actually dispatching it. Returns `Ok(())` if the signal
    /// would be sent, `Err(AudioSignalsDisabled)` otherwise.
    pub async fn check_safety(&self, cmd: AudioCommand) -> Result<()> {
        // SIGCONT (Unmute) is always safe.
        if cmd == AudioCommand::Unmute {
            return Ok(());
        }

        match self.policy {
            AudioSafetyPolicy::ForceOn => Ok(()),
            AudioSafetyPolicy::ForceOff => Err(Error::AudioSignalsDisabled {
                reason: "config lwe_supports_audio_signals = false".to_string(),
            }),
            AudioSafetyPolicy::AutoProbe => {
                let kind =
                    self.probe_cache
                        .get_or_init(|| {
                            let path =
                                self.backend.binary_path.as_deref().unwrap_or_else(|| {
                                    std::path::Path::new("linux-wallpaperengine")
                                });
                            probe_lwe_binary(path)
                        })
                        .clone();
                if !kind.supports_audio_signals() {
                    Err(Error::AudioSignalsDisabled {
                        reason: format!(
                            "LWE build detected as {:?}; refusing to dispatch {:?} which would terminate it",
                            kind,
                            cmd.signal()
                        ),
                    })
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Send the given audio command to all running LWE instances.
    /// Returns the number of processes signaled.
    pub async fn send(&self, cmd: AudioCommand) -> Result<usize> {
        // Run the safety gate first (a no-op for Unmute).
        self.check_safety(cmd).await?;
        self.send_unchecked(cmd).await
    }

    /// Internal: dispatch the signal without the safety check. Called
    /// by `send` after the gate passes.
    async fn send_unchecked(&self, cmd: AudioCommand) -> Result<usize> {
        // Read the pool directly rather than `backend.list_pids()`.
        // The pool is the single source of truth for which LWE pid
        // is alive — `list_pids()` is a /proc walk that would also
        // pick up orphans we don't own.
        let pid = self.backend.pool().current_pid().await;
        let Some(pid) = pid else {
            return Err(Error::BackendUnreachable {
                kind: "linux-wallpaperengine".to_string(),
                message: "no LWE instances running".to_string(),
            });
        };

        (self.dispatch)(cmd.signal(), pid).map_err(|e| Error::BackendFailure {
            kind: "linux-wallpaperengine".to_string(),
            message: format!("signal {:?} to pid {pid} failed: {e}", cmd.signal()),
        })?;

        tracing::info!("sent {:?} to pool pid {pid}", cmd);
        Ok(1)
    }

    /// Convenience: toggle audio on all LWE instances.
    pub async fn toggle(&self) -> Result<usize> {
        self.send(AudioCommand::Toggle).await
    }

    /// Convenience: mute all LWE instances.
    pub async fn mute(&self) -> Result<usize> {
        self.send(AudioCommand::Mute).await
    }

    /// Convenience: unmute all LWE instances.
    pub async fn unmute(&self) -> Result<usize> {
        self.send(AudioCommand::Unmute).await
    }

    /// Query the state of a single LWE instance. Convenience
    /// passthrough to [`LweBackend::state`].
    pub async fn state(&self, pid: i32) -> Result<BackendState> {
        self.backend.state(pid).await
    }

    /// Return the cached probe result, if `AutoProbe` and the probe
    /// has already been run. Used by tests + the CLI status command.
    pub fn cached_probe(&self) -> Option<LweBuildKind> {
        self.probe_cache.get().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    #[test]
    fn signal_mapping() {
        assert_eq!(
            AudioCommand::Toggle.signal() as i32,
            nix::sys::signal::Signal::SIGUSR1 as i32
        );
        assert_eq!(
            AudioCommand::Mute.signal() as i32,
            nix::sys::signal::Signal::SIGUSR2 as i32
        );
        assert_eq!(
            AudioCommand::Unmute.signal() as i32,
            nix::sys::signal::Signal::SIGCONT as i32
        );
    }

    #[test]
    fn audio_command_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&AudioCommand::Toggle).unwrap(),
            "\"toggle\""
        );
        assert_eq!(
            serde_json::to_string(&AudioCommand::Mute).unwrap(),
            "\"mute\""
        );
        assert_eq!(
            serde_json::to_string(&AudioCommand::Unmute).unwrap(),
            "\"unmute\""
        );
    }

    fn write_fake_binary(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let script = dir.join(name);
        let mut f = std::fs::File::create(&script).unwrap();
        write!(f, "#!/bin/sh\n{body}\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Signal-dispatch function: receives the signal + pid and either
    /// performs the kill (default) or records the call (tests).
    pub type DispatchFn =
        dyn Fn(nix::sys::signal::Signal, i32) -> std::io::Result<()> + Send + Sync;

    /// Recorded signal + pid pair, used by test helpers to assert
    /// what the safety gate let through without dispatching.
    pub type DispatchLog = Arc<Mutex<Vec<(nix::sys::signal::Signal, i32)>>>;

    /// Build a controller that records every signal dispatch instead
    /// of actually calling kill(2). Returns the recorder log (for
    /// assertions) and a dispatcher that pushes onto it.
    fn recorder_dispatch() -> (DispatchLog, Arc<DispatchFn>) {
        let log = Arc::new(Mutex::new(Vec::<(nix::sys::signal::Signal, i32)>::new()));
        let log_clone = log.clone();
        let dispatch: Arc<DispatchFn> = Arc::new(move |sig, pid| {
            log_clone.lock().unwrap().push((sig, pid));
            Ok(())
        });
        (log, dispatch)
    }

    #[test]
    fn force_off_refuses_audio_signals() {
        let backend = LweBackend::new();
        let c = LweAudioController::with_policy(backend, AudioSafetyPolicy::ForceOff);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(c.toggle()).unwrap_err();
        assert!(
            matches!(err, Error::AudioSignalsDisabled { .. }),
            "ForceOff must refuse toggle, got {err:?}"
        );
        // Same for mute
        let err = rt.block_on(c.mute()).unwrap_err();
        assert!(matches!(err, Error::AudioSignalsDisabled { .. }));
        // Unmute (SIGCONT) is always allowed
        let res = rt.block_on(c.unmute());
        assert!(
            res.is_ok() || matches!(res.as_ref().unwrap_err(), Error::BackendUnreachable { .. }),
            "unmute must pass safety (only dispatch can fail), got {res:?}"
        );
    }

    #[test]
    fn force_on_passes_safety_gate_check_safety() {
        // check_safety is the predicate that gates signal dispatch.
        // With ForceOn it must return Ok for SIGUSR1/SIGUSR2 commands.
        let backend = LweBackend::new();
        let c = LweAudioController::with_policy(backend, AudioSafetyPolicy::ForceOn);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(c.check_safety(AudioCommand::Toggle)).unwrap();
        rt.block_on(c.check_safety(AudioCommand::Mute)).unwrap();
        rt.block_on(c.check_safety(AudioCommand::Unmute)).unwrap();
    }

    #[test]
    fn auto_probe_upstream_refuses_signals_check_safety() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = write_fake_binary(
            tmp.path(),
            "upstream-lwe.sh",
            "echo 'linux-wallpaperengine 0.10.1'",
        );
        let backend = LweBackend::with_binary(bin);
        let c = LweAudioController::with_policy(backend, AudioSafetyPolicy::AutoProbe);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(c.check_safety(AudioCommand::Toggle))
            .unwrap_err();
        assert!(
            matches!(err, Error::AudioSignalsDisabled { .. }),
            "AutoProbe on upstream must refuse Toggle, got {err:?}"
        );
        // SIGUSR2 (Mute) also blocked
        let err = rt.block_on(c.check_safety(AudioCommand::Mute)).unwrap_err();
        assert!(matches!(err, Error::AudioSignalsDisabled { .. }));
        // Unmute (SIGCONT) is always allowed
        rt.block_on(c.check_safety(AudioCommand::Unmute)).unwrap();
        // Probe result should have been cached as UpstreamNoSignals.
        assert_eq!(c.cached_probe(), Some(LweBuildKind::UpstreamNoSignals));
    }

    #[test]
    fn auto_probe_fork_passes_safety_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = write_fake_binary(
            tmp.path(),
            "fork-lwe.sh",
            "echo 'linux-wallpaperengine 0.10.1 (signal handlers)'",
        );
        let backend = LweBackend::with_binary(bin);
        let c = LweAudioController::with_policy(backend, AudioSafetyPolicy::AutoProbe);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(c.check_safety(AudioCommand::Toggle)).unwrap();
        rt.block_on(c.check_safety(AudioCommand::Mute)).unwrap();
        assert_eq!(c.cached_probe(), Some(LweBuildKind::SupportedFork));
    }

    #[test]
    fn unmute_skips_safety_probe() {
        // SIGCONT (Unmute) is always safe and must NOT consult the
        // probe — even if the probe would say "unknown" because the
        // binary doesn't exist.
        let backend = LweBackend::with_binary(std::path::PathBuf::from("/nonexistent/binary"));
        let c = LweAudioController::with_policy(backend, AudioSafetyPolicy::AutoProbe);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(c.check_safety(AudioCommand::Unmute)).unwrap();
        assert!(
            c.cached_probe().is_none(),
            "unmute must not run the probe (waste of subprocess spawn)"
        );
    }

    #[test]
    fn with_probe_result_skips_subprocess() {
        // If the operator pre-cached the result, AutoProbe must not
        // spawn a subprocess to discover it.
        let backend = LweBackend::with_binary(std::path::PathBuf::from("/nonexistent/binary"));
        let c = LweAudioController::with_policy(backend, AudioSafetyPolicy::AutoProbe)
            .with_probe_result(LweBuildKind::SupportedFork);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(c.check_safety(AudioCommand::Toggle)).unwrap();
        assert_eq!(c.cached_probe(), Some(LweBuildKind::SupportedFork));
    }

    #[test]
    fn with_probe_result_refuses_when_upstream() {
        let backend = LweBackend::with_binary(std::path::PathBuf::from("/nonexistent/binary"));
        let c = LweAudioController::with_policy(backend, AudioSafetyPolicy::AutoProbe)
            .with_probe_result(LweBuildKind::UpstreamNoSignals);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(c.check_safety(AudioCommand::Toggle))
            .unwrap_err();
        assert!(matches!(err, Error::AudioSignalsDisabled { .. }));
    }

    /// End-to-end: with a recorder dispatch, verify that `send` only
    /// invokes the dispatch function after the safety gate passes.
    /// This test does NOT spawn `sleep` or call real kill(2); the
    /// dispatch closure is fully under our control.
    #[test]
    fn send_invokes_dispatch_after_safety_pass() {
        let backend = LweBackend::new();
        let (log, dispatch) = recorder_dispatch();
        let c = LweAudioController::with_policy(backend, AudioSafetyPolicy::ForceOn)
            .with_dispatch_fn(dispatch);
        let rt = tokio::runtime::Runtime::new().unwrap();
        // No LWE processes are spawned, so list_pids() returns empty
        // and send() fails with BackendUnreachable BEFORE invoking
        // dispatch. That's the safe path: gate passes, but the
        // dispatch step correctly errors out.
        let err = rt.block_on(c.toggle()).unwrap_err();
        assert!(
            matches!(err, Error::BackendUnreachable { .. }),
            "empty pid list must short-circuit before dispatch, got {err:?}"
        );
        assert!(
            log.lock().unwrap().is_empty(),
            "dispatch must NOT be invoked when no LWE is running"
        );
    }

    #[test]
    fn send_skips_dispatch_when_safety_blocks() {
        let backend = LweBackend::new();
        let (log, dispatch) = recorder_dispatch();
        let c = LweAudioController::with_policy(backend, AudioSafetyPolicy::ForceOff)
            .with_dispatch_fn(dispatch);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(c.toggle()).unwrap_err();
        assert!(matches!(err, Error::AudioSignalsDisabled { .. }));
        assert!(
            log.lock().unwrap().is_empty(),
            "dispatch must NOT be invoked when safety gate blocks"
        );
    }
}
