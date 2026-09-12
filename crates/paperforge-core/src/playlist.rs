//! Playlists per monitor — JSON files in
//! `~/.config/paperforge/playlists/<name>.json`.
//!
//! A playlist is an ordered list of wallpaper paths plus a target
//! set of Wayland outputs. Applying a playlist sets the wallpaper
//! for each output in turn.
//!
//! Most generic wallpaper switchers treat each wallpaper as a one-off
//! (path → monitor mapping only). With playlists, the operator can
//! switch the entire vibe of their desktop with one command
//! (`paperforge playlist apply focus`).

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
    /// small for the output. Standard `fill`/`fit`/`stretch`/`tile`
    /// options.
    #[serde(default = "default_fill")]
    pub fill: FillMode,
    /// Physical orientation of the monitor(s) this playlist targets.
    /// Drives the portrait-skip heuristic in the rotation scripts:
    /// when a wallpaper's detected orientation contradicts the
    /// monitor's, the rotation script advances to the next wallpaper
    /// instead of applying a mismatched scene.
    ///
    /// Default is `Landscape`, matching every modern desktop monitor.
    /// Operators with a vertical/portrait monitor should set this
    /// explicitly to `Portrait` so the heuristic inverts.
    #[serde(default = "default_monitor_orientation")]
    pub monitor_orientation: MonitorOrientation,
    /// Policy for orientations the parser could not determine
    /// (`Orientation::Unknown`). `Allow` (default) lets unparseable
    /// scenes through, assuming they're either correctly oriented or
    /// will fail visibly so the operator notices. `Skip` is opt-in
    /// strict mode for operators who want every applied scene
    /// confidently matched.
    #[serde(default = "default_orientation_fallback")]
    pub orientation_fallback: OrientationFallback,
}

fn default_fill() -> FillMode {
    FillMode::Fill
}

fn default_monitor_orientation() -> MonitorOrientation {
    MonitorOrientation::Landscape
}

fn default_orientation_fallback() -> OrientationFallback {
    OrientationFallback::Allow
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

/// Physical orientation of the monitor(s) a playlist targets.
///
/// Used by the rotation script's portrait-skip heuristic: when the
/// wallpaper's detected orientation (`Orientation::Portrait` /
/// `Landscape` / `Square`) contradicts the monitor's, the rotator
/// advances to the next wallpaper instead of applying a mismatched
/// scene that would render cropped or letterboxed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MonitorOrientation {
    /// Wide monitor (w > h). Default for desktop displays.
    Landscape,
    /// Tall monitor (h > w). For vertically-rotated displays or
    /// phones-as-monitors.
    Portrait,
    /// Either orientation is acceptable. Disables the heuristic.
    Any,
}

/// Policy applied when the orientation parser returns
/// `Orientation::Unknown` (web workshops, scene.pkg without
/// `orthogonalprojection`, ffprobe timeout, etc).
///
/// `Allow` is the default because most unparseable scenes are
/// actually well-oriented — web workshops can render fine on
/// either orientation, and a missing dimension in scene.pkg is
/// usually a publisher error rather than a portrait scene.
/// Operators who want strict matching can opt into `Skip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OrientationFallback {
    /// Include scenes whose orientation could not be determined.
    /// Default.
    Allow,
    /// Skip scenes whose orientation could not be determined.
    /// Opt-in strict mode.
    Skip,
}

