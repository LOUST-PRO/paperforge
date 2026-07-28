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

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod audio;
pub mod backend;
pub mod config;
pub mod error;
pub mod inventory;
pub mod paths;
pub mod playlist;

pub use audio::{AudioCommand, LweAudioController};
pub use backend::{BackendKind, BackendState, LweBackend, WallpaperBackend};
pub use config::{Config, ConfigPaths};
pub use error::{Error, Result};
pub use inventory::{Inventory, WallpaperEntry, WallpaperKind};
pub use paths::{default_paths, WorkshopPaths};
pub use playlist::{Playlist, PlaylistStore};

/// Crate version (matches `Cargo.toml` workspace version).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Crate name.
pub const NAME: &str = "paperforge-core";
