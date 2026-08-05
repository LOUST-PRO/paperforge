//! niri Wayland compositor fullscreen detection.
//!
//! Polls `niri msg --json` (windows + workspaces + outputs) and
//! produces a snapshot of which Wayland outputs are fully covered
//! by a fullscreen window. The daemon's [`crate::fullscreen_dispatcher`]
//! uses this to drive per-output LWE kill/resume so a Steam game
//! going fullscreen releases the GPU/DRM socket while the other
//! monitors keep rendering their wallpaper normally.
//!
//! Detection heuristic: a window is fullscreen on its output when
//! `layout.tile_size` matches the output's `logical.{width, height}`
//! within a small tolerance (5px to absorb compositor rounding +
//! minimal window decorations). This catches the common cases:
//!
//! - Steam game's native fullscreen: tile_size = output exact
//! - Steam borderless fullscreen: same, tile_size matches
//! - Wayland-native fullscreen request: same
//!
//! We deliberately do NOT infer fullscreen from `is_focused` (a
//! tiled focused window at 1920x1080 in a grid is NOT fullscreen
//! of any single output) and not from `app_id` substring (Steam
//! itself isn't fullscreen, only when a game is launched).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

use crate::error::{Error, Result};

/// Logical dimensions + transform for one Wayland output.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputLogical {
    /// Logical width in output-local pixels (post-transform).
    pub width: i32,
    /// Logical height in output-local pixels (post-transform).
    pub height: i32,
    /// `"Normal"`, `"90"`, `"180"`, `"270"` — niri's transform strings.
    pub transform: String,
}

/// Snapshot of one niri output.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    /// Wayland output name (e.g. `"eDP-1"`, `"DP-1"`, `"HDMI-A-1"`).
    pub name: String,
    /// Logical geometry used by niri to position windows.
    pub logical: OutputLogical,
}

/// Snapshot of one niri workspace.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceInfo {
    /// niri-assigned workspace id (stable across session).
    pub id: u64,
    /// Which output the workspace is currently on. `None` when
    /// the workspace is not attached to any output (e.g. hidden
    /// in another output's scrollable list).
    pub output: Option<String>,
    /// Whether this workspace is the active one on its output.
    /// Only the active workspace's windows are visible.
    pub is_active: bool,
}

/// Snapshot of one niri window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    /// niri-assigned window id (stable across the window's lifetime).
    pub id: u64,
    /// Workspace the window is on. `None` for floating/popped-out
    /// windows that aren't bound to a workspace.
    pub workspace_id: Option<u64>,
    /// App-id (e.g. `"vivaldi-stable"`, `"steam_app_1623730"`).
    /// `None` for unidentifiable windows.
    pub app_id: Option<String>,
    /// Window title. `None` when the window has no title.
    pub title: Option<String>,
    /// `(width, height)` of the tiled slot in output-local pixels.
    pub tile_size: (f32, f32),
}

/// Cross-linked snapshot of niri state. The watcher uses
/// [`NiriSnapshot::fullscreen_outputs`] to derive which outputs
/// need their LWE killed/resumed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NiriSnapshot {
    /// All Wayland outputs known to niri (keyed by name).
    pub outputs: BTreeMap<String, OutputInfo>,
    /// All workspaces known to niri (keyed by workspace id).
    pub workspaces: BTreeMap<u64, WorkspaceInfo>,
    /// All windows known to niri.
    pub windows: Vec<WindowInfo>,
}

impl NiriSnapshot {
    /// Set of output names that are currently fully covered by a
    /// fullscreen window (i.e. some window on the active workspace
    /// of that output has tile_size ~= output's logical size).
    pub fn fullscreen_outputs(&self) -> BTreeSet<String> {
        let mut fullscreen = BTreeSet::new();
        // For each active workspace, look at its windows and check
        // against its output's logical size.
        for ws in self.workspaces.values().filter(|w| w.is_active) {
            let Some(out_name) = ws.output.as_ref() else {
                continue;
            };
            let Some(output) = self.outputs.get(out_name) else {
                continue;
            };
            let ws_id = ws.id;
            let on_ws: Vec<&WindowInfo> = self
                .windows
                .iter()
                .filter(|w| w.workspace_id == Some(ws_id))
                .collect();
            if on_ws.iter().any(|w| is_window_fullscreen(w, output)) {
                fullscreen.insert(out_name.clone());
            }
        }
        fullscreen
    }
}

