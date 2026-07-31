//! Async data fetchers for the TUI.
//!
//! Each panel has a corresponding fetch function that pulls
//! state from `paperforge-core` and returns a snapshot. The TUI
//! loop refreshes panels on independent timers; failures are
//! surfaced as `DataError { source, message }` so the UI can show
//! them inline without crashing.
//!
//! All disk-touching work runs inside `tokio::task::spawn_blocking`
//! so the event loop is never blocked on slow filesystems.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use paperforge_core::{
    backend::{BackendState, LweBackend, WallpaperBackend},
    error::{Error, Result},
    hotplug::{CompositorHotplugSource, HotplugSource, Output},
    inventory::{Inventory, WallpaperEntry, WallpaperKind},
    playlist::{Playlist, PlaylistStore},
};

use crate::app::DataError;

/// Wallpaper runtime status as the TUI sees it.
#[derive(Debug, Clone)]
pub struct RunningInstance {
    /// PID of the LWE process.
    pub pid: i32,
    /// Live state read from `/proc/<pid>/status`.
    pub state: BackendState,
}

/// Snapshot of playlist list (names + on-disk file count).
#[derive(Debug, Clone)]
pub struct PlaylistSummary {
    /// Playlist name (filename stem).
    pub name: String,
    /// Number of wallpapers in this playlist.
    pub wallpapers: usize,
    /// Number of outputs the playlist targets.
    pub outputs: usize,
}

/// Aggregated data for a single TUI render.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Wayland outputs (refreshed every 2s).
    pub outputs: Vec<Output>,
    /// PIDs of running LWE instances + their state (every 5s).
    pub running: Vec<RunningInstance>,
    /// Playlist summaries (every 10s).
    pub playlists: Vec<PlaylistSummary>,
    /// Wallpaper inventory (every 30s).
    pub inventory: Vec<WallpaperEntry>,
    /// Errors from the last fetch, kept to surface in the UI.
    pub errors: Vec<DataError>,
}

impl Snapshot {
    /// Construct an empty snapshot (initial state on `App::new`).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Total inventory entries.
    pub fn inventory_count(&self) -> usize {
        self.inventory.len()
    }

    /// Number of LWE-compatible entries (Workshop scenes + loose videos).
    pub fn lwe_compatible_count(&self) -> usize {
        self.inventory
            .iter()
            .filter(|e| e.kind.lwe_compatible())
            .count()
    }
}

/// Fetch the wallpaper inventory across multiple source roots.
/// Walks each root at depth 4 (covers `~/<source>/<author>/<item>/project.json`).
async fn fetch_inventory(roots: Vec<PathBuf>) -> Result<Vec<WallpaperEntry>> {
    tokio::task::spawn_blocking(move || {
        let mut inventory = Inventory::new();
        for root in &roots {
            if root.exists() {
                let _ = inventory.scan(root, 4);
            }
        }
        Ok::<_, Error>(inventory.entries().cloned().collect())
    })
    .await
    .map_err(|e| Error::Other(anyhow::anyhow!("spawn_blocking: {e}")))?
}

/// Fetch playlist summaries from `store_root`. Returns sorted-by-name listings.
async fn fetch_playlists(store_root: PathBuf) -> Result<Vec<PlaylistSummary>> {
    tokio::task::spawn_blocking(move || -> Result<Vec<PlaylistSummary>> {
        let store = PlaylistStore::new(&store_root)?;
        let names = store.list()?;
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            // If a playlist file is malformed, surface a placeholder
            // rather than aborting the whole fetch — TUI should
            // show "broken playlist" inline.
            let pl: Playlist = store.load(&name).unwrap_or_else(|_| Playlist {
                name: name.clone(),
                description: None,
                outputs: Vec::new(),
                wallpapers: Vec::new(),
                fill: paperforge_core::playlist::FillMode::Fill,
            });
            out.push(PlaylistSummary {
                name: pl.name,
                wallpapers: pl.wallpapers.len(),
                outputs: pl.outputs.len(),
            });
        }
        Ok(out)
    })
    .await
    .map_err(|e| Error::Other(anyhow::anyhow!("spawn_blocking: {e}")))?
}

/// Fetch running LWE PIDs + their states.
async fn fetch_running(backend: Arc<LweBackend>) -> Result<Vec<RunningInstance>> {
    let pids = backend.list_pids().await?;
    let mut out = Vec::with_capacity(pids.len());
    for pid in pids {
        let s = backend.state(pid).await?;
        out.push(RunningInstance { pid, state: s });
    }
    Ok(out)
}

/// Fetch Wayland outputs from the compositor.
async fn fetch_outputs(src: Arc<CompositorHotplugSource>) -> Result<Vec<Output>> {
    src.list_outputs().await
}

