//! Preview pane — large static preview of a wallpaper entry.
//!
//! PR 8.2 ships a 480×270 panel that lives inside the Picker modal
//! as the right column. Renders the entry's `preview.jpg` (Workshop
//! scenes) or the file itself (LooseImage) via the SHA-256 PNG cache
//! introduced in PR 8.1.
//!
//! ## Why a separate component (and not a function inside `Picker`)
//!
//! - Reusable: any future panel that needs a "look at this wallpaper"
//!   affordance (e.g. a per-binding preview next to the bindings grid,
//!   or a `Last picked` chip in the toolbar) just renders a
//!   `<PreviewPane entry=... cache_dir=... />`.
//! - Testable: `thumbnail_data_url` is a pure helper, the resource
//!   lifecycle is owned by the component, and the rendering branch
//!   table is small (4 cases).
//!
//! ## State mapping
//!
//! The async [`load_thumbnail`](crate::data::thumbnails::load_thumbnail)
//! returns a [`ThumbnailState`] that the pane maps to a render
//! branch. Errors are downgraded to `None` so a corrupt preview.jpg
//! doesn't paint a red banner — the title-only fallback covers the
//! gap until the operator refreshes the inventory.
//!
//! ## Data URL encoding
//!
//! Dioxus 0.8-alpha supports `<img src="data:image/png;base64,...">`
//! in the WebView. The PNG bytes from the cache are base64-encoded
//! inline. Phase 2 (Fase 6C.2) may switch to a `dioxus-desktop`
//! custom protocol scheme if larger inventories make the inline
//! payload too heavy — until then, `data:` URLs are the simplest
//! path.

use std::path::PathBuf;

use base64::Engine;
use dioxus::prelude::*;
use paperforge_core::inventory::WallpaperEntry;

use crate::data::thumbnails::{load_thumbnail, Bytes, ThumbnailState};

/// Render the preview pane for an `Option<WallpaperEntry>`.
///
/// `None` (e.g. modal just opened, no hover yet) renders a quiet
/// placeholder so the layout doesn't shift on first hover. The
/// resource reruns when `entry` or `cache_dir` change, so swapping
/// the hovered row in the picker triggers a fresh decode.
#[allow(non_snake_case)]
#[component]
pub fn PreviewPane(entry: Option<WallpaperEntry>, cache_dir: PathBuf) -> Element {
    let resource = use_resource(move || {
        let entry = entry.clone();
        let cache_dir = cache_dir.clone();
        async move {
            match entry {
                None => ThumbnailState::None,
                Some(e) => match load_thumbnail(e, cache_dir).await {
                    Ok(s) => s,
                    Err(_) => ThumbnailState::None,
                },
            }
        }
    });

    let state: ThumbnailState = resource.cloned().unwrap_or(ThumbnailState::Loading);

    rsx! {
        div {
            style: "display: flex; flex-direction: column; align-items: stretch; justify-content: center; background: #161b22; border: 1px solid #21262d; border-radius: 6px; width: 480px; height: 270px; overflow: hidden;",
            match state {
                ThumbnailState::Loading => rsx! {
                    div {
                        style: "flex: 1; display: flex; align-items: center; justify-content: center; color: #8b949e; font-size: 0.875rem;",
                        "Loading preview…"
                    }
                },
                ThumbnailState::Ready(bytes) => rsx! {
                    img {
                        src: "{thumbnail_data_url(&bytes)}",
                        style: "width: 100%; height: 100%; object-fit: cover; display: block;",
                    }
                },
                ThumbnailState::Failed(_) => rsx! {
                    div {
                        style: "flex: 1; display: flex; flex-direction: column; align-items: center; justify-content: center; color: #8b949e; font-size: 0.875rem; padding: 1rem; text-align: center;",
                        div { "Preview unavailable" }
                        div {
                            style: "font-size: 0.75rem; margin-top: 0.4rem; color: #6e7681;",
                            "Source file could not be decoded."
                        }
                    }
                },
                ThumbnailState::None => rsx! {
                    div {
                        style: "flex: 1; display: flex; flex-direction: column; align-items: center; justify-content: center; color: #8b949e; font-size: 0.875rem; padding: 1rem; text-align: center;",
                        div { "No preview available" }
                        div {
                            style: "font-size: 0.75rem; margin-top: 0.4rem; color: #6e7681;",
                            "Workshop scenes ship a preview.jpg; loose videos get ffmpeg first-frame in Fase 6C.2."
                        }
                    }
                },
            }
        }
    }
}

/// Encode PNG bytes as a `data:image/png;base64,...` URL for
/// inlining inside `<img src=...>`. Public so unit tests can pin
/// the format (engine + padding choice).
pub fn thumbnail_data_url(bytes: &Bytes) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("data:image/png;base64,{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_url_pins_format_and_prefix() {
        // The WebView only accepts this exact prefix; pin it so
        // a base64 crate bump doesn't silently break the picker.
        let bytes: Bytes = b"\x89PNG\r\n\x1a\nhello".to_vec();
        let url = thumbnail_data_url(&bytes);
        assert!(url.starts_with("data:image/png;base64,"));
        // The base64 alphabet is `[A-Za-z0-9+/=]`; the trailing
        // `=` count depends on byte length mod 3.
        let payload = url.trim_start_matches("data:image/png;base64,");
        assert!(!payload.is_empty());
        assert!(payload
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }

    #[test]
    fn data_url_round_trips_through_engine() {
        // Sanity: decode the URL body and compare to the input.
        let bytes: Bytes = b"\x89PNG\r\n\x1a\nfake bytes".to_vec();
        let url = thumbnail_data_url(&bytes);
        let payload = url.trim_start_matches("data:image/png;base64,");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn data_url_handles_empty_payload() {
        // An empty PNG is a legitimate edge case (e.g. an inventory
        // scan during teardown). The function must not panic.
        let bytes: Bytes = Vec::new();
        let url = thumbnail_data_url(&bytes);
        assert_eq!(url, "data:image/png;base64,");
    }
}
