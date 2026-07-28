//! Runtime configuration: where to put state files, what backend to
//! use, what default playlist to apply on startup.
//!
//! On-disk location: `$XDG_CONFIG_HOME/paperforge/config.toml`
//! (typically `~/.config/paperforge/config.toml`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    backend::{BackendKind, LweBackend},
    error::{Error, Result},
    paths::{default_paths, WorkshopPaths},
};

/// Paths used at runtime for config, playlists, cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigPaths {
    /// `$XDG_CONFIG_HOME/paperforge/`.
    pub config_dir: PathBuf,
    /// `config_dir/playlists/`.
    pub playlists_dir: PathBuf,
    /// `$XDG_CACHE_HOME/paperforge/` (or `~/.cache/paperforge/`).
    pub cache_dir: PathBuf,
    /// `cache_dir/thumbnails/`.
    pub thumbnails_dir: PathBuf,
    /// `cache_dir/inventory.json`.
    pub inventory_cache: PathBuf,
}

impl ConfigPaths {
    /// Compute default locations, creating directories if missing.
    pub fn defaults() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| Error::Config("no config_dir".to_string()))?
            .join("paperforge");
        let playlists_dir = config_dir.join("playlists");
        let cache_dir = dirs::cache_dir()
            .ok_or_else(|| Error::Config("no cache_dir".to_string()))?
            .join("paperforge");
        let thumbnails_dir = cache_dir.join("thumbnails");
        let inventory_cache = cache_dir.join("inventory.json");

        for dir in [&config_dir, &playlists_dir, &cache_dir, &thumbnails_dir] {
            if !dir.exists() {
                std::fs::create_dir_all(dir)?;
            }
        }
        Ok(Self {
            config_dir,
            playlists_dir,
            cache_dir,
            thumbnails_dir,
            inventory_cache,
        })
    }

    /// Path to the main config file (`config.toml`).
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
}

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Which backend to use. Today only LWE is wired.
    pub backend: BackendKind,
    /// Path to the backend binary (optional; defaults to PATH lookup).
    #[serde(default)]
    pub backend_binary: Option<PathBuf>,
    /// Additional wallpaper source directories, on top of the
    /// auto-detected ones.
    #[serde(default)]
    pub extra_sources: Vec<PathBuf>,
    /// Auto-detected paths (computed at load time, not persisted).
    #[serde(skip)]
    pub default_paths: WorkshopPaths,
    /// Whether to auto-pause wallpapers when games launch.
    #[serde(default = "default_true")]
    pub auto_pause_on_game: bool,
    /// Whether to mute audio when fullscreen / games.
    #[serde(default = "default_true")]
    pub auto_mute_on_fullscreen: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: BackendKind::LinuxWallpaperEngine,
            backend_binary: None,
            extra_sources: Vec::new(),
            default_paths: default_paths(),
            auto_pause_on_game: true,
            auto_mute_on_fullscreen: true,
        }
    }
}

impl Config {
    /// Load config from `paths.config_file()`, falling back to
    /// defaults if the file does not exist.
    pub fn load(paths: &ConfigPaths) -> Result<Self> {
        let file = paths.config_file();
        let mut cfg = if file.exists() {
            let text = std::fs::read_to_string(&file)?;
            toml::from_str(&text).map_err(|e| Error::Config(format!("parse: {e}")))?
        } else {
            Self::default()
        };
        // Always recompute default_paths on load (it's filesystem-
        // dependent and not persisted).
        cfg.default_paths = default_paths();
        Ok(cfg)
    }

    /// Save config to `paths.config_file()`.
    pub fn save(&self, paths: &ConfigPaths) -> Result<()> {
        let file = paths.config_file();
        let toml =
            toml::to_string_pretty(self).map_err(|e| Error::Config(format!("serialize: {e}")))?;
        std::fs::write(&file, toml)?;
        Ok(())
    }

    /// Construct the [`LweBackend`] this config describes.
    pub fn backend(&self) -> LweBackend {
        match self.backend {
            BackendKind::LinuxWallpaperEngine => match &self.backend_binary {
                Some(p) => LweBackend::with_binary(p),
                None => LweBackend::new(),
            },
        }
    }

    /// All source roots: defaults + extras.
    pub fn source_roots(&self) -> Vec<&Path> {
        let mut roots: Vec<&Path> = self.default_paths.all().map(|p| p.as_path()).collect();
        for extra in &self.extra_sources {
            roots.push(extra.as_path());
        }
        roots
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::WallpaperBackend;

    #[test]
    fn defaults_backend_is_lwe() {
        let cfg = Config::default();
        assert_eq!(cfg.backend, BackendKind::LinuxWallpaperEngine);
    }

    #[test]
    fn roundtrip_via_tempfile() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = ConfigPaths {
            config_dir: tmp.path().to_path_buf(),
            playlists_dir: tmp.path().join("playlists"),
            cache_dir: tmp.path().join("cache"),
            thumbnails_dir: tmp.path().join("cache").join("thumbnails"),
            inventory_cache: tmp.path().join("cache").join("inventory.json"),
        };
        for d in [
            &paths.config_dir,
            &paths.playlists_dir,
            &paths.cache_dir,
            &paths.thumbnails_dir,
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        let cfg = Config::default();
        cfg.save(&paths).unwrap();
        let loaded = Config::load(&paths).unwrap();
        assert_eq!(loaded.backend, cfg.backend);
        assert_eq!(loaded.auto_pause_on_game, cfg.auto_pause_on_game);
    }

    #[test]
    fn roundtrip_preserves_extra_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = ConfigPaths {
            config_dir: tmp.path().to_path_buf(),
            playlists_dir: tmp.path().join("playlists"),
            cache_dir: tmp.path().join("cache"),
            thumbnails_dir: tmp.path().join("cache").join("thumbnails"),
            inventory_cache: tmp.path().join("cache").join("inventory.json"),
        };
        for d in [
            &paths.config_dir,
            &paths.playlists_dir,
            &paths.cache_dir,
            &paths.thumbnails_dir,
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        let cfg = Config {
            extra_sources: vec![PathBuf::from("/srv/wallpapers"), PathBuf::from("/opt/wp")],
            auto_pause_on_game: false,
            ..Config::default()
        };
        cfg.save(&paths).unwrap();
        let loaded = Config::load(&paths).unwrap();
        assert_eq!(loaded.extra_sources, cfg.extra_sources);
        assert!(!loaded.auto_pause_on_game);
    }

    #[test]
    fn backend_constructs_without_panic() {
        let cfg = Config::default();
        let backend = cfg.backend();
        assert_eq!(backend.kind(), BackendKind::LinuxWallpaperEngine);
    }

    #[test]
    fn source_roots_includes_extras() {
        let tmp = tempfile::tempdir().unwrap();
        let extra = tmp.path().join("extra");
        std::fs::create_dir_all(&extra).unwrap();

        let cfg = Config {
            extra_sources: vec![extra.clone()],
            ..Config::default()
        };
        let roots: Vec<&Path> = cfg.source_roots();
        assert!(
            roots.iter().any(|p| p == &extra),
            "extra_sources must appear in source_roots()"
        );
    }
}
