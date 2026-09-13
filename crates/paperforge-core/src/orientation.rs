//! Multi-stage orientation parser for Workshop scenes.
//!
//! Detects the physical orientation (Portrait vs Landscape vs Square
//! vs Unknown) of a Wallpaper Engine workshop directory by dispatching
//! on `project.json.type`:
//!
//! - `scene` (Workshop scenes with `scene.pkg`): bytestream scan for
//!   `camera.orthogonalprojection.{width,height}`. This is the
//!   authoritative source for Workshop scene dimensions — `project.json`
//!   itself does NOT carry width/height/aspect_ratio.
//! - Video workshops (mp4/webm/mkv/mov): shell out to `ffprobe` with
//!   a 5s timeout.
//! - Image workshops (PNG/JPEG): inline header parse (no extra crate,
//!   per `rust-compile-resource-hardening.md`).
//! - Web workshops (`index.html` present): always Unknown (we cannot
//!   determine orientation from filesystem alone).
//!
//! All paths return a [`DetectionResult`] carrying the observed
//! dimensions + [`DetectionSource`] tag for downstream diagnostics.
//! Width/height are zero for Unknown.
//!
//! ## Caching
//!
//! Caller is expected to memoise the result keyed by workshop_id +
//! `scene.pkg` mtime. The parser itself is stateless — running it
//! repeatedly on the same input returns identical results.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Physical orientation of a wallpaper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Orientation {
    /// Taller than wide (h > w). Common for vertical monitors and
    /// mobile/portrait games.
    Portrait,
    /// Wider than tall (w >= h). The default for desktop monitors
    /// and the case paperforge cares about most.
    Landscape,
    /// Exactly square (w == h). Rare; treated as Landscape by default
    /// since most desktop rotation logic equates the two.
    Square,
    /// We could not determine the orientation from the available
    /// metadata (web workshop, parse failure, missing field).
    /// Caller decides whether to skip or allow via
    /// [`OrientationFallback`].
    Unknown,
}

impl Orientation {
    /// Derive orientation from raw dimensions. Returns [`Unknown`]
    /// for zero-area or degenerate inputs.
    pub fn from_dimensions(width: u32, height: u32) -> Self {
        match (width, height) {
            (0, 0) => Self::Unknown,
            (w, h) if w > h => Self::Landscape,
            (w, h) if h > w => Self::Portrait,
            _ => Self::Square,
        }
    }
}

/// Where the parser learned the dimensions from. Used for
/// diagnostics + as a cache invalidation hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectionSource {
    /// Parsed `scene.pkg` PKGV0006 header for `orthogonalprojection`.
    ScenePkg,
    /// `ffprobe` shell-out for video workshops.
    Mp4Header,
    /// PNG IHDR / JPEG SOF0 header inline parse.
    PngHeader,
    /// JPEG SOF0 / SOF2 marker scan.
    JpegHeader,
    /// Web workshop (HTML/JS) — cannot determine from FS alone.
    WebUnknown,
    /// All dispatch branches failed; downstream should treat as
    /// [`Orientation::Unknown`] and apply fallback policy.
    ParseFailed,
}

/// A detection outcome carrying enough context for the caller to
/// log, cache, or apply the [`Orientation`] without re-parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionResult {
    /// Detected orientation. [`Orientation::Unknown`] when the
    /// parser could not determine dimensions (web workshop,
    /// missing field, ffprobe timeout).
    pub orientation: Orientation,
    /// Width in pixels. Zero when orientation is [`Orientation::Unknown`].
    pub width: u32,
    /// Height in pixels. Zero when orientation is [`Orientation::Unknown`].
    pub height: u32,
    /// Where the dimensions came from. Used for diagnostics and
    /// as a cache invalidation hint.
    pub source: DetectionSource,
}

impl DetectionResult {
    fn unknown(source: DetectionSource) -> Self {
        Self {
            orientation: Orientation::Unknown,
            width: 0,
            height: 0,
            source,
        }
    }
}

