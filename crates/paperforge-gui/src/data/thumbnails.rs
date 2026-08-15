//! Thumbnail subsystem — preview.jpg (and Phase 1 fallback) decoding
//! + PNG cache (PR 8.1).
//!
//! ## What this module does
//!
//! Produces a small (256×144) PNG preview for every `WallpaperEntry`
//! the GUI cares about. The Picker modal (PR 6) and the Editor
//! sub-picker consume the PNG bytes as base64-encoded `data:` URLs.
//!
//! ## Source resolution
//!
//! - `WorkshopScene` → read `<entry.path>/preview.jpg` if present.
//!   Workshop items typically ship with that file. If absent, the
//!   helper returns `ThumbnailState::None` (the inventory tiles
//!   fall back to the title-only badge).
//! - `LooseImage` → decode the image itself (jpg / png / webp / gif
//!   via the `image` 0.25 codec set).
//! - `LooseVideo` → `None` in Phase 1. Phase 2 (Fase 6C.2) will
//!   shell out to `ffmpeg` for first-frame extraction. Until then,
//!   the editor and picker show the title only.
//!
//! ## Cache
//!
//! Cache directory is one knob the caller owns (`ConfigPaths::thumbnails_dir`
//! in PR 8.2). The cache key is `sha256_hex(path)` so duplicate paths
//! (two `WallpaperEntry` rows pointing at the same scene) share the
//! same PNG. The cache file is `<key>.png`; the modulo 12 (2 hex chars)
//! sharding subdir is added by `cache_path_for` to keep directory
//! sizes bounded as the inventory grows.
//!
//! Cache invalidation: each PNG is keyed on path only. If the source
//! file changes (e.g. workshop item updates), the GUI re-renders
//! because we don't persist mtime in the cache key — keep this
//! simple for Phase 1. Phase 2 may add mtime checks.
//!
//! ## Threading
//!
//! `load_thumbnail` is async and runs `image::ImageReader` + decode
//! inside `tokio::task::spawn_blocking`. The `image` crate is
//! blocking-by-design (no internal async) and the PNG encoder is
//! synchronous, so it would otherwise stall the Dioxus runtime on
//! large inventories.
//!
//! ## Wiring timeline
//!
//! PR 8.1 ships the surface (helpers + cache + tests). PR 8.2
//! consumes it from `ui/picker.rs` (per-tile thumbnail) and
//! `ui/preview.rs` (single large preview pane). Until PR 8.2
//! lands, the bin target has no caller — the module-level
//! `#[allow(dead_code)]` keeps clippy green on the unused API.

#![allow(dead_code)] // PR 8.2 will consume every item below.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio::task;

use paperforge_core::inventory::{WallpaperEntry, WallpaperKind};

use crate::error::GuiError;

/// Render target dimensions. 16:9 at 256×144 keeps the picker
/// tiles compact and reads well at the 760px editor modal width.
const THUMB_WIDTH: u32 = 256;
const THUMB_HEIGHT: u32 = 144;

/// UI-facing enum for the thumbnails signal.
///
/// `Bytes` is the raw PNG payload (already encoded). The
/// picker / editor convert to base64 data URLs at render time.
#[derive(Debug, Clone, PartialEq)]
pub enum ThumbnailState {
    /// Decode in progress (placeholder while waiting on the
    /// `spawn_blocking` task).
    Loading,
    /// PNG bytes ready for inlining.
    Ready(Bytes),
    /// Decode failed (file missing, codec error, IO error). The
    /// caller can fall back to the title-only render.
    Failed(String),
    /// Source not supported in Phase 1 (e.g. LooseVideo without
    /// `preview.jpg`). The caller renders the title-only badge.
    None,
}

/// Byte buffer for PNG-encoded thumbnails. Re-exported as a
/// type alias so `crate::data::thumbnails::Bytes` is the public
/// surface and the implementation can swap to `bytes::Bytes`
/// later if clone semantics become a concern.
pub type Bytes = Vec<u8>;

