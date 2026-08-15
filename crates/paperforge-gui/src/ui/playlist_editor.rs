//! Playlist editor modal — click-to-add editor (PR 7/A).
//!
//! ## Layout
//!
//! Full-window overlay, centered card. Body splits into:
//!
//! - Header: playlist name (read-only) + description preview
//! - Body: ordered list of wallpapers, each row with `↑` / `↓`
//!   reorder buttons + a `Remove` button. Plus an `Add` button at
//!   the bottom that opens the per-output `Picker` (PR 6) via
//!   parent signal — the editor doesn't own a nested picker.
//! - Footer: `Save` (writes to disk via `data::playlists::save_playlist`)
//!   and `Cancel` (closes without writing).
//!
//! ## Why click-to-add first (not drag-drop)
//!
//! The Dioxus 0.8-alpha drag-drop story on Linux is shaky (Dioxus
//! issue #3961 — events don't fire on WebKitGTK for some
//! drag-target combinations). Per the Fase 6C plan, click-to-add
//! is the **contract**; drag-drop is the **enhancement**. PR 7/A
//! ships the click-to-add UX with full save support. PR 7/B will
//! layer `ondragstart` / `ondragover` / `ondrop` on top of the
//! same `editor_draft` state, reusing the `DragPayload` enum from
//! `app.rs`.
//!
//! ## State model (Signal-driven)
//!
//! The editor receives `draft: Signal<Option<OpenEditor>>` from
//! the parent. The parent sets `Some(OpenEditor)` to open, `None`
//! to close. Inside the editor:
//!
//! - Read with `draft.cloned().unwrap()` to render the current
//!   row list (cheap, `Signal` is `Copy`).
//! - Mutate with `draft.set(Some(new_state))` from the row buttons
//!   (`↑` / `↓` / `Remove`). Signals propagate back to the parent
//!   re-render so the editor stays consistent across renders.
//! - `Add` fires `on_add_requested(output_target)` — the parent
//!   reuses the existing `Picker` modal (PR 6) instead of nesting
//!   a second picker inside the editor. When the picker resolves,
//!   the parent appends the path to `draft` and closes the picker.
//!
//! `Save` fires `on_save(Playlist)` — the parent calls
//! `data::playlists::save_playlist` and closes the editor on
//! success. `Cancel` fires `on_cancel()` — the parent clears the
//! signal.

use std::path::PathBuf;

use dioxus::prelude::*;

use paperforge_core::inventory::{WallpaperEntry, WallpaperKind};
use paperforge_core::playlist::Playlist;

use crate::ui::theme::PANEL_BORDER;

/// What the editor is currently editing + the live draft.
///
/// The root owns `editor_draft: Signal<Option<OpenEditor>>`;
/// setting it to `Some(_)` opens the editor, `None` closes it.
/// The `draft` is the live editable state — the editor never
/// mutates the on-disk playlist until Save.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenEditor {
    /// Playlist name (matches the on-disk file stem).
    pub name: String,
    /// Editable draft. The original on-disk playlist is unchanged
    /// until the operator hits Save.
    pub draft: Playlist,
}

