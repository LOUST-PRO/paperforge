//! Playlists panel — local scan via `PlaylistStore::list`.
//!
//! PR 3 only reads (no create / delete / apply). Writes are PR 5+.

use std::path::PathBuf;

use paperforge_core::error::Error as CoreError;
use paperforge_core::playlist::{Playlist, PlaylistStore};

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
}
