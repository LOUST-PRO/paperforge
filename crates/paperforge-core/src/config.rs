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

    /// Pause behaviour. Replaces the v0.2 single SIGSTOP behaviour
    /// with three modes:
    ///
    /// - `hard` (v0.2 default if `pause.mode` is unset): SIGSTOP all
    ///   per-output pids. Cheapest, but the layer-shell surface stops
    ///   receiving frames so niri shows its layer background (grey).
    /// - `frame` (v0.3 default): SIGSTOP/SIGCONT clock — process sleeps
    ///   most of the cycle and wakes briefly to render a frame, keeping
    ///   the surface alive. ~2% CPU even when "paused".
    /// - `throttle`: re-spawn LWE with `--fps 1` when paused. Cleanest
    ///   visual, but costs ~2s of respawn latency on pause/resume.
    #[serde(default)]
    pub pause: PauseConfig,

    /// FPS cap for the LWE process when ACTIVE (not paused). LWE
    /// supports `--fps <N>` natively (default 30). Setting this lower
    /// than the monitor refresh trades smoothness for CPU/GPU.
    #[serde(default)]
    pub fps: FpsConfig,

    /// Static-photo fallback. When LWE is dead or the user pauses
    /// with `pause.mode = "hard"` and `fallback.enabled = true`,
    /// paperforge renders a still image into the layer via a small
    /// Wayland helper (`paperforge-fallback`, shipped as a separate
    /// binary). Defaults to looking under `~/.local/share/backgrounds/`
    /// and the last-rendered scene screenshot under
    /// `~/.cache/paperforge/last-frame/`.
    #[serde(default)]
    pub fallback: FallbackConfig,
}

/// Static-image fallback for when LWE is not running. The renderer
/// is a separate binary (`paperforge-fallback`) that creates a
/// wl_subsurface layer with the configured image. The binary is not
/// built by default; when it's missing, paperforge logs a warning
/// and the layer shows the compositor's default (grey on niri).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FallbackConfig {
    /// Master switch. When `true` and the fallback binary exists,
    /// paperforge spawns it when LWE dies.
    #[serde(default = "default_fallback_enabled")]
    pub enabled: bool,
    /// Search paths for static images, in priority order. The first
    /// existing file wins. Defaults to:
    /// 1. `~/.local/share/backgrounds/` (XDG standard)
    /// 2. `~/.cache/paperforge/last-frame/` (paperforge's own
    ///    last-rendered scene screenshots)
    /// 3. `~/.local/state/pollinations-gamebar/generated-images/` (operator's generated images)
    #[serde(default)]
    pub search_paths: Vec<PathBuf>,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            enabled: default_fallback_enabled(),
            search_paths: default_fallback_search_paths(),
        }
    }
}

fn default_fallback_enabled() -> bool {
    true
}

fn default_fallback_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".local/share/backgrounds"));
        paths.push(home.join(".cache/paperforge/last-frame"));
        paths.push(home.join(".local/state/pollinations-gamebar/generated-images"));
    }
    paths
}

/// Pause behaviour — see [`Config::pause`] for the high-level
/// rationale and the trade-off matrix.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PauseConfig {
    /// `hard` | `soft` | `throttle` (case-insensitive on load).
    pub mode: PauseMode,
    /// Effective fps target when paused in `soft` mode. Drives the
    /// SIGSTOP/SIGCONT duty cycle.
    #[serde(default = "default_paused_fps")]
    pub paused_fps: u32,
    /// `soft` mode: how long LWE is allowed to render before SIGSTOP
    /// (milliseconds). Smaller = more responsive to resume, but more
    /// wake-up overhead. Default 100 ms.
    #[serde(default = "default_clock_awake_ms")]
    pub clock_awake_ms: u64,
    /// `soft` mode: how long LWE is frozen before the next SIGCONT
    /// (milliseconds). Default 400 ms — combined with `clock_awake_ms`
    /// yields ~6 fps perceived.
    #[serde(default = "default_clock_asleep_ms")]
    pub clock_asleep_ms: u64,
}

impl Default for PauseConfig {
    fn default() -> Self {
        Self {
            mode: PauseMode::Frame,
            paused_fps: default_paused_fps(),
            clock_awake_ms: default_clock_awake_ms(),
            clock_asleep_ms: default_clock_asleep_ms(),
        }
    }
}

fn default_paused_fps() -> u32 {
    2
}

fn default_clock_awake_ms() -> u64 {
    100
}

fn default_clock_asleep_ms() -> u64 {
    400
}

/// Pause mode — string-compatible with the on-disk `mode = "frame"`
/// field. Unknown values fail at deserialize time (we'd rather a
/// config typo surface immediately than silently fall back to `hard`).
///
/// Semantics (v0.3):
///
/// - `hard`: pure SIGSTOP. Cheapest. Grey surface on niri because
///   the layer-shell stops receiving frames.
/// - `frame` (default, was called `soft` in early drafts): SIGSTOP /
///   SIGCONT duty cycle. Process sleeps most of the cycle and wakes
///   briefly to render a frame, keeping the surface alive. ~2% CPU
///   even when "paused", no grey. This is the "intermediate" pause:
///   not kernel-level kill, but the rendered frame is frozen during
///   the SIGSTOP window because the Wayland buffer is preserved.
/// - `throttle`: re-spawn LWE with `--fps 1`. Cleanest visual (frames
///   keep coming at 1 Hz so animation isn't fully frozen), but costs
///   ~2s respawn latency on pause / resume.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PauseMode {
    /// Pure SIGSTOP. Cheapest. Grey surface on niri.
    Hard,
    /// SIGSTOP/SIGCONT clock. Default in v0.3.
    Frame,
    /// Re-spawn LWE with `--fps 1`. Cleanest, but ~2s respawn cost.
    Throttle,
}

