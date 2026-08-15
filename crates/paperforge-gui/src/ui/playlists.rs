//! Playlists panel — list of stored playlists with per-row Apply
//! + Edit (PR 7).
//!
//! PR 3 only rendered names + entry counts (read-only). PR 5 added
//! per-row **Apply**. PR 7 adds per-row **Edit**, which opens the
//! `PlaylistEditor` modal loaded with the full on-disk playlist.
//! Real drag-drop UX lands in PR 7/B on top of the same editor state.

use dioxus::prelude::*;

use crate::data::playlists::PlaylistSummary;

/// Render the playlists panel.
///
/// Props:
/// - `playlists` — list summaries from the on-disk store
/// - `connected` — whether the IPC client is healthy; controls whether
///   the Apply / Edit buttons are interactive (greyed out during
///   reconnect)
/// - `on_apply` — callback fired with the playlist name when the
///   operator clicks Apply on a row
/// - `on_edit` — callback fired with the playlist name when the
///   operator clicks Edit on a row. The root owns the editor signal
///   and the modal; this callback kicks off the async load + open.
#[allow(non_snake_case)]
#[component]
pub fn PlaylistsPanel(
    playlists: Vec<PlaylistSummary>,
    connected: bool,
    on_apply: EventHandler<String>,
    on_edit: EventHandler<String>,
) -> Element {
    rsx! {
        div {
            style: "{crate::ui::theme::PANEL_BORDER} background: #161b22; padding: 0.75rem 1rem; flex: 1;",
            h3 {
                style: "font-size: 0.95rem; margin: 0 0 0.5rem 0; color: #e6edf3;",
                "Playlists"
            }
            if playlists.is_empty() {
                p {
                    style: "color: #8b949e; font-size: 0.875rem;",
                    "No playlists yet. Create one with the CLI: paperforge playlist save NAME."
                }
            } else {
                div {
                    style: "display: flex; flex-direction: column; gap: 0.25rem;",
                    for pl in playlists.into_iter() {
                        {
                            let name_for_apply = pl.name.clone();
                            let name_for_edit = pl.name.clone();
                            rsx! {
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
                                    button {
                                        style: "background: #21262d; color: #e6edf3; border: 1px solid #30363d; border-radius: 4px; padding: 0.2rem 0.6rem; font-size: 0.75rem; cursor: pointer;",
                                        disabled: !connected,
                                        onclick: move |_| {
                                            on_edit.call(name_for_edit.clone());
                                        },
                                        "Edit"
                                    }
                                    button {
                                        style: "background: #238636; color: #ffffff; border: 1px solid #2ea043; border-radius: 4px; padding: 0.2rem 0.6rem; font-size: 0.75rem; cursor: pointer;",
                                        disabled: !connected,
                                        onclick: move |_| {
                                            on_apply.call(name_for_apply.clone());
                                        },
                                        "Apply"
                                    }
                                }
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
