//! Playlists panel — local scan via `PlaylistStore::list`.
//!
//! PR 3 only reads (no create / delete / apply). Writes are PR 5+.

use std::path::PathBuf;

use paperforge_core::error::Error as CoreError;
use paperforge_core::playlist::{Playlist, PlaylistStore};

use crate::error::GuiError;
use crate::ipc::client::IpcClient;

/// Lightweight summary used by the GUI's playlists sidebar. The full
/// `Playlist` is loaded on demand by the editor (PR 7).
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // consumed by ui/playlists.rs in PR 3+
pub struct PlaylistSummary {
    /// Stored playlist name (filename without `.toml`).
    pub name: String,
    /// Number of wallpaper entries in the playlist.
    pub wallpapers: usize,
    /// Number of outputs the playlist targets.
    pub outputs: usize,
    /// Optional description from the playlist header.
    pub description: Option<String>,
}

/// Fetch playlist summaries from the default store location.
///
/// Returns `(summaries, error)`:
/// - On success: `(Vec<PlaylistSummary>, None)`.
/// - On failure: `(Vec::new(), Some(GuiError))` — caller keeps the
///   previous snapshot.
///
/// Per-file load errors are swallowed: a malformed playlist file is
/// reported as a placeholder summary (zero entries) rather than
/// hiding the rest. The full error is surfaced via `tracing::warn!`.
#[allow(dead_code)] // consumed by ui/root.rs in PR 3 coroutine
pub async fn refresh_playlists(store_root: PathBuf) -> (Vec<PlaylistSummary>, Option<CoreError>) {
    let join = tokio::task::spawn_blocking(move || -> std::result::Result<Vec<PlaylistSummary>, CoreError> {
        let store = PlaylistStore::new(&store_root)?;
        let names = store.list()?;
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let pl: Playlist = store.load(&name).unwrap_or_else(|e| {
                tracing::warn!(playlist = %name, error = %e, "broken playlist, showing placeholder");
                Playlist {
                    name: name.clone(),
                    description: Some(format!("(broken: {e})")),
                    outputs: Vec::new(),
                    wallpapers: Vec::new(),
                    fill: paperforge_core::playlist::FillMode::Fill,
                }
            });
            out.push(PlaylistSummary {
                name: pl.name,
                wallpapers: pl.wallpapers.len(),
                outputs: pl.outputs.len(),
                description: pl.description,
            });
        }
        Ok(out)
    })
    .await;

    let result: std::result::Result<Vec<PlaylistSummary>, CoreError> = match join {
        Err(join_err) => Err(CoreError::Other(anyhow::anyhow!(
            "spawn_blocking (playlists): {join_err}"
        ))),
        Ok(inner) => inner,
    };

    match result {
        Ok(v) => (v, None),
        Err(e) => (Vec::new(), Some(e)),
    }
}

/// Call `ApplyPlaylist(name)` on the daemon (PR 5/C).
///
/// Thin wrapper around [`IpcClient::apply_playlist`]. Mirrors
/// `unset_binding`: the data layer owns the IPC verb, the UI just
/// hands an `&IpcClient` and a playlist name.
///
/// The daemon iterates the stored playlist and binds each entry to
/// its declared outputs, then emits one `WallpaperStarted` per
/// bind. The IPC coroutine consumes those signals and rebuilds the
/// `bindings` signal — no optimistic local update needed.
///
/// Errors propagate verbatim; banner shows `kind = "apply_playlist"`.
#[allow(dead_code)] // consumed by ui/playlists.rs in PR 5/D
pub async fn apply_playlist(client: &IpcClient, name: &str) -> Result<(), GuiError> {
    client.apply_playlist(name).await
}

/// Persist a `Playlist` to the on-disk store (PR 7/A).
///
/// Wraps [`PlaylistStore::save`] in `spawn_blocking` so the file
/// write doesn't stall the Dioxus runtime. The operator edits the
/// playlist in the editor modal, hits Save, and the resulting
/// `Playlist` lands here as-is. No D-Bus involved — `save_playlist`
/// is purely a local-filesystem write; the next `apply_playlist`
/// call is what reaches the daemon.
///
/// Errors propagate verbatim; banner shows `kind = "save_playlist"`.
/// On success the next poll tick (10s) re-reads the store and
/// picks up the new entry count / description.
#[allow(dead_code)] // consumed by ui/playlist_editor.rs in PR 7/A
pub async fn save_playlist(store_root: PathBuf, playlist: Playlist) -> Result<(), GuiError> {
    tokio::task::spawn_blocking(move || -> std::result::Result<(), CoreError> {
        let store = PlaylistStore::new(&store_root)?;
        store.save(&playlist)?;
        Ok(())
    })
    .await
    .map_err(|join_err| GuiError::Core(format!("spawn_blocking (save_playlist): {join_err}")))?
    .map_err(GuiError::from_core)
}

#[cfg(test)]
mod tests {
    use super::*;
    use paperforge_core::playlist::PlaylistStore;

    #[tokio::test]
    async fn refresh_playlists_on_empty_dir_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let (v, err) = refresh_playlists(tmp.path().to_path_buf()).await;
        assert!(err.is_none());
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn refresh_playlists_summarises_existing_playlist() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PlaylistStore::new(tmp.path()).unwrap();
        let pl = Playlist {
            name: "demo".into(),
            description: Some("test playlist".into()),
            outputs: vec!["HDMI-A-1".into(), "DP-1".into()],
            wallpapers: vec!["wp1".into(), "wp2".into(), "wp3".into()],
            fill: paperforge_core::playlist::FillMode::Fill,
        };
        store.save(&pl).unwrap();

        let (v, err) = refresh_playlists(tmp.path().to_path_buf()).await;
        assert!(err.is_none(), "unexpected error: {err:?}");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "demo");
        assert_eq!(v[0].wallpapers, 3);
        assert_eq!(v[0].outputs, 2);
        assert_eq!(v[0].description.as_deref(), Some("test playlist"));
    }

    // See `tests/integration_playlists.rs` for the
    // `save_playlist_overwrites_and_round_trips` integration test
    // — it needs a `tempfile` fixture and exercises the full
    // store round-trip, which fits the integration slot better
    // than a unit test next to the helper.
}
