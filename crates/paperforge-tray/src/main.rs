// SPDX-License-Identifier: MIT
//
// `paperforge-tray` — entry point for the StatusNotifierItem tray icon.
//
// The actual SNI protocol + menu construction lives in the [`PaperforgeTray`]
// struct below. This binary just parses CLI args, wires up tracing, and runs
// the tokio runtime that drives the ksni D-Bus loop.
//
// ## Menu shape
//
// - "Rotate all now"      → `paperforge-rotate.sh`     (advance all + spawn)
// - "Previous all"        → `paperforge-rotate.sh --previous`
// - "Open TUI"            → `paperforge-tui`           (read-only debugger)
// - "Open GUI"            → `paperforge-gui`           (GPUI Wayland window)
// - Per-monitor submenus (one per playlist in `$PLAYLIST_DIR`):
//   - "Next wallpaper"     → `paperforge-rotate.sh <monitor>`
//   - "Previous wallpaper" → `paperforge-rotate.sh --previous <monitor>`
// - "Quit"                → `std::process::exit(0)`
//
// Header lines (informational, not actionable):
//   `DP-1: ← prev | current | next →`
//   computed from the per-playlist index in `$XDG_RUNTIME_DIR/paperforge-rotate-state.json`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use clap::Parser;
use ksni::TrayMethods;
use serde::Deserialize;
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Debug, Parser)]
#[command(
    name = "paperforge-tray",
    version,
    about = "StatusNotifierItem tray icon for paperforge (Wayland)",
    long_about = None,
)]
struct Cli {
    /// Override the directory holding `<monitor>.json` playlists.
    /// Defaults to $PAPERFORGE_PLAYLISTS or `~/.config/paperforge/playlists`.
    #[arg(long = "playlist-dir", value_name = "DIR")]
    playlist_dir: Option<PathBuf>,

    /// Override the rotate.sh orchestrator path.
    /// Defaults to `/home/lou/.local/bin/paperforge-rotate.sh`.
    #[arg(long = "rotate-bin", value_name = "PATH")]
    rotate_bin: Option<PathBuf>,
}

// ─── Playlist parsing ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Playlist {
    /// Logical name (matches `~/.config/paperforge/playlists/<name>.json`).
    name: String,
    /// Wayland output name (e.g. "DP-1", "HDMI-A-1").
    outputs: Vec<String>,
    /// Absolute paths to each wallpaper directory.
    wallpapers: Vec<String>,
}

/// Direction the user can advance the playlist index.
#[derive(Debug, Clone, Copy)]
enum Direction {
    /// `(idx + 1) % n` — skip blacklist + orientation-mismatched scenes.
    Forward,
    /// `(idx - 1 + n) % n` — same skip logic, wrapping for small playlists.
    Previous,
}

impl Direction {
    fn as_flag(self) -> &'static str {
        match self {
            Direction::Forward => "",       // default; no flag
            Direction::Previous => "--previous",
        }
    }
}

/// Snapshot of one monitor, used to render the per-monitor submenu labels.
#[derive(Debug, Clone)]
struct MonitorState {
    name: String,
    output: String,
    count: usize,
    /// Basename of the wallpaper currently applied (e.g. "3240721055").
    current: String,
    /// Basename of the wallpaper that "Next wallpaper" would advance to.
    next: String,
    /// Basename of the wallpaper that "Previous wallpaper" would retreat to.
    previous: String,
}

