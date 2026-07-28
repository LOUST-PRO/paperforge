//! Crate-wide error type.

use thiserror::Error;

/// Result alias for `lzt-wallcraft-core`.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur across the `lzt-wallcraft-core` API.
#[derive(Debug, Error)]
pub enum Error {
    /// Filesystem I/O failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to detect any wallpaper source directories.
    #[error("no wallpaper sources detected; pass --workshop-dir or --local-dir explicitly")]
    NoSources,

    /// Failed to parse a `project.json` (Wallpaper Engine metadata).
    #[error("project.json parse error in {path}: {message}")]
    ProjectJson {
        /// Path of the file we tried to parse.
        path: String,
        /// Why parsing failed.
        message: String,
    },

    /// Backend (e.g. LWE) is not running or could not be reached.
    #[error("backend '{kind}' not reachable: {message}")]
    BackendUnreachable {
        /// Which backend kind.
        kind: String,
        /// Why.
        message: String,
    },

    /// Backend returned a non-zero / unexpected status.
    #[error("backend '{kind}' returned error: {message}")]
    BackendFailure {
        /// Which backend kind.
        kind: String,
        /// What went wrong.
        message: String,
    },

    /// Playlist not found.
    #[error("playlist '{name}' not found in {store}")]
    PlaylistNotFound {
        /// Playlist name requested.
        name: String,
        /// Store path searched.
        store: String,
    },

    /// Config serialization/deserialization error.
    #[error("config error: {0}")]
    Config(String),

    /// Generic anyhow passthrough.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
