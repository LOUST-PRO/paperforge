//! `paperforge-tui` — read-only live debugger over `paperforge-core`.
//!
//! Four panels refresh on independent timers: outputs (2s), running
//! PIDs (5s), playlists (10s), inventory (30s). Keyboard: `1`-`4`
//! jumps panel, `Tab`/`Backtab` cycles, `↑↓`/`jk` navigates rows,
//! `r` forces refresh of the focused panel, `q` quits.
//!
//! Public surface:
//! - [`App`] — the application state.
//! - [`run`] — entry point that owns the terminal lifecycle.
//! - [`data`] — async fetchers for each panel.
//! - [`ui`] — ratatui rendering.
//! - [`event`] — keyboard input translation.
//!
//! All fetchers run inside `tokio::task::spawn_blocking` so the
//! render loop never stalls on slow filesystems.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod app;
pub mod data;
pub mod event;
pub mod ui;

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use paperforge_core::{
    backend::LweBackend, hotplug::CompositorHotplugSource, paths::default_paths,
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;

pub use app::{App, Focus};

/// Panel refresh intervals.
const OUTPUTS_TICK: Duration = Duration::from_secs(2);
const RUNNING_TICK: Duration = Duration::from_secs(5);
const PLAYLISTS_TICK: Duration = Duration::from_secs(10);
const INVENTORY_TICK: Duration = Duration::from_secs(30);

/// Render-loop tick (how often we redraw + drain input).
const LOOP_TICK: Duration = Duration::from_millis(250);

/// Run the TUI. Blocks until the user quits (`q` / `Esc` /
/// `Ctrl-C`). Returns `Ok(())` on clean exit, `Err(_)` on terminal
/// setup or restore failures.
///
/// `extra_roots` are additional source paths to scan for wallpapers
/// on top of [`paperforge_core::paths::default_paths`]. They are
/// typically passed via CLI flags like `--source /path/to/wallpapers`.
pub async fn run(extra_roots: Vec<PathBuf>) -> Result<()> {
    // Set up terminal (raw mode + alternate screen).
    let mut terminal = setup_terminal()?;

    // Backend + sources + store.
    let backend = Arc::new(LweBackend::new());
    let outputs_src = Arc::new(CompositorHotplugSource::detect());
    let playlists_root = dirs::config_dir()
        .map(|d| d.join("paperforge").join("playlists"))
        .unwrap_or_else(|| PathBuf::from(".paperforge/playlists"));

    let mut roots: Vec<PathBuf> = default_paths().all().cloned().collect();
    roots.extend(extra_roots);

    // App state.
    let mut app = App::new();

    // Spawn fetch tasks. Each one pushes a (panel_name, snapshot) update
    // through the same channel; the main loop receives and applies them.
    let (tx, mut rx) = mpsc::unbounded_channel::<FetchUpdate>();

    spawn_output_loop(outputs_src.clone(), tx.clone());
    spawn_running_loop(backend.clone(), tx.clone());
    spawn_playlist_loop(playlists_root.clone(), tx.clone());
    spawn_inventory_loop(roots, tx.clone());

    // Main loop: poll input + apply fetched updates.
    let result = run_loop(&mut terminal, &mut app, &mut rx).await;

    // Always restore terminal — even on error — so the user's shell
    // is not left in raw mode.
    restore_terminal(&mut terminal)?;
    result
}

/// Update pushed by the spawn_*_loop tasks. The main loop merges
/// these into the [`Snapshot`](crate::data::Snapshot).
#[derive(Debug)]
enum FetchUpdate {
    Outputs(Result<Vec<paperforge_core::hotplug::Output>>),
    Running(Result<Vec<crate::data::RunningInstance>>),
    Playlists(Result<Vec<crate::data::PlaylistSummary>>),
    Inventory(Result<Vec<paperforge_core::inventory::WallpaperEntry>>),
}

fn spawn_output_loop(src: Arc<CompositorHotplugSource>, tx: mpsc::UnboundedSender<FetchUpdate>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(OUTPUTS_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // first tick fires immediately
        loop {
            ticker.tick().await;
            let r = data::refresh_outputs(src.clone()).await;
            // refresh_outputs returns a tuple; we want to merge into a Result
            let v = match &r.1 {
                None => Ok(r.0),
                Some(e) => Err(anyhow::anyhow!("[outputs] {}", e.message)),
            };
            if tx.send(FetchUpdate::Outputs(v)).is_err() {
                break;
            }
        }
    });
}