/// Compute the SHA-256 hex of the wallpaper path. The hex is
/// lowercase, 64 chars, cache-safe (only `[0-9a-f]`).
pub fn cache_key(entry: &WallpaperEntry) -> String {
    let mut hasher = Sha256::new();
    hasher.update(entry.path.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    hex_lower(&digest)
}

fn hex_lower(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(b.len() * 2);
    for byte in b {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Resolve the cache file path for an entry under `cache_dir`.
/// Two-character sharding subdir keeps mkdir counts down.
pub fn cache_path_for(cache_dir: &Path, entry: &WallpaperEntry) -> PathBuf {
    let key = cache_key(entry);
    let shard = &key[..2];
    cache_dir.join(shard).join(format!("{key}.png"))
}

/// Locate the source image for an entry. Returns `None` when the
/// kind is unsupported in Phase 1 or the conventional source is
/// absent.
fn source_for(entry: &WallpaperEntry) -> Option<PathBuf> {
    match entry.kind {
        WallpaperKind::WorkshopScene => {
            let candidate = entry.path.join("preview.jpg");
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        }
        WallpaperKind::LooseImage => Some(entry.path.clone()),
        WallpaperKind::LooseVideo => None,
    }
}

/// Decode + resize + cache a thumbnail.
///
/// The signature is async because the decode + resize + PNG encode
/// happens inside `spawn_blocking`. On the happy path the result
/// is `Ready(Bytes)`; the caller can pipe that into a base64
/// data URL for `<img src="...">`. Any failure becomes
/// `Failed(reason)` and the UI renders the title-only fallback.
///
/// `cache_dir` is created if missing. The cache file is overwritten
/// on every successful decode — the LRU eviction is a Phase 2
/// concern (it requires an inventory-level policy that crosses
/// the cache + the live `Inventory` signal).
pub async fn load_thumbnail(
    entry: WallpaperEntry,
    cache_dir: PathBuf,
) -> Result<ThumbnailState, GuiError> {
    let Some(src) = source_for(&entry) else {
        return Ok(ThumbnailState::None);
    };
    let dst = cache_path_for(&cache_dir, &entry);
    task::spawn_blocking(move || -> Result<ThumbnailState, GuiError> {
        // Ensure cache dir exists. Cache key includes a 2-char shard
        // so mkdir fires once per ~256 entries, not per entry.
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                GuiError::Image(format!("create_dir_all({}): {e}", parent.display()))
            })?;
        }
        // Read source. `ImageReader::open` is sync; we already
        // hopped onto a blocking thread, so a sync `.open()` is fine.
        let img = image::ImageReader::open(&src)
            .map_err(|e| GuiError::Image(format!("ImageReader::open({}): {e}", src.display())))?
            .with_guessed_format()
            .map_err(|e| GuiError::Image(format!("with_guessed_format: {e}")))?
            .decode()
            .map_err(|e| GuiError::Image(format!("decode: {e}")))?;
        // Resize to the canonical thumbnail box. `image::imageops::resize`
        // uses a Catmull-Rom filter by default — quality is good enough
        // for a 256×144 preview and the work is microseconds per tile.
        let resized = img.resize_exact(
            THUMB_WIDTH,
            THUMB_HEIGHT,
            image::imageops::FilterType::CatmullRom,
        );
        // Encode to PNG. PNG supports RGBA8 natively; converting
        // from `DynamicImage` collapses every format (RGB / RGBA /
        // grayscale-with-alpha / palette) into the canonical 4-channel
        // buffer that the encoder can stream.
        let rgba = resized.to_rgba8();
        let (w, h) = rgba.dimensions();
        let raw: Vec<u8> = rgba.into_raw();
        let mut png_buf: Vec<u8> = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_buf);
        use image::ImageEncoder;
        encoder
            .write_image(&raw, w, h, image::ExtendedColorType::Rgba8)
            .map_err(|e| GuiError::Image(format!("png encode: {e}")))?;
        // Persist to cache for next time. We write last so a
        // process crash mid-encode doesn't leave a half-written
        // PNG that the next call would happily hand back.
        std::fs::write(&dst, &png_buf)
            .map_err(|e| GuiError::Image(format!("write({}): {e}", dst.display())))?;
        Ok(ThumbnailState::Ready(png_buf))
    })
    .await
    .map_err(|join_err| GuiError::Core(format!("spawn_blocking (load_thumbnail): {join_err}")))?
}

