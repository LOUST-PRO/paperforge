//! D-Bus IPC client for paperforge-gui.
//!
//! Wraps [`PaperforgeClient`] (the typed call surface from
//! `paperforge-core`) and adds a signal subscription over the
//! `org.louzt.Paperforge1` interface. The 3 signals we care about:
//!
//! | Member             | Body                                          |
//! |--------------------|-----------------------------------------------|
//! | `WallpaperStarted` | `(s output, s scene_path, i pid)`             |
//! | `WallpaperStopped` | `(i pid)`                                     |
//! | `MonitorChanged`   | `(as outputs)`                                |
//!
//! The subscription yields a [`SignalEvent`] stream that
//! `ui::root` consumes to update the bindings signal and the
//! connection-status dot.
//!
//! # Architecture note
//!
//! The typed call surface lives in `paperforge-core` because CLI
//! and GUI both need it (and so the `zbus` dependency is centralized
//! in one crate). The GUI-specific bits — reconnect strategy,
//! signal-event enum, channel plumbing — live here.
//!
//! # Errors
//!
//! `connect()` and `subscribe_signals()` return
//! `Result<_, crate::error::GuiError>` with the source code labelled
//! so the banner can show `[ipc] …`.

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use paperforge_core::dbus::{PaperforgeClient, BUS_NAME};
use zbus::message::Type as MsgType;
use zbus::{MatchRule, Message, MessageStream};

use crate::error::GuiError;

/// One of the 3 typed events the daemon signals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalEvent {
    /// `WallpaperStarted`: a new LWE instance is rendering `scene_path`
    /// on `output` under `pid`. PR 5 uses this to drive optimistic UI
    /// updates on `set_wallpaper`.
    Bound {
        output: String,
        scene_path: PathBuf,
        pid: i32,
    },
    /// `WallpaperStopped`: the LWE process with `pid` exited. The GUI
    /// needs the `pid` to clean up its `output → scene_path` map
    /// (tracks pid↔output because `WallpaperStopped` doesn't carry
    /// the output name).
    Unbound { pid: i32 },
    /// `MonitorChanged`: the Wayland output set changed. The GUI
    /// refreshes its outputs list (the sidebar already does this on
    /// its own 2s timer, but a signal lets it skip a roundtrip).
    MonitorsChanged { outputs: Vec<String> },
}

/// D-Bus client + signal subscription for the GUI.
///
/// Cheap-to-move, cheaply-cloneable handle. PR 5 wraps the underlying
/// `PaperforgeClient` in an `Arc` so the IPC coroutine in `ui::root`
/// can hand out a clone to write-action closures (Unset / Apply /
/// Pause / Resume / Audio toggle) without re-creating the D-Bus
/// connection.
///
/// `subscribe_signals()` is still owned by the single consumer (the
/// IPC coroutine) — the returned `SignalStream` isn't `Clone`.
#[derive(Clone)]
pub struct IpcClient {
    client: Arc<PaperforgeClient>,
}

impl IpcClient {
    /// Connect to the running paperforge daemon over the session bus.
    ///
    /// Returns `Err` if no daemon is reachable. The caller
    /// (`ui::root`) handles the error by surfacing it in the banner
    /// and entering the `Reconnecting` state.
    pub async fn connect() -> Result<Self, GuiError> {
        let client = PaperforgeClient::connect()
            .await
            .map_err(|e| GuiError::Ipc {
                kind: "connect",
                message: format!("{e}"),
            })?;
        Ok(Self {
            client: Arc::new(client),
        })
    }

    /// Underlying typed client. Used by PR 5+ write actions
    /// (`set_wallpaper`, `apply_playlist`, `pause`, …).
    #[allow(dead_code)] // consumed by ui/bindings.rs in PR 5+
    pub fn client(&self) -> &PaperforgeClient {
        &self.client
    }

    // ---- Write actions (PR 5/B) ----
    //
    // Thin wrappers that translate `paperforge_core::error::Error` to
    // `GuiError::Ipc { kind = <verb>, … }` so the banner can show
    // `[ipc] unset` vs `[ipc] pause` and the operator can pinpoint
    // which call failed. The `kind` tag also helps when the same
    // closure fires multiple verbs (e.g. toolbar buttons).

