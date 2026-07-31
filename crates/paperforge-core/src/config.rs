//! Runtime configuration: where to put state files, what backend to
//! use, what default playlist to apply on startup.
//!
//! On-disk location: `$XDG_CONFIG_HOME/paperforge/config.toml`
//! (typically `~/.config/paperforge/config.toml`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    backend::{BackendKind, LweBackend},
    daemon::LweBackendOps,
    error::{Error, Result},
    paths::{default_paths, WorkshopPaths},
};
use std::sync::Arc;

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
    /// Override the LWE binary version probe for audio signals.
    ///
    /// - `Some(true)`: trust the audio signals will be handled (do NOT probe)
    /// - `Some(false)`: refuse to send SIGUSR1/SIGUSR2 even if probe says ok
    /// - `None`: auto-probe `<backend_binary> --version` for the
    ///   "signal handlers" marker before sending
    ///
    /// Default is `None` (probe). The probe is cached for the process
    /// lifetime to avoid spawning `<binary> --version` on every audio
    /// command.
    #[serde(default)]
    pub lwe_supports_audio_signals: Option<bool>,
    /// When `true` (default), paperforge uses a single LWE process
    /// with multi-output argv (the v0.2 pool architecture). When
    /// `false` (legacy v0.1), each output gets its own LWE process
    /// — slower, ~3× more RSS, but bypasses any pool bug by falling
    /// back to per-process mode.
    ///
    /// Operators can disable the pool via `[pool_enabled]` in
    /// `~/.config/paperforge/config.toml` if they hit an edge case
    /// (e.g. a specific LWE build that mishandles merged argv).
    #[serde(default = "default_pool_enabled")]
    pub pool_enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_pool_enabled() -> bool {
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
            lwe_supports_audio_signals: None,
            pool_enabled: true,
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
            // Future: route to SwwwBackend / HyprpaperBackend here.
            // Today the CLI is LWE-only; this branch is unreachable
            // in practice until we add a `--backend swww` flag.
            BackendKind::SwwwDaemon | BackendKind::Hyprpaper | BackendKind::Mpvpaper => {
                LweBackend::new()
            }
        }
    }

    /// Build the daemon-side [`LweBackendOps`] this config describes.
    ///
    /// Differs from [`backend()`](Self::backend) in two ways:
    /// 1. Returns an `Arc<dyn BackendOps>` compatible with the daemon
    ///    entry point (which stores the backend as a trait object).
    /// 2. Reads [`pool_enabled`](Self::pool_enabled) and passes
    ///    `use_pool = pool_enabled` to `LweBackendOps`. Setting
    ///    `pool_enabled = false` in `config.toml` flips the daemon
    ///    to the v0.1 per-output spawn path.
    pub fn build_backend_ops(&self) -> Arc<LweBackendOps> {
        let ops = match &self.backend_binary {
            Some(p) => LweBackendOps::with_binary_and_pool(p, self.pool_enabled),
            None => LweBackendOps::with_pool(self.pool_enabled),
        };
        Arc::new(ops)
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
            lwe_supports_audio_signals: Some(true),
            ..Config::default()
        };
        cfg.save(&paths).unwrap();
        let loaded = Config::load(&paths).unwrap();
        assert_eq!(loaded.extra_sources, cfg.extra_sources);
        assert!(!loaded.auto_pause_on_game);
        assert_eq!(
            loaded.lwe_supports_audio_signals,
            Some(true),
            "lwe_supports_audio_signals override must roundtrip"
        );
    }

    #[test]
    fn default_audio_signals_is_auto_probe() {
        let cfg = Config::default();
        assert!(
            cfg.lwe_supports_audio_signals.is_none(),
            "default must be None so the probe runs"
        );
    }

    #[test]
    fn roundtrip_default_audio_signals_is_none() {
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
        assert!(
            loaded.lwe_supports_audio_signals.is_none(),
            "explicit default must roundtrip as None"
        );
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

    #[test]
    fn default_pool_enabled_is_true() {
        // The pool architecture is the v0.2 default. Operators opt
        // out via `pool_enabled = false` in config.toml.
        let cfg = Config::default();
        assert!(cfg.pool_enabled, "pool_enabled must default to true");
    }

    #[test]
    fn roundtrip_pool_enabled_false() {
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
            pool_enabled: false,
            ..Config::default()
        };
        cfg.save(&paths).unwrap();
        let loaded = Config::load(&paths).unwrap();
        assert!(
            !loaded.pool_enabled,
            "pool_enabled=false must roundtrip through toml"
        );
    }

    #[test]
    fn build_backend_ops_respects_pool_enabled() {
        let cfg_pool = Config::default();
        assert!(cfg_pool.pool_enabled);
        let ops_pool = cfg_pool.build_backend_ops();
        assert!(ops_pool.use_pool(), "pool_enabled=true → use_pool=true");

        let cfg_no_pool = Config {
            pool_enabled: false,
            ..Config::default()
        };
        let ops_no_pool = cfg_no_pool.build_backend_ops();
        assert!(
            !ops_no_pool.use_pool(),
            "pool_enabled=false → use_pool=false"
        );
    }

    #[test]
    fn build_backend_ops_with_binary_path() {
        let cfg = Config {
            backend_binary: Some(PathBuf::from("/opt/lwe/linux-wallpaperengine")),
            ..Config::default()
        };
        let ops = cfg.build_backend_ops();
        assert_eq!(
            ops.backend().binary_path.as_deref(),
            Some(Path::new("/opt/lwe/linux-wallpaperengine"))
        );
    }
}