/// Heuristic: is `window` covering `output`'s logical area fully?
///
/// We tolerate up to 5 px of slop because niri occasionally rounds
/// tile sizes down when decorations or scrollbar sliders are
/// visible. Anything tighter would risk false negatives on
/// borderless fullscreen Steam games.
pub fn is_window_fullscreen(window: &WindowInfo, output: &OutputInfo) -> bool {
    const TOLERANCE: f32 = 5.0;
    let dx = (window.tile_size.0 - output.logical.width as f32).abs();
    let dy = (window.tile_size.1 - output.logical.height as f32).abs();
    dx < TOLERANCE && dy < TOLERANCE
}

/// Query niri via three `niri msg --json X` invocations and parse
/// the results. Runs the three commands serially in a thread via
/// [`tokio::task::spawn_blocking`] so the async runtime isn't
/// blocked on the subprocess.
///
/// Returns an [`Error::BackendFailure`] if `niri` is missing or
/// any of the three commands fails (suggests the compositor isn't
/// niri, or the daemon has lost its IPC socket).
pub async fn snapshot() -> Result<NiriSnapshot> {
    tokio::task::spawn_blocking(snapshot_blocking)
        .await
        .map_err(|e| Error::BackendFailure {
            kind: "niri-ipc".to_string(),
            message: format!("spawn_blocking join error: {e}"),
        })?
}

fn snapshot_blocking() -> Result<NiriSnapshot> {
    let outputs_raw = run_niri(&["outputs"])?;
    let workspaces_raw = run_niri(&["workspaces"])?;
    let windows_raw = run_niri(&["windows"])?;

    let outputs = parse_outputs(&outputs_raw)?;
    let workspaces = parse_workspaces(&workspaces_raw)?;
    let windows = parse_windows(&windows_raw)?;

    Ok(NiriSnapshot {
        outputs,
        workspaces,
        windows,
    })
}

fn run_niri(args: &[&str]) -> Result<String> {
    let out = Command::new("niri")
        .arg("msg")
        .arg("--json")
        .args(args)
        .output()
        .map_err(|e| Error::BackendFailure {
            kind: "niri-ipc".to_string(),
            message: format!("failed to spawn `niri msg --json {}`: {e}", args.join(" ")),
        })?;
    if !out.status.success() {
        return Err(Error::BackendFailure {
            kind: "niri-ipc".to_string(),
            message: format!(
                "`niri msg --json {}` exited {}: {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse_outputs(raw: &str) -> Result<BTreeMap<String, OutputInfo>> {
    // niri's outputs JSON is an OBJECT (not array) keyed by output
    // name, each value carrying name, logical, etc. Use a permissive
    // parser (serde_json::Value) so a future schema addition
    // doesn't break us.
    let val: serde_json::Value = serde_json::from_str(raw).map_err(|e| Error::BackendFailure {
        kind: "niri-ipc".to_string(),
        message: format!("outputs JSON parse: {e}"),
    })?;
    let obj = val.as_object().ok_or_else(|| Error::BackendFailure {
        kind: "niri-ipc".to_string(),
        message: "outputs JSON is not an object".to_string(),
    })?;
    let mut out = BTreeMap::new();
    for (name, info) in obj {
        let logical = info.get("logical").ok_or_else(|| Error::BackendFailure {
            kind: "niri-ipc".to_string(),
            message: format!("output {name}: no logical field"),
        })?;
        let width = logical
            .get("width")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| Error::BackendFailure {
                kind: "niri-ipc".to_string(),
                message: format!("output {name}: logical.width not i64"),
            })? as i32;
        let height = logical
            .get("height")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| Error::BackendFailure {
                kind: "niri-ipc".to_string(),
                message: format!("output {name}: logical.height not i64"),
            })? as i32;
        let transform = logical
            .get("transform")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Normal")
            .to_string();
        out.insert(
            name.clone(),
            OutputInfo {
                name: name.clone(),
                logical: OutputLogical {
                    width,
                    height,
                    transform,
                },
            },
        );
    }
    Ok(out)
}

fn parse_workspaces(raw: &str) -> Result<BTreeMap<u64, WorkspaceInfo>> {
    let val: serde_json::Value = serde_json::from_str(raw).map_err(|e| Error::BackendFailure {
        kind: "niri-ipc".to_string(),
        message: format!("workspaces JSON parse: {e}"),
    })?;
    let arr = val.as_array().ok_or_else(|| Error::BackendFailure {
        kind: "niri-ipc".to_string(),
        message: "workspaces JSON is not an array".to_string(),
    })?;
    let mut out = BTreeMap::new();
    for ws in arr {
        let id = ws
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| Error::BackendFailure {
                kind: "niri-ipc".to_string(),
                message: "workspace: id not u64".to_string(),
            })?;
        let output = ws
            .get("output")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let is_active = ws
            .get("is_active")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        out.insert(
            id,
            WorkspaceInfo {
                id,
                output,
                is_active,
            },
        );
    }
    Ok(out)
}

fn parse_windows(raw: &str) -> Result<Vec<WindowInfo>> {
    let val: serde_json::Value = serde_json::from_str(raw).map_err(|e| Error::BackendFailure {
        kind: "niri-ipc".to_string(),
        message: format!("windows JSON parse: {e}"),
    })?;
    let arr = val.as_array().ok_or_else(|| Error::BackendFailure {
        kind: "niri-ipc".to_string(),
        message: "windows JSON is not an array".to_string(),
    })?;
    let mut out = Vec::new();
    for w in arr {
        let id = w
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| Error::BackendFailure {
                kind: "niri-ipc".to_string(),
                message: "window: id not u64".to_string(),
            })?;
        let workspace_id = w.get("workspace_id").and_then(serde_json::Value::as_u64);
        let app_id = w
            .get("app_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let title = w
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let layout = w.get("layout");
        let tile_size = layout
            .and_then(|l| l.get("tile_size"))
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                let w = a.first().and_then(serde_json::Value::as_f64).unwrap_or(0.0);
                let h = a.get(1).and_then(serde_json::Value::as_f64).unwrap_or(0.0);
                (w as f32, h as f32)
            })
            .unwrap_or((0.0, 0.0));
        out.push(WindowInfo {
            id,
            workspace_id,
            app_id,
            title,
            tile_size,
        });
    }
    Ok(out)
}