/// Top-level entry point. Dispatches on `project.json.type`:
/// - `video` → [`detect_video`]
/// - `image` / `png` / `jpeg` / `jpg` → [`detect_image`]
/// - `web` → Unknown (WebUnknown)
/// - everything else (incl. `scene` and missing) → [`detect_scene`]
///   against `<dir>/scene.pkg`.
///
/// Returns `Orientation::Unknown` rather than an error for benign
/// "cannot determine" cases (web workshop, missing scene.pkg) so
/// the bash helper can apply its fallback policy without parsing
/// a Result. True errors (I/O, JSON parse) bubble up via
/// [`crate::error::Error::Orientation`].
pub fn detect(dir: &Path) -> Result<DetectionResult, crate::error::Error> {
    use crate::error::Error;

    let project_json = dir.join("project.json");
    let pj: serde_json::Value = match std::fs::read(&project_json) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| Error::Orientation {
            path: project_json.display().to_string(),
            reason: format!("project.json parse: {e}"),
        })?,
        // No project.json → treat as Workshop scene (most common
        // legacy layout). This matches the upstream LWE scanner's
        // permissiveness.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return detect_workshop_dir(dir);
        }
        Err(e) => {
            return Err(Error::Orientation {
                path: project_json.display().to_string(),
                reason: format!("read: {e}"),
            });
        }
    };

    // Dispatch by `type` field (default "scene" if absent — same
    // convention as `inventory.rs`).
    let ty = pj.get("type").and_then(|v| v.as_str()).unwrap_or("scene");
    match ty {
        "video" => detect_video(dir),
        "image" | "png" | "jpeg" | "jpg" => detect_image(dir),
        "web" => Ok(DetectionResult::unknown(DetectionSource::WebUnknown)),
        // Scene + everything else (incl. asset-only items) → scene.pkg path.
        _ => detect_workshop_dir(dir),
    }
}

/// Scene-path dispatcher used as a fallback when `project.json` is
/// absent or `type` doesn't match a media variant. Tries scene.pkg,
/// then falls back to a loose media scan for video/image files in
/// the directory (some Workshop items don't carry scene.pkg but
/// have a single media file at the root).
fn detect_workshop_dir(dir: &Path) -> Result<DetectionResult, crate::error::Error> {
    let scene_pkg = dir.join("scene.pkg");
    if scene_pkg.is_file() {
        return detect_scene(&scene_pkg);
    }

    // Loose media fallback — scan dir for first .mp4/.webm/.mkv/.mov/.png/.jpg/.jpeg.
    let loose = first_media_file(dir).ok_or_else(|| crate::error::Error::Orientation {
        path: dir.display().to_string(),
        reason: "no scene.pkg and no media file found".into(),
    })?;
    detect_media_file(&loose)
}

fn first_media_file(dir: &Path) -> Option<PathBuf> {
    const VIDEO_EXTS: &[&str] = &["mp4", "webm", "mkv", "mov"];
    const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg"];
    let read = std::fs::read_dir(dir).ok()?;
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if let Some(e) = ext {
            if VIDEO_EXTS.contains(&e.as_str()) || IMAGE_EXTS.contains(&e.as_str()) {
                return Some(path);
            }
        }
    }
    None
}

fn detect_media_file(path: &Path) -> Result<DetectionResult, crate::error::Error> {
    use crate::error::Error;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mp4" | "webm" | "mkv" | "mov" => detect_video_file(path),
        "png" => detect_png_header(path),
        "jpg" | "jpeg" => detect_jpeg_header(path),
        _ => Err(Error::Orientation {
            path: path.display().to_string(),
            reason: format!("unsupported media extension: {ext:?}"),
        }),
    }
}

/// Bytestream scan of `scene.pkg` for `camera.orthogonalprojection`
/// block, then extract `width` and `height` integer fields within
/// a 1024-byte window. This is the only reliable source for scene
/// dimensions — `project.json` does NOT carry them.
///
/// We do NOT parse the full JSON embedded in scene.pkg because
/// (a) it's compressed/large, (b) we only need two integers, and
/// (c) the bytestream scan is ~µs vs ms for a JSON parse.
fn detect_scene(pkg_path: &Path) -> Result<DetectionResult, crate::error::Error> {
    use crate::error::Error;

    let bytes = std::fs::read(pkg_path).map_err(|e| Error::Orientation {
        path: pkg_path.display().to_string(),
        reason: format!("read scene.pkg: {e}"),
    })?;

    // Locate "orthogonalprojection" substring. PKGV0006 packs JSON
    // blobs into the bytestream; the key appears as a literal token.
    let needle = b"orthogonalprojection";
    let Some(start) = find_subslice(&bytes, needle) else {
        return Ok(DetectionResult::unknown(DetectionSource::ParseFailed));
    };

    // Capture up to 1 KiB from the start of the needle. width/height
    // appear within ~200 bytes of the key in every real Workshop
    // scene sampled 2026-09-12 (3422597274, 2001320927, 3621701496).
    let window_end = (start + 1024).min(bytes.len());
    let window = &bytes[start..window_end];

    let width = extract_u32_field(window, b"width").ok_or_else(|| Error::Orientation {
        path: pkg_path.display().to_string(),
        reason: "orthogonalprojection window missing width".into(),
    })?;
    let height = extract_u32_field(window, b"height").ok_or_else(|| Error::Orientation {
        path: pkg_path.display().to_string(),
        reason: "orthogonalprojection window missing height".into(),
    })?;

    Ok(DetectionResult {
        orientation: Orientation::from_dimensions(width, height),
        width,
        height,
        source: DetectionSource::ScenePkg,
    })
}

