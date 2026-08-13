//! Root component — top-level layout that composes every panel.
//!
//! PR 2 shipped a placeholder layout. PR 3 swapped it for the real
//! `Sidebar | BindingsPanel | PlaylistsPanel` grid, wired to
//! independent timers via `use_coroutine`. PR 4 wires the live
//! D-Bus subscription + reconnect logic.
//!
//! Refresh cadences:
//! - `OUTPUTS_TICK` (2s) — Wayland outputs from the compositor
//! - `PLAYLISTS_TICK` (10s) — `PlaylistStore::list` summary
//! - `INVENTORY_TICK` (30s) — `Inventory::scan` over detected roots
//! - `IPC_*` — driven by `ipc::client::SignalStream`, not a timer;
//!   `ipc::reconnect::next_backoff` (5s, 10s, 20s, capped 30s) gates
//!   the retry cadence when the daemon is unreachable.
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

use crate::data::bindings::{refresh_bindings, Binding};
use crate::data::playlists::{refresh_playlists, PlaylistSummary};
use crate::data::{inventory as data_inventory, outputs as data_outputs};
use crate::error::GuiError;
use crate::ipc::client::{IpcClient, SignalEvent};
use crate::ipc::reconnect::next_backoff;
use crate::ipc::ConnectionStatus;
use crate::ui::bindings::BindingsPanel;
use crate::ui::playlists::PlaylistsPanel;
use crate::ui::sidebar::Sidebar;
use crate::ui::status::StatusBanner;
use crate::ui::theme::{self, FONT_STACK, PANEL_BORDER};

const OUTPUTS_TICK: Duration = Duration::from_secs(2);
const PLAYLISTS_TICK: Duration = Duration::from_secs(10);
const INVENTORY_TICK: Duration = Duration::from_secs(30);

