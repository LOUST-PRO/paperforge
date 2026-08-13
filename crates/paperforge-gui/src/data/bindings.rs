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

use crate::error::GuiError;
use crate::ipc::client::IpcClient;

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

/// Call `UnsetWallpaper(output)` on the daemon (PR 5/C).
///
/// Thin wrapper around [`IpcClient::unset_wallpaper`]. Lives here
/// (not directly in `ui/bindings.rs`) so the data layer owns the
/// IPC verb, mirroring how `refresh_playlists` / `refresh_inventory`
/// own their read paths. The UI just hands an `&IpcClient` and an
/// output name.
///
/// On success the daemon emits `WallpaperStopped`, which the IPC
/// coroutine in `ui::root` consumes to drop the row from the
/// `bindings` signal. No optimistic local delete — the signal is
/// the source of truth.
///
/// Errors propagate verbatim to the caller, which surfaces them in
/// the banner with `kind = "unset_wallpaper"`.
#[allow(dead_code)] // consumed by ui/bindings.rs in PR 5/D
pub async fn unset_binding(client: &IpcClient, output: &str) -> Result<(), GuiError> {
    client.unset_wallpaper(output).await
}

/// Call `SetWallpaper(output, scene_path)` on the daemon (PR 6).
///
/// Same shape as [`unset_binding`]: thin wrapper around the PR 5/B
/// `IpcClient::set_wallpaper`. The Picker component hands the
/// selected `WallpaperEntry::path` as `scene_path` — for Workshop
/// scenes that's the directory, for loose media it's the file.
///
/// On success the daemon emits `WallpaperStarted`, which the IPC
/// coroutine consumes to insert the new row. No optimistic local
/// insert — the signal is the source of truth.
#[allow(dead_code)] // consumed by ui/picker.rs in PR 6
pub async fn set_binding(
    client: &IpcClient,
    output: &str,
    scene_path: &std::path::Path,
) -> Result<(), GuiError> {
    client
        .set_wallpaper(output, &scene_path.to_string_lossy())
        .await
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
