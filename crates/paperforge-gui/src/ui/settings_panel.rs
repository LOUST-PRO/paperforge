//! SettingsPanel — PR 9.4 — edit `Config::extra_sources`.
//!
//! Exposes the `extra_sources` list (a `Vec<PathBuf>` of wallpapers
//! directories on top of the auto-detected ones) so the operator
//! can add/remove custom wallpaper folders without editing TOML by
//! hand.
//!
//! ## Interactions
//!
//! - **Add**: input box + "+" button. On click, the path is
//!   deduped, persisted via `data::settings::save_config`, and the
//!   operator's `on_change` callback fires so the root can re-scan
//!   the inventory.
//! - **Remove**: per-row "✕" button. Same persistence + re-scan path.
//! - **Validation**: empty strings are rejected; non-existent paths
//!   are accepted (the operator may want to add a path before
//!   creating the directory) but a warning is shown next to the row.
//!
//! ## Why a separate component, not inline in `root.rs`
//!
//! The settings surface is its own state machine — adding a path,
//! showing the "saved" indicator, handling dedup feedback. Pulling
//! it out keeps `root.rs` small and the panel testable as a pure
//! render of `(extra_sources, on_change)`.

use std::path::PathBuf;

use dioxus::prelude::*;

use crate::ui::theme::PANEL_BORDER;

/// Settings panel — read-only list of `extra_sources` plus an
/// "Add path" form. All mutations go through `on_add` / `on_remove`
/// so the root owns the persistence + re-scan logic.
#[allow(non_snake_case)]
#[component]
pub fn SettingsPanel(
    extra_sources: Vec<PathBuf>,
    on_add: EventHandler<PathBuf>,
    on_remove: EventHandler<PathBuf>,
) -> Element {
    // Draft path in the input box. Cleared on successful add.
    let mut draft: Signal<String> = use_signal(String::new);
    // Last-action feedback (success / dedup). Cleared on the next
    // user input. Stored as (text, is_error) so the panel can
    // color it appropriately.
    let mut feedback: Signal<Option<(String, bool)>> = use_signal(|| None);

    rsx! {
        div {
            style: "{PANEL_BORDER} background: #161b22; padding: 0.75rem 1rem; display: flex; flex-direction: column; min-width: 0;",
            h3 {
                style: "font-size: 0.95rem; margin: 0 0 0.5rem 0; color: #e6edf3;",
                "Settings — source paths"
            }
            p {
                style: "color: #8b949e; font-size: 0.85rem; margin: 0 0 0.5rem 0;",
                "Auto-detected workshop paths are always scanned. Add a custom wallpaper \
                 folder below; paperforge will include it in the inventory on the next scan."
            }
            div {
                style: "display: flex; gap: 0.5rem; margin-bottom: 0.5rem;",
                input {
                    style: "flex: 1; background: #0d1117; color: #e6edf3; border: 1px solid #30363d; border-radius: 4px; padding: 0.4rem 0.6rem; font-family: monospace; font-size: 0.85rem;",
                    placeholder: "/home/lou/MisWallpapers",
                    value: "{draft}",
                    oninput: move |ev| {
                        draft.set(ev.value());
                        feedback.set(None);
                    },
                }
                button {
                    style: "background: #238636; color: #ffffff; border: 1px solid #2ea043; border-radius: 4px; padding: 0.4rem 0.9rem; font-size: 0.85rem; cursor: pointer;",
                    disabled: draft.read().trim().is_empty(),
                    onclick: move |_| {
                        let raw = draft.read().trim().to_string();
                        if raw.is_empty() {
                            feedback.set(Some(("Path is empty".to_string(), true)));
                            return;
                        }
                        let path = PathBuf::from(&raw);
                        // Hand off to root — it owns dedup, save, re-scan.
                        on_add.call(path);
                        draft.set(String::new());
                        feedback.set(Some((format!("Added: {raw}"), false)));
                    },
                    "Add"
                }
            }
            if let Some((msg, is_error)) = feedback.cloned() {
                p {
                    style: if is_error {
                        "color: #f85149; font-size: 0.8rem; margin: 0 0 0.5rem 0;"
                    } else {
                        "color: #3fb950; font-size: 0.8rem; margin: 0 0 0.5rem 0;"
                    },
                    "{msg}"
                }
            }
            if extra_sources.is_empty() {
                p {
                    style: "color: #6e7681; font-size: 0.85rem; font-style: italic;",
                    "No custom paths. Workshop content is scanned automatically."
                }
            } else {
                div {
                    style: "display: flex; flex-direction: column; gap: 0.3rem;",
                    for path in extra_sources.iter() {
                        {
                            let path_for_remove = path.clone();
                            let exists = path.exists();
                            let is_dir = path.is_dir();
                            rsx! {
                                div {
                                    key: "{path.display()}",
                                    style: "display: grid; grid-template-columns: 1fr max-content; gap: 0.5rem; align-items: center; padding: 0.3rem 0.5rem; background: #0d1117; border: 1px solid #21262d; border-radius: 4px;",
                                    div {
                                        style: "min-width: 0;",
                                        div {
                                            style: "color: #e6edf3; font-family: monospace; font-size: 0.85rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                                            title: "{path.display()}",
                                            "{path.display()}"
                                        }
                                        if !exists || !is_dir {
                                            span {
                                                style: "color: #d29922; font-size: 0.7rem;",
                                                if !exists {
                                                    "(path does not exist — will be ignored)"
                                                } else {
                                                    "(not a directory — will be ignored)"
                                                }
                                            }
                                        }
                                    }
                                    button {
                                        style: "background: #21262d; color: #f85149; border: 1px solid #30363d; border-radius: 4px; padding: 0.2rem 0.6rem; font-size: 0.75rem; cursor: pointer;",
                                        onclick: move |_| {
                                            on_remove.call(path_for_remove.clone());
                                        },
                                        "✕"
                                    }
                                }
                            }
                        }
                    }
                }
            }
            div {
                style: "color: #8b949e; font-size: 0.75rem; padding-top: 0.5rem; border-top: 1px solid #21262d; margin-top: 0.5rem;",
                "{extra_sources.len()} custom path(s) · persisted to ~/.config/paperforge/config.toml"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Pure UI rendering — the persistence logic lives in
    // `data::settings.rs` and is covered there.
}
