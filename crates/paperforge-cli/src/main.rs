//! paperforge CLI entry point.
//!
//! Subcommands (clap derive):
//! - `set <PATH> [--output <NAME>]` — launch LWE with a wallpaper
//! - `pause` — SIGSTOP all LWE instances
//! - `resume` — SIGCONT all LWE instances
//! - `list` — list currently-running LWE PIDs + state
//! - `scan` — scan default paths, print discovered entries
//! - `audio <toggle|mute|unmute>` — audio control via SIGUSR
//! - `playlist <list|show|save|apply|delete>` — playlist management
//! - `pool <status>` — show v0.2 single-process pool state
//! - `paths` — print detected source paths
//! - `daemon` — boot the LWE pool architecture (D-Bus + hotplug + LWE pool)

use anyhow::Context;
use clap::{Parser, Subcommand};
use std::sync::Arc;
use std::time::Duration;

use paperforge_core::{
    audio::AudioCommand,
    backend::{BackendState, WallpaperBackend},
    config::{Config, ConfigPaths},
    daemon::{BackendOps, PaperforgeDaemon},
    dbus::{serve_dbus, PaperforgeControl},
    hotplug::{CompositorHotplugSource, HotplugWatcher},
    inventory::Inventory,
    paths::default_paths,
    playlist::PlaylistStore,
};

/// `paperforge` — Wallpaper Engine Workshop manager for Linux.
#[derive(Debug, Parser)]
#[command(name = "paperforge", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Launch LWE with a wallpaper path (directory or loose file).
    Set {
        /// Path to a Wallpaper Engine scene directory or loose media.
        path: String,
        /// Wayland output name (DP-1, HDMI-A-1, eDP-1, ...).
        #[arg(long)]
        output: Option<String>,
    },
    /// SIGSTOP all LWE instances.
    Pause,
    /// SIGCONT all LWE instances.
    Resume,
    /// List running LWE PIDs with their state.
    List,
    /// Scan default paths and print discovered wallpapers.
    Scan {
        /// Maximum scan depth (default 8).
        #[arg(long, default_value_t = 8)]
        max_depth: usize,
    },
    /// Audio control via POSIX signals to LWE.
    Audio {
        #[command(subcommand)]
        action: AudioCmd,
    },
    /// Playlist management.
    Playlist {
        #[command(subcommand)]
        action: PlaylistCmd,
    },
    /// Print auto-detected source paths.
    Paths,
    /// Inspect the v0.2 single-process LWE pool (only meaningful when
    /// `pool_enabled = true` in config).
    Pool {
        #[command(subcommand)]
        action: PoolCmd,
    },
    /// Start the LWE pool daemon (D-Bus service + hotplug watcher).
    /// Blocks until SIGINT/SIGTERM, then performs graceful shutdown.
    Daemon,
}

#[derive(Debug, Subcommand)]
enum PoolCmd {
    /// Show current pool pid, bindings, and argv.
    Status,
}

#[derive(Debug, Subcommand)]
enum AudioCmd {
    /// Toggle mute (SIGUSR1).
    Toggle,
    /// Force mute (SIGUSR2).
    Mute,
    /// Force unmute (SIGCONT).
    Unmute,
}

impl From<AudioCmd> for AudioCommand {
    fn from(c: AudioCmd) -> Self {
        match c {
            AudioCmd::Toggle => AudioCommand::Toggle,
            AudioCmd::Mute => AudioCommand::Mute,
            AudioCmd::Unmute => AudioCommand::Unmute,
        }
    }
}

#[derive(Debug, Subcommand)]
enum PlaylistCmd {
    /// List all saved playlists.
    List,
    /// Print a playlist's contents.
    Show { name: String },
    /// Apply a playlist to its configured outputs.
    Apply { name: String },
    /// Delete a playlist by name.
    Delete { name: String },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();
    let paths = ConfigPaths::defaults().context("resolving config paths")?;
    let cfg = Config::load(&paths).context("loading config")?;

    let backend = cfg.backend();
    let store = PlaylistStore::default_location().context("opening playlist store")?;