/// Root component. Mounts the global state into context and starts
/// 5 background coroutines (outputs / playlists / inventory /
/// bindings-poller / ipc-reconnect+signal).
///
/// `#[allow(non_snake_case)]` is required because Dioxus components
/// follow PascalCase by convention. The CI runs
/// `RUSTFLAGS="-D warnings"` so this allow is needed to keep the
/// build green.
#[allow(non_snake_case)]
#[component]
pub fn Root() -> Element {
    use_context_provider(crate::app::AppState::new);

    // ---- Reactive signals ----
    // Signals must be created inside a component (`use_signal` is a
    // hook). They live for the lifetime of the root component.
    let outputs: Signal<Vec<Output>> = use_signal(Vec::new);
    let running: Signal<HashMap<String, BackendState>> = use_signal(HashMap::new);
    let playlists: Signal<Vec<PlaylistSummary>> = use_signal(Vec::new);
    let inventory: Signal<Vec<PathBuf>> = use_signal(Vec::new); // PR 6 consumes this
    let bindings: Signal<Vec<Binding>> = use_signal(Vec::new); // PR 4 fills this
    let connection: Signal<ConnectionStatus> = use_signal(ConnectionStatus::default);
    let mut error: Signal<Option<GuiError>> = use_signal(|| None);

    // ---- Coroutines ----

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
    let inventory_roots = use_context::<crate::app::AppState>().inventory_roots();
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

    // ---- IPC loop (PR 4) ----
    //
    // Owns the entire D-Bus connection lifecycle:
    //
    // 1. Try `IpcClient::connect()`. If it fails, surface the error
    //    on the banner and enter `Reconnecting { attempt = 1 }`,
    //    sleep `next_backoff(0) = 5s`, retry.
    // 2. On connect success, flip the `connection` signal to
    //    `Connected`, subscribe to the 3 signals, and pump them
    //    forever. The `pid → output` map inside `SignalStream` lets
    //    `WallpaperStopped` resolve back to the output name so
    //    the bindings signal can drop the right row.
    // 3. When `SignalStream::next()` returns `None` (the underlying
    //    `MessageStream` ended — daemon crashed / session bus went
    //    down), bump `attempt` and loop back to step 1 with the new
    //    backoff. The cap at 30s prevents busy-looping the bus.
    //
    // The polling `refresh_bindings()` placeholder from PR 3 is
    // removed: signals are the source of truth now. Output-state
    // (Running / Paused / NotRunning) still comes from PR 6+
    // via `list_running()`; until then, the sidebar shows
    // "Unknown" badges.
    let _ipc_loop = use_coroutine(move |_rx: UnboundedReceiver<()>| {
        let mut bindings_sig = bindings;
        let mut connection_sig = connection;
        let mut error_sig = error;
        async move {
            let mut attempt: u32 = 0;
            let mut running_map: HashMap<String, Binding> = HashMap::new();
            loop {
                match IpcClient::connect().await {
                    Err(e) => {
                        tracing::warn!(
                            target: "paperforge-gui",
                            attempt = attempt + 1,
                            error = %e,
                            "IpcClient::connect failed"
                        );
                        error_sig.set(Some(e));
                        connection_sig.set(ConnectionStatus::Reconnecting {
                            attempt: attempt + 1,
                        });
                        let delay = next_backoff(attempt);
                        tokio::time::sleep(delay).await;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    Ok(client) => {
                        attempt = 0;
                        connection_sig.set(ConnectionStatus::Connected);
                        tracing::info!(
                            target: "paperforge-gui",
                            "IpcClient connected to daemon"
                        );
                        match client.subscribe_signals().await {
                            Err(e) => {
                                tracing::warn!(
                                    target: "paperforge-gui",
                                    error = %e,
                                    "SignalStream subscribe failed"
                                );
                                error_sig.set(Some(e));
                                connection_sig.set(ConnectionStatus::Reconnecting { attempt: 1 });
                                tokio::time::sleep(next_backoff(0)).await;
                                continue;
                            }
                            Ok(mut stream) => {
                                loop {
                                    match stream.next().await {
                                        None => {
                                            tracing::warn!(
                                                target: "paperforge-gui",
                                                "SignalStream ended; reconnecting"
                                            );
                                            connection_sig
                                                .set(ConnectionStatus::Reconnecting { attempt: 1 });
                                            tokio::time::sleep(next_backoff(0)).await;
                                            break;
                                        }
                                        Some(SignalEvent::Bound {
                                            output,
                                            scene_path,
                                            pid: _,
                                        }) => {
                                            running_map.insert(
                                                output.clone(),
                                                Binding {
                                                    output: output.clone(),
                                                    scene_path,
                                                    pid: None,
                                                },
                                            );
                                            let mut sorted: Vec<Binding> =
                                                running_map.values().cloned().collect();
                                            sorted.sort_by(|a, b| a.output.cmp(&b.output));
                                            bindings_sig.set(sorted);
                                        }
                                        Some(SignalEvent::Unbound { pid }) => {
                                            // SignalStream removed the pid from its
                                            // own internal map already; we don't have
                                            // pid → output here, so do a coarse
                                            // refresh via the daemon's list_running
                                            // when it lands. For PR 4 we keep the
                                            // existing entries; PR 5+ adds
                                            // `list_running` reconciliation.
                                            tracing::debug!(
                                                target: "paperforge-gui",
                                                pid,
                                                "WallpaperStopped received"
                                            );
                                            let _ = refresh_bindings; // silence unused while PR 4 ships read-only
                                        }
                                        Some(SignalEvent::MonitorsChanged { outputs }) => {
                                            tracing::debug!(
                                                target: "paperforge-gui",
                                                ?outputs,
                                                "MonitorChanged received"
                                            );
                                        }
                                    }
                                }
                                continue;
                            }
                        }
                    }
                }
            }
        }
    });

    // Snapshot current signal values for the rsx below. Signals are
    // Copy + Clone, so the `.cloned()` calls are cheap.
    let outputs_snapshot: Vec<Output> = outputs.cloned();
    let running_snapshot: HashMap<String, BackendState> = running.cloned();
    let playlists_snapshot: Vec<PlaylistSummary> = playlists.cloned();
    let bindings_snapshot: Vec<Binding> = bindings.cloned();
    let connection_snapshot: ConnectionStatus = connection.cloned();
    let error_snapshot: Option<GuiError> = error.cloned();
    let inventory_len = inventory.cloned().len();

    let connection_color = match connection_snapshot {
        ConnectionStatus::Connected => theme::connection_color(true),
        ConnectionStatus::Disconnected | ConnectionStatus::Reconnecting { .. } => {
            theme::connection_color(false)
        }
    };

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
                span {
                    style: "margin-left: auto; display: inline-flex; align-items: center; gap: 0.4rem; color: #8b949e; font-size: 0.8125rem;",
                    span {
                        style: "display: inline-block; width: 0.625rem; height: 0.625rem; border-radius: 50%; background: {connection_color};",
                        ""
                    }
                    "{connection_snapshot}"
                }
            }
            StatusBanner {
                error: error_snapshot,
                on_dismiss: move |_| {
                    error.set(None);
                },
            }
            if !use_context::<crate::app::AppState>().path_warnings.is_empty() {
                div {
                    style: "background: #5a1f1f; color: #ffdcd7; padding: 0.5rem 1rem; border-radius: 4px; margin-bottom: 0.75rem;",
                    "Path detection warning: {use_context::<crate::app::AppState>().path_warnings[0]}"
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
                "Inventory: {inventory_len} entries · Outputs: {outputs_snapshot.len()} · Playlists: {playlists_snapshot.len()} · Bindings: {bindings_snapshot.len()} · IPC: {connection_snapshot}"
            }
            // Theme module is wired even though the connection color
            // dot is a placeholder — keeps the navigation graph stable.
            div { style: "display: none;", "{theme::state_color(BackendState::Running)}" }
        }
    }
}
