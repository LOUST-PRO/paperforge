//! Auto-detect wallpaper source directories.
//!
//! Looks for:
//! - Steam Workshop content folders
//!   (`~/.steam/root/steamapps/workshop/content/431960` and
//!   `~/.steam/steam/steamapps/workshop/content/431960`, Flatpak variant
//!   `~/.var/app/com.valvesoftware.Steam/...`).
//! - Conventional local wallpaper dirs
//!   (`~/Wallpapers`, `~/Pictures/Wallpapers`, `~/wallpapers`).
//!
//! Callers can always pass extra paths explicitly via CLI flags — these
//! defaults are convenience for the operator's daily setup.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Locations that may contain Steam Workshop Wallpaper Engine content.
/// `431960` is the Wallpaper Engine Steam app id.
pub const WWE_APP_ID: &str = "431960";

/// A set of paths grouped by source kind. Useful for diagnostics and
/// for CLI pretty-printing.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct WorkshopPaths {
    /// All detected Steam Workshop content roots for Wallpaper Engine.
    pub workshop_roots: Vec<PathBuf>,
    /// All detected local wallpaper directories.
    pub local_roots: Vec<PathBuf>,
}

impl WorkshopPaths {
    /// All detected roots in scan order (workshop first, then local).
    pub fn all(&self) -> impl Iterator<Item = &PathBuf> {
        self.workshop_roots.iter().chain(self.local_roots.iter())
    }
}

/// Compute the default wallpaper source paths for the current user.
///
/// Pure: no filesystem writes, no logging side-effects.
pub fn default_paths() -> WorkshopPaths {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));

    let mut workshop_roots = Vec::new();

    // Native Steam variants
    let steam_root = home.join(".steam").join("root").join("steamapps").join("workshop").join("content").join(WWE_APP_ID);
    if steam_root.exists() {
        workshop_roots.push(steam_root);
    }
    let steam_alt = home.join(".steam").join("steam").join("steamapps").join("workshop").join("content").join(WWE_APP_ID);
    if steam_alt.exists() {
        workshop_roots.push(steam_alt);
    }

    // Flatpak Steam
    let flatpak_steam = home
        .join(".var")
        .join("app")
        .join("com.valvesoftware.Steam")
        .join(".steam")
        .join("root")
        .join("steamapps")
        .join("workshop")
        .join("content")
        .join(WWE_APP_ID);
    if flatpak_steam.exists() {
        workshop_roots.push(flatpak_steam);
    }

    let mut local_roots = Vec::new();
    for candidate in ["Wallpapers", "wallpapers", "Pictures/Wallpapers"] {
        let p = home.join(candidate);
        if p.exists() && p.is_dir() {
            local_roots.push(p);
        }
    }

    WorkshopPaths { workshop_roots, local_roots }
}

/// Validate that at least one source directory exists. Used by CLI to
/// short-circuit with a helpful message instead of scanning an empty
/// tree.
pub fn require_at_least_one(paths: &WorkshopPaths) -> Result<()> {
    if paths.all().next().is_none() {
        Err(Error::NoSources)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wwe_app_id_constant() {
        assert_eq!(WWE_APP_ID, "431960");
    }

    #[test]
    fn default_paths_no_panic_without_steam() {
        // Just ensure it runs and returns a struct, even if everything is empty.
        let paths = default_paths();
        // total roots may be 0+ depending on the operator's environment
        let _ = paths.workshop_roots.len();
        let _ = paths.local_roots.len();
    }

    #[test]
    fn require_at_least_one_rejects_empty() {
        let empty = WorkshopPaths { workshop_roots: vec![], local_roots: vec![] };
        assert!(require_at_least_one(&empty).is_err());
    }
}