fn spawn_running_loop(backend: Arc<LweBackend>, tx: mpsc::UnboundedSender<FetchUpdate>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RUNNING_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let r = data::refresh_running(backend.clone()).await;
            let v = match &r.1 {
                None => Ok(r.0),
                Some(e) => Err(anyhow::anyhow!("[running] {}", e.message)),
            };
            if tx.send(FetchUpdate::Running(v)).is_err() {
                break;
            }
        }
    });
}

fn spawn_playlist_loop(root: PathBuf, tx: mpsc::UnboundedSender<FetchUpdate>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PLAYLISTS_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let r = data::refresh_playlists(root.clone()).await;
            let v = match &r.1 {
                None => Ok(r.0),
                Some(e) => Err(anyhow::anyhow!("[playlists] {}", e.message)),
            };
            if tx.send(FetchUpdate::Playlists(v)).is_err() {
                break;
            }
        }
    });
}

fn spawn_inventory_loop(roots: Vec<PathBuf>, tx: mpsc::UnboundedSender<FetchUpdate>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(INVENTORY_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let r = data::refresh_inventory(roots.clone()).await;
            let v = match &r.1 {
                None => Ok(r.0),
                Some(e) => Err(anyhow::anyhow!("[inventory] {}", e.message)),
            };
            if tx.send(FetchUpdate::Inventory(v)).is_err() {
                break;
            }
        }
    });
}

async fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<FetchUpdate>,
) -> Result<()> {
    loop {
        // Drain fetched updates without blocking.
        loop {
            match rx.try_recv() {
                Ok(update) => apply_update(app, update),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    // All spawners gone — shouldn't happen in normal operation.
                    // Continue anyway; new data may come from `r` key.
                }
            }
        }
        // Clamp selection in case data shrank.
        app.clamp_selection();

        // Poll input with a short timeout so we redraw even if no
        // input arrives (e.g. when a fetch update changed state).
        if let Ok(Some(k)) = event::latest_key() {
            let action = event::translate(k);
            event::apply(app, action);
        }

        // Render.
        terminal.draw(|f| ui::render_top_half_with_inventory(f, f.area(), app))?;

        if app.should_quit {
            return Ok(());
        }

        // Sleep LOOP_TICK so we don't busy-loop.
        tokio::time::sleep(LOOP_TICK).await;
    }
}

fn apply_update(app: &mut App, update: FetchUpdate) {
    match update {
        FetchUpdate::Outputs(Ok(v)) => {
            app.snapshot.outputs = v;
            app.snapshot.errors.retain(|e| e.source != "outputs");
        }
        FetchUpdate::Outputs(Err(e)) => {
            record_error(app, "outputs", e);
        }
        FetchUpdate::Running(Ok(v)) => {
            app.snapshot.running = v;
            app.snapshot.errors.retain(|e| e.source != "running");
        }
        FetchUpdate::Running(Err(e)) => {
            record_error(app, "running", e);
        }
        FetchUpdate::Playlists(Ok(v)) => {
            app.snapshot.playlists = v;
            app.snapshot.errors.retain(|e| e.source != "playlists");
        }
        FetchUpdate::Playlists(Err(e)) => {
            record_error(app, "playlists", e);
        }
        FetchUpdate::Inventory(Ok(v)) => {
            app.snapshot.inventory = v;
            app.snapshot.errors.retain(|e| e.source != "inventory");
        }
        FetchUpdate::Inventory(Err(e)) => {
            record_error(app, "inventory", e);
        }
    }
}

fn record_error(app: &mut App, source: &str, msg: anyhow::Error) {
    // Replace any existing error from the same source.
    app.snapshot.errors.retain(|e| e.source != source);
    app.snapshot.errors.push(app::DataError {
        source: source.to_string(),
        message: format!("{msg}"),
    });
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    use crossterm::{
        execute,
        terminal::{enable_raw_mode, EnterAlternateScreen},
    };
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    use crossterm::{
        execute,
        terminal::{disable_raw_mode, LeaveAlternateScreen},
    };
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_intervals_are_ordered_increasingly() {
        // Ensures faster-refreshing panels update before slower ones
        // when we add jitter — also a smoke that we didn't flip a
        // constant by accident.
        assert!(OUTPUTS_TICK < RUNNING_TICK);
        assert!(RUNNING_TICK < PLAYLISTS_TICK);
        assert!(PLAYLISTS_TICK < INVENTORY_TICK);
    }
}
