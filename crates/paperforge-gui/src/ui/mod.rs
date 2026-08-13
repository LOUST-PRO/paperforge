//! UI module declarations.
//!
//! Each panel of the GUI lives in its own submodule. The root
//! component (`ui/root.rs`) composes them. Sub-modules grow in
//! later PRs:
//!
//! - `sidebar` — PR 3 (outputs list with state badges)
//! - `bindings` — PR 3 (current `(output, scene)` grid)
//! - `status` — PR 3 (banner UX)
//! - `playlists` — PR 3 (read-only list), PR 7 (drag-drop editor)
//! - `picker` — PR 6 (per-output wallpaper picker)
//! - `preview` — PR 8 (static `preview.jpg` pane)
//!
//! Until each PR lands the module may be empty or absent. We
//! declare them eagerly here so the navigation graph is stable
//! across the sprint.

pub mod bindings;
pub mod picker;
pub mod playlists;
pub mod root;
pub mod sidebar;
pub mod status;
pub mod theme;
