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
use paperforge_core::inventory::WallpaperEntry;
use paperforge_core::playlist::{Playlist, PlaylistStore};

use crate::data::bindings::Binding;
use crate::data::playlists::{refresh_playlists, save_playlist, PlaylistSummary};
use crate::data::{inventory as data_inventory, outputs as data_outputs};
use crate::error::GuiError;
use crate::ipc::client::{IpcClient, SignalEvent};
use crate::ipc::reconnect::next_backoff;
use crate::ipc::ConnectionStatus;
use crate::ui::bindings::BindingsPanel;
use crate::ui::inventory_panel::InventoryPanel;
use crate::ui::picker::Picker;
use crate::ui::playlist_editor::{DragPayload, OpenEditor, PlaylistEditor};
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
    // PR 6: switch from Vec<PathBuf> to Vec<WallpaperEntry> so the
    // picker can render title + kind without re-scanning the disk.
    let inventory: Signal<Vec<WallpaperEntry>> = use_signal(Vec::new);
    let bindings: Signal<Vec<Binding>> = use_signal(Vec::new); // PR 4 fills this
    let connection: Signal<ConnectionStatus> = use_signal(ConnectionStatus::default);
    let mut error: Signal<Option<GuiError>> = use_signal(|| None);
    // The active IpcClient clone, set by the IPC loop on connect,
    // cleared on disconnect/reconnect. `None` while the daemon is
    // unreachable. The list_running poll task reads this each tick;
    // toolbar callbacks clone it before issuing a write.
    let client_signal: Signal<Option<IpcClient>> = use_signal(|| None);
    // PR 6: which output's picker modal is open. `None` = no modal.
    // When set, the Picker overlay renders on top of the panels.
    let picker_open_for: Signal<Option<String>> = use_signal(|| None);
    // PR 7: which playlist is being edited. `None` = editor closed.
    // `Some(OpenEditor)` carries the on-disk name + a live `Playlist`
    // draft the operator mutates before Save hits disk.
    let editor_draft: Signal<Option<OpenEditor>> = use_signal(|| None);
    // PR 7: sub-picker visibility inside the editor. Toggled by the
    // editor's "Add wallpaper" button. Lives here so the editor
    // remains a controlled component (no nested `use_signal`).
    let editor_show_picker: Signal<bool> = use_signal(|| false);
    // PR 7/B: in-flight drag payload inside the editor. Set in
    // `ondragstart` (picker entry or body row), consumed in `ondrop`.
    // The editor reads this signal to render drop-zone highlights.
    let editor_drag: Signal<Option<DragPayload>> = use_signal(|| None);

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
    //
    // PR 7: we clone the root into `playlists_root_for_editor`
    // before the `use_coroutine` consumes a clone of it. The editor's
    // `on_open_editor` / `on_save_editor` closures need their own
    // copy to load + save playlists asynchronously — the coroutine
    // captures its clone by `move` and the editor closures read
    // `playlists_root_for_editor` (also a clone).
    let playlists_root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
        .join("paperforge")
        .join("playlists");
    let playlists_root_for_editor = playlists_root.clone();
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
                // PR 6: keep the full WallpaperEntry (title + kind)
                // so the picker can render labels without re-scanning.
                inventory_sig.set(v);
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
    let inventory_snapshot: Vec<WallpaperEntry> = inventory.cloned();
    let bindings_snapshot: Vec<Binding> = bindings.cloned();
    let connection_snapshot: ConnectionStatus = connection.cloned();
    let error_snapshot: Option<GuiError> = error.cloned();
    let inventory_len = inventory_snapshot.len();
    let picker_target: Option<String> = picker_open_for.cloned();
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

    // ---- Picker callbacks (PR 6) ----
    //
    // The Sidebar Set button fires `on_open_picker(output)`. The
    // Picker modal's X / backdrop / Escape fires `on_close_picker`.
    // A row click fires `on_pick_wallpaper(path)`, which issues the
    // SetWallpaper D-Bus call and closes the modal on success.
    let mut picker_for_open = picker_open_for;
    let on_open_picker = move |output: String| {
        picker_for_open.set(Some(output));
    };

    let mut picker_for_close = picker_open_for;
    let on_close_picker = move |_| {
        picker_for_close.set(None);
    };

    // on_pick_wallpaper: fires `set_binding(output, scene_path)` on
    // the daemon. On success, closes the modal (the WallpaperStarted
    // signal will populate the new row via the IPC loop — no
    // optimistic local insert). On failure, surfaces the error and
    // keeps the modal open so the operator can retry.
    let client_for_pick = client_signal;
    let error_for_pick = error;
    let picker_for_pick = picker_open_for;
    let on_pick_wallpaper = move |scene_path: PathBuf| {
        // Read the target output from the signal at click time, not
        // from the captured snapshot, so a second click after a
        // signal-driven re-render still binds to the right output.
        let Some(output) = picker_for_pick.cloned() else {
            return;
        };
        let Some(client) = client_for_pick.cloned() else {
            return;
        };
        let mut err = error_for_pick;
        let mut picker = picker_for_pick;
        spawn(async move {
            if let Err(e) = crate::data::bindings::set_binding(&client, &output, &scene_path).await
            {
                err.set(Some(e));
            } else {
                picker.set(None);
            }
        });
    };

    // on_browse: PR 9.3 — fired by the always-visible InventoryPanel
    // when the operator clicks a wallpaper row. PR 9.3 doesn't have
    // a per-output preview pane yet (niri has no outputs), so we
    // surface the path in a Notice banner as the UX feedback. PR 9.4
    // will add a proper preview pane; this signal is the integration
    // point so the panel doesn't need to change again.
    let error_for_browse = error;
    let mut browse_preview: Signal<Option<PathBuf>> = use_signal(|| None);
    let on_browse = move |path: PathBuf| {
        // Stash the selected path so a future preview pane can read
        // it. For now, push a Notice with the path so the operator
        // sees their click landed.
        let mut notice = error_for_browse;
        browse_preview.set(Some(path.clone()));
        notice.set(Some(GuiError::Notice(format!(
            "Selected: {}",
            path.display()
        ))));
    };

    // ---- Playlist editor callbacks (PR 7) ----
    //
    // on_open_editor: kicks off an async load of the on-disk playlist
    // and sets `editor_draft` to `Some(OpenEditor)` on success. The
    // editor modal renders only when `editor_draft` is `Some(_)`.
    //
    // The load goes through `spawn_blocking` because `PlaylistStore`
    // is sync. On failure (broken / missing playlist file), we surface
    // the error via the banner and leave the editor closed.
    let editor_for_open = editor_draft;
    let error_for_open = error;
    let root_for_open = playlists_root_for_editor.clone();
    let on_open_editor = move |name: String| {
        let mut editor = editor_for_open;
        let mut err = error_for_open;
        let root = root_for_open.clone();
        spawn(async move {
            let load_result = tokio::task::spawn_blocking(move || -> Result<Playlist, GuiError> {
                let store = PlaylistStore::new(&root).map_err(GuiError::from_core)?;
                store.load(&name).map_err(GuiError::from_core)
            })
            .await;
            match load_result {
                Ok(Ok(playlist)) => {
                    editor.set(Some(OpenEditor {
                        name: playlist.name.clone(),
                        draft: playlist,
                    }));
                }
                Ok(Err(e)) => err.set(Some(e)),
                Err(join_err) => err.set(Some(GuiError::Core(format!(
                    "spawn_blocking (open_editor): {join_err}"
                )))),
            }
        });
    };

    // on_save_editor: writes the live draft back to disk via
    // `save_playlist`, then closes the editor on success. On failure,
    // surfaces the error but keeps the editor open so the operator
    // can retry (no data is lost — the draft is in `editor_draft`).
    let editor_for_save = editor_draft;
    let error_for_save = error;
    let root_for_save = playlists_root_for_editor;
    let on_save_editor = move |playlist: Playlist| {
        let mut editor = editor_for_save;
        let mut err = error_for_save;
        let root = root_for_save.clone();
        spawn(async move {
            match save_playlist(root, playlist).await {
                Ok(()) => editor.set(None),
                Err(e) => err.set(Some(e)),
            }
        });
    };

    // on_cancel_editor: drops the draft and closes the editor. No
    // disk write happens — the on-disk playlist is untouched.
    let mut editor_for_cancel = editor_draft;
    let on_cancel_editor = move |_| {
        editor_for_cancel.set(None);
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
                // PR 9.2: Switched from `display: flex` (rigid equal
                // widths) to CSS grid with named tracks. The
                // sidebar gets a fixed 240px track, the two main
                // panels share `1fr` and `1.4fr` respectively so
                // Playlists gets more room for its long names
                // (default · 10 wp · 2 out) without wrapping
                // awkwardly. On narrower windows the panels stay
                // side-by-side; window-level min size (800x500 from
                // main.rs) prevents pathological narrow layouts.
                style: "display: grid; grid-template-columns: 240px minmax(0, 1fr) minmax(0, 1.4fr); gap: 0.75rem; align-items: stretch;",
                Sidebar {
                    outputs: outputs_snapshot.clone(),
                    running: running_snapshot.clone(),
                    connected,
                    on_open_picker,
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
                    on_edit: on_open_editor,
                }
            }
            // PR 9.3: InventoryPanel — always-visible wallpapers
            // browser. Sits in its own row below the 3-column grid
            // because the modal Picker is unreachable when `outputs`
            // is empty (niri / headless). The operator needs to see
            // the inventory without a Set-action prerequisite. Click
            // → on_browse (currently surfaces the path in a Notice
            // banner; PR 9.4 wires a preview pane).
            InventoryPanel {
                entries: inventory_snapshot.clone(),
                cache_dir: use_context::<crate::app::AppState>().cache_paths.thumbnails_dir.clone(),
                on_browse,
            }
            // Picker modal (PR 6). Renders only when the operator
            // has clicked Set on some output. Sits at the bottom of
            // the rsx tree so its `position: fixed` overlay paints
            // above the panel row without z-index gymnastics.
            if let Some(out) = picker_target.clone() {
                Picker {
                    output: out,
                    entries: inventory_snapshot.clone(),
                    cache_dir: use_context::<crate::app::AppState>().cache_paths.thumbnails_dir.clone(),
                    on_pick: on_pick_wallpaper,
                    on_close: on_close_picker,
                }
            }
            // Playlist editor modal (PR 7). Renders only when
            // `editor_draft` is `Some(_)`. Mirrors the Picker pattern
            // but carries a `Signal<Option<OpenEditor>>` for
            // bidirectional draft mutation (Save reads the live
            // draft; Up/Down/Remove buttons mutate it).
            if editor_draft.cloned().is_some() {
                PlaylistEditor {
                    draft: editor_draft,
                    show_picker: editor_show_picker,
                    drag: editor_drag,
                    inventory: inventory_snapshot.clone(),
                    on_save: on_save_editor,
                    on_cancel: on_cancel_editor,
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