/// Render the playlist editor modal.
///
/// Props:
/// - `draft` — current open-editor state (read snapshot + Signal
///   for mutation). The render reads `draft.cloned().unwrap()`;
///   the row buttons call `draft.set(Some(new))` to mutate.
/// - `picker_target` — when set, the editor shows an "Add
///   wallpaper" mini-picker (a stripped-down inventory grid); when
///   cleared, the picker doesn't render. This is wired to the
///   parent's `picker_open_for` signal so the user can use the
///   same Picker UX as PR 6, but the editor owns no picker
///   component — it just consumes an `open_or_close` flag.
/// - `inventory` — wallpapers available to add
/// - `on_save` — fires with the draft `Playlist` when the operator
///   hits Save. The parent calls `data::playlists::save_playlist`
///   and closes the editor on success.
/// - `on_cancel` — fires when the operator hits Cancel. The parent
///   clears the editor signal.
#[allow(non_snake_case)]
#[component]
pub fn PlaylistEditor(
    draft: Signal<Option<OpenEditor>>,
    show_picker: Signal<bool>,
    inventory: Vec<WallpaperEntry>,
    on_save: EventHandler<Playlist>,
    on_cancel: EventHandler<()>,
) -> Element {
    let snapshot = draft.cloned().unwrap();
    let wallpapers_len = snapshot.draft.wallpapers.len();

    rsx! {
        div {
            style: "position: fixed; inset: 0; background: rgba(1, 4, 9, 0.78); display: flex; align-items: center; justify-content: center; z-index: 12;",
            tabindex: "0",
            onclick: move |ev| ev.stop_propagation(),
            // Card
            div {
                style: "background: #0d1117; border: 1px solid #30363d; border-radius: 8px; max-width: 760px; width: 94vw; max-height: 86vh; display: flex; flex-direction: column;",
                onclick: move |ev| ev.stop_propagation(),
                // Header
                div {
                    style: "display: flex; align-items: center; padding: 0.75rem 1rem; border-bottom: 1px solid #21262d;",
                    h3 {
                        style: "margin: 0; font-size: 1rem; color: #e6edf3; flex: 1;",
                        "Edit playlist: {snapshot.name}"
                    }
                    button {
                        style: "background: transparent; color: #8b949e; border: none; font-size: 1.25rem; cursor: pointer; padding: 0 0.4rem;",
                        onclick: move |_| on_cancel.call(()),
                        "×"
                    }
                }
                // Description preview (read-only in PR 7/A; editable
                // input lands in PR 7/D when we add a controlled
                // text input pattern).
                if let Some(desc) = snapshot.draft.description.as_deref() {
                    div {
                        style: "padding: 0.4rem 1rem; border-bottom: 1px solid #21262d; color: #8b949e; font-size: 0.8125rem; font-style: italic;",
                        "{desc}"
                    }
                }
                // Body: ordered wallpapers list
                div {
                    style: "overflow-y: auto; padding: 0.5rem 0.75rem; flex: 1;",
                    if snapshot.draft.wallpapers.is_empty() {
                        p {
                            style: "color: #8b949e; font-size: 0.875rem; padding: 1rem; text-align: center;",
                            "No wallpapers. Use the Add button below to pick from the inventory."
                        }
                    } else {
                        div {
                            style: "display: flex; flex-direction: column; gap: 0.4rem;",
                            for (i, wp) in snapshot.draft.wallpapers.iter().enumerate() {
                                {
                                    let display = wp
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .map(|s| s.to_string())
                                        .unwrap_or_else(|| wp.display().to_string());
                                    let is_first = i == 0;
                                    let is_last = i + 1 == snapshot.draft.wallpapers.len();
                                    let wp_for_remove = wp.clone();
                                    let mut draft_for_up = draft;
                                    let mut draft_for_down = draft;
                                    let mut draft_for_remove = draft;
                                    rsx! {
                                        div {
                                            style: "{PANEL_BORDER} background: #161b22; padding: 0.5rem 0.75rem; display: flex; align-items: center; gap: 0.5rem;",
                                            span {
                                                style: "color: #8b949e; font-family: monospace; font-size: 0.75rem; width: 1.5rem; text-align: right;",
                                                "{i + 1}"
                                            }
                                            span {
                                                style: "color: #e6edf3; font-size: 0.875rem; flex: 1; font-family: monospace; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                                                title: "{wp.display()}",
                                                "{display}"
                                            }
                                            // Up button
                                            button {
                                                style: "background: #21262d; color: #e6edf3; border: 1px solid #30363d; border-radius: 4px; padding: 0.15rem 0.4rem; font-size: 0.75rem; cursor: pointer;",
                                                disabled: is_first,
                                                onclick: move |_| {
                                                    if let Some(mut cur) = draft_for_up.cloned() {
                                                        if i > 0 {
                                                            cur.draft.wallpapers.swap(i, i - 1);
                                                            draft_for_up.set(Some(cur));
                                                        }
                                                    }
                                                },
                                                "↑"
                                            }
                                            // Down button
                                            button {
                                                style: "background: #21262d; color: #e6edf3; border: 1px solid #30363d; border-radius: 4px; padding: 0.15rem 0.4rem; font-size: 0.75rem; cursor: pointer;",
                                                disabled: is_last,
                                                onclick: move |_| {
                                                    if let Some(mut cur) = draft_for_down.cloned() {
                                                        if i + 1 < cur.draft.wallpapers.len() {
                                                            cur.draft.wallpapers.swap(i, i + 1);
                                                            draft_for_down.set(Some(cur));
                                                        }
                                                    }
                                                },
                                                "↓"
                                            }
                                            // Remove button
                                            button {
                                                style: "background: #5a1f1f; color: #ffdcd7; border: 1px solid #6e2a2a; border-radius: 4px; padding: 0.15rem 0.4rem; font-size: 0.75rem; cursor: pointer;",
                                                onclick: move |_| {
                                                    if let Some(mut cur) = draft_for_remove.cloned() {
                                                        cur.draft.wallpapers.retain(|p| p != &wp_for_remove);
                                                        draft_for_remove.set(Some(cur));
                                                    }
                                                },
                                                "×"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Add picker
                div {
                    style: "padding: 0.5rem 1rem; border-top: 1px solid #21262d;",
                    if !show_picker() {
                        button {
                            style: "background: #1f6feb; color: #ffffff; border: 1px solid #388bfd; border-radius: 4px; padding: 0.4rem 0.9rem; font-size: 0.8125rem; cursor: pointer;",
                            onclick: move |_| show_picker.set(true),
                            "+ Add wallpaper"
                        }
                    } else {
                        EditorPicker {
                            entries: inventory.clone(),
                            on_pick: move |path: PathBuf| {
                                if let Some(mut cur) = draft.cloned() {
                                    if !cur.draft.wallpapers.contains(&path) {
                                        cur.draft.wallpapers.push(path);
                                        draft.set(Some(cur));
                                    }
                                }
                                show_picker.set(false);
                            },
                            on_cancel: move |_| show_picker.set(false),
                        }
                    }
                }
                // Footer
                div {
                    style: "padding: 0.6rem 1rem; border-top: 1px solid #21262d; display: flex; justify-content: flex-end; gap: 0.5rem;",
                    span {
                        style: "color: #8b949e; font-size: 0.75rem; align-self: center; margin-right: auto;",
                        "{wallpapers_len} wallpaper(s) · click ↑↓ to reorder · × removes"
                    }
                    button {
                        style: "background: #21262d; color: #e6edf3; border: 1px solid #30363d; border-radius: 4px; padding: 0.35rem 0.9rem; font-size: 0.8125rem; cursor: pointer;",
                        onclick: move |_| on_cancel.call(()),
                        "Cancel"
                    }
                    button {
                        style: "background: #238636; color: #ffffff; border: 1px solid #2ea043; border-radius: 4px; padding: 0.35rem 0.9rem; font-size: 0.8125rem; cursor: pointer;",
                        onclick: {
                            let cur = draft.cloned().unwrap();
                            move |_| on_save.call(cur.draft.clone())
                        },
                        "Save"
                    }
                }
            }
        }
    }
}

/// Second-level picker — inline list of `WallpaperEntry` rows
/// embedded in the editor card. Lighter than the per-output
/// `Picker` (no overlay, no Escape handling) because it lives
/// inside the editor modal.
#[allow(non_snake_case)]
#[component]
fn EditorPicker(
    entries: Vec<WallpaperEntry>,
    on_pick: EventHandler<PathBuf>,
    on_cancel: EventHandler<()>,
) -> Element {
    rsx! {
        div {
            style: "background: #161b22; padding: 0.5rem; border-radius: 4px; max-height: 240px; overflow-y: auto;",
            div {
                style: "display: flex; align-items: center; margin-bottom: 0.4rem;",
                span {
                    style: "color: #8b949e; font-size: 0.75rem; flex: 1;",
                    "Pick a wallpaper to add ({entries.len()} available)"
                }
                button {
                    style: "background: transparent; color: #8b949e; border: none; font-size: 0.875rem; cursor: pointer;",
                    onclick: move |_| on_cancel.call(()),
                    "×"
                }
            }
            for entry in entries.iter() {
                {
                    let path_for_pick = entry.path.clone();
                    let display = entry
                        .title
                        .clone()
                        .unwrap_or_else(|| entry.path.display().to_string());
                    rsx! {
                        div {
                            style: "padding: 0.3rem 0.5rem; cursor: pointer; border-radius: 3px; display: flex; align-items: center; gap: 0.5rem;",
                            onclick: move |_| on_pick.call(path_for_pick.clone()),
                            span {
                                style: "background: #21262d; color: #8b949e; font-family: monospace; font-size: 0.65rem; padding: 0.1rem 0.3rem; border-radius: 3px;",
                                "{kind_short(entry.kind)}"
                            }
                            span {
                                style: "color: #e6edf3; font-size: 0.8rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1;",
                                title: "{entry.path.display()}",
                                "{display}"
                            }
                        }
                    }
                }
            }
            if entries.is_empty() {
                p {
                    style: "color: #8b949e; font-size: 0.75rem; padding: 0.5rem; text-align: center;",
                    "No wallpapers detected."
                }
            }
        }
    }
}

fn kind_short(kind: WallpaperKind) -> &'static str {
    match kind {
        WallpaperKind::WorkshopScene => "scene",
        WallpaperKind::LooseImage => "image",
        WallpaperKind::LooseVideo => "video",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_short_matches_badge_taxonomy() {
        // Same taxonomy as ui/picker.rs; keep them in sync.
        assert_eq!(kind_short(WallpaperKind::WorkshopScene), "scene");
        assert_eq!(kind_short(WallpaperKind::LooseImage), "image");
        assert_eq!(kind_short(WallpaperKind::LooseVideo), "video");
    }
}
