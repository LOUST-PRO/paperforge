//! paperforge-gpu — GPUI Niri spike
//!
//! Validates that GPUI 0.2.2 + gpui-component 0.5.1 compile + boot a window
//! on Niri (Wayland compositor). Adapted from the upstream
//! `examples/hello_world` in longbridge/gpui-component v0.5.2, simplified to
//! avoid the `gpui_platform` git dep (we use the published `gpui` crate
//! directly so the spike compiles without cloning the Zed repo).
//!
//! Run with: `cargo run -p paperforge-gpu` (requires a Wayland session).
//!
//! Outcome: if this prints "Hello, World!" + a button
//! in a Niri window, the GPUI stack works end-to-end and we can proceed to
//! the migration PRs (Sheet / Sidebar / DataTable / DockArea /
//! TrayIcon). If it crashes or fails to open a window, the spike blocked and
//! we re-evaluate.
//!
//! API note (2026-08-25): the upstream `examples/hello_world` in
//! longbridge/gpui-component targets 0.5.2 where `Root` implements `Styled`
//! and accepts `.bg(...)`. In crates.io 0.5.1, `Root` does NOT expose
//! `.bg(...)` — the background comes from `cx.theme().background` inside
//! `Root::render` (see `gpui-component-0.5.1/src/root.rs:409`). So the
//! hello-world skips the `.bg(...)` call here. If we need 0.5.2 features
//! later (Sheet/Sidebar/DockArea have richer APIs in 0.5.2), we'll bump
//! the pin and revisit.

use std::collections::HashMap;

use gpui::*;
use gpui_component::{Root, button::Button, button::ButtonVariants};

use paperforge_core::backend::BackendState;
use paperforge_core::hotplug::Output;

mod ui;

pub struct Example;

impl Render for Example {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // Fake data wiring so the visual parity of the Sidebar port is
        // visible without booting the full hotplug + IPC stack. The
        // real consumers (Task #8 IPC + hotplug source) wire these
        // from observed state in a follow-up commit.
        let outputs = vec![
            Output { name: "HDMI-A-1".into() },
            Output { name: "DP-1".into() },
            Output { name: "eDP-1".into() },
        ];
        let mut running = HashMap::new();
        running.insert("HDMI-A-1".into(), BackendState::Running);
        running.insert("DP-1".into(), BackendState::Running);
        running.insert("eDP-1".into(), BackendState::Paused);

        div()
            .flex()
            .flex_col()
            .size_full()
            .items_center()
            .justify_center()
            .p_8()
            .gap_4()
            .child(ui::sidebar::sidebar(
                outputs,
                running,
                /* connected */ true,
                |name, _window, _cx| println!("[paperforge-gpu] open picker for {name}"),
            ))
            .child(
                Button::new("ok")
                    .primary()
                    .label("Hello from paperforge-gpu")
                    .on_click(|_, _, _| println!("Clicked!")),
            )
    }
}

fn main() {
    // Note: the upstream example uses `gpui_platform::application().run(...)`
    // because gpui_platform wires the platform-specific lifecycle (Wayland /
    // X11 / macOS) for Zed. The published `gpui::Application::new()` from
    // crates.io already calls `current_platform(false)` internally, so for
    // this spike we skip gpui_platform and use the public API directly.
    Application::new().run(move |cx| {
        // This must be called before using any GPUI Component features.
        gpui_component::init(cx);

        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), |window, cx| {
                let view = cx.new(|_| Example);
                // Root is the first-level wrapper view on the window. It owns
                // the Sheet / Dialog / Notification layer state. Its render
                // (root.rs:396 in gpui-component 0.5.1) already paints the
                // background from `cx.theme().background`, so we do NOT pass
                // `.bg(...)` here — that API is only on 0.5.2+ where Root
                // implements Styled.
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("Failed to open window");
        })
        .detach();
    });
}