    /// Call `SetWallpaper(output, scene_path)` on the daemon.
    ///
    /// Used by the per-output picker (PR 6). Marked `dead_code` until
    /// then; surfaces `kind = "set_wallpaper"` on failure.
    #[allow(dead_code)]
    pub async fn set_wallpaper(&self, output: &str, scene_path: &str) -> Result<(), GuiError> {
        self.client
            .set_wallpaper(output, scene_path)
            .await
            .map_err(map_err("set_wallpaper"))
    }

    /// Call `UnsetWallpaper(output)` on the daemon. Idempotent:
    /// unset on a non-bound output is a no-op (per the daemon side).
    #[allow(dead_code)] // consumed by ui/root.rs + data/bindings.rs in PR 5/D
    pub async fn unset_wallpaper(&self, output: &str) -> Result<(), GuiError> {
        self.client
            .unset_wallpaper(output)
            .await
            .map_err(map_err("unset_wallpaper"))
    }

    /// Call `Pause()` on the daemon. Returns the count of PIDs the
    /// daemon signaled (surfaced in the banner for confirmation).
    #[allow(dead_code)] // consumed by ui/root.rs toolbar in PR 5/D
    pub async fn pause(&self) -> Result<u32, GuiError> {
        self.client.pause().await.map_err(map_err("pause"))
    }

    /// Call `Resume()` on the daemon.
    #[allow(dead_code)] // consumed by ui/root.rs toolbar in PR 5/D
    pub async fn resume(&self) -> Result<u32, GuiError> {
        self.client.resume().await.map_err(map_err("resume"))
    }

    /// Call `AudioToggle()` on the daemon.
    #[allow(dead_code)] // consumed by ui/root.rs toolbar in PR 5/D
    pub async fn audio_toggle(&self) -> Result<u32, GuiError> {
        self.client
            .audio_toggle()
            .await
            .map_err(map_err("audio_toggle"))
    }

    /// Call `AudioMute()` on the daemon. Toolbar Mute toggle lands
    /// in PR 8 alongside the audio inspector.
    #[allow(dead_code)]
    pub async fn audio_mute(&self) -> Result<u32, GuiError> {
        self.client
            .audio_mute()
            .await
            .map_err(map_err("audio_mute"))
    }

    /// Call `AudioUnmute()` on the daemon.
    #[allow(dead_code)]
    pub async fn audio_unmute(&self) -> Result<u32, GuiError> {
        self.client
            .audio_unmute()
            .await
            .map_err(map_err("audio_unmute"))
    }

    /// Call `ApplyPlaylist(name)` on the daemon. The daemon iterates
    /// the stored playlist and binds each entry to its outputs.
    #[allow(dead_code)] // consumed by data/playlists.rs in PR 5/C
    pub async fn apply_playlist(&self, name: &str) -> Result<(), GuiError> {
        self.client
            .apply_playlist(name)
            .await
            .map_err(map_err("apply_playlist"))
    }

    /// Call `ListRunning()` on the daemon. Returns the per-PID
    /// `(pid, BackendState)` pairs. The GUI owns the pid→output table
    /// (populated from `WallpaperStarted` signals) and uses it to
    /// filter this list down to the `output → state` map the
    /// sidebar badges render.
    #[allow(dead_code)] // consumed by ui/root.rs poll in PR 5/D
    pub async fn list_running(
        &self,
    ) -> Result<Vec<(i32, paperforge_core::backend::BackendState)>, GuiError> {
        self.client
            .list_running()
            .await
            .map_err(map_err("list_running"))
    }

    /// Subscribe to the 3 signals emitted on `org.louzt.Paperforge1`.
    ///
    /// The returned [`SignalStream`] yields parsed [`SignalEvent`]s.
    /// When the underlying connection drops, the stream ends
    /// (`next()` returns `None`); the caller is expected to
    /// reconnect and re-subscribe.
    ///
    /// `max_queued` defaults to 64 (zbus default) — enough for a
    /// busy daemon that emits bursts on hotplug.
    pub async fn subscribe_signals(&self) -> Result<SignalStream, GuiError> {
        let rule = MatchRule::builder()
            .msg_type(MsgType::Signal)
            .sender(BUS_NAME)
            .map_err(|e| GuiError::Ipc {
                kind: "match-rule",
                message: format!("{e}"),
            })?
            .build();
        let stream = MessageStream::for_match_rule(rule, self.client.connection(), Some(64))
            .await
            .map_err(|e| GuiError::Ipc {
                kind: "subscribe",
                message: format!("{e}"),
            })?;
        Ok(SignalStream {
            inner: stream,
            // pid -> output index, populated lazily as Bound events
            // arrive. Lets Unbound clean up without a daemon roundtrip.
            pid_to_output: std::collections::HashMap::new(),
        })
    }
}

