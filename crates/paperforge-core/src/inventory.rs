//! Wallpaper inventory — scan directories, detect wallpaper types,
//! cache mtime for fast incremental rescans.
//!
//! A "wallpaper" is any directory under a configured source root that
//! contains a `project.json` (Wallpaper Engine convention) or any
//! recognized media file (image / video).
//!
//! The scanner is intentionally filesystem-only — no Steam Workshop
//! scraping. The operator installs Wallpaper Engine Workshop items
//! locally (or symlinks them) and paperforge picks them up.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::error::{Error, Result};

/// The kind of wallpaper source: a Steam Workshop item folder, a
/// loose media file, or a local directory of loose media files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WallpaperKind {
    /// Wallpaper Engine Workshop item (`project.json` present).
    WorkshopScene,
    /// A loose image file (jpg, png, webp, gif).
    LooseImage,
    /// A loose video file (mp4, webm, mkv, mov).
    LooseVideo,
}

impl WallpaperKind {
    /// Classify by file extension.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "tiff" => Some(Self::LooseImage),
            "mp4" | "webm" | "mkv" | "mov" | "avi" => Some(Self::LooseVideo),
            _ => None,
        }
    }

    /// True if this kind can be played by `linux-wallpaperengine`.
    pub fn lwe_compatible(self) -> bool {
        // LWE plays video scenes natively; loose images need a wrapping
        // project.json (out of scope for 6A); loose videos work if the
        // path is passed to LWE which auto-detects format.
        matches!(self, Self::WorkshopScene | Self::LooseVideo)
    }
}

/// One wallpaper entry discovered by [`Inventory::scan`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct WallpaperEntry {
    /// Absolute path to the wallpaper (directory for Workshop scenes,
    /// file for loose media).
    pub path: PathBuf,
    /// Last-modified timestamp (from `std::fs::Metadata::modified`).
    pub mtime: SystemTime,
    /// What kind of wallpaper this is.
    pub kind: WallpaperKind,
    /// Optional display title (from `project.json` `title` field, or
    /// `None` for loose media).
    pub title: Option<String>,
    /// Optional Steam-published-file id for Workshop items.
    pub workshop_id: Option<String>,
}

impl WallpaperEntry {
    /// Path to use when handing the wallpaper to a backend. For
    /// Workshop scenes this is the parent directory; for loose media
    /// the file itself.
    pub fn backend_path(&self) -> &Path {
        &self.path
    }
}

/// In-memory inventory of wallpapers under one or more source roots.
///
/// The inventory does not persist itself — callers serialize the
/// entries via [`Inventory::entries`] and store the result in their
/// preferred format (e.g. JSON cache in `~/.cache/paperforge/`).
#[derive(Debug, Default, Clone)]
pub struct Inventory {
    /// All discovered entries, keyed by absolute path to avoid
    /// duplicates across overlapping source roots.
    entries: BTreeMap<PathBuf, WallpaperEntry>,
}

impl Inventory {
    /// Empty inventory.
    pub fn new() -> Self {
        Self::default()
    }

    /// All entries, sorted by absolute path.
    pub fn entries(&self) -> impl Iterator<Item = &WallpaperEntry> {
        self.entries.values()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if the inventory is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Scan a single source root and merge results into the inventory.
    ///
    /// Walks up to `max_depth` levels (default `8` — Workshop folders
    /// rarely nest that deep). Workshop scenes are detected by the
    /// presence of `project.json`; loose media is detected by file
    /// extension.
    pub fn scan(&mut self, root: &Path, max_depth: usize) -> Result<usize> {
        if !root.exists() {
            tracing::debug!("skip non-existent root: {}", root.display());
            return Ok(0);
        }

        let mut added = 0usize;

        for entry in WalkDir::new(root)
            .max_depth(max_depth)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            let file_name = match entry.file_name().to_str() {
                Some(n) => n,
                None => continue,
            };

            // Workshop scene: directory containing `project.json`.
            if entry.file_type().is_dir() {
                let project_json = path.join("project.json");
                if project_json.is_file() {
                    if let Ok(wp) = self.read_workshop_scene(&project_json, path) {
                        if self.entries.insert(wp.path.clone(), wp).is_none() {
                            added += 1;
                        }
                    }
                }
                continue;
            }

            // Loose media file.
            if entry.file_type().is_file() {
                let ext = match Path::new(file_name).extension().and_then(|e| e.to_str()) {
                    Some(e) => e,
                    None => continue,
                };
                if let Some(kind) = WallpaperKind::from_extension(ext) {
                    let mtime = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(SystemTime::UNIX_EPOCH);

                    let wp = WallpaperEntry {
                        path: path.to_path_buf(),
                        mtime,
                        kind,
                        title: None,
                        workshop_id: None,
                    };
                    if self.entries.insert(wp.path.clone(), wp).is_none() {
                        added += 1;
                    }
                }
            }
        }

        Ok(added)
    }

    /// Read a `project.json` and produce a [`WallpaperEntry`].
    fn read_workshop_scene(&self, project_json: &Path, scene_dir: &Path) -> Result<WallpaperEntry> {
        let text = std::fs::read_to_string(project_json).map_err(|e| Error::ProjectJson {
            path: project_json.display().to_string(),
            message: e.to_string(),
        })?;

        // Wallpaper Engine's project.json is a JSON5-ish format but
        // the fields we need (`title`, `id`, `type`) are valid JSON.
        // We do a permissive parse — if any field is missing we still
        // produce an entry, just with `None` for the optional fields.
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| Error::ProjectJson {
            path: project_json.display().to_string(),
            message: e.to_string(),
        })?;

        let title = v.get("title").and_then(|t| t.as_str()).map(String::from);
        let workshop_id = v.get("id").and_then(|t| t.as_str()).map(String::from);