/// Pure decision function used by the bash helper (and any future
/// Rust caller) to decide whether a scene with the given detected
/// `Orientation` should be applied to a monitor with the given
/// `MonitorOrientation`, honouring the `OrientationFallback`.
///
/// Truth table:
///
/// | Monitor / Scene | Landscape | Portrait | Square | Unknown (Allow) | Unknown (Skip) |
/// |-----------------|-----------|----------|--------|------------------|------------------|
/// | Landscape       | OK        | skip     | OK     | OK               | skip             |
/// | Portrait        | skip      | OK       | OK     | OK               | skip             |
/// | Any             | OK        | OK       | OK     | OK               | OK               |
///
/// `Square` is treated as compatible with both portrait and
/// landscape monitors because aspect-1:1 wallpapers (rare) usually
/// look intentional and don't suffer the same cropping artefacts
/// as 9:16 vs 16:9 mismatches.
pub fn orientation_compatible(
    monitor: MonitorOrientation,
    scene: crate::orientation::Orientation,
    fallback: OrientationFallback,
) -> bool {
    use crate::orientation::Orientation;
    match (monitor, scene) {
        (_, Orientation::Square) => true,
        (MonitorOrientation::Any, _) => true,
        (_, Orientation::Unknown) => matches!(fallback, OrientationFallback::Allow),
        (MonitorOrientation::Landscape, Orientation::Landscape) => true,
        (MonitorOrientation::Portrait, Orientation::Portrait) => true,
        _ => false,
    }
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
            monitor_orientation: MonitorOrientation::Landscape,
            orientation_fallback: OrientationFallback::Allow,
        };
        store.save(&pl).unwrap();
        let loaded = store.load("focus").unwrap();
        assert_eq!(loaded, pl);
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
                    monitor_orientation: MonitorOrientation::Landscape,
                    orientation_fallback: OrientationFallback::Allow,
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
            monitor_orientation: MonitorOrientation::Landscape,
            orientation_fallback: OrientationFallback::Allow,
        };
        let backend = LweBackend::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(store.apply(&pl, &backend));
        assert!(matches!(r, Err(Error::Config(_))));
    }

    #[test]
    fn playlist_default_orientation_is_landscape_allow() {
        // JSON without the new fields should default to
        // Landscape + Allow — backward compat for existing
        // playlists (dp-1.json, edp-1.json, hdmi-a-1.json).
        let raw = r#"{
            "name": "old",
            "outputs": ["eDP-1"],
            "wallpapers": ["/tmp/a"]
        }"#;
        let pl: Playlist = serde_json::from_str(raw).unwrap();
        assert_eq!(pl.monitor_orientation, MonitorOrientation::Landscape);
        assert_eq!(pl.orientation_fallback, OrientationFallback::Allow);
        assert_eq!(pl.fill, FillMode::Fill);
    }

    #[test]
    fn playlist_parses_explicit_orientation_fields() {
        let raw = r#"{
            "name": "v",
            "outputs": ["HDMI-A-1"],
            "wallpapers": ["/tmp/a"],
            "monitor_orientation": "portrait",
            "orientation_fallback": "skip"
        }"#;
        let pl: Playlist = serde_json::from_str(raw).unwrap();
        assert_eq!(pl.monitor_orientation, MonitorOrientation::Portrait);
        assert_eq!(pl.orientation_fallback, OrientationFallback::Skip);
    }

    #[test]
    fn playlist_roundtrip_with_orientation() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PlaylistStore::new(tmp.path()).unwrap();
        let pl = Playlist {
            name: "vert".into(),
            description: None,
            outputs: vec!["HDMI-A-1".into()],
            wallpapers: vec![PathBuf::from("/tmp/a")],
            fill: FillMode::Fill,
            monitor_orientation: MonitorOrientation::Portrait,
            orientation_fallback: OrientationFallback::Skip,
        };
        store.save(&pl).unwrap();
        let loaded = store.load("vert").unwrap();
        assert_eq!(loaded.monitor_orientation, MonitorOrientation::Portrait);
        assert_eq!(loaded.orientation_fallback, OrientationFallback::Skip);
    }

    #[test]
    fn orientation_compatible_skips_portrait_for_landscape_monitor() {
        use crate::orientation::Orientation;
        assert!(!orientation_compatible(
            MonitorOrientation::Landscape,
            Orientation::Portrait,
            OrientationFallback::Allow,
        ));
    }

    #[test]
    fn orientation_compatible_allows_landscape_for_landscape_monitor() {
        use crate::orientation::Orientation;
        assert!(orientation_compatible(
            MonitorOrientation::Landscape,
            Orientation::Landscape,
            OrientationFallback::Allow,
        ));
    }

    #[test]
    fn orientation_compatible_unknown_allowed_by_default() {
        use crate::orientation::Orientation;
        assert!(orientation_compatible(
            MonitorOrientation::Landscape,
            Orientation::Unknown,
            OrientationFallback::Allow,
        ));
    }

    #[test]
    fn orientation_compatible_unknown_skipped_when_fallback_skip() {
        use crate::orientation::Orientation;
        assert!(!orientation_compatible(
            MonitorOrientation::Landscape,
            Orientation::Unknown,
            OrientationFallback::Skip,
        ));
    }

    #[test]
    fn orientation_compatible_any_monitor_accepts_everything() {
        use crate::orientation::Orientation;
        for scene in [
            Orientation::Portrait,
            Orientation::Landscape,
            Orientation::Square,
            Orientation::Unknown,
        ] {
            assert!(
                orientation_compatible(
                    MonitorOrientation::Any,
                    scene,
                    OrientationFallback::Skip,
                ),
                "Any monitor should accept {scene:?} regardless of fallback"
            );
        }
    }

    #[test]
    fn orientation_compatible_square_compatible_with_either() {
        use crate::orientation::Orientation;
        // Square scenes don't suffer from the cropping artefact
        // that portrait/landscape mismatches cause.
        assert!(orientation_compatible(
            MonitorOrientation::Landscape,
            Orientation::Square,
            OrientationFallback::Allow,
        ));
        assert!(orientation_compatible(
            MonitorOrientation::Portrait,
            Orientation::Square,
            OrientationFallback::Allow,
        ));
    }
}