impl std::str::FromStr for PauseMode {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "hard" => Ok(Self::Hard),
            "frame" | "soft" => Ok(Self::Frame), // "soft" legacy alias
            "throttle" => Ok(Self::Throttle),
            other => Err(format!(
                "unknown pause mode: {other} (expected: hard | frame | throttle)"
            )),
        }
    }
}

impl std::fmt::Display for PauseMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Hard => "hard",
            Self::Frame => "frame",
            Self::Throttle => "throttle",
        })
    }
}

/// FPS cap configuration for the LWE process when active.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FpsConfig {
    /// FPS cap when active. Passed as `--fps <N>` to LWE. The pool's
    /// `set()` always re-uses this value, so changes take effect on
    /// the next respawn (or immediately if you call
    /// `BackendOps::set_active_fps`).
    #[serde(default = "default_active_max_fps")]
    pub active_max: u32,
    /// `auto` (default) detects monitor refresh via `niri msg outputs`
    /// at daemon start and clamps `active_max` to that. `fixed` uses
    /// `active_max` verbatim.
    #[serde(default = "default_fps_mode")]
    pub mode: FpsMode,
    /// Smart calibration — if GPU/CPU load is high, halve `active_max`
    /// until load drops below the threshold. Reads
    /// `/sys/class/drm/card*/device/gpu_busy_percent` (no sudo) and
    /// `/proc/loadavg` as a CPU fallback.
    #[serde(default)]
    pub smart: SmartCalibration,
}

impl Default for FpsConfig {
    fn default() -> Self {
        Self {
            active_max: default_active_max_fps(),
            mode: default_fps_mode(),
            smart: SmartCalibration::default(),
        }
    }
}

fn default_active_max_fps() -> u32 {
    60
}

fn default_fps_mode() -> FpsMode {
    FpsMode::Auto
}

/// How `active_max` is resolved. `auto` queries the compositor at
/// daemon start; `fixed` uses the config value verbatim.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FpsMode {
    /// Detect monitor refresh rate and clamp `active_max` to that.
    Auto,
    /// Use `active_max` as configured.
    Fixed,
}

/// Smart calibration: auto-halve `active_max` when system load is
/// hot. Disabled by default — opt in via `[fps.smart] enabled = true`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SmartCalibration {
    /// Master switch. When `false`, the rest of this struct is
    /// ignored and `active_max` is never adjusted at runtime.
    #[serde(default)]
    pub enabled: bool,
    /// Threshold above which `active_max` is halved. Range 0.0–1.0.
    /// 0.80 = "if GPU/CPU is more than 80% busy, throttle".
    #[serde(default = "default_gpu_load_high")]
    pub gpu_load_high_threshold: f32,
    /// Re-check interval (seconds). Smart calibration is a back-off:
    /// it does not poll every frame. Default 30s.
    #[serde(default = "default_smart_check_interval_s")]
    pub check_interval_s: u64,
}

impl Default for SmartCalibration {
    fn default() -> Self {
        Self {
            enabled: false,
            gpu_load_high_threshold: default_gpu_load_high(),
            check_interval_s: default_smart_check_interval_s(),
        }
    }
}

fn default_gpu_load_high() -> f32 {
    0.80
}

fn default_smart_check_interval_s() -> u64 {
    30
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
            pause: PauseConfig::default(),
            fps: FpsConfig::default(),
            fallback: FallbackConfig::default(),
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
    fn default_pause_mode_is_frame() {
        // The "intermediate" mode (SIGSTOP/SIGCONT clock) is the
        // v0.3 default — see PauseMode doc for rationale.
        let cfg = Config::default();
        assert_eq!(
            cfg.pause.mode,
            PauseMode::Frame,
            "pause.mode must default to Frame (the intermediate \
             SIGSTOP/SIGCONT-clock mode) so paused wallpapers stay \
             visible instead of niri showing the layer background"
        );
    }

    #[test]
    fn default_fps_active_max_is_60() {
        let cfg = Config::default();
        assert_eq!(cfg.fps.active_max, 60);
        assert_eq!(cfg.fps.mode, FpsMode::Auto);
    }

    #[test]
    fn default_smart_calibration_is_disabled() {
        let cfg = Config::default();
        assert!(
            !cfg.fps.smart.enabled,
            "smart calibration must be opt-in (default false)"
        );
    }

    #[test]
    fn roundtrip_pause_mode_frame() {
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
            pause: PauseConfig {
                mode: PauseMode::Frame,
                paused_fps: 4,
                clock_awake_ms: 80,
                clock_asleep_ms: 320,
            },
            ..Config::default()
        };
        cfg.save(&paths).unwrap();
        let loaded = Config::load(&paths).unwrap();
        assert_eq!(loaded.pause.mode, PauseMode::Frame);
        assert_eq!(loaded.pause.paused_fps, 4);
        assert_eq!(loaded.pause.clock_awake_ms, 80);
    }

    #[test]
    fn pause_mode_from_str_legacy_alias_soft() {
        // Pre-v0.3 configs used `mode = "soft"`. We still parse that
        // as Frame (the same mode under a new name).
        let mode: PauseMode = "soft".parse().unwrap();
        assert_eq!(mode, PauseMode::Frame);
        let mode: PauseMode = "frame".parse().unwrap();
        assert_eq!(mode, PauseMode::Frame);
        let mode: PauseMode = "hard".parse().unwrap();
        assert_eq!(mode, PauseMode::Hard);
        let mode: PauseMode = "throttle".parse().unwrap();
        assert_eq!(mode, PauseMode::Throttle);
        assert!("bogus".parse::<PauseMode>().is_err());
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
