//! Sidebar — outputs list with state badges + per-row Set button.
//!
//! PR 3 layout:
//!
//! ```text
//! Outputs
//! ┌────────────────────┐
//! │ ● HDMI-A-1  Run [Set]
//! │ ● DP-1      Run [Set]
//! │ ● eDP-1     Pause [Set]
//! └────────────────────┘
//! ```
//!
//! The **Set** button opens the Picker modal for that output (PR 6).
//! Disabled when the IPC client is disconnected.
//!
//! State badge color is delegated to `theme::state_color` so the
//! rest of the GUI stays consistent.

use dioxus::prelude::*;

use paperforge_core::backend::BackendState;
use paperforge_core::hotplug::Output;

use crate::ui::theme::{state_color, PANEL_BORDER, PANEL_PADDING};

/// Render the outputs sidebar.
///
/// Props:
/// - `outputs` — live outputs from the compositor hotplug source
/// - `running` — `output → BackendState` map from `list_running`
/// - `connected` — whether the IPC client is healthy; controls
///   whether the Set buttons are interactive (greyed out on
///   reconnect)
/// - `on_open_picker` — callback fired with the output name when
///   the operator clicks Set on a row
#[allow(non_snake_case)]
#[component]
pub fn Sidebar(
    outputs: Vec<Output>,
    running: std::collections::HashMap<String, BackendState>,
    connected: bool,
    on_open_picker: EventHandler<String>,
) -> Element {
    rsx! {
        div {
            style: "{PANEL_PADDING} {PANEL_BORDER} background: #161b22; min-width: 220px;",
            h3 {
                style: "font-size: 0.95rem; margin: 0 0 0.5rem 0; color: #e6edf3;",
                "Outputs"
            }
            if outputs.is_empty() {
                p {
                    style: "color: #8b949e; font-size: 0.875rem;",
                    "No outputs detected — start sway / Hyprland, or check $XDG_CURRENT_DESKTOP."
                }
            } else {
                for o in outputs.into_iter() {
                    {
                        let state = running.get(&o.name).copied().unwrap_or(BackendState::NotRunning);
                        let color = state_color(state);
                        let name_for_pick = o.name.clone();
                        rsx! {
                            div {
                                style: "display: flex; align-items: center; gap: 0.5rem; padding: 0.25rem 0;",
                                span {
                                    style: "display: inline-block; width: 0.625rem; height: 0.625rem; border-radius: 50%; background: {color}; flex-shrink: 0;",
                                    ""
                                }
                                span {
                                    style: "font-family: monospace; flex: 1;",
                                    "{o.name}"
                                }
                                span {
                                    style: "color: #8b949e; font-size: 0.75rem;",
                                    "{state_label(state)}"
                                }
                                button {
                                    style: "background: #1f6feb; color: #ffffff; border: 1px solid #388bfd; border-radius: 4px; padding: 0.15rem 0.5rem; font-size: 0.7rem; cursor: pointer;",
                                    disabled: !connected,
                                    onclick: move |_| {
                                        on_open_picker.call(name_for_pick.clone());
                                    },
                                    "Set"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn state_label(state: BackendState) -> &'static str {
    match state {
        BackendState::Running => "run",
        BackendState::Paused => "pause",
        BackendState::NotRunning => "—",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_label_matches_backend_state() {
        assert_eq!(state_label(BackendState::Running), "run");
        assert_eq!(state_label(BackendState::Paused), "pause");
        assert_eq!(state_label(BackendState::NotRunning), "—");
    }

    #[test]
    fn state_color_matches_theme_constants() {
        // Cross-check that sidebar uses the same colors as the rest
        // of the GUI. Drift here means the sidebar disagrees with
        // the status indicator in the title bar.
        assert_eq!(state_color(BackendState::Running), "#3fb950");
        assert_eq!(state_color(BackendState::Paused), "#d29922");
        assert_eq!(state_color(BackendState::NotRunning), "#6e7681");
    }
}
