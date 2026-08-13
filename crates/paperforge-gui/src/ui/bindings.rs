//! Bindings grid — current `(output, scene_path)` map.
//!
//! PR 3 showed a "Waiting for daemon..." placeholder. PR 4 wired the
//! `bindings` signal from D-Bus `WallpaperStarted` / `WallpaperStopped`
//! signals. PR 5 adds a per-row **Unset** button that calls
//! `data::bindings::unset_binding` — the IPC verb lives in the data
//! layer, this component just renders the click target.

use dioxus::prelude::*;

use crate::data::bindings::Binding;
use crate::ui::theme::PANEL_BORDER;

/// Render the bindings grid.
///
/// Props:
/// - `bindings` — current live map from D-Bus signals
/// - `connected` — whether the IPC client is healthy; controls whether
///   the Unset buttons are interactive (greyed out during reconnect)
/// - `on_unset` — callback fired with the output name when the
///   operator clicks the Unset button on a row
#[allow(non_snake_case)]
#[component]
pub fn BindingsPanel(
    bindings: Vec<Binding>,
    connected: bool,
    on_unset: EventHandler<String>,
) -> Element {
    rsx! {
        div {
            style: "{PANEL_BORDER} background: #161b22; padding: 0.75rem 1rem; flex: 1;",
            h3 {
                style: "font-size: 0.95rem; margin: 0 0 0.5rem 0; color: #e6edf3;",
                "Current bindings"
            }
            if bindings.is_empty() {
                p {
                    style: "color: #8b949e; font-size: 0.875rem;",
                    if connected {
                        "No bindings yet. Use the picker (PR 6) to bind a wallpaper."
                    } else {
                        "Waiting for daemon connection…"
                    }
                }
            } else {
                div {
                    style: "display: grid; grid-template-columns: max-content 1fr max-content max-content; gap: 0.5rem 1rem; align-items: center;",
                    for b in bindings.into_iter() {
                        {
                            let output_name = b.output.clone();
                            rsx! {
                                span {
                                    style: "font-family: monospace; color: #79c0ff;",
                                    "{b.output}"
                                }
                                span {
                                    style: "font-family: monospace; color: #8b949e; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                                    title: "{b.scene_path.display()}",
                                    "{b.scene_path.display()}"
                                }
                                span {
                                    style: "color: #8b949e; font-size: 0.75rem;",
                                    {b.pid.map(|p| format!("pid {p}")).unwrap_or_default()}
                                }
                                button {
                                    style: "background: #21262d; color: #e6edf3; border: 1px solid #30363d; border-radius: 4px; padding: 0.2rem 0.6rem; font-size: 0.75rem; cursor: pointer;",
                                    disabled: !connected,
                                    onclick: move |_| {
                                        on_unset.call(output_name.clone());
                                    },
                                    "Unset"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
