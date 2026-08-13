//! paperforge-gui — Fase 6C
//!
//! Dioxus 0.8-alpha desktop GUI for paperforge.
//!
//! Status: PR 3 (read-only panels with local data refresh loops).
//! Subsequent PRs add the D-Bus IPC client, write actions, picker,
//! drag-drop editor, and thumbnails.
//!
//! Pinned to `=0.8.0-alpha.0` per operator decision D2 (2026-07-28).
//!
//! # MSRV
//!
//! Dioxus 0.8.0-alpha's `#[component]` proc-macro emits code that
//! requires Rust ≥ 1.76 (e.g. `if let` chains in trait impls). The
//! workspace MSRV is 1.75, but this crate is the only Dioxus user
//! and the alpha pin already raises the floor anyway. The
//! crate-level allow below suppresses the clippy diagnostic that
//! would otherwise block CI.

#![allow(clippy::incompatible_msrv)]

use dioxus::prelude::launch;
// Crate names with `-` are referenced as `_` in Rust (Cargo convention).
// Aliased to `core` so clippy::single_component_path_imports stays clean.
use paperforge_core as core;

mod app;
mod data;
mod error;
mod ui;

fn main() {
    // Install tracing-subscriber so logs from the GUI surface in the same stream as CLI/core.
    // Dev: `RUST_LOG=debug cargo run -p paperforge-gui`.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    tracing::info!("paperforge-gui starting (Dioxus 0.8.0-alpha.0) · PR 3 read-only panels");

    // Verify the core lib integration works before launching the GUI.
    let paths = core::paths::default_paths();
    match core::paths::require_at_least_one(&paths) {
        Ok(()) => {
            tracing::info!(
                roots = ?paths.all().collect::<Vec<_>>(),
                "core lib OK — workshop paths detected"
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "core lib paths detection failed (non-fatal)");
        }
    }

    launch(ui::root::Root);
}
