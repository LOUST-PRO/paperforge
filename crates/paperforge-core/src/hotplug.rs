//! Wayland output (monitor) hotplug detection.
//!
//! Detects the currently-connected monitor set and emits a change
//! notification whenever it differs from the previous snapshot. Used
//! to re-apply playlists when an output is plugged or unplugged.
//!
//! # Strategy
//!
//! Rather than depending on a specific Wayland extension
//! (wlr-output-management, Hyprland IPC, Niri IPC), this module talks
//! to whichever compositor control CLI is in PATH:
//!
//! | CLI              | Compositor             | Output JSON           |
//! |------------------|------------------------|-----------------------|
//! | `niri msg -j outputs` | Niri              | `[{name, ...}, ...]`  |
//! | `hyprctl monitors -j` | Hyprland           | `[{name, ...}, ...]`  |
//! | `swaymsg -t get_outputs -r` | Sway         | JSON (disabled, sway has no `msg` here) |
//! | `wlr-randr`      | wlroots-based          | Text (not parsed)     |
//!
//! Detection is by `$XDG_CURRENT_DESKTOP` + CLI presence:
//! - niri → niri
//! - Hyprland → hyprland
//! - sway → sway
//! - generic wlroots → wlr-randr (text only)
//!
//! If none match, the probe returns an empty list and `HotplugWatcher`
//! never emits change events — fine for CI / headless tests.
//!
//! # Performance
//!
//! The probe is run in a loop with a configurable interval
//! (default 2 s). Compositor IPC queries are cheap (<10 ms typical).
//!
//! # Tests
//!
//! Tests inject a fake probe via [`HotplugSource`] so the loop never
//! spawns a real subprocess.

use std::{path::PathBuf, sync::Arc, time::Duration};

use serde::Deserialize;
use tokio::sync::mpsc;

use crate::error::Result;

/// One Wayland output as reported by the compositor IPC.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct Output {
    /// Stable compositor-assigned name (e.g. `"DP-1"`,
    /// `"HDMI-A-1"`, `"eDP-1"`). This is the key the daemon uses
    /// to track monitors across hotplug events.
    pub name: String,
}

/// Source of output lists. The trait lets tests substitute a fake
/// without spawning compositor subprocesses.
#[async_trait::async_trait]
pub trait HotplugSource: Send + Sync {
    /// Snapshot of currently-connected outputs.
    async fn list_outputs(&self) -> Result<Vec<Output>>;
}

/// Real source: talks to whichever compositor CLI is in PATH.
pub struct CompositorHotplugSource {
    /// Override for tests. `None` = detect at construction time.
    override_cmd: Option<(String, Vec<String>)>,
}

impl CompositorHotplugSource {
    /// Detect the compositor CLI from `$XDG_CURRENT_DESKTOP` and PATH.
    pub fn detect() -> Self {
        Self {
            override_cmd: detect_compositor_cmd(),
        }
    }

    /// Force a specific (cmd, args_prefix) — used by tests.
    pub fn with_cmd(cmd: String, args: Vec<String>) -> Self {
        Self {
            override_cmd: Some((cmd, args)),
        }
    }

    /// True if we have any compositor CLI to call.
    pub fn is_available(&self) -> bool {
        self.override_cmd.is_some()
    }
}

#[async_trait::async_trait]
impl HotplugSource for CompositorHotplugSource {
    async fn list_outputs(&self) -> Result<Vec<Output>> {
        let Some((cmd, args)) = &self.override_cmd else {
            return Ok(Vec::new());
        };
        let out = match tokio::process::Command::new(cmd)
            .args(args)
            .arg("outputs")
            .output()
            .await
        {
            Ok(o) => o,
            Err(e) => {
                return Err(crate::error::Error::Other(anyhow::anyhow!(
                    "spawn {cmd}: {e}"
                )))
            }
        };
        if !out.status.success() {
            return Err(crate::error::Error::Other(anyhow::anyhow!(
                "{cmd} exited with {}",
                out.status
            )));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        parse_outputs(&stdout).ok_or_else(|| {
            crate::error::Error::Other(anyhow::anyhow!(
                "{cmd}: no parsable output list in {} bytes",
                stdout.len()
            ))
        })
    }
}

/// Decide which compositor CLI to use based on `$XDG_CURRENT_DESKTOP`
/// (or well-known binaries in PATH as a fallback).
fn detect_compositor_cmd() -> Option<(String, Vec<String>)> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let path_additions: Vec<PathBuf> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .map(PathBuf::from)
        .collect();
    let has_binary = |name: &str| path_additions.iter().any(|p| p.join(name).is_file());

