//! Playlists per monitor — JSON files in
//! `~/.config/paperforge/playlists/<name>.json`.
//!
//! A playlist is an ordered list of wallpaper paths plus a target
//! set of Wayland outputs. Applying a playlist sets the wallpaper
//! for each output in turn.
//!
//! This is the killer feature that waypaper does NOT have. Waypaper
//! treats each wallpaper as a one-off (path → monitor mapping only).
//! With playlists, the operator can switch the entire vibe of their
//! desktop with one command (`paperforge playlist apply focus`).

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    backend::{BackendState, LweBackend, WallpaperBackend},
    error::{Error, Result},
};

/// A named, ordered collection of wallpapers plus a target set of
/// Wayland outputs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Playlist {
    /// Playlist name (must be filesystem-safe: no `/`, `..`, etc).
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// Wayland output names this playlist targets (e.g. `DP-1`,
    /// `HDMI-A-1`, `eDP-1`). Empty means "all detected outputs".
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Wallpaper paths in apply order. Cycles when shorter than
    /// outputs.
    pub wallpapers: Vec<PathBuf>,
    /// Fill mode applied across monitors when a wallpaper is too
    /// small for the output. Mirrors waypaper's fill option.
    #[serde(default = "default_fill")]
    pub fill: FillMode,
}

fn default_fill() -> FillMode {
    FillMode::Fill
}

/// How a wallpaper smaller than its output is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FillMode {
    /// Stretch to fill (may distort).
    Stretch,
    /// Cover (crop to fit, no distortion).
    Cover,
    /// Contain (letterbox, no distortion).
    Contain,
    /// Center at native size.
    Center,
    /// Tile (repeat).
    Tile,
    /// Fill (resize to fill, may crop).
    Fill,
}

/// Persists playlists to disk as one JSON file per playlist.
#[derive(Debug, Clone)]
pub struct PlaylistStore {
    /// Directory holding `<name>.json` files.
    root: PathBuf,
}

impl PlaylistStore {
    /// Construct a store rooted at the given directory. The directory
    /// is created if missing.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.exists() {
            std::fs::create_dir_all(&root)?;
        }
        Ok(Self { root })
    }

    /// Default location: `$XDG_CONFIG_HOME/paperforge/playlists/`
    /// (typically `~/.config/paperforge/playlists/`).
    pub fn default_location() -> Result<Self> {
        let base = dirs::config_dir()
            .ok_or_else(|| Error::Config("could not determine config_dir".to_string()))?
            .join("paperforge")
            .join("playlists");
        Self::new(base)
    }

    /// Path the on-disk file would live at for the given name.
    fn path_for(&self, name: &str) -> Result<PathBuf> {
        if name.contains('/') || name.contains("..") || name.is_empty() {
            return Err(Error::Config(format!("invalid playlist name: {name:?}")));
        }
        Ok(self.root.join(format!("{name}.json")))
    }

    /// Save a playlist, overwriting any existing file with the same name.
    pub fn save(&self, playlist: &Playlist) -> Result<()> {
        let path = self.path_for(&playlist.name)?;
        let json = serde_json::to_string_pretty(playlist)
            .map_err(|e| Error::Config(format!("serialize: {e}")))?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Load a playlist by name.
    pub fn load(&self, name: &str) -> Result<Playlist> {
        let path = self.path_for(name)?;
        if !path.exists() {
            return Err(Error::PlaylistNotFound {
                name: name.to_string(),
                store: self.root.display().to_string(),
            });
        }
        let text = std::fs::read_to_string(&path)?;
        let pl: Playlist = serde_json::from_str(&text)
            .map_err(|e| Error::Config(format!("parse {}: {e}", path.display())))?;
        Ok(pl)
    }

    /// List all playlist names (sorted).
    pub fn list(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        if !self.root.exists() {
            return Ok(names);
        }
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// Delete a playlist by name. Returns `true` if a file was removed.
    pub fn delete(&self, name: &str) -> Result<bool> {
        let path = self.path_for(name)?;
        if !path.exists() {
            return Ok(false);
        }
        std::fs::remove_file(&path)?;
        Ok(true)
    }

    /// Apply a playlist: launch LWE instances for each wallpaper,
    /// pinned to the playlist's outputs (cycling when there are more
    /// outputs than wallpapers).
    ///
    /// Returns a per-output summary of which wallpaper was applied.
    pub async fn apply(
        &self,
        playlist: &Playlist,
        backend: &LweBackend,
    ) -> Result<BTreeMap<String, PathBuf>> {
        if playlist.wallpapers.is_empty() {
            return Err(Error::Config(format!(
                "playlist '{}' has no wallpapers",
                playlist.name
            )));
        }

        let outputs: Vec<String> = if playlist.outputs.is_empty() {
            // Empty outputs = all detected. Caller should resolve this
            // before calling if they want explicit outputs; here we
            // bail with a clear error rather than guessing.
            return Err(Error::Config(
                "playlist has empty outputs — provide explicit outputs or use `apply --all`"
                    .to_string(),
            ));
        } else {
            playlist.outputs.clone()
        };

        let mut applied: BTreeMap<String, PathBuf> = BTreeMap::new();
        for (i, output) in outputs.iter().enumerate() {
            let scene = &playlist.wallpapers[i % playlist.wallpapers.len()];
            backend.set(scene, Some(output)).await?;
            applied.insert(output.clone(), scene.clone());
        }
        Ok(applied)
    }

    /// Report the runtime state of every LWE PID (running/paused).
    /// Useful for `paperforge playlist status` and for the TUI.
    pub async fn lwe_status(backend: &LweBackend) -> Result<BTreeMap<i32, BackendState>> {
        let pids = backend.list_pids().await?;
        let mut out = BTreeMap::new();
        for pid in pids {
            let s = backend.state(pid).await?;
            out.insert(pid, s);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PlaylistStore::new(tmp.path()).unwrap();
        let pl = Playlist {
            name: "focus".into(),
            description: Some("focus mode".into()),
            outputs: vec!["DP-1".into()],
            wallpapers: vec![PathBuf::from("/tmp/wp1"), PathBuf::from("/tmp/wp2")],
            fill: FillMode::Cover,
        };
        store.save(&pl).unwrap();
        let loaded = store.load("focus").unwrap();
        assert_eq!(loaded, pl);
    }

    #[test]
    fn list_empty_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PlaylistStore::new(tmp.path()).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn list_returns_sorted_names() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PlaylistStore::new(tmp.path()).unwrap();
        for name in ["zeta", "alpha", "mu"] {
            store
                .save(&Playlist {
                    name: name.into(),
                    description: None,
                    outputs: vec![],
                    wallpapers: vec![PathBuf::from("/x")],
                    fill: FillMode::Fill,
                })
                .unwrap();
        }
        assert_eq!(store.list().unwrap(), vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn rejects_unsafe_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PlaylistStore::new(tmp.path()).unwrap();
        assert!(store.path_for("../escape").is_err());
        assert!(store.path_for("").is_err());
        assert!(store.path_for("with/slash").is_err());
    }

    #[test]
    fn delete_missing_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PlaylistStore::new(tmp.path()).unwrap();
        assert!(!store.delete("nope").unwrap());
    }

    #[test]
    fn apply_empty_wallpapers_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PlaylistStore::new(tmp.path()).unwrap();
        let pl = Playlist {
            name: "x".into(),
            description: None,
            outputs: vec!["DP-1".into()],
            wallpapers: vec![],
            fill: FillMode::Fill,
        };
        let backend = LweBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(store.apply(&pl, &backend));
        assert!(matches!(r, Err(Error::Config(_))));
    }
}
