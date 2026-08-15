//! Picker modal — overlay that lets the operator choose a wallpaper
//! for one specific output (PR 6 + thumbnails + preview in PR 8.2).
//!
//! ## Layout
//!
//! Full-window dark backdrop, centered card, two-column layout:
//!
//! - Left column — scrollable list of one row per `WallpaperEntry`.
//!   Each row shows a 256×144 thumbnail (via
//!   `data::thumbnails::load_thumbnail`) on the left, the kind badge,
//!   the title, and the absolute path on the right. Click on a row
//!   → `on_pick(scene_path)`. Hovering updates the right-column
//!   preview pane.
//! - Right column — a 480×270 `<PreviewPane>` showing the hovered
//!   entry's full-size preview (PNG, inlined as a base64 data URL).
//!   When no entry is hovered, the first entry is used so the pane
//!   is never empty.
//!
//! Click on the backdrop, the X button, or press Escape → `on_close()`.
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

use crate::data::thumbnails::{load_thumbnail, ThumbnailState};
use crate::ui::preview::{thumbnail_data_url, PreviewPane};
use crate::ui::theme::PANEL_BORDER;

/// Render the picker modal.
///
/// Props:
/// - `output` — the Wayland output being configured (shown in the
///   header so the operator remembers which screen they're picking
///   for)
/// - `entries` — the live inventory snapshot from `data::inventory`
/// - `cache_dir` — typically `AppState::cache_paths.thumbnails_dir`;
///   passed down so each tile and the preview pane share the same
///   SHA-256 PNG cache
/// - `on_pick` — fires with the selected entry's path on click
/// - `on_close` — fires on backdrop click, X button, or Escape key
#[allow(non_snake_case)]
#[component]
pub fn Picker(
    output: String,
    entries: Vec<WallpaperEntry>,
    cache_dir: PathBuf,
    on_pick: EventHandler<PathBuf>,
    on_close: EventHandler<()>,
) -> Element {
    // Hovered row index. `None` until the first pointerenter; the
    // right preview pane falls back to entries[0] when this is
    // None so the layout doesn't shift.
    let mut hover: Signal<Option<usize>> = use_signal(|| None);

    // The entry whose preview the right pane renders. Defaults to
    // the first entry (so the pane is populated on modal open).
    let preview_entry: Option<WallpaperEntry> = match hover.cloned() {
        Some(idx) => entries.get(idx).cloned(),
        None => entries.first().cloned(),
    };

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
            // Card — wider than PR 6 to host the preview column.
            div {
                style: "background: #0d1117; border: 1px solid #30363d; border-radius: 8px; max-width: 1200px; width: 92vw; max-height: 82vh; display: grid; grid-template-columns: minmax(0, 1fr) 500px; gap: 0;",
                onclick: move |ev| ev.stop_propagation(),
                // LEFT: list of entries
                div {
                    style: "display: flex; flex-direction: column; min-width: 0;",
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
                    // Body: grid of entries with thumbnails
                    div {
                        style: "overflow-y: auto; padding: 0.5rem; flex: 1;",
                        if entries.is_empty() {
                            p {
                                style: "color: #8b949e; font-size: 0.875rem; padding: 1rem; text-align: center;",
                                "No wallpapers detected yet. Install Workshop items or \
                                 point $PAPERFORGE_STEAM_WORKSHOP at your library."
                            }
                        } else {
                            div {
                                style: "display: grid; grid-template-columns: 1fr; gap: 0.4rem;",
                                for (idx, entry) in entries.iter().enumerate() {
                                    {
                                        let path_for_pick = entry.path.clone();
                                        let display_title = entry
                                            .title
                                            .clone()
                                            .unwrap_or_else(|| basename(&entry.path));
                                        let cache_for_tile = cache_dir.clone();
                                        let entry_for_tile = entry.clone();
                                        let is_hovered = hover.cloned() == Some(idx);
                                        rsx! {
                                            div {
                                                key: "{entry.path.display()}",
                                                style: if is_hovered {
                                                    "{PANEL_BORDER} background: #1f6feb22; border-color: #1f6feb; padding: 0.4rem; cursor: pointer; display: flex; align-items: center; gap: 0.75rem;"
                                                } else {
                                                    "{PANEL_BORDER} background: #161b22; padding: 0.4rem; cursor: pointer; display: flex; align-items: center; gap: 0.75rem;"
                                                },
                                                onmouseenter: move |_| hover.set(Some(idx)),
                                                onclick: move |_| {
                                                    on_pick.call(path_for_pick.clone());
                                                },
                                                // Thumbnail
                                                TileThumb {
                                                    entry: entry_for_tile,
                                                    cache_dir: cache_for_tile,
                                                }
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
                // RIGHT: preview pane
                div {
                    style: "padding: 1rem; background: #0a0e14; border-left: 1px solid #21262d; display: flex; flex-direction: column; gap: 0.75rem; align-items: center; justify-content: flex-start;",
                    PreviewPane {
                        entry: preview_entry.clone(),
                        cache_dir: cache_dir.clone(),
                    }
                    if let Some(entry) = preview_entry.as_ref() {
                        div {
                            style: "width: 480px; text-align: center;",
                            div {
                                style: "color: #e6edf3; font-size: 0.9rem;",
                                "{entry.title.clone().unwrap_or_else(|| basename(&entry.path))}"
                            }
                            div {
                                style: "color: #6e7681; font-size: 0.7rem; font-family: monospace; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
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

/// Small tile thumbnail — async loads the 256×144 PNG and renders
/// it as a base64 data URL inline. Errors fall back to the kind
/// badge so the layout doesn't collapse when the source is corrupt
/// or absent.
#[allow(non_snake_case)]
#[component]
fn TileThumb(entry: WallpaperEntry, cache_dir: PathBuf) -> Element {
    let resource = use_resource(move || {
        let entry = entry.clone();
        let cache_dir = cache_dir.clone();
        async move {
            match load_thumbnail(entry, cache_dir).await {
                Ok(s) => s,
                Err(_) => ThumbnailState::None,
            }
        }
    });

    let state: ThumbnailState = resource.cloned().unwrap_or(ThumbnailState::Loading);

    let box_style = "width: 256px; height: 144px; background: #0a0e14; border-radius: 4px; \
         flex-shrink: 0; display: flex; align-items: center; justify-content: center; \
         overflow: hidden; color: #6e7681; font-size: 0.7rem;";

    rsx! {
        div {
            style: "{box_style}",
            match state {
                ThumbnailState::Loading => rsx! {
                    div { "…" }
                },
                ThumbnailState::Ready(bytes) => rsx! {
                    img {
                        src: "{thumbnail_data_url(&bytes)}",
                        style: "width: 100%; height: 100%; object-fit: cover; display: block;",
                    }
                },
                ThumbnailState::Failed(_) | ThumbnailState::None => rsx! {
                    div { "no preview" }
                },
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