/// Convenience composer: refresh only the running panel.
pub async fn refresh_running(
    backend: Arc<LweBackend>,
) -> (Vec<RunningInstance>, Option<DataError>) {
    match fetch_running(backend).await {
        Ok(v) => (v, None),
        Err(e) => (Vec::new(), Some(DataError::new("running", e))),
    }
}

/// Convenience composer: refresh only the outputs panel.
pub async fn refresh_outputs(
    src: Arc<CompositorHotplugSource>,
) -> (Vec<Output>, Option<DataError>) {
    match fetch_outputs(src).await {
        Ok(v) => (v, None),
        Err(e) => (Vec::new(), Some(DataError::new("outputs", e))),
    }
}

/// Convenience composer: refresh only the playlists panel.
pub async fn refresh_playlists(store_root: PathBuf) -> (Vec<PlaylistSummary>, Option<DataError>) {
    match fetch_playlists(store_root).await {
        Ok(v) => (v, None),
        Err(e) => (Vec::new(), Some(DataError::new("playlists", e))),
    }
}

/// Convenience composer: refresh only the inventory panel.
pub async fn refresh_inventory(roots: Vec<PathBuf>) -> (Vec<WallpaperEntry>, Option<DataError>) {
    match fetch_inventory(roots).await {
        Ok(v) => (v, None),
        Err(e) => (Vec::new(), Some(DataError::new("inventory", e))),
    }
}

/// Display a single inventory entry as a compact summary string.
pub fn format_entry_path(path: &Path, max_len: usize) -> String {
    let s = path.to_string_lossy();
    if s.chars().count() <= max_len {
        s.into_owned()
    } else {
        // Keep the tail (filename is what matters when paths are
        // long). Reserve 1 char for the ellipsis.
        let keep = max_len.saturating_sub(1);
        let tail: String = s
            .chars()
            .rev()
            .take(keep)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!("…{tail}")
    }
}

/// Format human-readable size (binary).
pub fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Compute file size (for loose media) or sum of dir (for Workshop scenes).
pub fn entry_size_on_disk(path: &Path, kind: WallpaperKind) -> u64 {
    match kind {
        WallpaperKind::LooseImage | WallpaperKind::LooseVideo => {
            std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
        }
        WallpaperKind::WorkshopScene => walkdir::WalkDir::new(path)
            .into_iter()
            .filter_map(|r: std::result::Result<walkdir::DirEntry, walkdir::Error>| r.ok())
            // Sum file bytes only — directories add their inode size
            // (typically 80+ bytes on ext4), which would inflate the
            // displayed total and confuse the user.
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_binary_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1024 * 1024), "1.0 MiB");
        assert_eq!(format_size(1024u64.pow(3)), "1.0 GiB");
    }

    #[test]
    fn format_entry_path_truncates_with_ellipsis() {
        let long = PathBuf::from("/very/very/very/long/path/to/wallpaper/scene/file.mp4");
        let formatted = format_entry_path(&long, 20);
        assert!(formatted.starts_with('…'));
        assert!(formatted.chars().count() <= 20);
    }

    #[test]
    fn format_entry_path_keeps_short_paths() {
        let short = PathBuf::from("/short");
        assert_eq!(format_entry_path(&short, 20), "/short");
    }

    #[test]
    fn format_entry_path_handles_unicode_lengths() {
        let unicode_path = PathBuf::from("/path/with/☃snowman/file.mp4");
        let formatted = format_entry_path(&unicode_path, 15);
        // Char-count based, not byte-count based — must not panic
        // on multi-byte chars mid-path.
        assert!(formatted.chars().count() <= 15);
    }

    #[test]
    fn snapshot_empty_inventory_count_is_zero() {
        let s = Snapshot::empty();
        assert_eq!(s.inventory_count(), 0);
        assert_eq!(s.lwe_compatible_count(), 0);
    }

    #[tokio::test]
    async fn refresh_inventory_on_nonexistent_root_returns_empty() {
        let roots = vec![PathBuf::from("/does/not/exist/12345")];
        let (v, err) = refresh_inventory(roots).await;
        assert!(v.is_empty());
        assert!(
            err.is_none(),
            "missing root is silently skipped, not an error"
        );
    }

    #[test]
    fn entry_size_on_disk_for_loose_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.txt");
        std::fs::write(&p, b"hello world").unwrap();
        assert_eq!(entry_size_on_disk(&p, WallpaperKind::LooseImage), 11);
    }

    #[test]
    fn entry_size_on_disk_for_workshop_dir_sums_contents() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("project.json"), b"{}").unwrap();
        std::fs::write(tmp.path().join("preview.jpg"), vec![0u8; 100]).unwrap();
        let total = entry_size_on_disk(tmp.path(), WallpaperKind::WorkshopScene);
        assert_eq!(total, 102);
    }

    #[test]
    fn entry_size_on_disk_for_missing_loose_file_is_zero() {
        let p = PathBuf::from("/no/such/file.mp4");
        assert_eq!(entry_size_on_disk(&p, WallpaperKind::LooseVideo), 0);
    }
}