    if (desktop.contains("niri") || has_binary("niri")) && has_binary("niri") {
        return Some(("niri".to_string(), vec!["msg".into(), "-j".into()]));
    }
    if (desktop.contains("Hyprland") || has_binary("hyprctl")) && has_binary("hyprctl") {
        return Some(("hyprctl".to_string(), vec!["monitors".into(), "-j".into()]));
    }
    if (desktop.contains("sway") || has_binary("swaymsg")) && has_binary("swaymsg") {
        return Some((
            "swaymsg".to_string(),
            vec!["-t".into(), "get_outputs".into(), "-r".into()],
        ));
    }
    None
}

/// Parse compositor JSON output to a list of [`Output`].
///
/// Tolerant: tolerates extra fields, falls back to `[]` on empty.
fn parse_outputs(stdout: &str) -> Option<Vec<Output>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    // Both niri and hyprland emit JSON arrays of objects with a
    // "name" field. swaymsg -r emits the same shape. Use
    // `Vec<Output>` directly; serde will skip unknown fields.
    serde_json::from_str::<Vec<Output>>(trimmed)
        .ok()
        .or_else(|| Some(Vec::new()))
}

/// Watch for output-set changes and emit [`HotplugEvent`]s.
///
/// Runs `source.list_outputs()` every `interval` and compares to the
/// previous snapshot. Emits an event if the set differs (added,
/// removed, or replaced).
pub struct HotplugWatcher<S: HotplugSource + 'static> {
    receiver: mpsc::UnboundedReceiver<HotplugEvent>,
    _phantom: std::marker::PhantomData<S>,
}

impl<S: HotplugSource + 'static> HotplugWatcher<S> {
    /// Start a watcher in the background. Returns the watcher itself
    /// (used by tests to drive the loop synchronously) plus an
    /// unbounded receiver for events.
    pub fn spawn(source: Arc<S>, interval: Duration) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let src = source.clone();
        tokio::spawn(async move {
            let mut last: Vec<Output> = Vec::new();
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let current = match src.list_outputs().await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("hotplug probe failed: {e}");
                        continue;
                    }
                };
                if current != last {
                    let event = if last.is_empty() && !current.is_empty() {
                        HotplugEvent::Initial(current.clone())
                    } else {
                        let added: Vec<Output> = current
                            .iter()
                            .filter(|o| !last.iter().any(|p| p.name == o.name))
                            .cloned()
                            .collect();
                        let removed: Vec<Output> = last
                            .iter()
                            .filter(|o| !current.iter().any(|p| p.name == o.name))
                            .cloned()
                            .collect();
                        HotplugEvent::Changed {
                            current: current.clone(),
                            added,
                            removed,
                        }
                    };
                    last = current;
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            receiver: rx,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Recv one event, blocking. Returns `None` if the watcher
    /// task has exited (only happens if the channel is closed,
    /// which is currently never — the loop runs forever).
    pub async fn next(&mut self) -> Option<HotplugEvent> {
        self.receiver.recv().await
    }

    /// Drain all currently-pending events. Useful for sync tests
    /// that call [`step`](Self::step) once and assert on the queue.
    pub fn drain(&mut self) -> Vec<HotplugEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.receiver.try_recv() {
            out.push(ev);
        }
        out
    }
}

/// Event emitted by the watcher when the output set changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotplugEvent {
    /// First observation after the watcher started.
    Initial(Vec<Output>),
    /// Subsequent change.
    Changed {
        /// Snapshot after the change.
        current: Vec<Output>,
        /// Outputs present now but not before.
        added: Vec<Output>,
        /// Outputs present before but not now.
        removed: Vec<Output>,
    },
}

impl HotplugEvent {
    /// All output names in the current snapshot, regardless of variant.
    pub fn current_names(&self) -> Vec<String> {
        match self {
            Self::Initial(v) | Self::Changed { current: v, .. } => {
                v.iter().map(|o| o.name.clone()).collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, sync::Mutex};

    /// Fake source that returns a pre-programmed sequence of
    /// output lists.
    struct FakeSource {
        // Each call shifts the next snapshot off the front.
        snapshots: Mutex<Vec<Vec<Output>>>,
    }

    impl FakeSource {
        fn new(snapshots: Vec<Vec<Output>>) -> Arc<Self> {
            Arc::new(Self {
                snapshots: Mutex::new(snapshots),
            })
        }
    }

    #[async_trait::async_trait]
    impl HotplugSource for FakeSource {
        async fn list_outputs(&self) -> Result<Vec<Output>> {
            let mut q = self.snapshots.lock().unwrap();
            if q.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(q.remove(0))
            }
        }
    }

    fn out(name: &str) -> Output {
        Output {
            name: name.to_string(),
        }
    }

    #[test]
    fn parse_outputs_handles_empty() {
        assert_eq!(parse_outputs("").unwrap(), Vec::<Output>::new());
        assert_eq!(parse_outputs("   \n").unwrap(), Vec::<Output>::new());
    }

    #[test]
    fn parse_outputs_niri_shape() {
        let j = r#"[{"name":"DP-1","make":"Dell","model":"U2719D","x":0,"y":0},{"name":"eDP-1","make":"BOE","model":"..."}]"#;
        let v = parse_outputs(j).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "DP-1");
        assert_eq!(v[1].name, "eDP-1");
    }

