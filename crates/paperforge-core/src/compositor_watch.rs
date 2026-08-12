//! Compositor output watch.
//!
//! Polls the active Wayland compositor (niri) for its output list
//! and emits structured tracing events when the set diverges from
//! paperforge's tracked outputs. Used by the daemon supervisor to
//! detect compositor restarts or hotplug events without depending
//! on a compositor-specific IPC protocol.
//!
//! # Why
//!
//! After a compositor crash + respawn, the `wl_output` handles LWE
//! attached to are gone but the LWE process keeps rendering to
//! phantom surfaces (the old globals). The user sees wallpapers
//! stuck or layered wrong, and paperforge has no signal to react.
//! This module emits a `CompositorOutputChanged` event so the
//! supervisor can `pool.unbind()` the stale outputs and re-bind
//! fresh ones.
//!
//! # Design
//!
//! [`query_niri_outputs`] shells out to `niri msg --json outputs`.
//! Returns `None` on niri-missing or non-zero exit so the caller
//! treats "no signal" as a non-event (don't spam the log when
//! the compositor CLI isn't installed in CI).
//!
//! [`parse_niri_outputs`] is the pure-function variant for tests.
//! Same JSON shape as [`crate::hotplug::parse_outputs`] but
//! tolerant of the niri-specific wrapper (`{"outputs": [...]}`).
//!
//! [`diff_outputs`] computes `(added, removed)` sets between the
//! compositor snapshot and paperforge's `LweBackend::outputs_with_pids()`
//! snapshot.
//!
//! [`watch_loop`] is the supervisor loop: polls every `interval`,
//! cancels on a [`tokio::sync::Notify`] signal.
//!
//! No new dependencies: `serde_json` is already in the workspace.

use std::collections::BTreeSet;
use std::time::Duration;

/// Compositor output snapshot, returned by [`query_niri_outputs`]
/// and [`parse_niri_outputs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositorOutputs {
    /// Set of currently-connected output names. Stored as
    /// `BTreeSet` so `diff_outputs` is deterministic (no hash
    /// randomization).
    pub outputs: BTreeSet<String>,
}

/// Query the compositor (`niri msg --json outputs`) and parse the
/// result. Returns `None` when:
///
/// - `niri` is not in `$PATH`,
/// - the process exits non-zero (e.g. no running compositor),
/// - the stdout is not valid JSON, or
/// - the JSON does not have an `outputs` array of objects with a
///   `name` field.
///
/// Callers should treat `None` as "no signal" — don't log spam,
/// don't error. A non-zero exit is expected in CI / headless
/// environments without a running compositor.
pub async fn query_niri_outputs() -> Option<CompositorOutputs> {
    let out = tokio::process::Command::new("niri")
        .arg("msg")
        .arg("--json")
        .arg("outputs")
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_niri_outputs(&out.stdout)
}