/// Find first occurrence of `needle` in `haystack`. Simple
/// O(n*m) search — fine for our small windows (~MB scene.pkg).
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Extract an integer value following a quoted JSON key. Looks for
/// `"<key>" : <digits>` or `"<key>":<digits>` patterns within the
/// window. Returns None if not found.
fn extract_u32_field(window: &[u8], key: &[u8]) -> Option<u32> {
    let needle = {
        let mut v = Vec::with_capacity(key.len() + 4);
        v.push(b'"');
        v.extend_from_slice(key);
        v.push(b'"');
        v
    };
    let rel = find_subslice(window, &needle)?;
    let after = &window[rel + needle.len()..];
    // Skip optional whitespace + ':' + optional whitespace.
    let mut i = 0;
    while i < after.len() && after[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= after.len() || after[i] != b':' {
        return None;
    }
    i += 1;
    while i < after.len() && after[i].is_ascii_whitespace() {
        i += 1;
    }
    // Read digits.
    let start = i;
    while i < after.len() && after[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    std::str::from_utf8(&after[start..i]).ok()?.parse().ok()
}

/// Dispatch video detection for a workshop dir. Finds the first
/// video file (mp4/webm/mkv/mov) and probes it via ffprobe.
fn detect_video(dir: &Path) -> Result<DetectionResult, crate::error::Error> {
    use crate::error::Error;

    let video_path = find_first_with_ext(dir, &["mp4", "webm", "mkv", "mov"]).ok_or_else(|| {
        Error::Orientation {
            path: dir.display().to_string(),
            reason: "video workshop but no media file found".into(),
        }
    })?;
    detect_video_file(&video_path)
}

fn find_first_with_ext(dir: &Path, exts: &[&str]) -> Option<PathBuf> {
    let read = std::fs::read_dir(dir).ok()?;
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if let Some(e) = ext {
            if exts.contains(&e.as_str()) {
                return Some(path);
            }
        }
    }
    None
}

/// Shell out to `ffprobe` with a 5s timeout. Returns
/// `Orientation::Unknown` (with `Mp4Header` source) on timeout /
/// non-zero exit / parse failure — the bash helper will apply
/// fallback policy.
///
/// We use tokio's process with timeout because blocking the
/// runtime on a hung ffprobe would freeze the CLI.
fn detect_video_file(path: &Path) -> Result<DetectionResult, crate::error::Error> {
    let path_str = path.display().to_string();

    // Synchronous path for unit tests (which build via
    // `tokio::process::Command`'s runtime). Production callers go
    // through the async CLI sub-command; here we use a small
    // runtime just for the timeout.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return Ok(DetectionResult::unknown(DetectionSource::Mp4Header)),
    };
    rt.block_on(async {
        let fut = tokio::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height",
                "-of",
                "csv=p=0",
                &path_str,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output();
        match tokio::time::timeout(Duration::from_secs(5), fut).await {
            Ok(Ok(out)) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let mut it = stdout.trim().split(',');
                let w: u32 = match it.next().and_then(|s| s.trim().parse().ok()) {
                    Some(v) => v,
                    None => return Ok(DetectionResult::unknown(DetectionSource::Mp4Header)),
                };
                let h: u32 = match it.next().and_then(|s| s.trim().parse().ok()) {
                    Some(v) => v,
                    None => return Ok(DetectionResult::unknown(DetectionSource::Mp4Header)),
                };
                Ok(DetectionResult {
                    orientation: Orientation::from_dimensions(w, h),
                    width: w,
                    height: h,
                    source: DetectionSource::Mp4Header,
                })
            }
            _ => Ok(DetectionResult::unknown(DetectionSource::Mp4Header)),
        }
    })
}

