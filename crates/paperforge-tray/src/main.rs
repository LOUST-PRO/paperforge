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
// - "Rotate all now"  → `paperforge-rotate.sh` (advance all monitors + spawn)
// - "Open TUI"        → `paperforge-tui` (the read-only debugger)
// - Per-monitor submenus (one per playlist in `$PLAYLIST_DIR`):
//   - "Next wallpaper" → `paperforge-rotate.sh <monitor>` (advance + apply)
// - "Quit"            → `std::process::exit(0)`
//
// Future Fase-2 actions (renew playlist, random from current, etc.) will
// hang off the per-monitor submenu and require extending
// `paperforge-rotate.sh` with new flags.

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
        let next_idx = (idx + 1) % n;
        let current = basename(&pl.wallpapers[idx]);
        let next = basename(&pl.wallpapers[next_idx]);
        out.push(MonitorState {
            name: pl.name,
            output: pl.outputs[0].clone(),
            count: n,
            current,
            next,
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
    // Fall back to $TMPDIR/paperforge-<uid>-runtime if even that is missing.
    if let Some(d) = dirs::runtime_dir() {
        d.join("paperforge-rotate-state.json")
    } else {
        let uid = nix::unistd::getuid().as_raw();
        let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(tmp).join(format!("paperforge-{uid}-runtime"))
    }
}

// ─── Action handlers (async, fire-and-forget) ───────────────────────────────

fn spawn_rotate(rotate_bin: PathBuf, monitor: Option<String>) {
    let mut cmd = tokio::process::Command::new(&rotate_bin);
    if let Some(m) = &monitor {
        cmd.arg(m);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    tokio::spawn(async move {
        match cmd.output().await {
            Ok(out) if out.status.success() => {
                info!(target: "paperforge-tray", monitor = ?monitor, "rotate OK");
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                error!(target: "paperforge-tray", monitor = ?monitor,
                    "rotate failed: exit={} stderr={stderr}", out.status);
            }
            Err(e) => {
                error!(target: "paperforge-tray", monitor = ?monitor,
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
                        label: format!("{}: {} → {}", mon.output, mon.current, mon.next),
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
                label: "Rotate all now".into(),
                activate: Box::new(move |_tray: &mut Self| {
                    // Clone inside the Fn closure — the captured rotate_bin
                    // must remain valid across multiple invocations.
                    let bin = rotate_bin.clone();
                    spawn_rotate(bin, None);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Open TUI".into(),
                activate: Box::new(move |_tray: &mut Self| {
                    spawn_tui();
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

/// Build the per-monitor submenu (currently just "Next wallpaper").
/// Returns `Vec<MenuItem<PaperforgeTray>>` so the closures stay
/// `Send + 'static` (rotate_bin is cloned into each closure).
fn build_monitor_submenu(
    mon: &MonitorState,
    rotate_bin: &Path,
) -> Vec<ksni::MenuItem<PaperforgeTray>> {
    use ksni::menu::StandardItem;

    let label_next = format!("Next wallpaper  (→ {})", mon.next);
    let monitor_name = mon.name.clone();
    let rotate_bin = rotate_bin.to_path_buf();

    vec![
        StandardItem {
            label: label_next,
            activate: Box::new(move |_tray: &mut PaperforgeTray| {
                // Clone inside the Fn closure so it can be called multiple times.
                let bin = rotate_bin.clone();
                spawn_rotate(bin, Some(monitor_name.clone()));
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