        // We treat type=="scene" or type=="wallpaper" both as
        // WorkshopScene — the actual playable distinction is encoded
        // in `properties.general.file` (out of scope for 6A).
        let kind = match v.get("type").and_then(|t| t.as_str()) {
            Some("scene") | Some("wallpaper") | Some("video") | None => {
                WallpaperKind::WorkshopScene
            }
            Some(other) => {
                tracing::debug!(
                    "scene {} has unrecognized type '{}', treating as WorkshopScene",
                    scene_dir.display(),
                    other
                );
                WallpaperKind::WorkshopScene
            }
        };

        let mtime = std::fs::metadata(scene_dir)
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        Ok(WallpaperEntry {
            path: scene_dir.to_path_buf(),
            mtime,
            kind,
            title,
            workshop_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_classification() {
        assert_eq!(
            WallpaperKind::from_extension("mp4"),
            Some(WallpaperKind::LooseVideo)
        );
        assert_eq!(
            WallpaperKind::from_extension("JPG"),
            Some(WallpaperKind::LooseImage)
        );
        assert_eq!(WallpaperKind::from_extension("xyz"), None);
        assert_eq!(WallpaperKind::from_extension(""), None);
    }

    #[test]
    fn lwe_compatible_only_for_video_workshop() {
        assert!(WallpaperKind::WorkshopScene.lwe_compatible());
        assert!(WallpaperKind::LooseVideo.lwe_compatible());
        assert!(!WallpaperKind::LooseImage.lwe_compatible());
    }

    #[test]
    fn scan_empty_root_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let mut inv = Inventory::new();
        let n = inv.scan(tmp.path(), 4).unwrap();
        assert_eq!(n, 0);
        assert!(inv.is_empty());
    }

    #[test]
    fn scan_nonexistent_root_returns_zero() {
        let mut inv = Inventory::new();
        let n = inv
            .scan(Path::new("/nonexistent/paperforge-test"), 4)
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn scan_finds_workshop_scene_with_project_json() {
        let tmp = tempfile::tempdir().unwrap();
        let scene = tmp.path().join("123456");
        std::fs::create_dir_all(&scene).unwrap();
        std::fs::write(
            scene.join("project.json"),
            r#"{"title":"Test Scene","type":"scene","id":"123456"}"#,
        )
        .unwrap();

        let mut inv = Inventory::new();
        let n = inv.scan(tmp.path(), 4).unwrap();
        assert_eq!(n, 1);
        let entries: Vec<_> = inv.entries().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title.as_deref(), Some("Test Scene"));
        assert_eq!(entries[0].workshop_id.as_deref(), Some("123456"));
        assert_eq!(entries[0].kind, WallpaperKind::WorkshopScene);
    }

    #[test]
    fn scan_finds_loose_video() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("cool.mp4"), b"fake-mp4").unwrap();
        let mut inv = Inventory::new();
        let n = inv.scan(tmp.path(), 4).unwrap();
        assert_eq!(n, 1);
        let e = inv.entries().next().unwrap();
        assert_eq!(e.kind, WallpaperKind::LooseVideo);
        assert!(e.title.is_none());
    }

    #[test]
    fn scan_dedupes_overlapping_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let scene = tmp.path().join("abc");
        std::fs::create_dir_all(&scene).unwrap();
        std::fs::write(scene.join("project.json"), r#"{"title":"x"}"#).unwrap();

        let mut inv = Inventory::new();
        inv.scan(tmp.path(), 4).unwrap();
        inv.scan(tmp.path(), 4).unwrap();
        assert_eq!(inv.len(), 1);
    }

    #[test]
    fn scan_recognises_video_typed_projects_as_workshop_scene() {
        let tmp = tempfile::tempdir().unwrap();
        let scene = tmp.path().join("video123");
        std::fs::create_dir_all(&scene).unwrap();
        std::fs::write(
            scene.join("project.json"),
            r#"{"title":"Video wallpaper","type":"video","id":"video123"}"#,
        )
        .unwrap();

        let mut inv = Inventory::new();
        let n = inv.scan(tmp.path(), 4).unwrap();
        assert_eq!(n, 1);
        let entry = inv.entries().next().unwrap();
        assert_eq!(entry.kind, WallpaperKind::WorkshopScene);
        assert_eq!(entry.title.as_deref(), Some("Video wallpaper"));
    }

    #[test]
    fn scan_recognises_unknown_type_as_workshop_scene() {
        let tmp = tempfile::tempdir().unwrap();
        let scene = tmp.path().join("mystery");
        std::fs::create_dir_all(&scene).unwrap();
        std::fs::write(
            scene.join("project.json"),
            r#"{"title":"Mystery","type":"some-new-type-from-steam"}"#,
        )
        .unwrap();

        let mut inv = Inventory::new();
        inv.scan(tmp.path(), 4).unwrap();
        let entry = inv.entries().next().unwrap();
        assert_eq!(entry.kind, WallpaperKind::WorkshopScene);
    }

    #[test]
    fn scan_skips_corrupt_project_json_but_continues() {
        let tmp = tempfile::tempdir().unwrap();

        let good = tmp.path().join("good");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join("project.json"), r#"{"title":"good"}"#).unwrap();

        let bad = tmp.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("project.json"), b"not json at all {{").unwrap();

        let mut inv = Inventory::new();
        let n = inv.scan(tmp.path(), 4).unwrap();
        assert_eq!(n, 1, "only the good scene should be added");
        let entry = inv.entries().next().unwrap();
        assert_eq!(entry.title.as_deref(), Some("good"));
    }

    #[test]
    fn empty_inventory_is_empty() {
        let inv = Inventory::new();
        assert!(inv.is_empty());
        assert_eq!(inv.len(), 0);
    }
}