    match cli.cmd {
        Cmd::Set { path, output } => {
            let p = std::path::PathBuf::from(&path);
            backend.set(&p, output.as_deref()).await?;
            println!("set {} on output {:?}", p.display(), output);
        }
        Cmd::Pause => {
            let n = backend.pause().await?;
            println!("paused {n} LWE instance(s)");
        }
        Cmd::Resume => {
            let n = backend.resume().await?;
            println!("resumed {n} LWE instance(s)");
        }
        Cmd::List => {
            // Prefer the daemon's view of LWE PIDs (via D-Bus) over
            // the local `backend.list_pids()` — when the daemon owns
            // the per-output children, only the daemon knows their
            // pids. Fall back to the local backend if no daemon is
            // reachable. We shell out to `gdbus` rather than linking
            // zbus into the CLI (keeps the CLI hermetic — only the
            // daemon process links the session bus).
            let pid_state_pairs: Vec<(i32, BackendState)> = match list_via_dbus().await {
                Some(v) => v,
                None => {
                    let map = PlaylistStore::lwe_status(&backend).await?;
                    map.into_iter().collect()
                }
            };
            if pid_state_pairs.is_empty() {
                println!("(no LWE instances running)");
            } else {
                for (pid, state) in pid_state_pairs {
                    let s = match state {
                        BackendState::Running => "running",
                        BackendState::Paused => "paused",
                        BackendState::NotRunning => "not-running",
                    };
                    println!("{pid}\t{s}");
                }
            }
        }
        Cmd::Scan { max_depth } => {
            let mut inv = Inventory::new();
            let mut total = 0;
            for root in cfg.source_roots() {
                let n = inv.scan(root, max_depth)?;
                total += n;
            }
            println!("scanned {} entries:", total);
            for entry in inv.entries() {
                println!(
                    "  {}  {:?}",
                    entry.kind.lwe_compatible(),
                    entry.path.display()
                );
            }
        }
        Cmd::Audio { action } => {
            let audio = backend.audio();
            let cmd: AudioCommand = action.into();
            let n = audio.send(cmd).await?;
            println!("sent {cmd:?} to {n} LWE instance(s)");
        }
        Cmd::Playlist { action } => match action {
            PlaylistCmd::List => {
                let names = store.list()?;
                if names.is_empty() {
                    println!("(no playlists saved)");
                } else {
                    for n in names {
                        println!("{n}");
                    }
                }
            }
            PlaylistCmd::Show { name } => {
                let pl = store.load(&name)?;
                let json = serde_json::to_string_pretty(&pl)?;
                println!("{json}");
            }
            PlaylistCmd::Apply { name } => {
                let pl = store.load(&name)?;
                let applied = store.apply(&pl, &backend).await?;
                println!("applied playlist '{}':", pl.name);
                for (output, path) in applied {
                    println!("  {output}\t{}", path.display());
                }
            }
            PlaylistCmd::Delete { name } => {
                let removed = store.delete(&name)?;
                if removed {
                    println!("deleted playlist '{name}'");
                } else {
                    println!("no playlist named '{name}'");
                }
            }
        },
        Cmd::Paths => {
            let p = default_paths();
            println!("workshop roots:");
            for r in &p.workshop_roots {
                println!("  {}", r.display());
            }
            println!("local roots:");
            for r in &p.local_roots {
                println!("  {}", r.display());
            }
        }
        Cmd::Pool { action } => match action {
            PoolCmd::Status => {
                if !cfg.pool_enabled {
                    println!(
                        "pool_enabled=false in config; pool is disabled (v0.1 per-output path)"
                    );
                    return Ok(());
                }
                let pool = backend.pool();
                let pid = pool.current_pid().await;
                let bindings = pool.bindings().await;
                let argv = pool.current_argv().await;
                println!("pool_enabled: true");
                match pid {
                    Some(p) => println!("current pid: {p}"),
                    None => println!("current pid: (none — pool not running)"),
                }
                if bindings.is_empty() {
                    println!("bindings: (none)");
                } else {
                    println!("bindings ({}):", bindings.len());
                    for (out, content_id) in &bindings {
                        println!("  {out}\t{content_id}");
                    }
                }
                match argv {
                    Some(args) => {
                        println!("argv ({} tokens):", args.len());
                        for (i, tok) in args.iter().enumerate() {
                            println!("  [{i}] {tok}");
                        }
                    }
                    None => println!("argv: (none — pool not running)"),
                }
            }
        },
        Cmd::Daemon => {
            run_daemon(cfg).await?;
        }
    }
    Ok(())
}

