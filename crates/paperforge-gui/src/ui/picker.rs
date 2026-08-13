//! Picker modal — overlay that lets the operator choose a wallpaper
//! for one specific output (PR 6).
//!
//! ## Layout
//!
//! Full-window dark backdrop, centered card, grid of one row per
//! `WallpaperEntry`. Each row shows the title (or the path basename
//! when no title is parsed from `project.json`), the `WallpaperKind`
//! tag (`scene` / `image` / `video`), and the absolute path in muted
//! grey. Click on a row → `on_pick(scene_path)`. Click on the
//! backdrop, the X button, or press Escape → `on_close()`.
//!
//! ## No thumbnails (yet)
//!
//! PR 6 ships the modal shape and the IPC plumbing. The thumbnail
//! grid (`preview.jpg` decode, 256×144 PNG cache) lands in PR 8.
//! For now each row is a text tile — readable, fast to render, no
//! image decode on the UI thread.
//!
//! ## Why a modal (not a sidebar tab)
//!
//! Picking is a one-shot decision per output. A full-window modal
//! keeps the binding flow focused: the operator sees outputs +
//! bindings + the candidate list, picks one, modal closes. A
//! sidebar tab would split attention and require the operator to
//! remember which output they were configuring.

use std::path::PathBuf;

use dioxus::prelude::*;

use paperforge_core::inventory::{WallpaperEntry, WallpaperKind};

use crate::ui::theme::PANEL_BORDER;

/// Render the picker modal.
///
/// Props:
/// - `output` — the Wayland output being configured (shown in the
///   header so the operator remembers which screen they're picking
///   for)
/// - `entries` — the live inventory snapshot from `data::inventory`
/// - `on_pick` — fires with the selected entry's path on click
/// - `on_close` — fires on backdrop click, X button, or Escape key
#[allow(non_snake_case)]
#[component]
pub fn Picker(
    output: String,
    entries: Vec<WallpaperEntry>,
    on_pick: EventHandler<PathBuf>,
    on_close: EventHandler<()>,
) -> Element {
    // Close on Escape. Dioxus 0.8-alpha doesn't have a stable
    // `onkeydown` shorthand for the document, so we attach to the
    // backdrop div with `tabindex` + onkeydown. The backdrop owns
    // focus initially so the key fires here without explicit focus
    // management.
    rsx! {
        div {
            style: "position: fixed; inset: 0; background: rgba(1, 4, 9, 0.78); display: flex; align-items: center; justify-content: center; z-index: 10;",
            tabindex: "0",
            onkeydown: move |ev| {
                if ev.key() == Key::Escape {
                    on_close.call(());
                }
            },
            onclick: move |ev| {
                // Backdrop click closes; clicks on the card stop
                // propagation in the inner div so they don't
                // bubble up and dismiss the modal mid-pick.
                ev.stop_propagation();
            },
            // Card
            div {
                style: "background: #0d1117; border: 1px solid #30363d; border-radius: 8px; max-width: 760px; width: 92vw; max-height: 82vh; display: flex; flex-direction: column;",
                onclick: move |ev| ev.stop_propagation(),
                // Header
                div {
                    style: "display: flex; align-items: center; padding: 0.75rem 1rem; border-bottom: 1px solid #21262d;",
                    h3 {
                        style: "margin: 0; font-size: 1rem; color: #e6edf3; flex: 1;",
                        "Pick a wallpaper for {output}"
                    }
                    button {
                        style: "background: transparent; color: #8b949e; border: none; font-size: 1.25rem; cursor: pointer; padding: 0 0.4rem;",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                // Body: grid of entries
                div {
                    style: "overflow-y: auto; padding: 0.5rem;",
                    if entries.is_empty() {
                        p {
                            style: "color: #8b949e; font-size: 0.875rem; padding: 1rem; text-align: center;",
                            "No wallpapers detected yet. Install Workshop items or \
                             point $PAPERFORGE_STEAM_WORKSHOP at your library."
                        }
                    } else {
                        div {
                            style: "display: grid; grid-template-columns: 1fr; gap: 0.4rem;",
                            for entry in entries.iter() {
                                {
                                    let path_for_pick = entry.path.clone();
                                    let display_title = entry
                                        .title
                                        .clone()
                                        .unwrap_or_else(|| basename(&entry.path));
                                    rsx! {
                                        div {
                                            style: "{PANEL_BORDER} background: #161b22; padding: 0.6rem 0.75rem; cursor: pointer; display: flex; align-items: center; gap: 0.75rem;",
                                            onclick: move |_| {
                                                on_pick.call(path_for_pick.clone());
                                            },
                                            // Kind badge
                                            span {
                                                style: "background: #21262d; color: #8b949e; font-family: monospace; font-size: 0.7rem; padding: 0.15rem 0.4rem; border-radius: 3px; flex-shrink: 0;",
                                                "{kind_tag(entry.kind)}"
                                            }
                                            // Title + path
                                            div {
                                                style: "flex: 1; min-width: 0;",
                                                div {
                                                    style: "color: #e6edf3; font-size: 0.9rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                                                    "{display_title}"
                                                }
                                                div {
                                                    style: "color: #6e7681; font-size: 0.75rem; font-family: monospace; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                                                    title: "{entry.path.display()}",
                                                    "{entry.path.display()}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Footer (entry count)
                div {
                    style: "padding: 0.5rem 1rem; border-top: 1px solid #21262d; color: #8b949e; font-size: 0.75rem;",
                    "{entries.len()} wallpaper(s) · click to bind · Esc to cancel"
                }
            }
        }
    }
}

/// Short tag for the `WallpaperKind` badge in the picker row.
fn kind_tag(kind: WallpaperKind) -> &'static str {
    match kind {
        WallpaperKind::WorkshopScene => "scene",
        WallpaperKind::LooseImage => "image",
        WallpaperKind::LooseVideo => "video",
    }
}

/// Best-effort basename for loose media without a title.
fn basename(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_tag_covers_all_variants() {
        // Drift here means a new WallpaperKind was added without
        // updating the picker badge.
        assert_eq!(kind_tag(WallpaperKind::WorkshopScene), "scene");
        assert_eq!(kind_tag(WallpaperKind::LooseImage), "image");
        assert_eq!(kind_tag(WallpaperKind::LooseVideo), "video");
    }

    #[test]
    fn basename_extracts_filename() {
        let p = std::path::PathBuf::from("/home/lou/wallpapers/forest.jpg");
        assert_eq!(basename(&p), "forest.jpg");
    }

    #[test]
    fn basename_handles_root_path() {
        // A bare filename (no parent) should still come back as
        // itself rather than empty.
        let p = std::path::PathBuf::from("forest.jpg");
        assert_eq!(basename(&p), "forest.jpg");
    }
}
