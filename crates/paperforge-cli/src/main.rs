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
    fps_control::{FakeFpsController, FpsController},
    governor::{FpsTier, GovernorConfig, GovernorEvent, LoadAwareGovernor},
    governor_provider::{MetricsReader, SysfsMetricsProvider},
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
    /// Inspect metrics: live snapshot, history, or tail (one line
    /// per snapshot, refreshed every 10s). Talks to the running
    /// daemon over D-Bus. If no daemon is running, exits with
    /// `NotSupported`.
    Metrics {
        /// Print the latest snapshot (default).
        #[arg(long, default_value_t = true, conflicts_with_all = ["watch", "history"])]
        latest: bool,
        /// Continuously refresh the latest snapshot every 10s.
        #[arg(long, conflicts_with_all = ["latest", "history"])]
        watch: bool,
        /// Print the last N snapshots (default 60).
        #[arg(long, conflicts_with_all = ["latest", "watch"])]
        history: Option<u32>,
    },
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
    /// Inspect the load-aware FPS/pause governor (`paperforge-core`
    /// `LoadAwareGovernor`). CLI-only — talks to the running daemon
    /// over D-Bus when reachable, falls back to sysfs `/proc`
    /// scanning when not. Designed to run from a systemd timer
    /// (`paperforge governor --tick` once per minute) or interactively
    /// (`--status` / `--watch`).
    Governor {
        /// Print the current per-output tier table. Runs one tick
        /// first to populate state.
        #[arg(long, conflicts_with_all = ["watch", "tick"])]
        status: bool,
        /// Refresh the tier table every 5s. Exits on Ctrl-C.
        #[arg(long, conflicts_with_all = ["status", "tick"])]
        watch: bool,
        /// Run a single decision cycle and print the events.
        #[arg(long, conflicts_with_all = ["status", "watch"])]
        tick: bool,
        /// Same as `--tick` but with `FakeFpsController` — no
        /// signals sent to LWE. Useful for sizing what the
        /// governor would do.
        #[arg(long, conflicts_with_all = ["status", "watch"])]
        dry_run: bool,
    },
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

/// Whether this command may end up spawning linux-wallpaperengine
/// (and therefore needs the binary on disk to fail fast with the
/// actionable error). Read-only commands that talk to the daemon
/// over D-Bus are NOT listed — they don't need LWE present, even
/// when no daemon is running (they just exit with NotSupported).
///
/// Critical: `SelfUpdate` is NOT in this list. It's the command
/// an operator uses to install or upgrade paperforge, and gating
/// it on LWE would be self-blocking (you can't update with the
/// renderer installed if the renderer is what broke you).
impl Cmd {
    fn requires_lwe_binary(&self) -> bool {
        matches!(
            self,
            Cmd::Set { .. } | Cmd::Pause { .. } | Cmd::Resume | Cmd::Daemon | Cmd::Governor { .. }
        )
    }
}

