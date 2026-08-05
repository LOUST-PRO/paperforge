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
use tokio::sync::RwLock;

use std::collections::BTreeMap;

use paperforge_core::{
    audio::AudioCommand,
    backend::BackendState,
    config::{Config, ConfigPaths},
    daemon::{BackendOps, PaperforgeDaemon},
    dbus::{serve_dbus, PaperforgeControl},
    hotplug::{CompositorHotplugSource, HotplugWatcher},
    inventory::Inventory,
    paths::default_paths,
    playlist::PlaylistStore,
    updater::{Channel, UpdateInfo, Updater, UpdaterConfig},
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
    /// Pause wallpapers. Default mode is `frame` (SIGSTOP/SIGCONT
    /// clock so the layer-shell surface stays alive — no grey).
    /// Override per-call with `--mode`.
    Pause {
        /// `hard` (pure SIGSTOP, grey surface), `frame` (default,
        /// SIGSTOP/SIGCONT clock), or `throttle` (respawn with
        /// `--fps 1`).
        #[arg(long)]
        mode: Option<String>,
    },
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
    /// Manually trigger a self-heal pass: re-bind any output whose
    /// LWE process has died. Useful when the background reconciler
    /// in the daemon hasn't run yet (e.g. right after the daemon
    /// starts, or after a manual kill of a single LWE process for
    /// debugging).
    ///
    /// This talks to the running daemon over D-Bus — it does NOT
    /// spawn its own LWE instances. If no daemon is running, the
    /// command exits with an error.
    Reconcile,
    /// Self-update: query, apply, or roll back a paperforge upgrade.
    /// Off-by-default; requires `enabled = true` in
    /// `~/.config/paperforge/updater.toml`.
    SelfUpdate {
        /// Just query the GitHub release feed; print result, exit.
        #[arg(long)]
        check: bool,
        /// Download, verify (SHA-256), and atomically swap the binary.
        #[arg(long)]
        apply: bool,
        /// Track pre-releases instead of stable (with --check/--apply).
        #[arg(long)]
        pre: bool,
        /// Restore the most recent backup (undoes the last --apply).
        #[arg(long)]
        rollback: bool,
        /// Skip the confirmation prompt before --apply / --rollback.
        #[arg(long)]
        yes: bool,
        /// List retained backups (newest first).
        #[arg(long)]
        list_backups: bool,
        /// Print the loaded updater config and exit.
        #[arg(long)]
        config: bool,
    },
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

    // Use `build_backend_ops()` (which honours `pool_enabled`) for
    // destructive commands (`set`/`pause`/`resume`). Use the raw
    // `backend()` (always `use_pool = true`) for read-only introspection
    // where `pool_enabled` doesn't change semantics.
    let backend_ops = cfg.build_backend_ops();
    let backend = cfg.backend();
    let store = PlaylistStore::default_location().context("opening playlist store")?;

    match cli.cmd {
        Cmd::Set { path, output } => {
            let p = std::path::PathBuf::from(&path);
            // Try to route through the running daemon first — when
            // available, the daemon's LweBackendOps owns a single
            // per_output_pids / per_output_scenes map shared with the
            // fullscreen dispatcher + SIGCHLD reaper. Falling back to
            // a local spawn only when no daemon is reachable ensures
            // CLI mutations stay consistent with daemon-side state.
            let routed = match output.as_deref() {
                Some(o) => daemon_set_wallpaper(o, &path).await,
                None => daemon_set_default_wallpaper(&path).await,
            };
            match routed {
                Ok(true) => {
                    println!("set {} on output {:?}", p.display(), output);
                    return Ok(());
                }
                Ok(false) => {} // daemon unreachable, fall through to local
                Err(e) => {
                    tracing::warn!("daemon SetWallpaper returned error: {e}; falling back to local spawn");
                }
            }
            // Local fallback: route through BackendOps so `pool_enabled=false`
            // actually triggers per-output spawn. Direct calls to
            // `backend.set()` (WallpaperBackend trait) ignore
            // `pool_enabled` and always go through the pool.
            backend_ops
                .set(output.as_deref().unwrap_or(""), &path)
                .await?;
            println!("set {} on output {:?}", p.display(), output);
        }
        Cmd::Pause { mode } => {
            // v0.3 supports three modes; `--mode` overrides the
            // `[pause].mode` from config.toml.
            let cfg = paperforge_core::config::Config::load(
                &paperforge_core::config::ConfigPaths::defaults()
                    .expect("config paths resolvable in CLI context"),
            )
            .unwrap_or_default();
            let mode = match mode {
                Some(s) => s
                    .parse::<paperforge_core::config::PauseMode>()
                    .map_err(|e| anyhow::anyhow!("invalid --mode value: {e}"))?,
                None => cfg.pause.mode,
            };
            let n = backend_ops
                .pause_with_mode(
                    mode,
                    cfg.pause.paused_fps,
                    cfg.pause.clock_awake_ms,
                    cfg.pause.clock_asleep_ms,
                )
                .await?;
            println!("paused {n} LWE instance(s) (mode={mode})");
        }
        Cmd::Resume => {
            let n = backend_ops.resume().await?;
            println!("resumed {n} LWE instance(s)");
        }
        Cmd::List => {
            // Prefer the daemon's view of LWE PIDs (via D-Bus) over
            // the local backend — when the daemon owns the per-output
            // children, only the daemon knows their pids. Fall back
            // to the local backend if no daemon is reachable. We shell
            // out to `gdbus` rather than linking zbus into the CLI
            // (keeps the CLI hermetic — only the daemon process
            // links the session bus). For the local fallback we use
            // `backend_ops.list()` so per-output children spawned by
            // earlier CLI invocations are visible via /proc.
            let dbus_result = list_via_dbus().await;
            tracing::debug!("list_via_dbus returned: {dbus_result:?}");
            let pid_state_pairs: Vec<(i32, BackendState)> = match dbus_result {
                Some(v) => v,
                None => backend_ops.list().await?,
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
            let audio = backend_ops.audio();
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
                // Route through the running daemon first so all output
                // binds land in the daemon's LweBackendOps state
                // (the source of truth shared with fullscreen
                // detection + SIGCHLD reaper). Fall back to local
                // spawn only if no daemon is on the session bus.
                match daemon_apply_playlist(&name).await {
                    Ok(true) => {
                        println!("applied playlist '{}' (via daemon):", pl.name);
                        for (i, output) in pl.outputs.iter().enumerate() {
                            let scene = &pl.wallpapers[i % pl.wallpapers.len()];
                            println!("  {output}\t{}", scene.display());
                        }
                        return Ok(());
                    }
                    Ok(false) => {} // daemon unreachable, fall through
                    Err(e) => {
                        tracing::warn!(
                            "daemon ApplyPlaylist returned error: {e}; falling back to local spawn"
                        );
                    }
                }
                if pl.wallpapers.is_empty() {
                    anyhow::bail!("playlist '{}' has no wallpapers", pl.name);
                }
                if pl.outputs.is_empty() {
                    anyhow::bail!(
                        "playlist '{}' has empty outputs — provide explicit outputs",
                        pl.name
                    );
                }
                // Local fallback: route through `backend_ops` (which
                // honours `pool_enabled`) instead of `store.apply(...
                // &backend)`. `LweBackend::set` always uses the pool
                // regardless of `pool_enabled`, so passing the raw
                // backend there would bypass the operator's intent
                // and re-introduce the upstream-LWE 2+-bindings crash.
                let mut applied = BTreeMap::new();
                for (i, output) in pl.outputs.iter().enumerate() {
                    let scene = &pl.wallpapers[i % pl.wallpapers.len()];
                    let scene_str = scene.to_string_lossy().to_string();
                    backend_ops.set(output, &scene_str).await?;
                    applied.insert(output.clone(), scene.clone());
                }
                println!("applied playlist '{}':", pl.name);
                for (output, path) in &applied {
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
        Cmd::Reconcile => {
            // Talk to the running daemon over D-Bus to trigger an
            // immediate self-heal pass. If no daemon is up, the
            // client surfaces a connection error instead of
            // silently doing nothing.
            let client = paperforge_core::dbus::PaperforgeClient::connect()
                .await
                .context("connecting to paperforge D-Bus interface (is `paperforge daemon` running?)")?;
            match client.reconcile().await {
                Ok(pairs) => {
                    if pairs.is_empty() {
                        println!("reconcile: nothing to re-bind (all LWE PIDs alive)");
                    } else {
                        for (output, pid) in &pairs {
                            println!("reconcile: re-bound {output} pid={pid}");
                        }
                        println!("reconcile: {} output(s) re-bound", pairs.len());
                    }
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("reconcile failed: {e}"));
                }
            }
        }
        Cmd::SelfUpdate {
            check,
            apply,
            pre,
            rollback,
            yes,
            list_backups,
            config: show_config,
        } => {
            let opts = SelfUpdateOpts {
                check,
                apply,
                pre,
                rollback,
                yes,
                list_backups,
                config: show_config,
            };
            run_self_update(&paths, opts).await?;
        }
    }
    Ok(())
}

async fn run_self_update(paths: &ConfigPaths, opts: SelfUpdateOpts) -> anyhow::Result<()> {
    let updater_cfg_path = paths.config_dir.join("updater.toml");
    let mut updater_cfg = UpdaterConfig::load_or_default(&updater_cfg_path)?;

    if opts.pre {
        updater_cfg.channel = Channel::Pre;
    }

    if opts.config {
        println!(
            "config file: {}\n{}",
            updater_cfg_path.display(),
            toml::to_string_pretty(&updater_cfg)?
        );
        return Ok(());
    }

    if opts.list_backups {
        let updater = Updater::new(updater_cfg)?;
        let backups = updater.list_backups()?;
        if backups.is_empty() {
            println!("(no backups retained)");
        } else {
            for b in backups {
                println!(
                    "{}\t{}\t{}",
                    b.version,
                    b.created_at.to_rfc3339(),
                    b.path.display()
                );
            }
        }
        return Ok(());
    }

    if opts.rollback {
        let updater = Updater::new(updater_cfg)?;
        if !opts.yes {
            let backups = updater.list_backups()?;
            let newest = backups
                .first()
                .ok_or_else(|| anyhow::anyhow!("no backups to roll back to"))?;
            eprintln!(
                "About to roll back to version '{}' from {}",
                newest.version,
                newest.path.display()
            );
            eprintln!(
                "This will replace the running binary at {}.",
                updater.binary_path().display()
            );
            eprintln!("Re-run with --yes to skip this prompt.");
            return Ok(());
        }
        let restored = updater.rollback().await?;
        println!(
            "rolled back to {} (backup: {})",
            restored.version,
            restored.path.display()
        );
        return Ok(());
    }

    // check / apply path
    let updater = Updater::new(updater_cfg)?;
    let info = updater.check().await?;
    print_check_summary(&info);

    if !info.update_available {
        return Ok(());
    }
    if opts.check || !opts.apply {
        // Default when no flag is given: behave like --check.
        eprintln!("run with --apply to install this update (--yes to skip prompt).");
        return Ok(());
    }

    if !opts.yes {
        eprintln!(
            "About to apply update {} -> {} (asset {}).",
            info.current_version, info.latest_version, info.asset_name,
        );
        eprintln!(
            "This will replace the binary at {} and create a backup.",
            updater.binary_path().display()
        );
        eprintln!("Re-run with --yes to skip this prompt.");
        return Ok(());
    }

    // We need a fresh info with the SHA-256 populated. The check
    // path leaves it blank (deferred fetch). Re-fetch it here.
    let backup = updater.apply(&info).await?;
    println!(
        "updated {} -> {} (backup at {}).",
        info.current_version,
        info.latest_version,
        backup.path.display()
    );
    Ok(())
}

/// Aggregated options for `self-update`, parsed out of the clap
/// subcommand fields so the worker function does not trip
/// `clippy::too_many_arguments`.
#[derive(Debug)]
struct SelfUpdateOpts {
    check: bool,
    apply: bool,
    pre: bool,
    rollback: bool,
    yes: bool,
    list_backups: bool,
    config: bool,
}

fn print_check_summary(info: &UpdateInfo) {
    println!("current: {}", info.current_version);
    println!("latest:  {} ({})", info.latest_version, info.release_tag);
    println!("asset:   {}", info.asset_name);
    if let Some(sz) = info.size_bytes {
        println!("size:    {} bytes", sz);
    }
    if info.is_prerelease {
        println!("(this release is a pre-release)");
    }
    if info.update_available {
        println!("status:  update available");
    } else {
        println!("status:  up to date");
    }
}

async fn run_daemon(cfg: Config) -> anyhow::Result<()> {
    tracing::info!(
        "paperforge daemon starting (pool_enabled={})",
        cfg.pool_enabled
    );

    // 1-3. Build LWE ops + daemon.
    let lwe_ops = cfg.build_backend_ops();
    let backend_dyn: Arc<dyn BackendOps> = lwe_ops.clone();
    // Load the configured pause mode so the daemon's D-Bus
    // pause/resume honour `[pause]` from config.toml instead of
    // always issuing plain SIGSTOP (which drops the surface to
    // grey). Test contexts use the default mode if `cfg.pause`
    // is absent.
    let pause_cfg = paperforge_core::config::Config::load(
        &paperforge_core::config::ConfigPaths::defaults()
            .expect("config paths resolvable in CLI context"),
    )
    .map(|c| c.pause)
    .unwrap_or_default();
    let (daemon, event_rx) = PaperforgeDaemon::with_lwe_backend_ops_store_and_pause(
        backend_dyn,
        lwe_ops.clone(),
        Arc::new(RwLock::new(PlaylistStore::default_location()?)),
        Arc::new(RwLock::new(pause_cfg)),
    );

    // 4. D-Bus layer (consumes event_rx).
    let ctrl: Arc<dyn PaperforgeControl> = daemon.clone();
    let dbus_handle = tokio::spawn(serve_dbus(ctrl, event_rx));

    // 5. Hotplug dispatcher (every 2s, matches the default in the
    //    hotplug module's doc comment).
    let hotplug_handle = tokio::spawn(hotplug_dispatcher(daemon.clone(), Duration::from_secs(2)));

    // 5b. Self-heal reconciler: every ~30s, re-bind outputs whose
    //     LWE process has died (SIGCHLD left a stale pid in the map).
    //     Cheap (one /proc read per output) and bounded (no I/O burst).
    let reconcile_handle = tokio::spawn(reconcile_dispatcher(
        daemon.clone(),
        Duration::from_secs(30),
    ));

    // 5c. Fullscreen-aware per-output pause/resume. Polls niri's
    //     IPC every ~2s; when a window goes fullscreen on an
    //     output, kill LWE for that output to free the GPU; when
    //     fullscreen clears (e.g. user switches workspace), re-spawn
    //     LWE with the last-known scene.
    //
    //     The watcher is best-effort: `niri msg --json` failures
    //     are logged but don't abort the task (e.g. niri restart,
    //     session bus hiccup). The previous state is preserved so
    //     a transient miss doesn't thrash.
    let fullscreen_handle = tokio::spawn(fullscreen_dispatcher(
        daemon.clone(),
        Duration::from_secs(2),
    ));

    // 6. Wait for SIGINT/SIGTERM.
    let sig = wait_for_shutdown_signal().await?;
    tracing::info!("received {sig}; shutting down");

    // 7. Graceful shutdown.
    dbus_handle.abort();
    hotplug_handle.abort();
    reconcile_handle.abort();
    fullscreen_handle.abort();
    let _ = dbus_handle.await;
    let _ = hotplug_handle.await;
    let _ = reconcile_handle.await;
    let _ = fullscreen_handle.await;

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

/// Self-heal reconciler: periodically calls `daemon.reconcile()` to
/// re-bind outputs whose LWE process has died (SIGCHLD leaks). Cheap
/// (one `/proc/<pid>/status` read per output) and bounded (single
/// spawn attempt per dead output per pass).
async fn reconcile_dispatcher(daemon: Arc<PaperforgeDaemon>, poll_interval: Duration) {
    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // First tick fires immediately; skip it so we don't reconcile
    // before the daemon has finished its own startup (which sets
    // `known_outputs` via hotplug).
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let respawned = daemon.reconcile().await;
        if !respawned.is_empty() {
            tracing::info!(
                target: "paperforge",
                "reconcile pass complete: {} output(s) re-spawned: {:?}",
                respawned.len(),
                respawned.iter().map(|(o, p)| format!("{o}={p}")).collect::<Vec<_>>()
            );
        }
    }
}

/// Per-output auto-pause driven by niri fullscreen detection.
///
/// Algorithm:
/// 1. Every `poll_interval`, query niri's IPC for outputs /
///    workspaces / windows.
/// 2. Compute the set of outputs currently covered by a fullscreen
///    window (window.tile_size ~= output.logical within 5px).
/// 3. Diff against the previous snapshot.
/// 4. For outputs that became fullscreen: `kill_per_output(name)`
///    — SIGTERM the LWE child to free the GPU/DRM socket.
/// 5. For outputs that stopped being fullscreen:
///    `resume_per_output_specific(name)` — re-spawn LWE with the
///    last-known scene so the wallpaper comes back smoothly.
///
/// Best-effort: niri IPC failures are logged at warn and the
/// previous snapshot is preserved (no thrash from a transient
/// hiccup). The watcher is intentionally idempotent — calling
/// kill on an already-dead pid is a no-op.
async fn fullscreen_dispatcher(daemon: Arc<PaperforgeDaemon>, poll_interval: Duration) {
    use std::collections::BTreeSet;
    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // First tick fires immediately; skip it so we don't poll
    // before the daemon's hotplug task has registered known
    // outputs.
    ticker.tick().await;
    let mut prev: BTreeSet<String> = BTreeSet::new();
    loop {
        ticker.tick().await;
        let snap = match paperforge_core::fullscreen::snapshot().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "paperforge",
                    "fullscreen snapshot failed (niri IPC): {e}; keeping previous state"
                );
                continue;
            }
        };
        let current = snap.fullscreen_outputs();
        // Compute transitions: fullscreen-on and fullscreen-off.
        for output in current.difference(&prev) {
            match daemon.kill_per_output(output).await {
                Ok(()) => tracing::info!(
                    target: "paperforge",
                    "fullscreen ON on {output}: killed LWE to free GPU/DRM socket"
                ),
                Err(e) => tracing::warn!(
                    target: "paperforge",
                    "kill_per_output({output}) failed: {e}"
                ),
            }
        }
        for output in prev.difference(&current) {
            match daemon.resume_per_output_specific(output).await {
                Ok(pid) => tracing::info!(
                    target: "paperforge",
                    "fullscreen OFF on {output}: re-spawned LWE pid={pid}"
                ),
                Err(e) => tracing::warn!(
                    target: "paperforge",
                    "resume_per_output_specific({output}) failed: {e}"
                ),
            }
        }
        prev = current;
    }
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