/// Pure-function variant of [`query_niri_outputs`]. Takes the niri
/// JSON output bytes and parses the output names. Returns `None`
/// on any parse failure — same semantics as the production entry
/// point so tests cover the same edge cases.
pub fn parse_niri_outputs(json_bytes: &[u8]) -> Option<CompositorOutputs> {
    let json: serde_json::Value = serde_json::from_slice(json_bytes).ok()?;
    // The shape is `{"outputs": [{"name": "DP-1", ...}, ...]}`.
    // Be tolerant: also accept a bare array `[{"name": ...}]`
    // (swaymsg-like) by falling back if the `outputs` key is missing.
    let arr = json
        .get("outputs")
        .and_then(|v| v.as_array())
        .or_else(|| json.as_array())?;
    let names: BTreeSet<String> = arr
        .iter()
        .filter_map(|o| o.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    Some(CompositorOutputs { outputs: names })
}

/// Compute the divergence between the compositor's reported
/// outputs and the set the caller knows paperforge supervises.
/// Returns `(added, removed)` — outputs that appeared or vanished
/// from the compositor's view since the last poll.
///
/// `backend_outputs` is typically the result of
/// [`crate::backend::LweBackend::outputs_with_pids`] (an async
/// call the supervisor makes before invoking `diff_outputs`); we
/// accept it as a plain set to keep this function sync + testable.
pub fn diff_outputs(
    compositor: &CompositorOutputs,
    backend_outputs: &BTreeSet<String>,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let added: BTreeSet<String> = compositor
        .outputs
        .difference(backend_outputs)
        .cloned()
        .collect();
    let removed: BTreeSet<String> = backend_outputs
        .difference(&compositor.outputs)
        .cloned()
        .collect();
    (added, removed)
}

/// Watch loop. Polls the compositor every `interval` and emits a
/// structured tracing event when the output set diverges from
/// paperforge's tracked outputs. Exits when `cancel` fires (the
/// `tokio::sync::Notify` is shared with the supervisor).
///
/// The loop is "fire-and-forget": no event channel back to the
/// daemon. Operators get the signal via journald
/// (`journalctl -u paperforge | grep event=compositor_outputs_diverged`).
/// The D-Bus layer can subscribe to journald via its own monitor
/// if it wants to surface this to the GUI.
///
/// `backend_outputs` is captured by the caller (typically
/// `LweBackend::outputs_with_pids().await` once before
/// `tokio::spawn`) and passed in as a shared `BTreeSet` so the
/// supervisor can keep refreshing it without re-spawning the
/// watch loop.
pub async fn watch_loop(
    backend_outputs: std::sync::Arc<tokio::sync::RwLock<BTreeSet<String>>>,
    interval: Duration,
    cancel: tokio::sync::Notify,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancel.notified() => {
                tracing::info!(
                    target: "paperforge",
                    event = "compositor_watch_shutdown",
                    "compositor watch loop shutting down"
                );
                return;
            }
            _ = ticker.tick() => {
                if let Some(comp) = query_niri_outputs().await {
                    let backend_set = backend_outputs.read().await.clone();
                    let (added, removed) = diff_outputs(&comp, &backend_set);
                    if !added.is_empty() || !removed.is_empty() {
                        tracing::warn!(
                            target: "paperforge",
                            event = "compositor_outputs_diverged",
                            added = ?added,
                            removed = ?removed,
                            "compositor outputs diverged from paperforge state"
                        );
                    } else {
                        tracing::debug!(
                            target: "paperforge",
                            event = "compositor_outputs_in_sync",
                            count = comp.outputs.len(),
                            "compositor outputs in sync with paperforge state"
                        );
                    }
                }
                // None = niri unavailable; stay silent (don't spam).
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_niri_outputs_basic() {
        let json = br#"{"outputs":[{"name":"DP-1"},{"name":"eDP-1"}]}"#;
        let parsed = parse_niri_outputs(json).expect("parse");
        assert!(parsed.outputs.contains("DP-1"));
        assert!(parsed.outputs.contains("eDP-1"));
        assert_eq!(parsed.outputs.len(), 2);
    }

    #[test]
    fn parse_niri_outputs_with_extra_fields() {
        // niri embeds more than just `name` (make, model, x, y, ...).
        // Tolerant parsing: ignore unknown fields.
        let json = br#"{"outputs":[
            {"name":"DP-1","make":"Dell","model":"U2719D","x":0,"y":0},
            {"name":"eDP-1","make":"BOE"}
        ]}"#;
        let parsed = parse_niri_outputs(json).expect("parse");
        assert_eq!(parsed.outputs.len(), 2);
        assert!(parsed.outputs.contains("DP-1"));
    }

    #[test]
    fn parse_niri_outputs_empty() {
        let json = br#"{"outputs":[]}"#;
        let parsed = parse_niri_outputs(json).expect("parse");
        assert!(parsed.outputs.is_empty());
    }

    #[test]
    fn parse_niri_outputs_bare_array_fallback() {
        // Some compositors emit a bare array (swaymsg shape).
        let json = br#"[{"name":"HDMI-A-1"}]"#;
        let parsed = parse_niri_outputs(json).expect("parse");
        assert!(parsed.outputs.contains("HDMI-A-1"));
    }

    #[test]
    fn parse_niri_outputs_malformed() {
        // Not JSON at all.
        assert!(parse_niri_outputs(b"not json").is_none());
        // JSON but no `outputs` field and not an array.
        assert!(parse_niri_outputs(br#"{"wrong_key":[]}"#).is_none());
        // JSON but malformed syntax.
        assert!(parse_niri_outputs(b"{outputs:[}]").is_none());
        // Empty input.
        assert!(parse_niri_outputs(b"").is_none());
    }

    #[test]
    fn parse_niri_outputs_skips_entries_without_name() {
        let json = br#"{"outputs":[{"name":"DP-1"},{"no_name":true},{"name":""}]}"#;
        let parsed = parse_niri_outputs(json).expect("parse");
        // Only the entry with a real `name` survives. The empty
        // string is technically a valid `name` field but useless —
        // we keep it because it's a real string.
        assert!(parsed.outputs.contains("DP-1"));
        assert_eq!(parsed.outputs.len(), 2); // DP-1 + "" (empty)
    }

    /// Pure-function diff math exercised against two
    /// `CompositorOutputs` snapshots. We don't need a real
    /// `LweBackend` here — the spec is on the set algebra, not on
    /// the backend accessor.
    #[test]
    fn diff_outputs_added_and_removed() {
        let comp = CompositorOutputs {
            outputs: BTreeSet::from(["DP-1".into(), "HDMI-A-1".into()]),
        };
        let backend_outputs = BTreeSet::from(["DP-1".into(), "eDP-1".into()]);
        let (added, removed) = diff_outputs(&comp, &backend_outputs);
        assert_eq!(added, BTreeSet::from(["HDMI-A-1".into()]));
        assert_eq!(removed, BTreeSet::from(["eDP-1".into()]));
    }

    #[test]
    fn diff_outputs_identical_sets_yield_empty() {
        let comp = CompositorOutputs {
            outputs: BTreeSet::from(["DP-1".into()]),
        };
        let backend_outputs = BTreeSet::from(["DP-1".into()]);
        let (added, removed) = diff_outputs(&comp, &backend_outputs);
        assert!(added.is_empty());
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_outputs_empty_compositor_yields_removed() {
        // Compositor reports nothing (could be in the middle of a
        // restart before outputs come back). Everything in
        // `backend_outputs` is "removed" from paperforge's POV.
        let comp = CompositorOutputs {
            outputs: BTreeSet::new(),
        };
        let backend_outputs = BTreeSet::from(["DP-1".into(), "eDP-1".into()]);
        let (added, removed) = diff_outputs(&comp, &backend_outputs);
        assert!(added.is_empty());
        assert_eq!(removed.len(), 2);
        assert!(removed.contains("DP-1"));
        assert!(removed.contains("eDP-1"));
    }

    #[test]
    fn diff_outputs_superset_compositor_yields_added() {
        let comp = CompositorOutputs {
            outputs: BTreeSet::from(["DP-1".into(), "HDMI-A-1".into(), "eDP-1".into()]),
        };
        let backend_outputs = BTreeSet::from(["DP-1".into()]);
        let (added, removed) = diff_outputs(&comp, &backend_outputs);
        assert_eq!(added.len(), 2);
        assert!(added.contains("HDMI-A-1"));
        assert!(added.contains("eDP-1"));
        assert!(removed.is_empty());
    }
}