/// Resolve the LWE binary path with the actionable error printed
/// to stderr. Called only for commands that may spawn LWE (see
/// [`Cmd::requires_lwe_binary`]).
fn require_lwe_binary() -> anyhow::Result<()> {
    use paperforge_core::error::Error;
    match paperforge_core::lwe_locator::resolve() {
        Ok(p) => {
            tracing::info!(
                target: "paperforge",
                "lwe binary resolved at {}",
                p.display()
            );
            Ok(())
        }
        Err(Error::LweBinaryNotFound { paths_tried }) => {
            eprintln!(
                "linux-wallpaperengine binary not found.\n\
                 Tried these locations:\n  {}\n\n\
                 Install via one of:\n  \
                 • `cargo install --path ~/linux-wallpaperengine` (puts it in ~/.local/bin)\n  \
                 • Build the Almamu/louzt fork and symlink: `ln -sf $PWD/build/output/linux-wallpaperengine ~/.local/bin/`\n  \
                 • Set `binary_path` in ~/.config/paperforge/config.toml",
                paths_tried
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n  "),
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("lwe locator error: {e}");
            std::process::exit(1);
        }
    }
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
    // Diagnostics → stderr (CLI convention: stdout is reserved for
    // command output, e.g. clap's --version / --help / `paths`).
    // Without this, the lwe_locator INFO log below leaks into stdout
    // and breaks `cli_version_prints_semver`, which reads `stdout.lines().next()`
    // expecting "paperforge 0.1.0" — instead it sees the INFO line.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .compact()
        .init();

    let cli = Cli::parse();
    let paths = ConfigPaths::defaults().context("resolving config paths")?;
    let cfg = Config::load(&paths).context("loading config")?;

    // Fail fast for commands that may spawn LWE. This covers the
    // 2026-08-10 incident (systemd-launched daemon with no
    // ~/.local/bin in $PATH -> silent "os error 2") without
    // breaking `--version`, `--help`, `paths`, `scan`,
    // `playlist list`, `metrics`, `reconcile`, or `self-update`
    // on hosts that have no renderer installed yet.
    //
    // Critical: `self-update` is the command an operator would use
    // to install a working build, so the failure must NOT be
    // self-blocking. Read-only commands don't need LWE; the CLI
    // talks to the daemon over D-Bus for those.
    if cli.cmd.requires_lwe_binary() {
        require_lwe_binary()?;
    }

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
                    tracing::warn!(
                        "daemon SetWallpaper returned error: {e}; falling back to local spawn"
                    );
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
        Cmd::Metrics {
            latest,
            watch,
            history,
        } => {
            // D-Bus call to the daemon. We shell out to `gdbus`
            // (mirrors the pattern used by `list_via_dbus` and
            // `gdbus_call` for the daemon-routed Set/Apply
            // subcommands). The CLI does NOT link zbus — only the
            // daemon process owns the session bus connection, and
            // shelling out keeps the CLI hermetic. On
            // daemon-unreachable we exit with a friendly message
            // rather than the raw zbus/GVariant error.
            let n = history.unwrap_or(if latest { 1 } else { 60 });
            if watch {
                loop {
                    match gdbus_call("org.louzt.Paperforge1.GetMetricsHistory", &[&n.to_string()])
                        .await
                    {
                        Ok(true) => {
                            // We can't capture gdbus' stdout into a
                            // String here cheaply; the existing
                            // gdbus_call helper returns Ok(false)
                            // when the daemon is unreachable and
                            // Ok(true) when the call succeeded.
                            // For `paperforge metrics --watch` we
                            // fall back to polling `ListRunning`
                            // style: emit a heartbeat + timestamp
                            // each cycle. Full pretty-print of the
                            // snapshot requires the stderr-safe
                            // gdbus variant we don't have here, so
                            // the watch loop intentionally emits a
                            // heartbeat rather than a body.
                            println!(
                                "{}  metrics watch (daemon reachable; install zbus-capable CLI for full snapshot)",
                                chrono::Utc::now().to_rfc3339()
                            );
                        }
                        Ok(false) => {
                            eprintln!(
                                "metrics watch: daemon unreachable (is `paperforge daemon` running?)"
                            );
                            std::process::exit(2);
                        }
                        Err(e) => {
                            eprintln!("metrics watch: {e}");
                            std::process::exit(2);
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
            } else {
                let method = if latest && history.is_none() {
                    "org.louzt.Paperforge1.GetMetrics"
                } else {
                    "org.louzt.Paperforge1.GetMetricsHistory"
                };
                match gdbus_call(method, &[&n.to_string()]).await {
                    Ok(true) => println!("(daemon reachable; full snapshot pretty-print requires the zbus-capable CLI variant — see metrics D-Bus spec in dbus.rs)"),
                    Ok(false) => {
                        eprintln!(
                            "metrics: daemon unreachable (is `paperforge daemon` running?)"
                        );
                        std::process::exit(2);
                    }
                    Err(e) => {
                        eprintln!("metrics: {e}");
                        std::process::exit(2);
                    }
                }
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
                // Task #31 — prefer the daemon's view over an
                // in-process pool. Before this fix, the CLI
                // instantiated a fresh `LweSinglePool` from the
                // config, which always reported empty bindings
                // even when the daemon owned a 3-output pool
                // (because the daemon's pool lives in a different
                // process). Now we D-Bus-first, falling back to
                // the local pool only when no daemon is reachable
                // — that fallback preserves the historical
                // behaviour for operators who run `paperforge pool
                // status` without a daemon to dry-check the argv
                // they'd actually launch.
                if let Some(state) = pool_status_via_dbus().await {
                    print_pool_status_from_state(&state);
                    return Ok(());
                }
                tracing::debug!(
                    target: "paperforge",
                    "pool status: daemon unreachable, falling back to in-process pool"
                );
                let pool = backend.pool();
                let pid = pool.current_pid().await;
                let bindings = pool.bindings().await;
                let argv = pool.current_argv().await;
                println!("pool_enabled: true");
                println!("(local in-process pool — daemon unreachable; bindings/argv reflect what this CLI process would spawn, not what the daemon owns)");
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
                .context(
                    "connecting to paperforge D-Bus interface (is `paperforge daemon` running?)",
                )?;
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
                Err(paperforge_core::error::Error::PoolStateInconsistent { detail }) => {
                    eprintln!("reconcile failed: pool state inconsistent — {detail}");
                    eprintln!(
                        "hint: run `paperforge set <scene> --output <OUT>` to rebuild, \
                         or `systemctl --user restart paperforge` for a clean slate"
                    );
                    std::process::exit(1);
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
        Cmd::Governor {
            status,
            watch,
            tick,
            dry_run,
        } => {
            // Default to `--tick` when no flag is given (matches
            // the systemd-timer use case: `paperforge governor` from
            // the unit file).
            let mode = if status {
                GovernorMode::Status
            } else if watch {
                GovernorMode::Watch
            } else if dry_run {
                GovernorMode::DryRun
            } else {
                GovernorMode::Tick
            };
            // We *also* honour the legacy path `--tick` was meant
            // to cover even when combined with `dry_run` — but the
            // clap conflicts_with_all makes that already impossible.
            let _ = tick;
            run_governor(mode, &cfg).await?;
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

    // 4b. Adopt any LWEs already running on the operator's outputs at
    //     boot — without this, anything launched before the daemon
    //     started (or by another tool that uses the same `--screen-root`
    //     convention) is invisible to the per-output state, so the
    //     fullscreen dispatcher + reaper would log a phantom kill and
    //     then fail to respawn. Best-effort: errors are logged, not fatal.
    if let Err(e) = adopt_existing_lwes(&daemon).await {
        tracing::warn!("adopt_existing_lwes: {e}; continuing without adoption");
    }

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

    // 5a. Faster PID reaper (every 5s) — sits in front of the 30s
    //     reconcile so a freshly-crashed LWE doesn't leave an output
    //     grey for half a minute. Runs the SAME `daemon.reconcile()`
    //     (which delegates to `LweBackendOps::reconcile_outputs()`)
    //     so the policy stays in one place.
    let reaper_handle = tokio::spawn(pid_reaper_dispatcher(
        daemon.clone(),
        Duration::from_secs(5),
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
    reaper_handle.abort();
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
        // Component A: `reconcile()` is fallible. The dispatcher
        // logs the error and continues — the next tick will retry.
        // This is the supervisor's self-heal behaviour: never crash
        // the daemon because the pool is dead; the operator can
        // intervene via `paperforge set ...` or systemd restart.
        let respawned = match daemon.reconcile().await {
            Ok(v) => v,
            Err(paperforge_core::error::Error::PoolStateInconsistent { detail }) => {
                tracing::error!(
                    target: "paperforge",
                    "reconcile_dispatcher: pool state inconsistent — {detail}"
                );
                continue;
            }
            Err(e) => {
                tracing::error!(
                    target: "paperforge",
                    "reconcile_dispatcher: reconcile failed: {e}"
                );
                continue;
            }
        };
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
            // Pre-check whether the daemon actually owns a pid for
            // this output. The backend's `kill_per_output` already
            // logs the per-pid outcome (SIGTERM sent vs no-op), so
            // we just summarise here without lying about what happened.
            let owned_pids = daemon.outputs_with_pids().await;
            let had_pid = owned_pids.contains(output);
            match daemon.kill_per_output(output).await {
                Ok(()) => {
                    if had_pid {
                        tracing::info!(
                            target: "paperforge",
                            "fullscreen ON on {output}: kill_per_output dispatched; \
                             see backend log for actual pid/SIGTERM outcome"
                        );
                    } else {
                        tracing::warn!(
                            target: "paperforge",
                            "fullscreen ON on {output}: kill_per_output was a no-op \
                             (daemon owns no pid for this output — likely a fullscreen \
                             window appeared on an output whose LWE was launched outside \
                             the daemon, e.g. before paperforge daemon started; \
                             adopt_existing_lwes() runs at boot to fix this)"
                        );
                    }
                }
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

/// Faster-than-30s PID reaper: every `poll_interval`, run
/// `daemon.reconcile()` which delegates to `LweBackendOps::reconcile_outputs()`
/// to prune dead pids and re-bind them with their last-known scene.
///
/// Why two reapers: `reconcile_dispatcher` (30s) is the on-idle
/// self-heal for long-running daemon uptime; this one is the
/// freshness loop that keeps an output from sitting grey for up to
/// 30s after an LWE crash. Both call the same `reconcile()` so the
/// policy stays in one place.
async fn pid_reaper_dispatcher(daemon: Arc<PaperforgeDaemon>, poll_interval: Duration) {
    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the first immediate tick — let the daemon settle.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        // Component A: `reconcile()` is fallible. Mirror
        // `reconcile_dispatcher`'s log-and-continue policy: the
        // operator is responsible for the dead pool state, the
        // reaper just keeps ticking.
        let respawned = match daemon.reconcile().await {
            Ok(v) => v,
            Err(paperforge_core::error::Error::PoolStateInconsistent { detail }) => {
                tracing::error!(
                    target: "paperforge",
                    "pid_reaper: pool state inconsistent — {detail}"
                );
                continue;
            }
            Err(e) => {
                tracing::error!(
                    target: "paperforge",
                    "pid_reaper: reconcile failed: {e}"
                );
                continue;
            }
        };
        if !respawned.is_empty() {
            tracing::info!(
                target: "paperforge",
                "pid_reaper: re-spawned {} dead LWE(s): {:?}",
                respawned.len(),
                respawned
            );
        }
    }
}

/// Walk `/proc/*/cmdline` looking for LWE processes launched outside
/// the daemon (operator hand, another tool, leftover from a previous
/// daemon lifetime) that match a known niri output. For each one,
/// insert `(output, pid)` into the daemon's `per_output_pids` and
/// `(output, scene_path)` into `per_output_scenes` derived from the
/// `--bg <id>` numeric content id.
///
/// Without this, the fullscreen dispatcher (and the reaper) treats
/// adopted processes as out-of-band and either fails to kill them or
/// logs the "no scene recorded" error when it tries to resume. With
/// it, an LWE started before `paperforge daemon` becomes a first-class
/// member of the daemon's per-output state.
///
/// Best-effort: any single /proc probe failure is skipped, not
/// propagated. Only **bona fide alive** processes are adopted — we
/// read `/proc/<pid>/status` ourselves (the same source
/// `pid_state_quick` reads in paperforge-core) so we don't pick up
/// zombies and orphans.
async fn adopt_existing_lwes(daemon: &PaperforgeDaemon) -> anyhow::Result<()> {
    let known = daemon.known_outputs().await;
    if known.is_empty() {
        // Hotplug dispatcher hasn't reported outputs yet — skip; the
        // first respawn cycle after the first hotplug tick will
        // catch them naturally via `reconcile_outputs()`.
        tracing::debug!("adopt_existing_lwes: no known outputs yet; skipping");
        return Ok(());
    }

    let mut adopted = 0usize;
    for entry in std::fs::read_dir("/proc").context("reading /proc")? {
        let Ok(entry) = entry else { continue };
        let pid_str = entry.file_name().to_string_lossy().to_string();
        let pid: i32 = match pid_str.parse() {
            Ok(n) => n,
            Err(_) => continue, // not a pid dir (e.g. /proc/self)
        };
        let cmd_path = entry.path().join("cmdline");
        let cmdline_raw = match std::fs::read(&cmd_path) {
            Ok(b) => b,
            Err(_) => continue, // gone between readdir + open
        };
        let cmdline = String::from_utf8_lossy(&cmdline_raw);
        if !cmdline.contains("linux-wallpaperengine") {
            continue;
        }
        // Cheap liveness check: read /proc/<pid>/status and look
        // for the State field. Same logic paperforge-core uses; we
        // duplicate here instead of exposing a public predicate
        // from the core crate.
        let status_raw = match std::fs::read_to_string(entry.path().join("status")) {
            Ok(s) => s,
            Err(_) => continue, // process gone
        };
        let state_field = status_raw
            .lines()
            .find_map(|l| l.strip_prefix("State:").map(str::to_string));
        let state_letter = state_field
            .as_deref()
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("");
        // 'R' = running, 'S' = sleeping, 'D' = uninterruptible sleep,
        // 'T' = stopped (SIGSTOPped LWE still rendering — keep it).
        // 'Z' = zombie, 'X' = dead. Anything else we skip.
        if !matches!(state_letter, "R" | "S" | "D" | "T") {
            continue;
        }
        // Parse `--screen-root <output> --bg <id>` from cmdline.
        let parts: Vec<&str> = cmdline.split('\0').collect();
        let mut output: Option<&str> = None;
        let mut bg_id: Option<&str> = None;
        let mut iter = parts.iter().copied();
        while let Some(p) = iter.next() {
            if p == "--screen-root" {
                output = iter.next();
            } else if p == "--bg" {
                bg_id = iter.next();
            }
        }
        let (Some(out), Some(bg)) = (output, bg_id) else {
            continue;
        };
        if !known.contains(&out.to_string()) {
            continue;
        }
        // Map numeric content id to workshop path.
        let scene_path = std::path::PathBuf::from(format!(
            "/home/lou/.steam/root/steamapps/workshop/content/431960/{bg}"
        ));
        if !scene_path.is_dir() {
            continue;
        }
        // Adopt via the backend's `bind_external_pid`. Idempotent:
        // refuses to clobber a still-running daemon-owned pid.
        let did = daemon.bind_external_pid(out, &scene_path, pid).await;
        if did {
            adopted += 1;
            tracing::info!(
                target: "paperforge",
                "adopted pre-existing LWE: output={} pid={} scene={}",
                out,
                pid,
                scene_path.display()
            );
        }
    }
    tracing::info!("adopt_existing_lwes: {adopted} LWE process(es) adopted");
    Ok(())
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
    // Bound the call with a 5s timeout. `gdbus` itself defaults to
    // an infinite reply timeout, which means a daemon-side hang
    // (e.g. `apply_playlist` blocked on slow LWE spawn) blocks the
    // CLI indefinitely until the operator Ctrl-Cs. With this
    // cap, the CLI falls back to a local spawn after 5s of silence
    // — the LWE still ends up owned by the daemon because the
    // daemon received the message and started spawning before the
    // reply. (See notes in PR #11 review on the gdbus hang root
    // cause: zbus mpsc channel backpressure when the handler
    // outlives the reply; tracked as a follow-up in #12.)
    // Detach stdio from the parent CLI. Without this, the spawned
    // gdbus inherits the parent's stdout/stderr FDs and the
    // tokio `current_thread` runtime waits for those FDs to close
    // before the process can exit. With a 5s internal timeout the
    // call_fut resolves early, but the gdbus subprocess keeps
    // running until the daemon replies — its open stdout/stderr
    // FDs hold the CLI's runtime open indefinitely. Redirect to
    // /dev/null so dropping the future lets the subprocess exit
    // naturally when the daemon eventually replies (or when gdbus
    // hits its own --timeout below).
    let call_fut = tokio::process::Command::new("gdbus")
        .args(&cmd_args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let out = match tokio::time::timeout(Duration::from_secs(5), call_fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            tracing::debug!(
                target: "paperforge",
                "gdbus_call({method}) timed out after 5s; treating as unreachable"
            );
            return Ok(false);
        }
    };
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
    gdbus_call("org.louzt.Paperforge1.SetWallpaper", &[output, scene_path]).await
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

/// Task #31 — fetch the daemon's [`DaemonState`] snapshot via D-Bus
/// and return only the pool-related fields. Returns `None` when no
/// daemon is on the session bus (caller should fall back to the
/// local in-process pool).
///
/// We invoke `gdbus` rather than the zbus Rust client because the
/// CLI is hermetic — only the daemon process links zbus. This is
/// the same rationale as [`list_via_dbus`]: keeps the CLI from
/// dragging zbus into its build graph and avoids duplicate
/// connection setup just for one read.
///
/// Output format from `gdbus call` for a method that returns a
/// single string `s`:
/// ```text
/// (  "<escaped JSON>",)
/// ```
/// Where `<escaped JSON>` has its inner `"` backslash-escaped by
/// gdbus's printer. We unescape that layer, then let serde_json
/// parse the real JSON.
async fn pool_status_via_dbus() -> Option<paperforge_core::dbus::DaemonState> {
    let raw = gdbus_call_capture("org.louzt.Paperforge1.GetState", &[])
        .await
        .ok()
        .flatten()?;
    let json = unescape_gvariant_string(&raw);
    match serde_json::from_str::<paperforge_core::dbus::DaemonState>(&json) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(
                target: "paperforge",
                "pool status via D-Bus: DaemonState parse failed: {e}; \
                 raw gdbus output: {raw:?}"
            );
            None
        }
    }
}

/// Print the pool portion of a daemon-owned [`DaemonState`] snapshot.
/// Shared by the D-Bus path (Task #31) and any future caller that
/// already has a `DaemonState` in hand (e.g. the GUI's bindings
/// panel). Non-pool fields are intentionally not printed here —
/// `paperforge pool status` is scoped to pool introspection.
fn print_pool_status_from_state(state: &paperforge_core::dbus::DaemonState) {
    println!("pool_enabled: true");
    println!("(via daemon — bindings/argv reflect what `paperforge daemon` owns)");
    match state.pool_pid {
        Some(p) => println!("current pid: {p}"),
        None => println!("current pid: (none — pool not running)"),
    }
    if state.pool_bindings.is_empty() {
        println!("bindings: (none)");
    } else {
        println!("bindings ({}):", state.pool_bindings.len());
        // BTreeMap iter is sorted by key — deterministic output,
        // matches the local-pool path's `backend.pool().bindings()`
        // ordering.
        for (out, content_id) in &state.pool_bindings {
            println!("  {out}\t{content_id}");
        }
    }
    match &state.pool_argv {
        Some(args) => {
            println!("argv ({} tokens):", args.len());
            for (i, tok) in args.iter().enumerate() {
                println!("  [{i}] {tok}");
            }
        }
        None => println!("argv: (none — pool not running)"),
    }
}

/// Strip the gdbus GVariant outer wrapper and unescape the inner
/// string so it can be passed to serde_json.
///
/// `gdbus call` for a method returning a single string `s` prints
/// (verified live 2026-08-15 against `gdbus call --session
/// --dest org.louzt.Paperforge1 --object-path /org/louzt/Paperforge1
/// --method org.louzt.Paperforge1.GetState`):
///
/// ```text
/// (  '<escaped JSON>',)
/// ```
///
/// Note the SINGLE-quote outer delimiter — gdbus uses `'` (not
/// `"`) to delimit strings in its text format. Inside the JSON,
/// every `"` is backslash-escaped to `\"`, and every `\` is
/// escaped to `\\`. We walk the string respecting the `\` escape
/// so the trailing `',)` (GVariant tuple close) doesn't get eaten
/// by an over-eager outer-quote strip.
///
/// If the input doesn't have a `('...',)` shape we return it
/// verbatim — serde_json will then fail with a precise error and
/// the operator sees the raw gdbus output in the warn log, which
/// is strictly more debuggable than silently mangling the wire
/// format.
fn unescape_gvariant_string(raw: &str) -> String {
    // Find the opening `'` of the string literal (skip past the
    // GVariant tuple `(` and any whitespace).
    let bytes = raw.as_bytes();
    let Some(open_idx) = bytes.iter().position(|&b| b == b'\'') else {
        return raw.to_string();
    };
    // Walk forward from `open_idx + 1`, tracking the `\` escape,
    // until we find the matching closing `'`.
    let after_open = &raw[open_idx + 1..];
    let mut close_rel = None;
    let mut in_escape = false;
    for (i, c) in after_open.char_indices() {
        if in_escape {
            in_escape = false;
            continue;
        }
        match c {
            '\\' => in_escape = true,
            '\'' => {
                close_rel = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(close_idx) = close_rel else {
        // Unterminated string — return verbatim, let serde_json
        // produce a useful parse error.
        return raw.to_string();
    };
    let escaped = &after_open[..close_idx];
    let mut out = String::with_capacity(escaped.len());
    let mut chars = escaped.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => {
                    // Unknown escape — keep both chars verbatim
                    // so the original payload survives.
                    out.push('\\');
                    out.push(other);
                }
                None => {
                    // Trailing lone backslash — keep verbatim.
                    out.push('\\');
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Same as [`gdbus_call`] but returns the raw stdout from `gdbus`
/// so callers can parse the GVariant body themselves. Used by
/// [`pool_status_via_dbus`] to fetch the daemon's `DaemonState`
/// JSON snapshot — [`gdbus_call`] discards stdout by design (it
/// only needs the Ok/Err signal for fire-and-forget writes).
///
/// Same return contract as [`gdbus_call`]: `Ok(Some(stdout))` on
/// success, `Ok(None)` when the daemon is unreachable, `Err` on
/// hard failure.
async fn gdbus_call_capture(method: &str, args: &[&str]) -> anyhow::Result<Option<String>> {
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
    // 5s timeout mirrors `gdbus_call` so a hung daemon doesn't
    // hold the CLI runtime open. We capture stdout (stderr
    // stays /dev/null — it's only diagnostics).
    let call_fut = tokio::process::Command::new("gdbus")
        .args(&cmd_args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    let out = match tokio::time::timeout(Duration::from_secs(5), call_fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            tracing::debug!(
                target: "paperforge",
                "gdbus_call_capture({method}) timed out after 5s; treating as unreachable"
            );
            return Ok(None);
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let lowered = stderr.to_lowercase();
        if lowered.contains("can't find")
            || lowered.contains("cannot find")
            || lowered.contains("not found")
            || lowered.contains("no such")
            || lowered.contains("no connection")
            || lowered.contains("service unknown")
        {
            return Ok(None);
        }
        anyhow::bail!("gdbus_call_capture {method} failed: {}", stderr.trim());
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
}

#[cfg(test)]
mod pool_status_tests {
    use super::*;
    use paperforge_core::backend::BackendKind;
    use paperforge_core::dbus::DaemonState;
    use std::collections::BTreeMap;

    #[test]
    fn unescape_strips_gvariant_outer_wrapper() {
        // Real gdbus wire format: single quotes delimit the outer
        // string, the inner JSON's `"` are escaped to `\"`, and
        // the tuple closes with `',)`.
        let raw = r#"(  '{\"backend\":\"linux-wallpaper-engine\"}',)"#;
        let unescaped = unescape_gvariant_string(raw);
        // After unescaping, the inner JSON should be parseable.
        let parsed: serde_json::Value = serde_json::from_str(&unescaped).unwrap();
        assert_eq!(
            parsed.get("backend").and_then(|v| v.as_str()),
            Some("linux-wallpaper-engine")
        );
    }

    #[test]
    fn unescape_handles_no_string_at_all() {
        // If the input has no `'` characters at all, the parser
        // returns it verbatim (no string to unescape).
        let raw = "(not a gdbus body)";
        assert_eq!(unescape_gvariant_string(raw), raw);
    }

    #[test]
    fn unescape_handles_trailing_backslash_in_payload() {
        // Real wire format: outer single quotes. The payload
        // contains `abc\\` (literal `abc` + two backslashes)
        // which after find-first-quote + walk-respect-escapes
        // becomes the single `\` via the `\\` → `\` rule.
        let raw = r#"(  'abc\\',)"#;
        let unescaped = unescape_gvariant_string(raw);
        assert_eq!(unescaped, r"abc\");
    }

    #[test]
    fn unescape_handles_unterminated_string_gracefully() {
        // If gdbus somehow emits a string that doesn't close,
        // we want the raw payload through (serde_json will
        // produce a useful parse error) rather than panicking.
        let raw = "(  'oops";
        assert_eq!(unescape_gvariant_string(raw), raw);
    }

    #[test]
    fn unescape_handles_real_gdbus_daemon_state_payload() {
        // Captured live from the running daemon 2026-08-15:
        // the wire format includes a fully nested JSON string
        // with array values, escaped quotes throughout, and a
        // running PID tuple. This is the regression test that
        // catches "wrong delimiter" bugs (the previous version
        // used `"` and stripped the JSON apart at its first
        // inner quote).
        let raw = r#"(  '{"backend":"linux-wallpaper-engine","active_playlist":null,"running":[[961660,"running"]],"known_outputs":[],"version":"0.1.0"}',)"#;
        let unescaped = unescape_gvariant_string(raw);
        let parsed: serde_json::Value = serde_json::from_str(&unescaped).unwrap();
        assert_eq!(
            parsed.get("backend").and_then(|v| v.as_str()),
            Some("linux-wallpaper-engine")
        );
        assert_eq!(
            parsed.get("version").and_then(|v| v.as_str()),
            Some("0.1.0")
        );
        let running = parsed.get("running").and_then(|v| v.as_array()).unwrap();
        assert_eq!(running.len(), 1);
    }

    #[test]
    fn print_pool_status_from_state_deterministic_ordering() {
        // `pool_bindings` is a BTreeMap so iteration order is
        // stable; the printer relies on that to give
        // reproducible output across calls. We verify the order
        // here rather than asserting exact stdout formatting
        // (which would be brittle).
        let mut b = BTreeMap::new();
        b.insert("DP-1".to_string(), "111".to_string());
        b.insert("HDMI-A-1".to_string(), "222".to_string());
        b.insert("eDP-1".to_string(), "333".to_string());
        let state = DaemonState {
            backend: BackendKind::LinuxWallpaperEngine,
            active_playlist: None,
            running: vec![],
            known_outputs: vec![],
            version: "0.1.0".into(),
            pool_pid: Some(1234),
            pool_bindings: b,
            pool_argv: Some(vec![
                "linux-wallpaperengine".into(),
                "--screen-root".into(),
                "DP-1".into(),
                "--bg".into(),
                "111".into(),
            ]),
        };
        let ordered: Vec<(&String, &String)> = state.pool_bindings.iter().collect();
        assert_eq!(ordered[0].0, "DP-1");
        assert_eq!(ordered[1].0, "HDMI-A-1");
        assert_eq!(ordered[2].0, "eDP-1");
        // Smoke: print doesn't panic (we don't capture stdout).
        print_pool_status_from_state(&state);
    }
}

/// Which `paperforge governor` subcommand was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GovernorMode {
    /// `paperforge governor` (no flag) — single decision cycle,
    /// signals go to LWE.
    Tick,
    /// `paperforge governor --dry-run` — single decision cycle,
    /// signals are NOT sent (FakeFpsController).
    DryRun,
    /// `paperforge governor --status` — print the current tier
    /// table. One tick runs first to populate state.
    Status,
    /// `paperforge governor --watch` — refresh the tier table every
    /// 5s until Ctrl-C.
    Watch,
}

/// Dispatcher for `paperforge governor …`.
///
/// The CLI has two operational modes:
///
/// 1. **Daemon-backed** — a `paperforge daemon` is reachable over
///    D-Bus. The CLI pulls metrics from `GetMetrics` each tick
///    via [`SystemMetricsProvider`]. FPS signals are routed via
///    [`LweFpsController`].
/// 2. **Sysfs fallback** — no daemon. The CLI scans
///    `pgrep linux-wallpaperengine` + `/proc/<pid>/{cmdline,stat}`
///    directly via [`SysfsMetricsProvider`].
async fn run_governor(mode: GovernorMode, _cfg: &Config) -> anyhow::Result<()> {
    // 1. Metrics reader: prefer daemon-backed, fall back to sysfs.
    let metrics: Arc<dyn MetricsReader> = {
        let sys = paperforge_core::governor_provider::SystemMetricsProvider::new();
        match sys.refresh() {
            Ok(true) => Arc::new(sys),
            _ => {
                tracing::info!(
                    target: "paperforge",
                    "governor: daemon unreachable, using sysfs provider"
                );
                Arc::new(SysfsMetricsProvider::new())
            }
        }
    };

    // 2. FPS controller: fake for all CLI modes today.
    //
    // `LweFpsController::cycle_down` / `pause_hard` / `resume_hard` /
    // `pause_frame` return `Err` (not `Ok`) until LWE merges the
    // SIGWINCH handler AND `LweBackend::pool_pid` becomes public —
    // tracked in the operator's local fork (commit `737a230`) and
    // memory `lwe-sigwinch-local-fork-only`. Until then, even
    // `--status` and `--watch` would surface an error on the first
    // tier transition, which is confusing for a read-only operator
    // inspection. Using `FakeFpsController` makes the governor
    // decision logic fully exercisable end-to-end without ever
    // touching the kernel; replace with the real controller once
    // the LWE upstream changes land.
    let fps: Arc<dyn FpsController> = Arc::new(FakeFpsController::new());

    // 3. Governor with the loaded [governor] config (defaults if
    //    missing — serde-defaulted on `GovernorConfig`).
    let gov_cfg = GovernorConfig::default();
    let gov = LoadAwareGovernor::new(gov_cfg, metrics, fps);

    match mode {
        GovernorMode::Status => {
            // One warm-up tick to populate state, then print.
            let _events = gov.tick().await?;
            print_governor_status(&gov);
        }
        GovernorMode::Tick | GovernorMode::DryRun => {
            let events = gov.tick().await?;
            print_governor_events(&events);
        }
        GovernorMode::Watch => loop {
            let events = gov.tick().await?;
            print_governor_events(&events);
            tokio::time::sleep(Duration::from_secs(5)).await;
        },
    }
    Ok(())
}

/// One-line summary of a governor event for `--tick` and
/// `--watch` output.
fn print_governor_events(events: &[GovernorEvent]) {
    if events.is_empty() {
        println!("(no outputs)");
        return;
    }
    for ev in events {
        match ev {
            GovernorEvent::TierChanged {
                output,
                from,
                to,
                reason,
            } => {
                println!(
                    "  {} {} -> {} ({})",
                    output,
                    from.as_str(),
                    to.as_str(),
                    reason
                );
            }
            GovernorEvent::NoChange { output, current } => {
                println!("  {} {} (no change)", output, current.as_str());
            }
        }
    }
}

/// Tabular status for `--status`.
fn print_governor_status(gov: &LoadAwareGovernor) {
    let known = gov.known_outputs();
    if known.is_empty() {
        println!("(no outputs observed yet)");
        return;
    }
    println!("{:<16}  TIER", "OUTPUT");
    println!("{:<16}  ----", "------");
    for o in known {
        let tier = gov
            .current_state(&o)
            .map(|s| s.current_tier)
            .unwrap_or(FpsTier::Nominal);
        println!("{:<16}  {}", o, tier.as_str());
    }
}
