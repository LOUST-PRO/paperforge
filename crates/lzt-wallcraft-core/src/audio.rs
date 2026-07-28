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
//! locating LWE PIDs through the same `pgrep` mechanism used by
//! [`crate::backend::LweBackend::list_pids`].
//!
//! **Caveat**: SIGUSR1/SIGUSR2 handling in LWE is not universally
//! present across builds. If the signal is unhandled, it terminates
//! the process (default action). Test on the operator's fork before
//! relying on this in production.

use serde::{Deserialize, Serialize};

use crate::{
    backend::{BackendState, LweBackend, WallpaperBackend},
    error::{Error, Result},
};

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

/// Controller that dispatches [`AudioCommand`]s to running LWE
/// instances. Constructed via [`LweBackend::audio`].
#[derive(Debug, Clone)]
pub struct LweAudioController {
    backend: LweBackend,
}

impl LweAudioController {
    /// Construct from an existing [`LweBackend`].
    pub fn new(backend: LweBackend) -> Self {
        Self { backend }
    }

    /// Send the given audio command to all running LWE instances.
    /// Returns the number of processes signaled.
    pub async fn send(&self, cmd: AudioCommand) -> Result<usize> {
        let pids = self.backend.list_pids().await?;
        if pids.is_empty() {
            return Err(Error::BackendUnreachable {
                kind: "linux-wallpaperengine".to_string(),
                message: "no LWE instances running".to_string(),
            });
        }

        for pid in &pids {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(*pid),
                cmd.signal(),
            )
            .map_err(|e| Error::BackendFailure {
                kind: "linux-wallpaperengine".to_string(),
                message: format!("signal {:?} to pid {pid} failed: {e}", cmd.signal()),
            })?;
        }

        tracing::info!("sent {:?} to {} LWE pid(s)", cmd, pids.len());
        Ok(pids.len())
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