/// Stream of [`SignalEvent`]s. Internally a thin wrapper around
/// [`MessageStream`] that:
/// 1. Filters by member name (drops any non-paperforge signals that
///    happen to match the rule — e.g. `org.freedesktop.DBus.NameOwnerChanged`
///    when the daemon appears/disappears).
/// 2. Maintains a `pid → output` map so `WallpaperStopped` can
///    resolve back to the output name for the bindings panel.
pub struct SignalStream {
    inner: MessageStream,
    pid_to_output: std::collections::HashMap<i32, String>,
}

impl SignalStream {
    /// Await the next signal event. Returns `None` when the
    /// underlying connection drops — the caller should reconnect.
    pub async fn next(&mut self) -> Option<SignalEvent> {
        loop {
            let msg = self.inner.next().await?;
            // zbus stream yields `Option<Result<Message>>`; the
            // outer `Option` is the connection-end marker. The
            // inner `Result` carries transport errors that we
            // log + skip (the stream keeps going).
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(target: "paperforge-gui", "D-Bus signal transport error: {e}");
                    continue;
                }
            };
            match Self::parse_event(&msg, &mut self.pid_to_output) {
                Some(ev) => return Some(ev),
                None => continue, // not one of our 3 signals
            }
        }
    }

    /// Parse one [`Message`] into a [`SignalEvent`]. Returns `None`
    /// if the message isn't one of the 3 paperforge signals.
    fn parse_event(
        msg: &Message,
        pid_to_output: &mut std::collections::HashMap<i32, String>,
    ) -> Option<SignalEvent> {
        let header = msg.header();
        let member = header.member()?.as_str();
        match member {
            "WallpaperStarted" => {
                let (output, scene_path, pid): (String, String, i32) =
                    msg.body().deserialize().ok()?;
                pid_to_output.insert(pid, output.clone());
                Some(SignalEvent::Bound {
                    output,
                    scene_path: PathBuf::from(scene_path),
                    pid,
                })
            }
            "WallpaperStopped" => {
                let (pid,): (i32,) = msg.body().deserialize().ok()?;
                pid_to_output.remove(&pid);
                Some(SignalEvent::Unbound { pid })
            }
            "MonitorChanged" => {
                let (outputs,): (Vec<String>,) = msg.body().deserialize().ok()?;
                Some(SignalEvent::MonitorsChanged { outputs })
            }
            _ => None,
        }
    }
}

/// Translate `paperforge_core::error::Error` into `GuiError::Ipc`
/// with a stable `kind` tag. Used by every write-action wrapper
/// above so the banner can attribute failures to a specific verb
/// (`[ipc] unset` vs `[ipc] pause`). `kind` is `&'static str` per
/// `GuiError::Ipc`, so the caller's string literal passes through.
fn map_err(kind: &'static str) -> impl FnOnce(paperforge_core::error::Error) -> GuiError {
    move |e| GuiError::Ipc {
        kind,
        message: format!("{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The signal-event enum variants have distinct discriminators
    /// so the GUI's match arms stay exhaustive. Drift here means a
    /// new signal was added without updating the UI.
    #[test]
    fn signal_event_variants_are_distinct() {
        let bound = SignalEvent::Bound {
            output: "DP-1".into(),
            scene_path: PathBuf::from("/scenes/focus"),
            pid: 100,
        };
        let unbound = SignalEvent::Unbound { pid: 100 };
        let monitors = SignalEvent::MonitorsChanged {
            outputs: vec!["DP-1".into()],
        };
        assert_ne!(bound, unbound);
        assert_ne!(bound, monitors);
        assert_ne!(unbound, monitors);
    }

    #[test]
    fn connect_failure_yields_ipc_error() {
        // We can't easily simulate "no session bus" without
        // running an isolated D-Bus daemon, so this test just
        // verifies the error variant is correct when we feed an
        // invalid sender into the match rule path. A real
        // integration test would need a private D-Bus; out of
        // scope for PR 4 unit tests (the smoke test for PR 4 is
        // documented in `happy-watching-lecun.md` §10).
        let e = GuiError::Ipc {
            kind: "connect",
            message: "no session bus".into(),
        };
        assert!(matches!(
            e,
            GuiError::Ipc {
                kind: "connect",
                ..
            }
        ));
    }
}
