//! lzt-wallcraft CLI entry point.
//!
//! Subcommands (clap derive):
//! - `set <PATH> [--output <NAME>]` — launch LWE with a wallpaper
//! - `pause` — SIGSTOP all LWE instances
//! - `resume` — SIGCONT all LWE instances
//! - `list` — list currently-running LWE PIDs + state
//! - `scan` — scan default paths, print discovered entries
//! - `audio <toggle|mute|unmute>` — audio control via SIGUSR
//! - `playlist <list|show|save|apply|delete>` — playlist management
//! - `paths` — print detected source paths

use anyhow::Context;
use clap::{Parser, Subcommand};
use lzt_wallcraft_core::{
    audio::AudioCommand,
    backend::{BackendState, WallpaperBackend},
    config::{Config, ConfigPaths},
    inventory::Inventory,
    paths::default_paths,
    playlist::PlaylistStore,
};

/// `lzt-wallcraft` — Wallpaper Engine Workshop manager for Linux.
#[derive(Debug, Parser)]
#[command(name = "lzt-wallcraft", version, about, long_about = None)]
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
            let status = PlaylistStore::lwe_status(&backend).await?;
            if status.is_empty() {
                println!("(no LWE instances running)");
            } else {
                for (pid, state) in status {
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
    }
    Ok(())
}
