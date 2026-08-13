//! Root component — top-level layout that composes every panel.
//!
//! PR 2 shipped a placeholder layout. PR 3 swaps it for the real
//! `Sidebar | BindingsPanel | PlaylistsPanel` grid, wired to
//! independent timers via `use_coroutine`. Each coroutine refreshes
//! one slice of state on its own cadence (lifted from the TUI's
//! `lib.rs:38-41`).
//!
//! Refresh cadences:
//! - `OUTPUTS_TICK` (2s) — Wayland outputs from the compositor
//! - `PLAYLISTS_TICK` (10s) — `PlaylistStore::list` summary
//! - `INVENTORY_TICK` (30s) — `Inventory::scan` over detected roots
//! - bindings tick (5s) — placeholder until PR 4 wires IPC
//!
//! Banners are kept-stale on refresh errors: a transient failure
//! does not blank the panels.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dioxus::prelude::*;

use paperforge_core::backend::BackendState;
use paperforge_core::hotplug::{CompositorHotplugSource, Output};

use crate::app::AppState;
use crate::data::bindings::{refresh_bindings, Binding};
use crate::data::playlists::{refresh_playlists, PlaylistSummary};
use crate::data::{inventory as data_inventory, outputs as data_outputs};
use crate::error::GuiError;
use crate::ui::bindings::BindingsPanel;
use crate::ui::playlists::PlaylistsPanel;
use crate::ui::sidebar::Sidebar;
use crate::ui::status::StatusBanner;
use crate::ui::theme::{self, FONT_STACK, PANEL_BORDER};

const OUTPUTS_TICK: Duration = Duration::from_secs(2);
const PLAYLISTS_TICK: Duration = Duration::from_secs(10);
const INVENTORY_TICK: Duration = Duration::from_secs(30);
const BINDINGS_TICK: Duration = Duration::from_secs(5);

