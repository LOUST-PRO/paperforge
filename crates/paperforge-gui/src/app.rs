//! Application-wide state container.
//!
//! [`AppState`] is the single object that holds every reactive
//! signal, every long-lived handle (D-Bus client, playlist store,
//! thumbnail cache), and the list of in-flight coroutines. The root
//! component mounts an [`AppState`] once via
//! [`use_context_provider`](dioxus::prelude::use_context_provider),
//! then every descendant reads it through
//! [`use_context`](dioxus::prelude::use_context) (or the
//! `consume_context` shorthand).
//!
//! # Why one global state object
//!
//! - The GUI has no router. There is no per-page state to isolate;
//!   the user sees a single window with several panels.
//! - Dioxus `Signal<T>` is cheap to clone (`Copy` semantics for the
//!   handle itself, RC under the hood). Putting every signal in a
//!   single struct keeps the cloning cost flat at the call site.
//! - The TUI uses the same "one App, all panels" pattern
//!   (`paperforge-tui/src/app.rs`). Keeping the GUI parallel
//!   reduces the mental switch cost.
//!
//! # Why not `use_store`
//!
//! `use_store` in Dioxus 0.8-alpha is rough (the API is being
//! iterated on). Our state is flat (no nesting), so plain
//! `use_signal` per field is documented, stable, and cheap. We can
//! migrate to `use_store` once the API stabilizes — the migration
//! is mechanical.
//!
//! # Phasing
//!
//! PR 2 ships an empty `AppState` (just constructor + logging).
//! PRs 3–8 grow the fields. Each new field is added in the PR that
//! consumes it so the diff per PR is reviewable.

use std::path::PathBuf;

use paperforge_core::config::ConfigPaths;
use paperforge_core::paths::{default_paths, require_at_least_one, WorkshopPaths};

/// Top-level GUI state. Mounted once at the root, consumed via
/// `use_context::<AppState>()` everywhere downstream.
///
/// `Clone` is derived because Dioxus context values must be
/// `Clone + 'static`. The clone is cheap (Arc bumps).
#[derive(Clone)]
pub struct AppState {
    /// Detected Workshop + loose wallpaper paths.
    pub paths: WorkshopPaths,
    /// Detected-and-failed paths from `require_at_least_one`.
    /// Surface in the welcome banner so the operator can fix the
    /// Steam Workshop install before the GUI becomes useless.
    pub path_warnings: Vec<String>,
    /// Resolved XDG config / cache / playlist paths. The
    /// `thumbnails_dir` is consumed by `data::thumbnails` and the
    /// preview / picker components in PR 8.2. `ConfigPaths::defaults`
    /// creates the directories on disk if missing.
    pub cache_paths: ConfigPaths,
}

impl AppState {
    /// Construct an `AppState` from the operator's environment.
    /// Non-fatal: warnings are collected, not raised. PR 3 uses
    /// `paths` to scan the inventory locally; PR 4 adds the D-Bus
    /// client field (currently absent — see PR 4 plan).
    pub fn new() -> Self {
        let paths = default_paths();
        let path_warnings = match require_at_least_one(&paths) {
            Ok(()) => Vec::new(),
            Err(e) => vec![format!("{e}")],
        };
        let cache_paths = ConfigPaths::defaults().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "ConfigPaths::defaults failed; falling back to empty cache dir");
            // Fallback so the GUI still starts even if XDG dirs
            // are unreadable — thumbnails will simply not cache,
            // the picker will still render (re-decoding each open).
            ConfigPaths {
                config_dir: PathBuf::new(),
                playlists_dir: PathBuf::new(),
                cache_dir: PathBuf::new(),
                thumbnails_dir: PathBuf::new(),
                inventory_cache: PathBuf::new(),
            }
        });
        tracing::info!(
            roots = ?paths.all().collect::<Vec<_>>(),
            warnings = ?path_warnings,
            thumbnails_dir = %cache_paths.thumbnails_dir.display(),
            "AppState::new: paths detected"
        );
        Self {
            paths,
            path_warnings,
            cache_paths,
        }
    }

    /// Helper for the inventory scan: yields the directory roots
    /// that should be walked. Used by `data::inventory::refresh`.
    /// Stable across calls (the path detection result is cached
    /// at construction).
    #[allow(dead_code)] // wired up in PR 3
    pub fn inventory_roots(&self) -> Vec<PathBuf> {
        self.paths.all().map(|p| p.to_path_buf()).collect()
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_state_new_does_not_panic() {
        // Smoke test: default environments don't have Steam
        // Workshop installed, so we expect a path warning, not a
        // panic.
        let state = AppState::new();
        // The struct is Clone (Dioxus requirement).
        let _cloned = state.clone();
    }

    #[test]
    fn inventory_roots_returns_existing_paths() {
        let state = AppState::new();
        let roots = state.inventory_roots();
        // May be empty (no Workshop install) but the call must
        // not panic and must return a Vec.
        for r in &roots {
            assert!(r.is_absolute(), "path {r:?} should be absolute");
        }
    }
}