/// Dispatch image detection for a workshop dir. Finds the first
/// image file (png/jpg/jpeg) and parses its header.
fn detect_image(dir: &Path) -> Result<DetectionResult, crate::error::Error> {
    use crate::error::Error;
    let image_path =
        find_first_with_ext(dir, &["png", "jpg", "jpeg"]).ok_or_else(|| Error::Orientation {
            path: dir.display().to_string(),
            reason: "image workshop but no media file found".into(),
        })?;
    let ext = image_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => detect_png_header(&image_path),
        "jpg" | "jpeg" => detect_jpeg_header(&image_path),
        _ => Err(Error::Orientation {
            path: image_path.display().to_string(),
            reason: format!("unsupported image extension: {ext:?}"),
        }),
    }
}

/// Parse PNG IHDR chunk for width/height. PNG layout:
/// - Bytes 0..8: signature (`89 50 4E 47 0D 0A 1A 0A`)
/// - Bytes 8..: chunks. First chunk is always IHDR.
/// - IHDR layout: 4-byte length, "IHDR" tag, width(4 BE), height(4 BE),
///   bit_depth, color_type, ...
fn detect_png_header(path: &Path) -> Result<DetectionResult, crate::error::Error> {
    use crate::error::Error;

    let bytes = std::fs::read(path).map_err(|e| Error::Orientation {
        path: path.display().to_string(),
        reason: format!("read PNG: {e}"),
    })?;
    if bytes.len() < 24 {
        return Ok(DetectionResult::unknown(DetectionSource::PngHeader));
    }
    const SIG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if &bytes[..8] != SIG {
        return Ok(DetectionResult::unknown(DetectionSource::PngHeader));
    }
    // Width @ bytes 16..20 BE, Height @ bytes 20..24 BE.
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Ok(DetectionResult {
        orientation: Orientation::from_dimensions(width, height),
        width,
        height,
        source: DetectionSource::PngHeader,
    })
}

