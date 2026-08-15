//! Settings panel data layer — load + save `Config::extra_sources`.
//!
//! PR 9.4: The GUI exposes the operator's `extra_sources` (a
//! `Vec<PathBuf>` of paths to scan, on top of the auto-detected
//! ones) so wallpapers in custom directories don't require editing
//! `~/.config/paperforge/config.toml` by hand.
//!
//! The `Config` struct in `paperforge-core` already has
//! `extra_sources` and `Config::load` / `Config::save` round-trip
//! to TOML. This module is just a thin wrapper with the dedup +
//! merge helpers that the GUI's settings callbacks need.
//!
//! `merge_inventory_roots` is the helper that combines the
//! auto-detected paths with `extra_sources` into a single dedup
//! `Vec<PathBuf>` for the inventory scan. Dedup is important so
//! the same path shows up once in the inventory.

use std::path::{Path, PathBuf};

use paperforge_core::config::Config;
use paperforge_core::paths::WorkshopPaths;

use crate::error::GuiError;

/// Save `Config` to disk. Overwrites the existing TOML file. The
/// `extra_sources` mutations are pure on the in-memory `Config`
/// struct; this is the only place that hits disk.
pub fn save_config(cache_paths: &paperforge_core::config::ConfigPaths, cfg: &Config) -> Result<(), GuiError> {
    let cp = cache_paths.clone();
    let cfg_clone = cfg.clone();
    tokio::task::block_in_place(move || cfg_clone.save(&cp))
        .map_err(|e| GuiError::Config(format!("config save: {e}")))?;
    Ok(())
}

/// Add a path to `extra_sources`, dedup-ing against existing
/// entries. Returns `true` if the path was actually added (i.e.
/// it wasn't already there).
///
/// Pure function — callers handle persistence + signal broadcast.
pub fn push_extra_source(cfg: &mut Config, path: PathBuf) -> bool {
    if cfg.extra_sources.iter().any(|p| same_path(p, &path)) {
        return false;
    }
    cfg.extra_sources.push(path);
    true
}

/// Remove a path from `extra_sources`. Returns `true` if the path
/// was actually removed.
pub fn remove_extra_source(cfg: &mut Config, path: &Path) -> bool {
    let before = cfg.extra_sources.len();
    cfg.extra_sources.retain(|p| !same_path(p, path));
    cfg.extra_sources.len() < before
}

/// Combine the auto-detected paths with `extra_sources` into a
/// single deduped `Vec<PathBuf>`. This is what `inventory_roots`
/// should consume so the GUI sees operators' custom wallpapers.
pub fn merge_inventory_roots(detected: &WorkshopPaths, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = detected.all().cloned().collect();
    for p in extra {
        if !out.iter().any(|q| same_path(q, p)) {
            out.push(p.clone());
        }
    }
    out
}

/// Path equality that treats the trailing-separator case as equal
/// (`/foo` and `/foo/` resolve to the same directory). Without
/// this, the same path added via different UI paths would dedup
/// miss.
fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    // Try canonicalize-fallback (best effort, may fail for non-existent).
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => {
            // Strip trailing separator (if any) and retry.
            let norm_a = a.to_string_lossy().trim_end_matches('/').to_string();
            let norm_b = b.to_string_lossy().trim_end_matches('/').to_string();
            norm_a == norm_b
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_cfg() -> Config {
        Config::default()
    }

    #[test]
    fn push_extra_source_dedups_exact_match() {
        let mut cfg = empty_cfg();
        assert!(push_extra_source(&mut cfg, PathBuf::from("/home/lou/Wallpapers")));
        assert!(!push_extra_source(&mut cfg, PathBuf::from("/home/lou/Wallpapers")));
        assert_eq!(cfg.extra_sources.len(), 1);
    }

    #[test]
    fn push_extra_source_dedups_trailing_separator() {
        let mut cfg = empty_cfg();
        assert!(push_extra_source(&mut cfg, PathBuf::from("/home/lou/Wallpapers")));
        assert!(!push_extra_source(&mut cfg, PathBuf::from("/home/lou/Wallpapers/")));
        assert_eq!(cfg.extra_sources.len(), 1);
    }

    #[test]
    fn remove_extra_source_returns_true_on_hit() {
        let mut cfg = empty_cfg();
        cfg.extra_sources.push(PathBuf::from("/tmp/abc"));
        assert!(remove_extra_source(&mut cfg, Path::new("/tmp/abc")));
        assert!(!remove_extra_source(&mut cfg, Path::new("/tmp/abc")));
    }

    #[test]
    fn merge_inventory_roots_prefers_detected_then_appends_extra() {
        let detected = WorkshopPaths {
            workshop_roots: vec![PathBuf::from("/workshop")],
            local_roots: vec![PathBuf::from("/home/lou/Wallpapers")],
        };
        let extra = vec![PathBuf::from("/home/lou/Wallpapers"), PathBuf::from("/data/wps")];
        let merged = merge_inventory_roots(&detected, &extra);
        assert_eq!(merged.len(), 3); // deduped locally
        assert_eq!(merged[0], PathBuf::from("/workshop"));
        assert_eq!(merged[1], PathBuf::from("/home/lou/Wallpapers"));
        assert!(merged.contains(&PathBuf::from("/data/wps")));
    }
}
