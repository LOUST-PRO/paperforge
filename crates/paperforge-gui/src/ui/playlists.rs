//! Playlists panel — read-only list of stored playlists.
//!
//! PR 3 only renders the names + entry counts. Apply/edit buttons are
//! PR 5 (apply) and PR 7 (drag-drop editor). The shape is final, so
//! the visual layout stays stable across the PR boundary.

use dioxus::prelude::*;

use crate::data::playlists::PlaylistSummary;
use crate::ui::theme::PANEL_BORDER;

/// Render the playlists panel.
///
/// Empty `playlists` renders a placeholder explaining that no
/// playlists have been created yet. The actual create / apply / edit
/// controls land in PR 5 and PR 7 — for PR 3 we only show the list
/// so the user knows the on-disk state.
#[allow(non_snake_case)]
#[component]
pub fn PlaylistsPanel(playlists: Vec<PlaylistSummary>) -> Element {
    rsx! {
        div {
            style: "{PANEL_BORDER} background: #161b22; padding: 0.75rem 1rem; flex: 1;",
            h3 {
                style: "font-size: 0.95rem; margin: 0 0 0.5rem 0; color: #e6edf3;",
                "Playlists"
            }
            if playlists.is_empty() {
                p {
                    style: "color: #8b949e; font-size: 0.875rem;",
                    "No playlists yet. Apply / edit controls arrive in PR 5+."
                }
            } else {
                div {
                    style: "display: flex; flex-direction: column; gap: 0.25rem;",
                    for pl in playlists.iter() {
                        div {
                            style: "display: flex; align-items: center; gap: 0.5rem; padding: 0.25rem 0;",
                            span {
                                style: "font-family: monospace; flex: 1;",
                                "{pl.name}"
                            }
                            span {
                                style: "color: #8b949e; font-size: 0.75rem;",
                                "{pl.wallpapers} wp · {pl.outputs} out"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Pure UI rendering — no logic worth unit-testing beyond
    // `PlaylistSummary` shape (covered in `data/playlists.rs`).
}