/// Parse JPEG SOF0/SOF2 marker for width/height. JPEG layout:
/// - Bytes 0..2: 0xFF 0xD8 (SOI marker)
/// - Then segments: 0xFF <marker> <length BE 2B> <payload>
/// - SOF0 (0xFFC0) and SOF2 (0xFFC2) carry the actual image
///   dimensions. The length is followed by precision(1), height(2 BE),
///   width(2 BE), components(1), ...
fn detect_jpeg_header(path: &Path) -> Result<DetectionResult, crate::error::Error> {
    use crate::error::Error;

    let bytes = std::fs::read(path).map_err(|e| Error::Orientation {
        path: path.display().to_string(),
        reason: format!("read JPEG: {e}"),
    })?;
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return Ok(DetectionResult::unknown(DetectionSource::JpegHeader));
    }
    // Walk segments. Skip 0xFF fill bytes between markers.
    let mut i = 2usize;
    while i + 3 < bytes.len() {
        // Find next 0xFF marker (skip fill).
        while i < bytes.len() && bytes[i] != 0xFF {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Skip multiple 0xFF (fill bytes).
        while i < bytes.len() && bytes[i] == 0xFF {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let marker = bytes[i];
        i += 1;
        // SOI/EOI/RST markers have no payload.
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if i + 1 >= bytes.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
        if marker == 0xC0 || marker == 0xC2 {
            // SOF0/SOF2: precision(1), height(2 BE), width(2 BE), ...
            // After marker byte, i points at the length field (2 bytes).
            // Payload layout: precision(1) height(2 BE) width(2 BE) ...
            if seg_len < 7 || i + 7 >= bytes.len() {
                return Ok(DetectionResult::unknown(DetectionSource::JpegHeader));
            }
            let height = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as u32;
            let width = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            eprintln!("DBG result width={} height={}", width, height);
            return Ok(DetectionResult {
                orientation: Orientation::from_dimensions(width, height),
                width,
                height,
                source: DetectionSource::JpegHeader,
            });
        }
        // Skip segment payload.
        i += seg_len;
    }
    Ok(DetectionResult::unknown(DetectionSource::JpegHeader))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir();
        let unique = format!(
            "paperforge-orient-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let dir = base.join(unique);
        std::fs::create_dir_all(&dir).expect("tempdir");
        dir
    }

    #[test]
    fn orientation_from_dimensions_classifies_well_known_cases() {
        assert_eq!(
            Orientation::from_dimensions(1920, 1080),
            Orientation::Landscape
        );
        assert_eq!(
            Orientation::from_dimensions(1080, 1920),
            Orientation::Portrait
        );
        assert_eq!(Orientation::from_dimensions(512, 512), Orientation::Square);
        assert_eq!(Orientation::from_dimensions(0, 0), Orientation::Unknown);
        // Equal-ish degenerate: 0x1 vs 0x1 → Square (not Unknown).
        assert_eq!(Orientation::from_dimensions(1, 1), Orientation::Square);
    }

    #[test]
    fn find_subslice_basic() {
        assert_eq!(find_subslice(b"hello world", b"world"), Some(6));
        assert_eq!(find_subslice(b"hello world", b"absent"), None);
        assert_eq!(find_subslice(b"", b"x"), None);
    }

    #[test]
    fn extract_u32_field_handles_whitespace_around_colon() {
        // Compact JSON: `"width":1080`
        let window = b"{\"orthogonalprojection\":{\"width\":1080,\"height\":1920}}";
        assert_eq!(extract_u32_field(window, b"width"), Some(1080));
        assert_eq!(extract_u32_field(window, b"height"), Some(1920));
        // Spaced JSON: `"width" : 1920`
        let spaced = b"\"width\" : 1920";
        assert_eq!(extract_u32_field(spaced, b"width"), Some(1920));
        // Missing key → None
        assert_eq!(extract_u32_field(window, b"depth"), None);
    }

    #[test]
    fn detect_scene_parses_orthogonalprojection_portrait() {
        let dir = tempdir();
        let scene_pkg = dir.join("scene.pkg");
        // Synthetic scene.pkg payload containing the
        // orthogonalprojection block with portrait dimensions.
        let payload: &[u8] =
            b"PKGV0006\x00{\"camera\":{\"orthogonalprojection\":{\"width\":1080,\"height\":1920,\"foo\":\"bar\"}}}";
        std::fs::write(&scene_pkg, payload).unwrap();
        let res = detect_scene(&scene_pkg).unwrap();
        assert_eq!(res.orientation, Orientation::Portrait);
        assert_eq!(res.width, 1080);
        assert_eq!(res.height, 1920);
        assert_eq!(res.source, DetectionSource::ScenePkg);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detect_scene_returns_landscape_when_wider_than_tall() {
        let dir = tempdir();
        let scene_pkg = dir.join("scene.pkg");
        let payload: &[u8] =
            b"PKGV0006\x00{\"camera\":{\"orthogonalprojection\":{\"width\":1920,\"height\":1080}}}";
        std::fs::write(&scene_pkg, payload).unwrap();
        let res = detect_scene(&scene_pkg).unwrap();
        assert_eq!(res.orientation, Orientation::Landscape);
        assert_eq!(res.width, 1920);
        assert_eq!(res.height, 1080);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detect_scene_unknown_when_no_orthogonalprojection_key() {
        let dir = tempdir();
        let scene_pkg = dir.join("scene.pkg");
        // Payload with camera block but no orthogonalprojection.
        let payload: &[u8] = b"PKGV0006\x00{\"camera\":{\"perspective\":{\"fov\":90}}}";
        std::fs::write(&scene_pkg, payload).unwrap();
        let res = detect_scene(&scene_pkg).unwrap();
        assert_eq!(res.orientation, Orientation::Unknown);
        assert_eq!(res.source, DetectionSource::ParseFailed);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detect_scene_errors_when_window_missing_width() {
        let dir = tempdir();
        let scene_pkg = dir.join("scene.pkg");
        // orthogonalprojection block with only height (degenerate).
        let payload: &[u8] =
            b"PKGV0006\x00{\"camera\":{\"orthogonalprojection\":{\"height\":1920}}}";
        std::fs::write(&scene_pkg, payload).unwrap();
        let res = detect_scene(&scene_pkg);
        assert!(res.is_err(), "expected error when width missing");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detect_image_png_header_portrait() {
        let dir = tempdir();
        let png_path = dir.join("poster.png");
        // Synthetic PNG header: signature + 8 byte IHDR chunk
        // header + 13 byte IHDR payload.
        let mut bytes = Vec::with_capacity(33);
        bytes.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        // IHDR chunk length (13 bytes payload) BE.
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        // width = 1080 BE
        bytes.extend_from_slice(&1080u32.to_be_bytes());
        // height = 1920 BE
        bytes.extend_from_slice(&1920u32.to_be_bytes());
        // bit_depth, color_type, compression, filter, interlace — 5 bytes.
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        // CRC32 of "IHDR"+data — not parsed by us, so 4 zero bytes fine.
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let mut f = std::fs::File::create(&png_path).unwrap();
        f.write_all(&bytes).unwrap();
        let res = detect_png_header(&png_path).unwrap();
        assert_eq!(res.orientation, Orientation::Portrait);
        assert_eq!(res.width, 1080);
        assert_eq!(res.height, 1920);
        assert_eq!(res.source, DetectionSource::PngHeader);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detect_image_jpeg_header_landscape() {
        let dir = tempdir();
        let jpg_path = dir.join("cover.jpg");
        // Build minimal JPEG with SOI + APP0 (jfif stub) + SOF0.
        // JPEG segment length includes the 2 length bytes themselves.
        let mut bytes = Vec::new();
        // SOI
        bytes.extend_from_slice(&[0xFF, 0xD8]);
        // APP0 stub: 0xFF 0xE0 length=16 "JFIF\0" + 9 bytes padding (14 payload bytes total).
        bytes.extend_from_slice(&[0xFF, 0xE0]);
        bytes.extend_from_slice(&16u16.to_be_bytes());
        bytes.extend_from_slice(b"JFIF\x00");
        bytes.extend_from_slice(&[1, 1, 0, 0, 1, 0, 1, 0, 0]);
        // SOF0: 0xFF 0xC0 length=11 precision=8 height=1080 width=1920 ncomponents=3.
        // length INCLUDES itself: 2 length bytes + 1 precision + 2 height + 2 width + 1 ncomp
        // + 2*3 component descriptor bytes = 11.
        bytes.extend_from_slice(&[0xFF, 0xC0]);
        bytes.extend_from_slice(&11u16.to_be_bytes());
        bytes.push(8); // precision
        bytes.extend_from_slice(&1080u16.to_be_bytes());
        bytes.extend_from_slice(&1920u16.to_be_bytes());
        bytes.push(1); // ncomponents (grayscale SOF0)
                       // 1 component descriptor triple (id, sampling, qtable).
        bytes.extend_from_slice(&[1, 0x11, 0]);
        // SOS marker (0xFF 0xDA) — minimal 3-byte header for scanner to skip.
        bytes.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x03, 0x01, 0x00]);
        // EOI
        bytes.extend_from_slice(&[0xFF, 0xD9]);
        std::fs::write(&jpg_path, &bytes).unwrap();
        let res = detect_jpeg_header(&jpg_path).unwrap();
        assert_eq!(res.orientation, Orientation::Landscape);
        assert_eq!(res.width, 1920);
        assert_eq!(res.height, 1080);
        assert_eq!(res.source, DetectionSource::JpegHeader);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detect_dispatches_scene_to_scene_pkg() {
        let dir = tempdir();
        let scene_pkg = dir.join("scene.pkg");
        std::fs::write(
            &scene_pkg,
            b"{\"camera\":{\"orthogonalprojection\":{\"width\":1920,\"height\":1080}}}",
        )
        .unwrap();
        let project_json = dir.join("project.json");
        std::fs::write(&project_json, br#"{"type":"scene","title":"x"}"#).unwrap();
        let res = detect(&dir).unwrap();
        assert_eq!(res.orientation, Orientation::Landscape);
        assert_eq!(res.source, DetectionSource::ScenePkg);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detect_dispatches_web_to_unknown() {
        let dir = tempdir();
        std::fs::write(dir.join("index.html"), "<html></html>").unwrap();
        std::fs::write(dir.join("project.json"), br#"{"type":"web"}"#).unwrap();
        let res = detect(&dir).unwrap();
        assert_eq!(res.orientation, Orientation::Unknown);
        assert_eq!(res.source, DetectionSource::WebUnknown);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detect_falls_back_when_project_json_absent() {
        let dir = tempdir();
        let scene_pkg = dir.join("scene.pkg");
        std::fs::write(
            &scene_pkg,
            b"{\"camera\":{\"orthogonalprojection\":{\"width\":512,\"height\":512}}}",
        )
        .unwrap();
        // No project.json — should fall back to scene.pkg scan.
        let res = detect(&dir).unwrap();
        assert_eq!(res.orientation, Orientation::Square);
        assert_eq!(res.source, DetectionSource::ScenePkg);
        let _ = std::fs::remove_dir_all(dir);
    }
}
