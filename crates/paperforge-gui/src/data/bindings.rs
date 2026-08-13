//! Bindings panel — placeholder for PR 4.
//!
//! In PR 3 we render the panel with a "Waiting for daemon..." state
//! because bindings are populated from the `WallpaperStarted` /
//! `WallpaperStopped` D-Bus signals (`paperforge-core/src/dbus.rs`).
//! Wiring the IPC client + signal subscription is PR 4 work.
//!
//! The shape returned here is the final one PR 4 will use, so the UI
//! can compile against it now and stay stable across the PR.

use std::path::PathBuf;

/// One row in the bindings grid: a Wayland output mapped to the
/// scene path that's currently being rendered on it.
///
/// `pid` is optional because `list_running` returns `(pid, state)`
/// pairs but the daemon's `WallpaperStarted` signal also carries
/// the scene path; the GUI tracks pid↔output mapping to clean up
/// on `WallpaperStopped`.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // consumed by ui/bindings.rs in PR 3+
pub struct Binding {
    /// Output name (e.g. "HDMI-A-1").
    pub output: String,
    /// Absolute scene path the daemon is rendering.
    pub scene_path: PathBuf,
    /// PID of the running LWE instance, if known.
    pub pid: Option<i32>,
}

/// Empty placeholder for PR 3. PR 4 returns the live map populated
/// from D-Bus signals.
#[allow(dead_code)] // consumed by ui/bindings.rs in PR 3+
pub async fn refresh_bindings() -> Vec<Binding> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refresh_bindings_returns_empty_until_pr4() {
        let v = refresh_bindings().await;
        assert!(v.is_empty());
    }
}
