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

use crate::data::bindings::Binding;
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
/// `list_running` poll cadence. Cheap (one D-Bus method call) and the
/// result feeds the sidebar state badges. 5s matches the human reaction
/// time for "did my click land?" without thrashing the bus.
const RUNNING_TICK: Duration = Duration::from_secs(5);

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
    // The active IpcClient clone, set by the IPC loop on connect,
    // cleared on disconnect/reconnect. `None` while the daemon is
    // unreachable. The list_running poll task reads this each tick;
    // toolbar callbacks clone it before issuing a write.
    let client_signal: Signal<Option<IpcClient>> = use_signal(|| None);

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
        let mut client_sig = client_signal;
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
                        // Expose the live client to the poll task and
                        // toolbar callbacks. Cloned (Arc under the hood)
                        // so the IPC loop retains its own copy too.
                        client_sig.set(Some(client.clone()));
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
                                client_sig.set(None);
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
                                            client_sig.set(None);
                                            tokio::time::sleep(next_backoff(0)).await;
                                            break;
                                        }
                                        Some(SignalEvent::Bound {
                                            output,
                                            scene_path,
                                            pid,
                                        }) => {
                                            // PR 5/D: persist the pid so
                                            // the list_running poll task
                                            // can map pid→output when
                                            // reconciling sidebar badges.
                                            running_map.insert(
                                                output.clone(),
                                                Binding {
                                                    output: output.clone(),
                                                    scene_path,
                                                    pid: Some(pid),
                                                },
                                            );
                                            let mut sorted: Vec<Binding> =
                                                running_map.values().cloned().collect();
                                            sorted.sort_by(|a, b| a.output.cmp(&b.output));
                                            bindings_sig.set(sorted);
                                        }
                                        Some(SignalEvent::Unbound { pid }) => {
                                            // SignalStream already removed the
                                            // pid from its internal map; here
                                            // we sweep our running_map for the
                                            // entry that owns this pid.
                                            let to_remove: Vec<String> = running_map
                                                .iter()
                                                .filter_map(|(out, b)| {
                                                    b.pid.filter(|p| *p == pid).map(|_| out.clone())
                                                })
                                                .collect();
                                            for out in to_remove {
                                                running_map.remove(&out);
                                            }
                                            let mut sorted: Vec<Binding> =
                                                running_map.values().cloned().collect();
                                            sorted.sort_by(|a, b| a.output.cmp(&b.output));
                                            bindings_sig.set(sorted);
                                            tracing::debug!(
                                                target: "paperforge-gui",
                                                pid,
                                                "WallpaperStopped received; bindings pruned"
                                            );
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

    // ---- list_running poll (PR 5/D) ----
    //
    // Every 5s, ask the daemon for the per-PID `(pid, BackendState)`
    // list, then project to `output → BackendState` using the pid map
    // we already maintain in `bindings`. The result populates the
    // `running` signal that the sidebar consumes for state badges.
    //
    // Skips the tick cleanly when `client_signal` is `None` (daemon
    // down / reconnecting). No error surfacing for transient failures
    // here — a stale sidebar badge is preferable to a banner flood.
    let _running_poll = use_coroutine(move |_rx: UnboundedReceiver<()>| {
        let client_sig = client_signal;
        let mut running_sig = running;
        let bindings_sig = bindings;
        async move {
            let mut ticker = tokio::time::interval(RUNNING_TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Some(client) = client_sig.cloned() else {
                    continue;
                };
                // Build pid→output from the bindings snapshot. If a
                // binding has no pid yet (initial state, signal race),
                // it just won't get a state badge this round.
                let pid_map: HashMap<i32, String> = bindings_sig
                    .cloned()
                    .iter()
                    .filter_map(|b| b.pid.map(|p| (p, b.output.clone())))
                    .collect();
                match client.list_running().await {
                    Ok(pairs) => {
                        let mut m: HashMap<String, BackendState> = HashMap::new();
                        for (pid, state) in pairs {
                            if let Some(out) = pid_map.get(&pid) {
                                m.insert(out.clone(), state);
                            }
                        }
                        running_sig.set(m);
                    }
                    Err(e) => {
                        tracing::debug!(
                            target: "paperforge-gui",
                            error = %e,
                            "list_running poll failed; keeping previous state"
                        );
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
    let connected: bool = matches!(connection_snapshot, ConnectionStatus::Connected);

    let connection_color = match connection_snapshot {
        ConnectionStatus::Connected => theme::connection_color(true),
        ConnectionStatus::Disconnected | ConnectionStatus::Reconnecting { .. } => {
            theme::connection_color(false)
        }
    };

    // ---- Write-action closures (PR 5/D) ----
    //
    // These are passed as `EventHandler` props to the panels. Each
    // captures `client_signal` (Copy) and `error` (needs `mut`), and
    // spawns a one-shot task that issues the D-Bus call. On failure,
    // the error signal is set so the banner surfaces it.
    //
    // `spawn` is `dioxus::prelude::spawn`, which dispatches onto the
    // Dioxus runtime. The async block captures clones of `client_signal`
    // and `error` so the closures themselves can be `move` and short.
    //
    // Toolbar verbs (pause / resume / audio_toggle) follow the same
    // pattern; they live in the rsx below rather than as `on_*`
    // callbacks because there's exactly one of each.

    // on_unset: called by BindingsPanel rows. Clones the client
    // out of the signal at call time so a stale closure (bound
    // before the daemon was reachable) still sees the current state.
    let error_for_unset = error;
    let client_for_unset = client_signal;
    let on_unset = move |output: String| {
        let Some(client) = client_for_unset.cloned() else {
            return;
        };
        let mut err = error_for_unset;
        spawn(async move {
            if let Err(e) = crate::data::bindings::unset_binding(&client, &output).await {
                err.set(Some(e));
            }
        });
    };

    // on_apply: same shape, calls data::playlists::apply_playlist.
    let error_for_apply = error;
    let client_for_apply = client_signal;
    let on_apply = move |name: String| {
        let Some(client) = client_for_apply.cloned() else {
            return;
        };
        let mut err = error_for_apply;
        spawn(async move {
            if let Err(e) = crate::data::playlists::apply_playlist(&client, &name).await {
                err.set(Some(e));
            }
        });
    };

    // Toolbar closures (PR 5/D). Each takes the client at call time
    // and surfaces failures via `error`.
    let error_for_toolbar = error;
    let client_for_pause = client_signal;
    let on_pause = move |_| {
        let Some(client) = client_for_pause.cloned() else {
            return;
        };
        let mut err = error_for_toolbar;
        spawn(async move {
            if let Err(e) = client.pause().await {
                err.set(Some(e));
            }
        });
    };

    let error_for_resume = error;
    let client_for_resume = client_signal;
    let on_resume = move |_| {
        let Some(client) = client_for_resume.cloned() else {
            return;
        };
        let mut err = error_for_resume;
        spawn(async move {
            if let Err(e) = client.resume().await {
                err.set(Some(e));
            }
        });
    };

    let error_for_audio = error;
    let client_for_audio = client_signal;
    let on_audio_toggle = move |_| {
        let Some(client) = client_for_audio.cloned() else {
            return;
        };
        let mut err = error_for_audio;
        spawn(async move {
            if let Err(e) = client.audio_toggle().await {
                err.set(Some(e));
            }
        });
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
            // Toolbar (PR 5/D). Pauses all running backends, resumes
            // them, or toggles the audio mute state. Disabled while
            // disconnected so the operator can't enqueue writes that
            // will fail. Sits between the header and the panel row.
            div {
                style: "display: inline-flex; gap: 0.5rem; margin-bottom: 0.75rem;",
                button {
                    style: "background: #21262d; color: #e6edf3; border: 1px solid #30363d; border-radius: 4px; padding: 0.35rem 0.9rem; font-size: 0.8125rem; cursor: pointer;",
                    disabled: !connected,
                    onclick: on_pause,
                    "Pause all"
                }
                button {
                    style: "background: #21262d; color: #e6edf3; border: 1px solid #30363d; border-radius: 4px; padding: 0.35rem 0.9rem; font-size: 0.8125rem; cursor: pointer;",
                    disabled: !connected,
                    onclick: on_resume,
                    "Resume all"
                }
                button {
                    style: "background: #21262d; color: #e6edf3; border: 1px solid #30363d; border-radius: 4px; padding: 0.35rem 0.9rem; font-size: 0.8125rem; cursor: pointer;",
                    disabled: !connected,
                    onclick: on_audio_toggle,
                    "Audio toggle"
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
                    connected,
                    on_unset,
                }
                PlaylistsPanel {
                    playlists: playlists_snapshot.clone(),
                    connected,
                    on_apply,
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