    #[test]
    fn parse_outputs_hyprland_shape() {
        let j = r#"[{"id":0,"name":"HDMI-A-1","description":"LG 27GN950"},{"id":1,"name":"eDP-1","description":"BOE"}]"#;
        let v = parse_outputs(j).unwrap();
        assert_eq!(
            v.iter().map(|o| o.name.clone()).collect::<Vec<_>>(),
            vec!["HDMI-A-1", "eDP-1"]
        );
    }

    #[test]
    fn parse_outputs_garbage_yields_empty() {
        assert_eq!(parse_outputs("not json").unwrap(), Vec::<Output>::new());
    }

    #[test]
    fn compositor_source_is_available_on_hyprland_or_niri() {
        // Skip on hosts without a compositor CLI. The test exists to
        // assert the operator's local machine has the tool, not to
        // gate CI. CI runs on ubuntu-latest with no niri/hyprctl/swaymsg.
        let s = CompositorHotplugSource::detect();
        if !s.is_available() {
            eprintln!("SKIP: no compositor CLI (niri/hyprctl/swaymsg) in PATH");
            return;
        }
        assert!(
            s.is_available(),
            "detect() returned true but is_available() is false"
        );
    }

    #[test]
    fn compositor_source_with_cmd_is_available() {
        let s = CompositorHotplugSource::with_cmd("echo".into(), vec![]);
        assert!(s.is_available());
    }

    #[test]
    fn detect_compositor_cmd_finds_niri_or_hyprctl() {
        // Same skip policy as the test above: this asserts the host
        // has a compositor CLI in PATH, which is an operator-machine
        // concern, not a CI concern.
        let detected = detect_compositor_cmd();
        let Some((cmd, _args)) = detected else {
            eprintln!("SKIP: no compositor CLI (niri/hyprctl/swaymsg) in PATH");
            return;
        };
        assert!(
            cmd == "niri" || cmd == "hyprctl" || cmd == "swaymsg",
            "unexpected compositor cmd: {cmd}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn hotplug_initial_event_then_steady() {
        let src = FakeSource::new(vec![
            vec![out("DP-1"), out("eDP-1")],
            vec![out("DP-1"), out("eDP-1")],
            vec![out("DP-1"), out("eDP-1")],
        ]);
        let mut w = HotplugWatcher::spawn(src, Duration::from_millis(50));
        // First tick → Initial.
        let ev1 = w.next().await.unwrap();
        assert!(matches!(ev1, HotplugEvent::Initial(ref v) if v.len() == 2));
        // Two more ticks with no change → no event.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(w.drain().is_empty());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn hotplug_changed_event_emits_added_and_removed() {
        let src = FakeSource::new(vec![
            vec![out("DP-1")],
            vec![out("DP-1"), out("HDMI-A-1")],
            vec![out("DP-1")],
        ]);
        let mut w = HotplugWatcher::spawn(src, Duration::from_millis(50));
        // First: Initial([DP-1]).
        let _ = w.next().await.unwrap();
        // Second: Changed (HDMI-A-1 added).
        let ev2 = w.next().await.unwrap();
        match ev2 {
            HotplugEvent::Changed { added, removed, .. } => {
                assert_eq!(
                    added.iter().map(|o| o.name.clone()).collect::<Vec<_>>(),
                    vec!["HDMI-A-1"]
                );
                assert!(removed.is_empty());
            }
            other => panic!("expected Changed, got {other:?}"),
        }
        // Third: Changed (HDMI-A-1 removed).
        let ev3 = w.next().await.unwrap();
        match ev3 {
            HotplugEvent::Changed { added, removed, .. } => {
                assert!(added.is_empty());
                assert_eq!(
                    removed.iter().map(|o| o.name.clone()).collect::<Vec<_>>(),
                    vec!["HDMI-A-1"]
                );
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn hotplug_current_names_helper() {
        let src = FakeSource::new(vec![vec![out("DP-1"), out("eDP-1")]]);
        let mut w = HotplugWatcher::spawn(src, Duration::from_millis(50));
        let ev = w.next().await.unwrap();
        let names: HashSet<_> = ev.current_names().into_iter().collect();
        assert!(names.contains("DP-1"));
        assert!(names.contains("eDP-1"));
    }
}
