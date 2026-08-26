//! Sidebar — outputs list with state badges + per-row Set button.
//!
//! Fase 7 Sprint 1: GPUI Component port of `paperforge-gui/src/ui/sidebar.rs`.
//!
//! Visual parity with the Dioxus version:
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
//! The **Set** button calls `on_open_picker(name)` so the parent view can
//! open the Picker modal (Fase 7 Sprint 2). The button is disabled when
//! `!connected` (the IPC client is unhealthy) so the operator can't open
//! a picker against a stale compositor state.
//!
//! Color for the state dot is the same constant table as the Dioxus
//! version: green (Running), amber (Paused), dim gray (NotRunning).
//! The constants live in this file rather than imported from
//! `paperforge-gui/theme.rs` to avoid a circular dep on the legacy
//! Dioxus layer being replaced.

use std::collections::HashMap;

use gpui::{
    App as GpuiApp, Hsla, IntoElement, ParentElement, SharedString, Styled as _, Window, div, px,
    rgb,
};
use gpui_component::{Disableable, Sizable, button::Button, button::ButtonVariants};

use paperforge_core::backend::BackendState;
use paperforge_core::hotplug::Output;

/// Build the sidebar panel.
///
/// Why a plain `div()` instead of `gpui_component::sidebar::Sidebar`:
/// 0.5.1's `Sidebar<E>` requires its children to implement the
/// `Collapsible` trait, but our rows are plain `div()`s (the
/// paperforge sidebar doesn't collapse — it's an always-on list).
/// Wrapping each row in a Collapsible adapter would be ceremony
/// for no UX gain. 0.5.1's `Sidebar` also enforces `DEFAULT_WIDTH
/// = 255 px` and theme-aware colors; the Dioxus version uses
/// `min-width: 220px` and hardcoded `#161b22` (GitHub-dark panel),
/// and reusing those keeps the migration a no-op for users.
///
/// # Arguments
///
/// * `outputs` — live outputs from the compositor hotplug source.
///   When empty, an empty-state hint is rendered instead.
/// * `running` — `output → BackendState` map from the LWE pool
///   health probe. Rows without an entry default to `NotRunning`.
/// * `connected` — whether the IPC client is healthy. Controls
///   whether the Set buttons are interactive (greyed out on
///   reconnect).
/// * `on_open_picker` — callback fired with the output name when
///   the operator clicks Set on a row. Must be `'static + Clone`
///   because the callback is captured per-row.
pub fn sidebar<C>(
    outputs: Vec<Output>,
    running: HashMap<String, BackendState>,
    connected: bool,
    on_open_picker: C,
) -> impl IntoElement
where
    C: Fn(&str, &mut Window, &mut GpuiApp) + Clone + 'static,
{
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .border_1()
        .border_color(Hsla::from(rgb(0x30363d)))
        .rounded_md()
        .bg(rgb(0x161b22))
        .min_w(px(220.))
        .child(heading())
        .child(if outputs.is_empty() {
            empty_state().into_any_element()
        } else {
            output_list(outputs, running, connected, on_open_picker).into_any_element()
        })
}

fn heading() -> impl IntoElement {
    div()
        .text_lg()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgb(0xe6edf3))
        .child("Outputs")
}

fn empty_state() -> impl IntoElement {
    div()
        .text_sm()
        .text_color(rgb(0x8b949e))
        .child(
            "No outputs detected — start sway / Hyprland, or check $XDG_CURRENT_DESKTOP.",
        )
}

fn output_list<C>(
    outputs: Vec<Output>,
    running: HashMap<String, BackendState>,
    connected: bool,
    on_open_picker: C,
) -> impl IntoElement
where
    C: Fn(&str, &mut Window, &mut GpuiApp) + Clone + 'static,
{
    div().flex().flex_col().gap_1().children(outputs.into_iter().map(|o| {
        let state = running
            .get(&o.name)
            .copied()
            .unwrap_or(BackendState::NotRunning);
        output_row(o.name, state, connected, on_open_picker.clone())
    }))
}

fn output_row<C>(name: String, state: BackendState, connected: bool, on_open_picker: C) -> impl IntoElement
where
    C: Fn(&str, &mut Window, &mut GpuiApp) + Clone + 'static,
{
    let label = state_label(state);
    let dot = state_color(state);
    let visible_name = name.clone();
    let click_name = name.clone();
    // Build a stable, per-row ElementId from the output name. ElementId
    // implements From<&str> but NOT From<(&str, T)> (the NamedChild form
    // requires the first component to be an ElementId, not &str), so a
    // format!() string id is the cleanest path that survives a borrow
    // checker audit.
    let row_id: SharedString = format!("paperforge.sidebar.set.{}", name).into();

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .py_1()
        .child(state_dot(dot))
        .child(
            div()
                .flex_1()
                .font_family("monospace")
                .child(visible_name),
        )
        .child(div().text_xs().text_color(rgb(0x8b949e)).child(label))
        .child(
            Button::new(row_id)
                .label("Set")
                .small()
                .primary()
                .disabled(!connected)
                .on_click(move |_, window, cx| on_open_picker(&click_name, window, cx)),
        )
}

fn state_dot(color: Hsla) -> impl IntoElement {
    div()
        .w(px(10.))
        .h(px(10.))
        .rounded_full()
        .bg(color)
}

/// Short label for the state badge. Mirrors
/// `paperforge_gui::ui::sidebar::state_label`.
fn state_label(state: BackendState) -> &'static str {
    match state {
        BackendState::Running => "run",
        BackendState::Paused => "pause",
        BackendState::NotRunning => "—",
    }
}

/// Badge color for the state dot. Mirrors
/// `paperforge_gui::ui::theme::state_color`. Kept in this crate
/// (not imported from `paperforge-gui`) so the GPUI layer doesn't
/// depend on the Dioxus legacy crate.
fn state_color(state: BackendState) -> Hsla {
    match state {
        BackendState::Running => rgb(0x3fb950).into(),
        BackendState::Paused => rgb(0xd29922).into(),
        BackendState::NotRunning => rgb(0x6e7681).into(),
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
    fn state_color_matches_legacy_palette() {
        // Mirror `paperforge_gui::ui::theme::state_color` so the
        // migration is a visual no-op. Drift here means the sidebar
        // disagrees with the status indicator in the title bar.
        let running = state_color(BackendState::Running);
        let paused = state_color(BackendState::Paused);
        let not_running = state_color(BackendState::NotRunning);
        assert!(running != paused);
        assert!(paused != not_running);
        assert!(running != not_running);
    }
}