async fn run_daemon(cfg: Config) -> anyhow::Result<()> {
    tracing::info!(
        "paperforge daemon starting (pool_enabled={})",
        cfg.pool_enabled
    );

    // 1-3. Build LWE ops + daemon.
    let lwe_ops = cfg.build_backend_ops();
    let backend_dyn: Arc<dyn BackendOps> = lwe_ops.clone();
    let (daemon, event_rx) = PaperforgeDaemon::with_lwe_backend_ops(backend_dyn, lwe_ops.clone())
        .context("constructing PaperforgeDaemon")?;

    // 4. D-Bus layer (consumes event_rx).
    let ctrl: Arc<dyn PaperforgeControl> = daemon.clone();
    let dbus_handle = tokio::spawn(serve_dbus(ctrl, event_rx));

    // 5. Hotplug dispatcher (every 2s, matches the default in the
    //    hotplug module's doc comment).
    let hotplug_handle = tokio::spawn(hotplug_dispatcher(daemon.clone(), Duration::from_secs(2)));

    // 6. Wait for SIGINT/SIGTERM.
    let sig = wait_for_shutdown_signal().await?;
    tracing::info!("received {sig}; shutting down");

    // 7. Graceful shutdown.
    dbus_handle.abort();
    hotplug_handle.abort();
    let _ = dbus_handle.await;
    let _ = hotplug_handle.await;

    if let Err(e) = lwe_ops.backend().pool().shutdown().await {
        tracing::warn!("pool shutdown: {e}");
    }

    // Drop the daemon last so any in-flight D-Bus methods see
    // shutdown before the interface is gone.
    drop(daemon);
    tracing::info!("paperforge daemon exited cleanly");
    Ok(())
}

/// Forward `HotplugEvent`s from a `HotplugWatcher<CompositorHotplugSource>`
/// to `daemon.handle_hotplug()`. Exits cleanly when the watcher
/// channel closes (never, today — the loop is infinite).
async fn hotplug_dispatcher(daemon: Arc<PaperforgeDaemon>, poll_interval: Duration) {
    let source = Arc::new(CompositorHotplugSource::detect());
    let mut watcher = HotplugWatcher::spawn(source, poll_interval);
    while let Some(ev) = watcher.next().await {
        let unbound = daemon.handle_hotplug(ev).await;
        if !unbound.is_empty() {
            let _ = daemon.emit(paperforge_core::DaemonEvent::MonitorChanged {
                outputs: daemon.known_outputs().await,
                at: chrono::Utc::now(),
            });
        }
    }
    tracing::debug!(target: "paperforge", "hotplug dispatcher exiting");
}

/// Block SIGINT/SIGTERM/SIGHUP in a dedicated thread, then `sigwait`.
/// Bridge to the async runtime via `oneshot`. Returns the signal name
/// for logging. Uses `nix` (already a dep) — we cannot use
/// `tokio::signal::ctrl_c()` because the `signal` feature is not
/// enabled on the workspace tokio crate.
async fn wait_for_shutdown_signal() -> anyhow::Result<&'static str> {
    let (tx, rx) = tokio::sync::oneshot::channel::<&'static str>();
    std::thread::spawn(move || {
        use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow, Signal};
        let mut mask = SigSet::empty();
        mask.add(Signal::SIGINT);
        mask.add(Signal::SIGTERM);
        mask.add(Signal::SIGHUP);
        let _ = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&mask), None);
        let sig = mask.wait().unwrap_or(Signal::SIGTERM);
        let name = match sig {
            Signal::SIGINT => "SIGINT",
            Signal::SIGTERM => "SIGTERM",
            Signal::SIGHUP => "SIGHUP",
            _ => "unknown",
        };
        let _ = tx.send(name);
    });
    rx.await
        .map_err(|e| anyhow::anyhow!("shutdown channel: {e}"))
}

/// Query the daemon's `ListRunning` D-Bus method via `gdbus`.
///
/// Returns `None` if the daemon is not running on the session bus
/// (so the caller should fall back to the local backend).
async fn list_via_dbus() -> Option<Vec<(i32, paperforge_core::backend::BackendState)>> {
    use paperforge_core::backend::BackendState;
    let out = tokio::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.louzt.Paperforge",
            "--object-path",
            "/org/louzt/Paperforge",
            "--method",
            "org.louzt.Paperforge1.ListRunning",
        ])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Expected shape: `([(123, 'running'), (456, 'paused')],)`.
    // Slice out the `[...]` array, drop the outer parens.
    let open = stdout.find('[')?;
    let close = stdout.rfind(']')? + 1;
    let body = stdout
        .get(open..close)?
        .trim_matches(|c: char| c == '[' || c == ']')
        .trim();
    if body.is_empty() {
        return Some(Vec::new());
    }
    let mut out_vec = Vec::new();
    // Split by `), (` to get each (pid, 'state') tuple.
    for tuple in body.split("), (") {
        let cleaned = tuple.trim_start_matches('(').trim_end_matches(')');
        let mut parts = cleaned.splitn(2, ',');
        let pid_str = parts.next()?.trim();
        let state_str = parts.next()?.trim().trim_matches('\'');
        let pid: i32 = pid_str.parse().ok()?;
        let state = match state_str {
            "running" => BackendState::Running,
            "paused" => BackendState::Paused,
            _ => BackendState::NotRunning,
        };
        out_vec.push((pid, state));
    }
    Some(out_vec)
}
