//! `paperforge-tui` — entry point for the read-only TUI debugger.
//!
//! The actual render loop lives in [`paperforge_tui::run`]. This bin
//! only parses CLI args, wires up tracing, and hands control over to
//! the tokio runtime. Useful flags:
//!
//! - `--source <PATH>` (repeatable) — additional wallpaper roots to
//!   scan on top of the defaults discovered by
//!   [`paperforge_core::paths::default_paths`].
//!
//! Inside the TUI:
//!
//! - `1` / `2` / `3` / `4` — jump to the matching panel.
//! - `Tab` / `Backtab` — cycle focus forward / backward.
//! - `↑` / `↓` (or `k` / `j`) — navigate the focused panel.
//! - `r` — force refresh of the focused panel.
//! - `q` / `Esc` / `Ctrl-C` — quit (terminal is always restored).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Debug, Parser)]
#[command(
    name = "paperforge-tui",
    version,
    about = "Read-only TUI debugger for paperforge",
    long_about = None
)]
struct Cli {
    /// Additional wallpaper roots to scan for inventory. Repeatable.
    ///
    /// Defaults are discovered by `paperforge_core::paths::default_paths`
    /// (`~/Pictures/wallpapers`, `/usr/share/backgrounds`, the workshop's
    /// `assets/`, etc.). Pass `--source` to point the inventory panel at
    /// extra trees (e.g. a personal collection or a mounted share).
    #[arg(long = "source", value_name = "PATH")]
    source: Vec<PathBuf>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    match real_main().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Best-effort tracing if the subscriber wasn't installed.
            eprintln!("paperforge-tui: error: {err:#}");
            tracing::error!(error = %err, "tui exited with error");
            ExitCode::FAILURE
        }
    }
}

async fn real_main() -> Result<()> {
    install_tracing();

    let cli = Cli::parse();
    tracing::debug!(extra_sources = cli.source.len(), "starting paperforge-tui");

    paperforge_tui::run(cli.source)
        .await
        .context("tui runtime failed")
}

fn install_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,paperforge=debug,paperforge_tui=debug"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false))
        .init();
}