/// Convenience for testing: load a snapshot from disk-built JSON
/// fixtures (no `niri` binary required). Used by the unit tests
/// in this module.
#[cfg(test)]
pub fn snapshot_from_fixtures(
    outputs: &str,
    workspaces: &str,
    windows: &str,
) -> Result<NiriSnapshot> {
    Ok(NiriSnapshot {
        outputs: parse_outputs(outputs)?,
        workspaces: parse_workspaces(workspaces)?,
        windows: parse_windows(windows)?,
    })
}

// Suppress unused import warning for PathBuf (kept for API symmetry
// with future modules that may want to express paths).
#[allow(dead_code)]
fn _pathbuf_keep(_: PathBuf) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty snapshot returns no fullscreen outputs.
    #[test]
    fn empty_snapshot_has_no_fullscreen_outputs() {
        let snap = NiriSnapshot::default();
        assert!(snap.fullscreen_outputs().is_empty());
    }

    /// `is_window_fullscreen` true when tile_size matches logical
    /// exactly (the Steam fullscreen case).
    #[test]
    fn window_fullscreen_when_tile_matches_logical() {
        let output = OutputInfo {
            name: "eDP-1".to_string(),
            logical: OutputLogical {
                width: 1920,
                height: 1080,
                transform: "Normal".to_string(),
            },
        };
        let win = WindowInfo {
            id: 1,
            workspace_id: Some(7),
            app_id: Some("steam_app_1623730".to_string()),
            title: Some("Pal".to_string()),
            tile_size: (1920.0, 1080.0),
        };
        assert!(is_window_fullscreen(&win, &output));
    }

    /// Sub-5px slop is tolerated (compositor rounding + tiny
    /// decorations).
    #[test]
    fn window_fullscreen_tolerates_5px_slop() {
        let output = OutputInfo {
            name: "DP-1".to_string(),
            logical: OutputLogical {
                width: 1920,
                height: 1080,
                transform: "Normal".to_string(),
            },
        };
        let win = WindowInfo {
            id: 2,
            workspace_id: Some(8),
            app_id: None,
            title: None,
            tile_size: (1918.0, 1078.0),
        };
        assert!(is_window_fullscreen(&win, &output));
    }

    /// Tiled focused windows are NOT fullscreen even when focused.
    #[test]
    fn tiled_window_is_not_fullscreen() {
        let output = OutputInfo {
            name: "HDMI-A-1".to_string(),
            logical: OutputLogical {
                width: 1080,
                height: 1920,
                transform: "90".to_string(),
            },
        };
        let win = WindowInfo {
            id: 3,
            workspace_id: Some(9),
            app_id: Some("vivaldi-stable".to_string()),
            title: Some("Discord".to_string()),
            tile_size: (1861.0, 1042.0),
        };
        assert!(!is_window_fullscreen(&win, &output));
    }

    /// End-to-end: snapshot parsed from the user's actual niri
    /// state. Window 206 ("Pal") is fullscreen on eDP-1; windows
    /// 168 (Steam) and 191 (Discord) are NOT fullscreen on their
    /// outputs.
    #[test]
    fn real_world_snapshot_detects_pal_fullscreen_on_edp() {
        let outputs_json = r#"{
            "eDP-1": {"name": "eDP-1", "logical": {"width": 1920, "height": 1080, "transform": "Normal"}},
            "HDMI-A-1": {"name": "HDMI-A-1", "logical": {"width": 1080, "height": 1920, "transform": "90"}},
            "DP-1": {"name": "DP-1", "logical": {"width": 1920, "height": 1080, "transform": "Normal"}}
        }"#;
        let workspaces_json = r#"[
            {"id": 63, "idx": 1, "name": null, "output": "eDP-1", "is_active": true, "is_focused": false, "active_window_id": 206},
            {"id": 8, "idx": 2, "name": null, "output": "HDMI-A-1", "is_active": true, "is_focused": false, "active_window_id": 60},
            {"id": 67, "idx": 3, "name": null, "output": "DP-1", "is_active": true, "is_focused": true, "active_window_id": 191}
        ]"#;
        let windows_json = r#"[
            {"id": 168, "title": "Steam", "app_id": "steam", "workspace_id": 63, "is_focused": false, "is_floating": false, "layout": {"tile_size": [1010.0, 1026.0], "window_size": [1010, 1026]}},
            {"id": 191, "title": "Discord", "app_id": "discord", "workspace_id": 67, "is_focused": true, "is_floating": false, "layout": {"tile_size": [1861.0, 1042.0], "window_size": [1861, 1042]}},
            {"id": 206, "title": "Pal", "app_id": "steam_app_1623730", "workspace_id": 63, "is_focused": false, "is_floating": false, "layout": {"tile_size": [1920.0, 1080.0], "window_size": [1920, 1080]}}
        ]"#;

        let snap = snapshot_from_fixtures(outputs_json, workspaces_json, windows_json).unwrap();
        let fs = snap.fullscreen_outputs();
        assert_eq!(fs.len(), 1, "exactly eDP-1 should be fullscreen");
        assert!(fs.contains("eDP-1"));
        assert!(!fs.contains("DP-1"));
        assert!(!fs.contains("HDMI-A-1"));
    }

    /// Windows on inactive workspaces don't count, even if
    /// fullscreen-sized. The active workspace is what determines
    /// what's visible on each output.
    #[test]
    fn fullscreen_window_on_inactive_workspace_doesnt_count() {
        let outputs_json = r#"{
            "DP-1": {"name": "DP-1", "logical": {"width": 1920, "height": 1080, "transform": "Normal"}}
        }"#;
        let workspaces_json = r#"[
            {"id": 1, "idx": 1, "name": null, "output": "DP-1", "is_active": true, "is_focused": false, "active_window_id": null},
            {"id": 2, "idx": 2, "name": null, "output": null, "is_active": false, "is_focused": false, "active_window_id": null}
        ]"#;
        let windows_json = r#"[
            {"id": 50, "title": "Game", "app_id": "steam_app_x", "workspace_id": 2, "is_focused": false, "is_floating": false, "layout": {"tile_size": [1920.0, 1080.0], "window_size": [1920, 1080]}}
        ]"#;
        let snap = snapshot_from_fixtures(outputs_json, workspaces_json, windows_json).unwrap();
        assert!(snap.fullscreen_outputs().is_empty());
    }

    /// `parse_outputs` tolerates missing optional fields (e.g. a
    /// future niri that omits `transform`).
    #[test]
    fn parse_outputs_uses_normal_when_transform_missing() {
        let raw = r#"{
            "DP-2": {"name": "DP-2", "logical": {"width": 2560, "height": 1440}}
        }"#;
        let out = parse_outputs(raw).unwrap();
        let dp = out.get("DP-2").unwrap();
        assert_eq!(dp.logical.width, 2560);
        assert_eq!(dp.logical.height, 1440);
        assert_eq!(dp.logical.transform, "Normal");
    }

    /// Empty parse should not panic.
    #[test]
    fn parse_empty_inputs() {
        assert!(parse_workspaces("[]").unwrap().is_empty());
        assert!(parse_windows("[]").unwrap().is_empty());
        assert!(parse_outputs("{}").unwrap().is_empty());
    }
}
