//! Bindings grid — current `(output, scene_path)` map.
//!
//! PR 3 shows a "Waiting for daemon..." placeholder because the
//! `bindings` signal is populated from D-Bus signals (PR 4). The
//! shape returned from `data::bindings` is final, so the component
//! stays stable across the PR boundary.

use dioxus::prelude::*;

use crate::data::bindings::Binding;
use crate::ui::theme::PANEL_BORDER;

/// Render the bindings grid.
///
/// Empty `bindings` renders a placeholder explaining that the daemon
/// connection is pending. Once PR 4 wires `IpcClient` + signal
/// subscription, this component renders one row per `Binding`.
#[allow(non_snake_case)]
#[component]
pub fn BindingsPanel(bindings: Vec<Binding>) -> Element {
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
                    "Waiting for daemon connection (PR 4 wires D-Bus signals)…"
                }
            } else {
                div {
                    style: "display: grid; grid-template-columns: max-content 1fr max-content; gap: 0.5rem 1rem; align-items: center;",
                    for b in bindings.iter() {
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
                    }
                }
            }
        }
    }
}