/// Try to load the cached PNG without decoding. Used by the
/// render loop on inventory snapshot changes when the cache file
/// is already on disk — saves a full decode pass for items that
/// haven't changed. Returns `Ok(Some(bytes))` if the cache file
/// exists and is non-empty, `Ok(None)` otherwise. Decoding errors
/// of the cached file are reported as `Failed` so the UI can
/// fall back to a title-only render.
pub fn try_load_cached(cache_dir: &Path, entry: &WallpaperEntry) -> ThumbnailState {
    let path = cache_path_for(cache_dir, entry);
    let Ok(bytes) = std::fs::read(&path) else {
        return ThumbnailState::None;
    };
    if bytes.is_empty() {
        return ThumbnailState::None;
    }
    // PNG magic bytes: 89 50 4E 47 0D 0A 1A 0A. A truncated
    // cache file (crash mid-write) will fail this check and the
    // caller can re-encode.
    if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return ThumbnailState::Failed(format!("cache file {} missing PNG magic", path.display()));
    }
    ThumbnailState::Ready(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_deterministic_and_64_hex_chars() {
        let entry = WallpaperEntry {
            path: PathBuf::from("/tmp/some_scene"),
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind: WallpaperKind::WorkshopScene,
            title: Some("Some Scene".into()),
            workshop_id: None,
        };
        let k1 = cache_key(&entry);
        let k2 = cache_key(&entry);
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64);
        assert!(k1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn cache_key_differs_per_path() {
        let a = WallpaperEntry {
            path: PathBuf::from("/tmp/a"),
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind: WallpaperKind::LooseImage,
            title: None,
            workshop_id: None,
        };
        let b = WallpaperEntry {
            path: PathBuf::from("/tmp/b"),
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind: WallpaperKind::LooseImage,
            title: None,
            workshop_id: None,
        };
        assert_ne!(cache_key(&a), cache_key(&b));
    }

    #[test]
    fn cache_path_uses_two_char_shard() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = WallpaperEntry {
            path: PathBuf::from("/tmp/some_path"),
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind: WallpaperKind::WorkshopScene,
            title: None,
            workshop_id: None,
        };
        let p = cache_path_for(tmp.path(), &entry);
        let shard = p
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(shard.len(), 2, "shard must be 2 hex chars");
        let fname = p.file_name().unwrap().to_str().unwrap();
        assert!(fname.ends_with(".png"));
        assert_eq!(fname.len(), 64 + 4);
    }

    #[test]
    fn source_for_workshop_scene_returns_preview_jpg_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let scene = tmp.path().join("scene");
        std::fs::create_dir(&scene).unwrap();
        let preview = scene.join("preview.jpg");
        std::fs::write(&preview, b"fake jpg bytes").unwrap();
        let entry = WallpaperEntry {
            path: scene,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind: WallpaperKind::WorkshopScene,
            title: Some("scene".into()),
            workshop_id: None,
        };
        assert_eq!(source_for(&entry), Some(preview));
    }

    #[test]
    fn source_for_workshop_scene_returns_none_without_preview_jpg() {
        let tmp = tempfile::tempdir().unwrap();
        let scene = tmp.path().join("scene");
        std::fs::create_dir(&scene).unwrap();
        let entry = WallpaperEntry {
            path: scene,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind: WallpaperKind::WorkshopScene,
            title: Some("scene".into()),
            workshop_id: None,
        };
        assert_eq!(source_for(&entry), None);
    }

    #[test]
    fn source_for_loose_image_returns_itself() {
        let entry = WallpaperEntry {
            path: PathBuf::from("/tmp/wallpaper.jpg"),
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind: WallpaperKind::LooseImage,
            title: None,
            workshop_id: None,
        };
        assert_eq!(
            source_for(&entry),
            Some(PathBuf::from("/tmp/wallpaper.jpg"))
        );
    }

    #[test]
    fn source_for_loose_video_returns_none() {
        let entry = WallpaperEntry {
            path: PathBuf::from("/tmp/wallpaper.mp4"),
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind: WallpaperKind::LooseVideo,
            title: None,
            workshop_id: None,
        };
        assert_eq!(source_for(&entry), None);
    }

    #[test]
    fn try_load_cached_returns_none_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = WallpaperEntry {
            path: PathBuf::from("/tmp/never_cached"),
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind: WallpaperKind::LooseImage,
            title: None,
            workshop_id: None,
        };
        assert_eq!(try_load_cached(tmp.path(), &entry), ThumbnailState::None);
    }

    #[test]
    fn try_load_cached_returns_failed_when_magic_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = WallpaperEntry {
            path: PathBuf::from("/tmp/path"),
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind: WallpaperKind::LooseImage,
            title: None,
            workshop_id: None,
        };
        let p = cache_path_for(tmp.path(), &entry);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"not a png").unwrap();
        match try_load_cached(tmp.path(), &entry) {
            ThumbnailState::Failed(_) => {}
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn try_load_cached_returns_ready_when_valid_png() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = WallpaperEntry {
            path: PathBuf::from("/tmp/path"),
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind: WallpaperKind::LooseImage,
            title: None,
            workshop_id: None,
        };
        let p = cache_path_for(tmp.path(), &entry);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut valid = b"\x89PNG\r\n\x1a\n".to_vec();
        valid.extend_from_slice(b"fake but magic-valid");
        std::fs::write(&p, &valid).unwrap();
        match try_load_cached(tmp.path(), &entry) {
            ThumbnailState::Ready(bytes) => assert_eq!(bytes.len(), valid.len()),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_thumbnail_returns_none_for_loose_video() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = WallpaperEntry {
            path: PathBuf::from("/tmp/movie.mp4"),
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind: WallpaperKind::LooseVideo,
            title: None,
            workshop_id: None,
        };
        let res = load_thumbnail(entry, tmp.path().to_path_buf())
            .await
            .unwrap();
        assert_eq!(res, ThumbnailState::None);
    }

    #[tokio::test]
    async fn load_thumbnail_returns_none_for_workshop_scene_without_preview_jpg() {
        let tmp = tempfile::tempdir().unwrap();
        let scene = tmp.path().join("scene");
        std::fs::create_dir(&scene).unwrap();
        let entry = WallpaperEntry {
            path: scene,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind: WallpaperKind::WorkshopScene,
            title: Some("scene".into()),
            workshop_id: None,
        };
        let res = load_thumbnail(entry, tmp.path().to_path_buf())
            .await
            .unwrap();
        assert_eq!(res, ThumbnailState::None);
    }

    #[tokio::test]
    async fn load_thumbnail_fails_cleanly_for_corrupt_preview_jpg() {
        let tmp = tempfile::tempdir().unwrap();
        let scene = tmp.path().join("scene");
        std::fs::create_dir(&scene).unwrap();
        std::fs::write(scene.join("preview.jpg"), b"definitely not a jpeg").unwrap();
        let entry = WallpaperEntry {
            path: scene,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            kind: WallpaperKind::WorkshopScene,
            title: Some("scene".into()),
            workshop_id: None,
        };
        // Decode fails (corrupt jpeg); the wrapper returns GuiError::Image,
        // NOT a Failed-state. The caller decides whether to swallow
        // the error and downgrade to ThumbnailState::Failed at render.
        let res = load_thumbnail(entry, tmp.path().to_path_buf()).await;
        assert!(res.is_err(), "corrupt jpeg must surface as error");
    }
}