/// Run a `gdbus call` against the daemon. Returns:
///
/// - `Ok(true)` if the daemon accepted the call (we routed through it).
/// - `Ok(false)` if `gdbus` failed because no daemon is on the session
///   bus (caller should fall back to local spawn).
/// - `Err(anyhow::Error)` if the daemon *was* reachable but the
///   method returned an error (caller decides whether to retry or
///   surface the error).
async fn gdbus_call(method: &str, args: &[&str]) -> anyhow::Result<bool> {
    let mut cmd_args: Vec<String> = vec![
        "call".into(),
        "--session".into(),
        "--dest".into(),
        "org.louzt.Paperforge".into(),
        "--object-path".into(),
        "/org/louzt/Paperforge".into(),
        "--method".into(),
        method.into(),
    ];
    for a in args {
        cmd_args.push(a.to_string());
    }
    let out = tokio::process::Command::new("gdbus")
        .args(&cmd_args)
        .output()
        .await?;
    if out.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    // `gdbus` returns "Can't find object /org/louzt/Paperforge" (or
    // similar) when no daemon is on the bus. Treat that as "not
    // reachable" rather than a real error so callers can fall back.
    let lowered = stderr.to_lowercase();
    if lowered.contains("can't find")
        || lowered.contains("cannot find")
        || lowered.contains("not found")
        || lowered.contains("no such")
        || lowered.contains("no connection")
        || lowered.contains("service unknown")
    {
        return Ok(false);
    }
    anyhow::bail!("gdbus call {method} failed: {}", stderr.trim())
}

/// Forward `paperforge set <scene>` to the running daemon so the
/// daemon's LweBackendOps owns the per-output state. Returns
/// `Ok(true)` if daemon handled it, `Ok(false)` if no daemon is
/// reachable (caller should fall back to local spawn).
async fn daemon_set_wallpaper(output: &str, scene_path: &str) -> anyhow::Result<bool> {
    gdbus_call(
        "org.louzt.Paperforge1.SetWallpaper",
        &[output, scene_path],
    )
    .await
}

/// Forward `paperforge set <scene>` (no `--output`) — daemon picks
/// the default output (active workspace's output).
async fn daemon_set_default_wallpaper(scene_path: &str) -> anyhow::Result<bool> {
    gdbus_call("org.louzt.Paperforge1.SetWallpaper", &["", scene_path]).await
}

/// Forward `paperforge playlist apply <name>` to the daemon.
async fn daemon_apply_playlist(name: &str) -> anyhow::Result<bool> {
    gdbus_call("org.louzt.Paperforge1.ApplyPlaylist", &[name]).await
}
