//! Display formatters shared between the TUI and the GUI.
//!
//! These helpers are pure (no state, no async), so they live in the
//! core crate to avoid drift between the two front-ends. Each
//! function has tests in `paperforge-core/src/format.rs` (plus the
//! original TUI lifters in `paperforge-tui/src/data.rs` and
//! `paperforge-tui/src/ui.rs`).
//!
//! ## Why in core, not in either frontend
//!
//! - The TUI was the first consumer (`format_size`, `format_entry_path`,
//!   `entry_size_on_disk`) and the GUI needs the same helpers for its
//!   inventory panel.
//! - `format_mtime_ago` is used by both UIs to render the inventory
//!   timeline.
//! - Keeping them in core means a single source of truth for the
//!   identical arithmetic and `display` rules. If we ever change the
//!   size unit scheme (e.g. add IEC vs SI toggle), both UIs update
//!   together.

use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use walkdir::WalkDir;

use crate::inventory::WallpaperKind;

/// Format a byte count as binary IEC units (KiB, MiB, GiB).
///
/// Examples:
/// - `format_size(0)` → `"0 B"`
/// - `format_size(1024)` → `"1.0 KiB"`
/// - `format_size(1024 * 1024)` → `"1.0 MiB"`
/// - `format_size(1024u64.pow(3))` → `"1.0 GiB"`
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

/// Truncate a path string to `max_len` characters, keeping the tail
/// (the filename is the readable part when paths are long).
///
/// Char-count based, not byte-count based — must not panic on
/// multi-byte chars mid-path.
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

/// Compute the on-disk size of a wallpaper entry.
///
/// - For loose media (`LooseImage`, `LooseVideo`): the file's byte length.
/// - For Workshop scenes: the sum of all regular file bytes under the
///   scene directory. Directory inodes are NOT counted (they add
///   ~80 bytes on ext4 and would inflate the displayed total).
pub fn entry_size_on_disk(path: &Path, kind: WallpaperKind) -> u64 {
    match kind {
        WallpaperKind::LooseImage | WallpaperKind::LooseVideo => {
            std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
        }
        WallpaperKind::WorkshopScene => WalkDir::new(path)
            .into_iter()
            .filter_map(|r: std::result::Result<walkdir::DirEntry, walkdir::Error>| r.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum(),
    }
}

/// Render a `SystemTime` as an ISO-style date string (`YYYY-MM-DD`).
///
/// Used by the inventory panel for both the TUI and the GUI. The
/// format is intentionally naive (always UTC, no timezone awareness)
/// because both UIs label the column "mtime" and the user is expected
/// to understand the file's own timestamp semantics.
pub fn format_mtime_ago(mtime: SystemTime) -> String {
    let dt: DateTime<Utc> = mtime.into();
    dt.format("%Y-%m-%d").to_string()
}

/// Resolve the canonical absolute path for a wallpaper entry's
/// on-disk location. For Workshop scenes this is the scene directory;
/// for loose media it is the file itself. This is a thin helper used
/// by both UIs to normalise display paths.
pub fn backend_path(path: &Path) -> &Path {
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

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
    fn format_mtime_ago_returns_iso_date() {
        let mtime = UNIX_EPOCH + Duration::from_secs(86400 * 30);
        assert_eq!(format_mtime_ago(mtime), "1970-01-31");
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

    #[test]
    fn backend_path_returns_pathbuf_as_path() {
        let p = PathBuf::from("/scenes/forest");
        assert_eq!(backend_path(&p), p.as_path());
    }
}
