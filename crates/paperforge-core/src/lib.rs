//! paperforge-core
//!
//! Core library for `paperforge` — wallpaper inventory, LWE backend
//! IPC, playlists per monitor, and audio control via POSIX signals
//! (SIGUSR1/SIGUSR2) sent to running `linux-wallpaperengine` instances.
//!
//! # Design
//!
//! The core is intentionally backend-agnostic: any future backend
//! (swww, hyprpaper, mpvpaper, awww, etc.) implements
//! [`backend::WallpaperBackend`]. Today only [`backend::LweBackend`]
//! is implemented, targeting the
//! [`louzt/linux-wallpaperengine`](https://github.com/louzt/linux-wallpaperengine)
//! fork.
//!
//! License: MIT.
//!
//! # Modules
//!
//! - [`inventory`] — walkdir-based wallpaper scanner + mtime cache
//! - [`paths`] — auto-detect Steam Workshop + local wallpaper dirs
//! - [`backend`] — `WallpaperBackend` trait + `LweBackend` impl
//! - [`audio`] — `LweAudioController` (mute/unmute via SIGUSR)
//! - [`playlist`] — `Playlist` + `PlaylistStore` (JSON files)
//! - [`config`] — runtime configuration paths
//! - [`metrics`] — runtime metrics collector (per-output CPU/RSS, GPU, ring buffer + JSONL)

// `#![deny(unsafe_code)]` rather than `#![forbid(unsafe_code)]` so
// that the small `detach` helper module (which uses `pre_exec` to
// call `setsid(2)` for the LWE process) can opt back in via a
// scoped `#[allow(unsafe_code)]`. `forbid` is not overridable; `deny`
// is. See crates/paperforge-core/src/detach.rs for the rationale.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod audio;
pub mod backend;
pub mod config;
pub mod daemon;
pub mod dbus;
pub mod detach;
pub mod error;
pub mod fullscreen;
pub mod hotplug;
pub mod inventory;
pub mod lwe_probe;
pub mod metrics;
pub mod paths;
pub mod playlist;
pub mod pool;
pub mod updater;

pub use audio::{AudioCommand, LweAudioController};
pub use backend::{
    BackendKind, BackendState, HyprpaperBackend, LweBackend, MpvpaperBackend, SwwwBackend,
    WallpaperBackend,
};
pub use config::{Config, ConfigPaths};
pub use daemon::{BackendOps, DaemonEvent, LweBackendOps, PaperforgeDaemon};
pub use dbus::{
    serve_dbus, DaemonState, PaperforgeControl, PaperforgeInterface, BUS_NAME, OBJECT_PATH,
};
pub use error::{Error, Result};
pub use hotplug::{CompositorHotplugSource, HotplugEvent, HotplugSource, HotplugWatcher, Output};
pub use inventory::{Inventory, WallpaperEntry, WallpaperKind};
pub use lwe_probe::{probe_lwe_binary, LweBuildKind};
pub use metrics::{
    DaemonMetrics, GpuMetrics, MetricsCollector, MetricsSnapshot, OutputMetrics,
};
pub use paths::{default_paths, WorkshopPaths};
pub use playlist::{Playlist, PlaylistStore};
pub use pool::LweSinglePool;
pub use updater::{BackupEntry, Channel, UpdateInfo, Updater, UpdaterConfig};

/// Crate version (matches `Cargo.toml` workspace version).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Crate name.
pub const NAME: &str = "paperforge-core";