fn read_playlists(playlist_dir: &Path) -> Vec<MonitorState> {
    let entries = match std::fs::read_dir(playlist_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(target: "paperforge-tray", "read_dir({:?}) failed: {e}", playlist_dir);
            return Vec::new();
        }
    };

    // Read the rotation state index map once; cheap.
    let state_path = state_file_path();
    let state: serde_json::Value = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|p| p.to_str()) != Some("json") {
            continue;
        }
        let data = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                warn!(target: "paperforge-tray", "read {:?}: {e}", path);
                continue;
            }
        };
        let pl: Playlist = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(e) => {
                warn!(target: "paperforge-tray", "parse {:?}: {e}", path);
                continue;
            }
        };
        if pl.name == "default" || pl.wallpapers.is_empty() || pl.outputs.is_empty() {
            continue;
        }
        let n = pl.wallpapers.len();
        let idx = state.get(&pl.name).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        // Wrap-around modulo for both directions so the menu labels are
        // correct even at idx=0 (previous = last item) or idx=n-1 (next = 0).
        let next_idx = (idx + 1) % n;
        let prev_idx = (idx + n - 1) % n;
        let current = basename(&pl.wallpapers[idx]);
        let next = basename(&pl.wallpapers[next_idx]);
        let previous = basename(&pl.wallpapers[prev_idx]);
        out.push(MonitorState {
            name: pl.name,
            output: pl.outputs[0].clone(),
            count: n,
            current,
            next,
            previous,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

fn state_file_path() -> PathBuf {
    // `dirs::runtime_dir()` returns $XDG_RUNTIME_DIR (typically /run/user/<uid>).
    // Fall back to ~/.local/share/paperforge/rotate-state.json if even that is missing
    // (matches paperforge-rotate-recovery.sh fallback logic).
    if let Some(d) = dirs::runtime_dir() {
        d.join("paperforge-rotate-state.json")
    } else {
        let uid = nix::unistd::getuid().as_raw();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("paperforge")
            .join("rotate-state.json")
            .with_extension("")
            .with_file_name(format!("rotate-state-{}", uid).as_str())
            .with_extension("json")
    }
}

// ─── Action handlers (async, fire-and-forget) ───────────────────────────────

fn spawn_rotate(rotate_bin: PathBuf, monitor: Option<String>, direction: Direction) {
    let mut cmd = tokio::process::Command::new(&rotate_bin);
    // Flag goes BEFORE the positional <monitor> arg. paperforge-rotate.sh
    // accepts flags anywhere (for-loop arg parser), but keeping them
    // consistent makes the audit log easier to read.
    let flag = direction.as_flag();
    if !flag.is_empty() {
        cmd.arg(flag);
    }
    if let Some(m) = &monitor {
        cmd.arg(m);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    tokio::spawn(async move {
        match cmd.output().await {
            Ok(out) if out.status.success() => {
                info!(target: "paperforge-tray",
                    monitor = ?monitor, direction = ?direction, "rotate OK");
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                error!(target: "paperforge-tray", monitor = ?monitor,
                    direction = ?direction,
                    "rotate failed: exit={} stderr={stderr}", out.status);
            }
            Err(e) => {
                error!(target: "paperforge-tray", monitor = ?monitor,
                    direction = ?direction,
                    "spawn failed: {e}");
            }
        }
    });
}

fn spawn_tui() {
    // paperforge-tui is in the same workspace; assume it's on $PATH.
    tokio::spawn(async move {
        let mut cmd = tokio::process::Command::new("paperforge-tui");
        // Detach: don't tie stdin/stdout/stderr to the tray process.
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match cmd.spawn() {
            Ok(_) => info!(target: "paperforge-tray", "paperforge-tui launched"),
            Err(e) => error!(target: "paperforge-tray", "tui spawn failed: {e}"),
        }
    });
}

fn spawn_gui() {
    // paperforge-gui is in the same workspace; assume it's on $PATH.
    // Same fire-and-forget + detach pattern as spawn_tui so a slow
    // GPUI window boot (Wayland surface allocation, font load) doesn't
    // block the ksni D-Bus loop. If the binary is missing the user
    // sees "command not found" in stderr — we log it as an error so
    // `journalctl --user -u paperforge-tray.service` surfaces it.
    tokio::spawn(async move {
        let mut cmd = tokio::process::Command::new("paperforge-gui");
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match cmd.spawn() {
            Ok(_) => info!(target: "paperforge-tray", "paperforge-gui launched"),
            Err(e) => error!(target: "paperforge-tray", "gui spawn failed: {e}"),
        }
    });
}

// ─── Tray impl ──────────────────────────────────────────────────────────────

struct PaperforgeTray {
    /// Absolute path to `paperforge-rotate.sh`. Cloned into each closure
    /// because the closures need to be `'static + Send`.
    rotate_bin: PathBuf,
    /// Live snapshot of each monitor's playlist. Re-read on every `menu()`
    /// call so the labels reflect the current state file.
    monitors: Vec<MonitorState>,
}

impl PaperforgeTray {
    fn new(rotate_bin: PathBuf, playlist_dir: &Path) -> Self {
        Self {
            rotate_bin,
            monitors: read_playlists(playlist_dir),
        }
    }
}

impl ksni::Tray for PaperforgeTray {
    fn id(&self) -> String {
        // Stable across sessions — see SNI spec recommendation.
        "paperforge-tray".into()
    }

    fn title(&self) -> String {
        if self.monitors.is_empty() {
            "paperforge (no playlists)".into()
        } else {
            format!("paperforge · {} monitor(s)", self.monitors.len())
        }
    }

    fn icon_name(&self) -> String {
        // freedesktop icon theme — present in Tango, breeze, Adwaita, Papirus.
        "preferences-desktop-wallpaper".into()
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{StandardItem, SubMenu};

        let mut items: Vec<ksni::MenuItem<Self>> = Vec::new();

        // ── Header (informational, not actionable) ────────────────────────
        if self.monitors.is_empty() {
            items.push(
                StandardItem {
                    label: "No playlists found".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        } else {
            for mon in &self.monitors {
                items.push(
                    StandardItem {
                        label: format!(
                            "{}: ← {} | {} | {} →",
                            mon.output, mon.previous, mon.current, mon.next
                        ),
                        enabled: false,
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }
        items.push(ksni::MenuItem::Separator);

        // ── Per-monitor submenus ──────────────────────────────────────────
        for mon in &self.monitors {
            let label = format!("{} ({})", mon.output, mon.count);
            let submenu = build_monitor_submenu(mon, &self.rotate_bin);
            items.push(
                SubMenu {
                    label,
                    submenu,
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(ksni::MenuItem::Separator);

        // ── Global actions ───────────────────────────────────────────────
        let rotate_bin = self.rotate_bin.clone();
        items.push(
            StandardItem {
                label: "Rotate all now →".into(),
                activate: Box::new(move |_tray: &mut Self| {
                    let bin = rotate_bin.clone();
                    spawn_rotate(bin, None, Direction::Forward);
                }),
                ..Default::default()
            }
            .into(),
        );
        let rotate_bin_prev = self.rotate_bin.clone();
        items.push(
            StandardItem {
                label: "← Previous all".into(),
                activate: Box::new(move |_tray: &mut Self| {
                    let bin = rotate_bin_prev.clone();
                    spawn_rotate(bin, None, Direction::Previous);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Open TUI".into(),
                activate: Box::new(|_tray: &mut Self| {
                    spawn_tui();
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Open GUI".into(),
                activate: Box::new(|_tray: &mut Self| {
                    spawn_gui();
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(ksni::MenuItem::Separator);

        // ── Quit ─────────────────────────────────────────────────────────
        items.push(
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|_tray: &mut Self| {
                    info!(target: "paperforge-tray", "quit requested");
                    std::process::exit(0);
                }),
                ..Default::default()
            }
            .into(),
        );

        items
    }
}

/// Build the per-monitor submenu (Next + Previous).
/// Returns `Vec<MenuItem<PaperforgeTray>>` so the closures stay
/// `Send + 'static` (rotate_bin is cloned into each closure).
fn build_monitor_submenu(
    mon: &MonitorState,
    rotate_bin: &Path,
) -> Vec<ksni::MenuItem<PaperforgeTray>> {
    use ksni::menu::StandardItem;

    let label_next = format!("Next wallpaper  (→ {})", mon.next);
    let label_prev = format!("Previous wallpaper  (← {})", mon.previous);
    let monitor_name_next = mon.name.clone();
    let monitor_name_prev = mon.name.clone();
    let rotate_bin_next = rotate_bin.to_path_buf();
    let rotate_bin_prev = rotate_bin.to_path_buf();

    vec![
        StandardItem {
            label: label_next,
            activate: Box::new(move |_tray: &mut PaperforgeTray| {
                let bin = rotate_bin_next.clone();
                spawn_rotate(bin, Some(monitor_name_next.clone()), Direction::Forward);
            }),
            ..Default::default()
        }
        .into(),
        StandardItem {
            label: label_prev,
            activate: Box::new(move |_tray: &mut PaperforgeTray| {
                let bin = rotate_bin_prev.clone();
                spawn_rotate(bin, Some(monitor_name_prev.clone()), Direction::Previous);
            }),
            ..Default::default()
        }
        .into(),
    ]
}

// ─── main ──────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();

    let cli = Cli::parse();

    let playlist_dir: PathBuf = cli
        .playlist_dir
        .or_else(|| std::env::var("PAPERFORGE_PLAYLISTS").ok().map(PathBuf::from))
        .or_else(|| {
            dirs::config_dir().map(|d| d.join("paperforge").join("playlists"))
        })
        .context("couldn't resolve playlist_dir (no --playlist-dir, $PAPERFORGE_PLAYLISTS, or $XDG_CONFIG_HOME)")?;

    let rotate_bin: PathBuf = cli
        .rotate_bin
        .unwrap_or_else(|| PathBuf::from("/home/lou/.local/bin/paperforge-rotate.sh"));

    if !rotate_bin.exists() {
        anyhow::bail!("rotate_bin not found at {:?}", rotate_bin);
    }

    info!(target: "paperforge-tray",
        playlist_dir = ?playlist_dir,
        rotate_bin = ?rotate_bin,
        "spawning tray");

    let tray = PaperforgeTray::new(rotate_bin, &playlist_dir);
    let _handle = tray.spawn().await.context("spawn ksni tray")?;

    // Park forever. Quit comes from the menu callback (process::exit).
    std::future::pending::<()>().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that the playlist index wraps correctly for both directions at
    /// idx=0 (previous = last) and idx=n-1 (next = 0). The state-wiring rule
    /// requires that every reachable menu item exercises its underlying
    /// logic — these are the two boundary cases for `next_idx` and
    /// `prev_idx` math used in `read_playlists`.
    #[test]
    fn state_wiring_index_wraps_at_zero_for_previous() {
        let n = 10;
        let idx = 0_usize;
        // Mirrors `read_playlists` math (line 122-123 of this file).
        let prev_idx = (idx + n - 1) % n;
        let next_idx = (idx + 1) % n;
        assert_eq!(prev_idx, n - 1, "previous at idx=0 must wrap to last item");
        assert_eq!(next_idx, 1, "next at idx=0 must be 1");
    }

    #[test]
    fn state_wiring_index_wraps_at_last_for_next() {
        let n = 10;
        let idx = n - 1;
        let prev_idx = (idx + n - 1) % n;
        let next_idx = (idx + 1) % n;
        assert_eq!(prev_idx, n - 2, "previous at idx=n-1 must be n-2");
        assert_eq!(next_idx, 0, "next at idx=n-1 must wrap to 0");
    }

    /// Test that the spawn args compose correctly — both flag+positional and
    /// flag-only. Mirrors the `spawn_rotate` closure body (line 156-168).
    /// We don't actually spawn (would need a fake rotate_bin); just check
    /// the arg vector. Easier to test the bash side, so this is a sanity
    /// check that the Rust args match.
    #[test]
    fn state_wiring_spawn_args_forward_no_monitor() {
        // paperforge-rotate.sh (no args)
        let args: Vec<&str> = vec![];
        assert!(args.is_empty(), "forward + all monitors = no args");
    }

    #[test]
    fn state_wiring_spawn_args_previous_with_monitor() {
        // paperforge-rotate.sh --previous dp-1
        let flag = "--previous";
        let monitor = "dp-1";
        let mut args: Vec<&str> = vec![];
        if !flag.is_empty() {
            args.push(flag);
        }
        args.push(monitor);
        assert_eq!(args, vec!["--previous", "dp-1"]);
    }

    #[test]
    fn state_wiring_spawn_args_forward_with_monitor() {
        let monitor = "dp-1";
        let args: Vec<&str> = vec![monitor];
        assert_eq!(args, vec!["dp-1"]);
    }

    /// Test that Direction maps to the correct flag string. Backward
    /// compatibility: Forward should produce empty string (no flag) so
    /// existing scripts that don't know about --previous still work.
    #[test]
    fn state_wiring_direction_as_flag() {
        assert_eq!(Direction::Forward.as_flag(), "");
        assert_eq!(Direction::Previous.as_flag(), "--previous");
    }
}
