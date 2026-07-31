//! Application state for the TUI.
//!
//! Tracks the focused panel, the selection within each panel, and
//! the latest [`Snapshot`](crate::data::Snapshot) of fetched data.
//! Designed for read-only debug visualization — does not mutate
//! any external state.

use std::time::Instant;

use paperforge_core::error::Error;

use crate::data::Snapshot;

/// Which panel is currently focused (keyboard navigation target).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// Wayland outputs (DP-1, eDP-1, ...).
    Outputs,
    /// Running LWE PIDs.
    Running,
    /// Playlists (focus, relax, ...).
    Playlists,
    /// Inventory (wallpapers found on disk).
    Inventory,
}

impl Focus {
    /// All variants in render order (left-to-right top, then bottom).
    pub const ALL: [Focus; 4] = [
        Focus::Outputs,
        Focus::Running,
        Focus::Playlists,
        Focus::Inventory,
    ];

    /// Move to the next panel, wrapping at the end.
    pub fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    /// Move to the previous panel, wrapping at the start.
    pub fn prev(self) -> Self {
        let idx = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// A fetch failure from one of the data sources.
#[derive(Debug, Clone)]
pub struct DataError {
    /// Which panel reported the failure (e.g. `"inventory"`).
    pub source: String,
    /// Human-readable error message (already formatted).
    pub message: String,
}

impl DataError {
    /// Construct from a [`paperforge_core::error::Error`].
    pub fn new(source: impl Into<String>, err: Error) -> Self {
        Self {
            source: source.into(),
            message: format!("{err}"),
        }
    }
}

/// Aggregate application state.
///
/// Cheap to clone (the snapshot is the only heavy field, and it's
/// already cheap to clone — Vec<struct> + small strings).
#[derive(Debug, Clone)]
pub struct App {
    /// Which panel has keyboard focus.
    pub focus: Focus,
    /// Selected row index within the focused panel.
    pub selection: usize,
    /// Latest snapshot of fetched data.
    pub snapshot: Snapshot,
    /// App start time (for uptime display).
    pub started_at: Instant,
    /// Most recent status message (success/error from manual refresh).
    pub status: Option<String>,
    /// Whether the user requested a quit (`q` / `Esc`).
    pub should_quit: bool,
}

impl App {
    /// Construct a fresh app with empty data + focus on Outputs.
    pub fn new() -> Self {
        Self {
            focus: Focus::Outputs,
            selection: 0,
            snapshot: Snapshot::empty(),
            started_at: Instant::now(),
            status: None,
            should_quit: false,
        }
    }

    /// Mark the app for graceful shutdown. The render loop checks
    /// this each tick and exits cleanly.
    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Set a transient status message (overwrites any prior).
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some(msg.into());
    }

    /// Length of the row list for the currently-focused panel.
    pub fn current_rows(&self) -> usize {
        match self.focus {
            Focus::Outputs => self.snapshot.outputs.len(),
            Focus::Running => self.snapshot.running.len(),
            Focus::Playlists => self.snapshot.playlists.len(),
            Focus::Inventory => self.snapshot.inventory.len(),
        }
    }

    /// Move selection up by one (clamped at 0).
    pub fn selection_up(&mut self) {
        if self.selection > 0 {
            self.selection -= 1;
        }
    }

    /// Move selection down by one (clamped at end-of-list).
    pub fn selection_down(&mut self) {
        let max = self.current_rows().saturating_sub(1);
        if self.selection < max {
            self.selection += 1;
        }
    }

    /// Clamp selection after data refresh (in case rows shrank).
    pub fn clamp_selection(&mut self) {
        let max = self.current_rows().saturating_sub(1);
        if self.selection > max {
            self.selection = max;
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_cycles_through_all_panels() {
        let f = Focus::Outputs;
        assert_eq!(f.next(), Focus::Running);
        assert_eq!(Focus::Running.next(), Focus::Playlists);
        assert_eq!(Focus::Playlists.next(), Focus::Inventory);
        assert_eq!(Focus::Inventory.next(), Focus::Outputs, "wraps around");
    }

    #[test]
    fn focus_prev_wraps_around() {
        assert_eq!(Focus::Outputs.prev(), Focus::Inventory, "wraps backward");
        assert_eq!(Focus::Running.prev(), Focus::Outputs);
    }

    #[test]
    fn app_default_focus_is_outputs() {
        let app = App::new();
        assert_eq!(app.focus, Focus::Outputs);
        assert_eq!(app.selection, 0);
        assert!(!app.should_quit);
    }

    #[test]
    fn app_quit_sets_flag() {
        let mut app = App::new();
        app.quit();
        assert!(app.should_quit);
    }

    #[test]
    fn app_selection_up_clamped_at_zero() {
        let mut app = App::new();
        app.selection_up();
        assert_eq!(app.selection, 0);
    }

    #[test]
    fn app_selection_down_clamps_at_rows_minus_one() {
        let mut app = App::new();
        // Empty snapshot: rows = 0 → max = 0 → cannot go down.
        app.selection_down();
        assert_eq!(app.selection, 0);
    }

    #[test]
    fn app_clamp_selection_when_data_shrinks() {
        let mut app = App::new();
        app.selection = 5;
        // Empty snapshot: max = 0 → clamp down to 0.
        app.clamp_selection();
        assert_eq!(app.selection, 0);
    }

    #[test]
    fn data_error_carries_source_and_message() {
        let e = DataError::new("inventory", Error::Other(anyhow::anyhow!("oops")));
        assert_eq!(e.source, "inventory");
        assert!(e.message.contains("oops"));
    }

    #[test]
    fn app_status_overwrites() {
        let mut app = App::new();
        app.set_status("first");
        app.set_status("second");
        assert_eq!(app.status.as_deref(), Some("second"));
    }
}