/// Root component. Mounts [`AppState`] into context so every
/// descendant can read it via `use_context::<AppState>()`.
///
/// `#[allow(non_snake_case)]` is required because Dioxus components
/// follow PascalCase by convention. The CI runs
/// `RUSTFLAGS="-D warnings"` so this allow is needed to keep the
/// build green.
#[allow(non_snake_case)]
#[component]
pub fn Root() -> Element {
    // `use_context_provider` mounts `AppState::new()` into the
    // Dioxus context tree once at the root. The closure runs once
    // (during the initial render). Children call
    // `use_context::<AppState>()` to read it.
    use_context_provider(AppState::new);

    let state = use_context::<AppState>();

    // ---- Reactive signals ----
    // Signals must be created inside a component (`use_signal` is a
    // hook). They live for the lifetime of the root component.
    let outputs: Signal<Vec<Output>> = use_signal(Vec::new);
    let running: Signal<HashMap<String, BackendState>> = use_signal(HashMap::new);
    let playlists: Signal<Vec<PlaylistSummary>> = use_signal(Vec::new);
    let inventory: Signal<Vec<PathBuf>> = use_signal(Vec::new); // PR 6 consumes this
    let bindings: Signal<Vec<Binding>> = use_signal(Vec::new); // PR 4 fills this
    let mut error: Signal<Option<GuiError>> = use_signal(|| None);

    // ---- Coroutines ----
    // One per refresh cadence. Each owns its own ticker and writes
    // the corresponding signal on every tick. PR 4 adds the IPC
    // subscription coroutine.

    // Outputs loop: every 2s, drain the compositor source.
    let outputs_src = Arc::new(CompositorHotplugSource::detect());
    let _outputs_loop = use_coroutine(move |_rx: UnboundedReceiver<()>| {
        let src = outputs_src.clone();
        let mut outputs_sig = outputs;
        let mut error_sig = error;
        async move {
            let mut ticker = tokio::time::interval(OUTPUTS_TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // first tick fires immediately
            loop {
                ticker.tick().await;
                let (v, err) = data_outputs::refresh_outputs(src.clone()).await;
                if !v.is_empty() {
                    outputs_sig.set(v);
                }
                if let Some(e) = err {
                    error_sig.set(Some(e));
                }
            }
        }
    });

    // Playlists loop: every 10s, list summaries from the on-disk
    // playlist store. Missing root defaults to
    // ~/.config/paperforge/playlists — matches the TUI.
    let playlists_root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
        .join("paperforge")
        .join("playlists");
    let _playlists_loop = use_coroutine(move |_rx: UnboundedReceiver<()>| {
        let root = playlists_root.clone();
        let mut playlists_sig = playlists;
        let mut error_sig = error;
        async move {
            let mut ticker = tokio::time::interval(PLAYLISTS_TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let (v, err) = refresh_playlists(root.clone()).await;
                playlists_sig.set(v);
                if let Some(e) = err {
                    error_sig.set(Some(GuiError::from_core(e)));
                }
            }
        }
    });

    // Inventory loop: every 30s, scan detected roots at depth 4.
    let inventory_roots = state.inventory_roots();
    let _inventory_loop = use_coroutine(move |_rx: UnboundedReceiver<()>| {
        let roots = inventory_roots.clone();
        let mut inventory_sig = inventory;
        let mut error_sig = error;
        async move {
            let mut ticker = tokio::time::interval(INVENTORY_TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let (v, err) = data_inventory::refresh_inventory(roots.clone()).await;
                // PR 3 only stores the path list — full WallpaperEntry
                // shape lands in PR 6 (picker grid).
                let paths: Vec<PathBuf> = v.into_iter().map(|e| e.path).collect();
                inventory_sig.set(paths);
                if let Some(e) = err {
                    error_sig.set(Some(e));
                }
            }
        }
    });

    // Bindings loop: placeholder until PR 4 wires the IPC. Refreshes
    // every 5s anyway so the polling cadence is visible in traces.
    let _bindings_loop = use_coroutine(move |_rx: UnboundedReceiver<()>| {
        let mut bindings_sig = bindings;
        async move {
            let mut ticker = tokio::time::interval(BINDINGS_TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let v = refresh_bindings().await;
                bindings_sig.set(v);
            }
        }
    });

    // Snapshot current signal values for the rsx below. Signals are
    // Copy + Clone, so the `.cloned()` calls are cheap.
    let outputs_snapshot: Vec<Output> = outputs.cloned();
    let running_snapshot: HashMap<String, BackendState> = running.cloned();
    let playlists_snapshot: Vec<PlaylistSummary> = playlists.cloned();
    let bindings_snapshot: Vec<Binding> = bindings.cloned();
    let error_snapshot: Option<GuiError> = error.cloned();
    let inventory_len = inventory.cloned().len();

    rsx! {
        document::Title { "paperforge — Dioxus GUI" }
        div {
            style: "padding: 1rem 1.5rem; font-family: {FONT_STACK}; background: #0d1117; color: #e6edf3; min-height: 100vh;",
            header {
                style: "display: flex; align-items: center; gap: 0.75rem; margin-bottom: 1rem;",
                h1 {
                    style: "font-size: 1.25rem; margin: 0;",
                    "paperforge"
                }
                span {
                    style: "color: #8b949e; font-size: 0.875rem;",
                    "Fase 6C.0 · Dioxus 0.8.0-alpha.0"
                }
                // Connection-status dot (placeholder until PR 4).
                span {
                    style: "margin-left: auto; display: inline-block; width: 0.625rem; height: 0.625rem; border-radius: 50%; background: {theme::connection_color(false)};",
                    title: "PR 4 wires the live connection status",
                    ""
                }
            }
            StatusBanner {
                error: error_snapshot,
                on_dismiss: move |_| {
                    error.set(None);
                },
            }
            if !state.path_warnings.is_empty() {
                div {
                    style: "background: #5a1f1f; color: #ffdcd7; padding: 0.5rem 1rem; border-radius: 4px; margin-bottom: 0.75rem;",
                    "Path detection warning: {state.path_warnings[0]}"
                }
            }
            div {
                style: "display: flex; gap: 0.75rem; align-items: stretch;",
                Sidebar {
                    outputs: outputs_snapshot.clone(),
                    running: running_snapshot.clone(),
                }
                BindingsPanel {
                    bindings: bindings_snapshot.clone(),
                }
                PlaylistsPanel {
                    playlists: playlists_snapshot.clone(),
                }
            }
            div {
                style: "{PANEL_BORDER} background: #161b22; padding: 0.5rem 0.75rem; margin-top: 0.75rem; color: #8b949e; font-size: 0.75rem;",
                "Inventory: {inventory_len} entries · Outputs: {outputs_snapshot.len()} · Playlists: {playlists_snapshot.len()} · Bindings: {bindings_snapshot.len()}"
            }
            // Theme module is wired even though the connection color
            // dot is a placeholder — keeps the navigation graph stable.
            div { style: "display: none;", "{theme::state_color(BackendState::Running)}" }
        }
    }
}
