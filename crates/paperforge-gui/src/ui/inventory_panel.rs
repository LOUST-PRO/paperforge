//! InventoryPanel — persistent, always-visible wallpapers browser.
//!
//! PR 9.3: The picker modal is one-shot per output (you pick one
//! wallpaper and it closes). On compositors like niri that don't
//! expose a sway/Hyprland IPC, `outputs` is always empty, so the
//! Picker was unreachable from the UI. This panel is the always-on
//! counterpart: it lists every entry in the inventory with a 256×144
//! thumbnail (loaded via the same `data::thumbnails::load_thumbnail`
//! path the Picker uses) and a click on a row triggers
//! `on_browse(path)` — a callback the root composes to do whatever
//! makes sense for the operator's flow (e.g. open the Picker with
//! the entry pre-selected, or push to the preview pane).
//!
//! Why a separate component (not "just open the Picker always"):
//!   - Picker is per-output, this panel is output-agnostic.
//!   - Picker has a preview column (480×270) that the inventory
//!     browse view doesn't need (preview is a separate panel,
//!     wired via `on_browse`).
//!   - Keeping it light means the panel stays responsive even
//!     with 825+ entries; the Picker would re-mount and re-load
//!     every entry's thumbnail every time the user re-opens it.
//!
//! The scroll container is bounded by `max-height: 50vh` so the
//! layout below (status bar, footer) always remains visible.

use std::path::PathBuf;

use dioxus::prelude::*;

use paperforge_core::inventory::{WallpaperEntry, WallpaperKind};

use crate::data::thumbnails::{load_thumbnail, ThumbnailState};
use crate::ui::theme::PANEL_BORDER;

/// Always-visible inventory browser. Shows every entry with a
/// thumbnail, kind badge, and title. Click → `on_browse(path)`.
///
/// Props:
/// - `entries` — live inventory snapshot
/// - `cache_dir` — same thumbnails dir as the Picker; both share
///   the SHA-256 PNG cache so the second consumer is a fast
///   cache-hit
/// - `on_browse` — fired with the entry path on row click. Root
///   composes this; typically forwards to the preview pane or to
///   "apply to selected output" logic.
#[allow(non_snake_case)]
#[component]
pub fn InventoryPanel(
    entries: Vec<WallpaperEntry>,
    cache_dir: PathBuf,
    on_browse: EventHandler<PathBuf>,
) -> Element {
    // Hovered index — purely cosmetic, highlights the row.
    let mut hover: Signal<Option<usize>> = use_signal(|| None);

    rsx! {
        div {
            // PR 9.5: was `padding + display: flex; flex-direction: column`
            // only. The parent (root.rs) now wraps us in a
            // `flex: 1; min-height: 0` column, so we add `flex: 1; min-height: 0`
            // here so our panel grows to fill that space, and the
            // internal scroll area can shrink inside it. Without
            // `min-height: 0` flex children won't shrink below their
            // content size and the scroll area collapses.
            style: "{PANEL_BORDER} background: #161b22; padding: 0.75rem 1rem; display: flex; flex-direction: column; min-width: 0; flex: 1; min-height: 0;",
            div {
                style: "display: flex; align-items: baseline; gap: 0.75rem; margin-bottom: 0.4rem;",
                h3 {
                    style: "font-size: 0.95rem; margin: 0; color: #e6edf3;",
                    "Inventario"
                }
                span {
                    style: "color: #8b949e; font-size: 0.875rem;",
                    if entries.is_empty() {
                        "No wallpapers detected"
                    } else {
                        "{entries.len()} entries · click to preview"
                    }
                }
            }
            if entries.is_empty() {
                p {
                    style: "color: #8b949e; font-size: 0.875rem;",
                    "Add a path via Settings → Source paths, or install Workshop items into your Steam library."
                }
            } else {
                div {
                    // PR 9.5: was `overflow-y: auto; max-height: 50vh`
                    // which capped the inventory at half the viewport.
                    // Now `flex: 1; min-height: 0` lets it grow to fill
                    // the parent (which is the flex-grow column in
                    // root.rs). The scroll area adapts to whatever
                    // vertical space is left after the header.
                    style: "overflow-y: auto; flex: 1; min-height: 0; padding-right: 0.25rem;",
                    div {
                        style: "display: grid; grid-template-columns: 1fr; gap: 0.4rem;",
                        for (idx, entry) in entries.iter().enumerate() {
                            {
                                let path_for_browse = entry.path.clone();
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
                                            "{PANEL_BORDER} background: #0d1117; padding: 0.4rem; cursor: pointer; display: flex; align-items: center; gap: 0.75rem;"
                                        },
                                        onmouseenter: move |_| hover.set(Some(idx)),
                                        onclick: move |_| {
                                            on_browse.call(path_for_browse.clone());
                                        },
                                        TileThumb {
                                            entry: entry_for_tile,
                                            cache_dir: cache_for_tile,
                                        }
                                        span {
                                            style: "background: #21262d; color: #8b949e; font-family: monospace; font-size: 0.7rem; padding: 0.15rem 0.4rem; border-radius: 3px; flex-shrink: 0;",
                                            "{kind_tag(entry.kind)}"
                                        }
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
        }
    }
}

/// Local copy of the picker tile component. Kept private to this
/// module to avoid coupling — the public version in `picker.rs` is
/// used by the modal and may diverge (e.g. add a preview tooltip).
/// When the divergence stabilises, we should promote one to
/// `crate::ui::tile_thumb`.
#[allow(non_snake_case)]
#[component]
fn TileThumb(entry: WallpaperEntry, cache_dir: PathBuf) -> Element {
    // Capture `kind` for the failure-mode placeholder label before
    // `entry` itself moves into the resource closure. Without the
    // clone, the `_ | None` arm below can't render the kind tag.
    let kind_for_placeholder = entry.kind;
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

    let box_style = "width: 128px; height: 72px; background: #0a0e14; border-radius: 4px; \
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
                        src: "{crate::ui::preview::thumbnail_data_url(&bytes)}",
                        style: "width: 100%; height: 100%; object-fit: cover; display: block;",
                    }
                },
                ThumbnailState::Failed(_) | ThumbnailState::None => rsx! {
                    div {
                        style: "color: #6e7681; font-size: 0.65rem; font-family: monospace;",
                        "{kind_short(kind_for_placeholder)}"
                    }
                },
            }
        }
    }
}

fn kind_tag(kind: WallpaperKind) -> &'static str {
    match kind {
        WallpaperKind::WorkshopScene => "scene",
        WallpaperKind::LooseImage => "image",
        WallpaperKind::LooseVideo => "video",
    }
}

fn kind_short(kind: WallpaperKind) -> &'static str {
    match kind {
        WallpaperKind::WorkshopScene => "scene",
        WallpaperKind::LooseImage => "img",
        WallpaperKind::LooseVideo => "vid",
    }
}

fn basename(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("<unknown>")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_tag_covers_all_variants() {
        // Belt-and-braces: if WallpaperKind gets a new variant, the
        // tag map will fail to compile and surface the missing case.
        assert_eq!(kind_tag(WallpaperKind::WorkshopScene), "scene");
        assert_eq!(kind_tag(WallpaperKind::LooseImage), "image");
        assert_eq!(kind_tag(WallpaperKind::LooseVideo), "video");
    }

    #[test]
    fn basename_extracts_filename() {
        assert_eq!(
            basename(std::path::Path::new("/home/lou/Wallpapers/sample.mp4")),
            "sample.mp4"
        );
    }
}
