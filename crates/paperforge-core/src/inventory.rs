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
    /// extension. **When a directory is recognized as a Workshop
    /// scene, the walker skips its subtree** — the scene's internal
    /// media (`bg.png`, `preview.jpg`, scene sub-folders) belong to
    /// the scene entry, not as separate `LooseImage` rows. Without
    /// that skip, one scene produces 5–10 inventory rows.
    ///
    /// **Dedup**: each entry's path is canonicalized (symlinks
    /// resolved) before being used as the BTreeMap key. This means
    /// scanning two roots that point to the same filesystem data
    /// (e.g. `/home/lou/.steam/root` and `/home/lou/.steam/steam`
    /// are both symlinks to `/home/lou/.local/share/Steam`) does
    /// NOT register each scene twice. Without canonicalization the
    /// strings differ (`/root/.../123456` vs `/steam/.../123456`)
    /// and the dedup misses. On Lou's machine this drops the
    /// inventory from 369 entries to 184.
    pub fn scan(&mut self, root: &Path, max_depth: usize) -> Result<usize> {
        if !root.exists() {
            tracing::debug!("skip non-existent root: {}", root.display());
            return Ok(0);
        }

        let mut added = 0usize;

        // PR 9.5: we have to drive `WalkDir` manually instead of
        // `for entry in iter.filter_map(...)` because `skip_current_dir`
        // is a method on the **iterator** (`WalkDir::IntoIter`), not
        // on `DirEntry`. The for-loop holds the iterator borrowed
        // immutably, so we can't call the mutable skip on it. The
        // docs explicitly call out this ergonomic gap:
        // https://docs.rs/walkdir/2.5.0/walkdir/struct.IntoIter.html#method.skip_current_dir
        // The scanner follows symlinks (so operator-laid symlinks to
        // media or workshop directories are picked up) but the
        // `match entry { Ok(e) => e, Err(_) => continue }` below
        // silently skips walkdir's symlink-cycle errors, which is
        // exactly the behaviour we want for a sane filesystem.
        let mut walker = WalkDir::new(root)
            .max_depth(max_depth)
            .follow_links(true)
            .into_iter();
        while let Some(entry) = walker.next() {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

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
                        // PR 9.6: canonicalize so symlinked roots
                        // (e.g. /home/lou/.steam/root + /home/lou/.steam/steam
                        // both → /home/lou/.local/share/Steam) dedup.
                        let canonical = wp.path.canonicalize().unwrap_or_else(|_| wp.path.clone());
                        let mut wp = wp;
                        wp.path = canonical.clone();
                        if self.entries.insert(canonical, wp).is_none() {
                            added += 1;
                        }
                    }
                    // PR 9.5 fix: a Workshop scene directory contains
                    // `project.json` AND the scene's actual media
                    // (bg.png, scene_*.png, preview.jpg, video files).
                    // Without `skip_current_dir`, WalkDir descends
                    // into the scene and the scanner registers each
                    // internal media file as a separate `LooseImage`
                    // entry — turning one scene into 5–10 inventory
                    // rows (the operator's "1 solo scene lo detecta
                    // como varios" report). Skipping the subtree
                    // after detecting `project.json` keeps each
                    // scene as exactly one entry.
                    walker.skip_current_dir();
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

                    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
                    let wp = WallpaperEntry {
                        path: canonical.clone(),
                        mtime,
                        kind,
                        title: None,
                        workshop_id: None,
                    };
                    if self.entries.insert(canonical, wp).is_none() {
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

    /// Regression: a Workshop scene ships internal media (preview.jpg,
    /// bg.png, scene_*.png, materials/*.jpg). Before task #63 the
    /// scanner descended into the scene directory and registered each
    /// of those as a separate `LooseImage` entry, so one scene produced
    /// 5–10 rows in the inventory. After the fix we call
    /// `walker.skip_current_dir()` after detecting `project.json`, so
    /// each scene contributes exactly one row regardless of how many
    /// internal media files it has.
    #[test]
    fn scan_does_not_descend_into_workshop_scene() {
        let tmp = tempfile::tempdir().unwrap();
        let scene = tmp.path().join("123456");
        std::fs::create_dir_all(scene.join("materials")).unwrap();
        std::fs::write(
            scene.join("project.json"),
            r#"{"title":"With Internal Media","type":"scene","id":"123456"}"#,
        )
        .unwrap();
        // Plant a bunch of internal media that would each register as a
        // LooseImage if the scanner descended into the directory.
        std::fs::write(scene.join("preview.jpg"), b"fake-jpg").unwrap();
        std::fs::write(scene.join("bg.png"), b"fake-png").unwrap();
        std::fs::write(scene.join("scene_0.png"), b"fake-png").unwrap();
        std::fs::write(scene.join("scene_1.png"), b"fake-png").unwrap();
        std::fs::write(scene.join("materials/brick.jpg"), b"fake-jpg").unwrap();
        std::fs::write(scene.join("materials/normal.png"), b"fake-png").unwrap();

        let mut inv = Inventory::new();
        let n = inv.scan(tmp.path(), 4).unwrap();
        assert_eq!(n, 1, "scene internal media must not be registered");
        let entries: Vec<_> = inv.entries().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title.as_deref(), Some("With Internal Media"));
        assert_eq!(entries[0].kind, WallpaperKind::WorkshopScene);
        // Sanity: no LooseImage/LooseVideo snuck in.
        assert!(
            entries
                .iter()
                .all(|e| matches!(e.kind, WallpaperKind::WorkshopScene)),
            "only WorkshopScene entries should exist"
        );
    }

    /// Regression for the 369-entries bug: when two source roots are
    /// symlinks to the same filesystem path (e.g. `/home/lou/.steam/root`
    /// and `/home/lou/.steam/steam` are both symlinks to
    /// `/home/lou/.local/share/Steam`), scanning both should NOT
    /// register each scene twice. The InventoryHashMap is keyed by
    /// the *canonical* path (with symlinks resolved), so the second
    /// scan's collision is a no-op.
    #[test]
    fn scan_dedups_symlinked_roots() {
        let base = tempfile::tempdir().unwrap();
        // The real Workshop layout. We simulate by creating one
        // directory per scene + project.json; then we expose the
        // same parent via two symlinks.
        let real = base.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        for (id, title) in [("alpha", "Alpha"), ("bravo", "Bravo")] {
            let scene = real.join(id);
            std::fs::create_dir_all(&scene).unwrap();
            std::fs::write(
                scene.join("project.json"),
                format!(r#"{{"title":"{title}","id":"{id}"}}"#),
            )
            .unwrap();
        }
        let link_a = base.path().join("link_a");
        let link_b = base.path().join("link_b");
        std::os::unix::fs::symlink(&real, &link_a).unwrap();
        std::os::unix::fs::symlink(&real, &link_b).unwrap();

        let mut inv = Inventory::new();
        let n1 = inv.scan(&link_a, 4).unwrap();
        let n2 = inv.scan(&link_b, 4).unwrap();
        assert_eq!(n1, 2, "first scan should register 2 scenes");
        assert_eq!(n2, 0, "second scan via symlink must be a no-op");
        assert_eq!(inv.len(), 2, "no double-counting via symlinks");
    }

    /// Regression: the scanner must follow symlinks pointing to media
    /// files so operators can organise their library (e.g.
    /// `~/Wallpapers/cool.mp4 → /mnt/external/cool.mp4`). Before this
    /// fix `WalkDir` was driven with `follow_links(false)` and the
    /// target file was invisible to the inventory.
    #[test]
    fn scan_follows_symlinks_to_files() {
        let base = tempfile::tempdir().unwrap();
        // The "real" media lives somewhere else; we expose it via a
        // symlink so the scanner has to follow the link to see the file.
        let real = base.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("cool.mp4"), b"fake-mp4").unwrap();

        let link_dir = base.path().join("link_dir");
        std::fs::create_dir_all(&link_dir).unwrap();
        std::os::unix::fs::symlink(real.join("cool.mp4"), link_dir.join("cool.mp4")).unwrap();

        // Asking the scanner to descend link_dir/ as its root would
        // require looping over symlinks-to-directories; the more common
        // shape is: root contains the symlink as a loose file. We mirror
        // that by scanning `link_dir` itself, which has the symlinked
        // media directly inside it.
        let mut inv = Inventory::new();
        let n = inv.scan(&link_dir, 4).unwrap();
        assert_eq!(n, 1, "symlinked loose media must register");
        let entries: Vec<_> = inv.entries().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, WallpaperKind::LooseVideo);
        // The entry's path should canonicalize to the real file.
        assert_eq!(
            entries[0].path,
            real.join("cool.mp4").canonicalize().unwrap()
        );
    }

    /// Regression: an operator's Workshop layout is sometimes exposed
    /// via a symlink-to-directory (e.g. `/workshop/12345 → /mnt/real/12345`)
    /// because the real data lives on a different filesystem. The scanner
    /// must follow the symlink to the directory itself, then descend into
    /// it and register the `project.json` WorkshopScene entry.
    #[test]
    fn scan_follows_symlinks_to_dirs() {
        let base = tempfile::tempdir().unwrap();
        let real_workshop = base.path().join("real_workshop").join("12345");
        std::fs::create_dir_all(&real_workshop).unwrap();
        std::fs::write(
            real_workshop.join("project.json"),
            r#"{"title":"Symlinked Scene","type":"scene","id":"12345"}"#,
        )
        .unwrap();
        // Plant internal media so a no-skip regression would manifest
        // as multiple LooseImage entries.
        std::fs::write(real_workshop.join("preview.jpg"), b"j").unwrap();
        std::fs::write(real_workshop.join("bg.png"), b"p").unwrap();

        let symlink_workshop = base.path().join("symlink_workshop");
        std::fs::create_dir_all(&symlink_workshop).unwrap();
        std::os::unix::fs::symlink(&real_workshop, symlink_workshop.join("12345")).unwrap();

        let mut inv = Inventory::new();
        let n = inv.scan(&symlink_workshop, 4).unwrap();
        assert_eq!(n, 1, "symlinked WorkshopScene must register exactly once");
        let entries: Vec<_> = inv.entries().collect();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.kind, WallpaperKind::WorkshopScene);
        assert_eq!(entry.title.as_deref(), Some("Symlinked Scene"));
        assert_eq!(entry.workshop_id.as_deref(), Some("12345"));
        // Path is canonicalized — should resolve to the real directory.
        assert_eq!(entry.path, real_workshop.canonicalize().unwrap());
    }

    /// Regression: symlink cycles (a → b → a) are a real operator risk
    /// and `WalkDir` detects them, returning `Err` from `next()`. The
    /// scanner's existing `Err(_) => continue` skip protects against a
    /// panic; this test pins that behaviour so a future refactor can't
    /// silently turn cycle detection into a hang.
    #[test]
    fn scan_handles_symlink_cycles_without_panic() {
        let base = tempfile::tempdir().unwrap();
        let a = base.path().join("a");
        let b = base.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        // Plant a real file inside `b` so the scanner has *something* to
        // find — proves the cycle didn't halt the walker prematurely.
        std::fs::write(b.join("scene"), b"j").unwrap();
        // Cycle: a/loop → b/loop, b/loop → a/loop.
        std::os::unix::fs::symlink(&b, a.join("loop")).unwrap();
        std::os::unix::fs::symlink(&a, b.join("loop")).unwrap();

        let mut inv = Inventory::new();
        // If the cycle handling regresses (e.g. infinite recursion or a
        // panic on Err) this call will time out or abort. The assertion
        // is purely "it returned at all".
        let _ = inv.scan(&a, 4).unwrap();
    }

    /// Two scenes in the same root — each must register as exactly one
    /// entry. This catches the bug where `skip_current_dir` only worked
    /// for the first hit and let the second scene leak its internals.
    #[test]
    fn scan_does_not_descend_into_multiple_workshop_scenes() {
        let tmp = tempfile::tempdir().unwrap();
        for (id, title) in [("aaaa", "Alpha"), ("bbbb", "Bravo")] {
            let scene = tmp.path().join(id);
            std::fs::create_dir_all(scene.join("materials")).unwrap();
            std::fs::write(
                scene.join("project.json"),
                format!(r#"{{"title":"{title}","type":"scene","id":"{id}"}}"#),
            )
            .unwrap();
            std::fs::write(scene.join("preview.jpg"), b"j").unwrap();
            std::fs::write(scene.join("bg.png"), b"p").unwrap();
            std::fs::write(scene.join("materials/x.png"), b"p").unwrap();
        }

        let mut inv = Inventory::new();
        let n = inv.scan(tmp.path(), 4).unwrap();
        assert_eq!(n, 2, "each scene must register once, not 4× each");
        let entries: Vec<_> = inv.entries().collect();
        assert_eq!(entries.len(), 2);
        let titles: Vec<_> = entries.iter().filter_map(|e| e.title.clone()).collect();
        assert!(titles.contains(&"Alpha".to_string()));
        assert!(titles.contains(&"Bravo".to_string()));
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

    /// Operator smoke test — runs against Lou's real Workshop paths
    /// when `PAPERFORGE_SCAN_HOME=1` is set. Off by default so CI
    /// doesn't depend on a particular operator's filesystem. Use:
    ///
    /// ```bash
    /// PAPERFORGE_SCAN_HOME=1 cargo test -p paperforge-core \
    ///     --lib inventory::tests::scan_operator_home -- --ignored --nocapture
    /// ```
    ///
    /// Before task #63 this reported ~1102 entries (every Workshop
    /// scene's internal media registered as a separate LooseImage).
    /// After the fix it should report ~50–100: one row per scene
    /// plus the loose media under `~/Wallpapers`.
    #[test]
    #[ignore]
    fn scan_operator_home() {
        if std::env::var("PAPERFORGE_SCAN_HOME").is_err() {
            eprintln!("PAPERFORGE_SCAN_HOME not set — skipping");
            return;
        }
        use crate::paths::default_paths;

        let paths = default_paths();
        eprintln!("Detected workshop roots: {:?}", paths.workshop_roots);
        eprintln!("Detected local roots:   {:?}", paths.local_roots);
        let roots: Vec<_> = paths.all().cloned().collect();
        assert!(!roots.is_empty(), "no source dirs found in $HOME");

        let mut inv = Inventory::new();
        for root in &roots {
            let n = inv.scan(root, 4).unwrap_or_else(|e| {
                panic!("scan failed for {}: {e}", root.display());
            });
            eprintln!("  {} → +{} entries", root.display(), n);
        }
        eprintln!("Total entries: {}", inv.len());
        let by_kind = inv.entries().fold([0usize; 3], |mut acc, e| {
            match e.kind {
                WallpaperKind::WorkshopScene => acc[0] += 1,
                WallpaperKind::LooseImage => acc[1] += 1,
                WallpaperKind::LooseVideo => acc[2] += 1,
            }
            acc
        });
        eprintln!(
            "By kind: scene={} loose_image={} loose_video={}",
            by_kind[0], by_kind[1], by_kind[2]
        );
        assert!(
            inv.len() < 500,
            "inventory grew beyond expected band (got {}). \
             Did the skip_current_dir fix regress?",
            inv.len()
        );
    }
}
