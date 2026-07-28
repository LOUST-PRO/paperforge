//! paperforge-gui — Fase 6C
//!
//! Dioxus 0.8-alpha desktop GUI for paperforge.
//!
//! Status: hello-world scaffold (pre-Fase 6C implementation).
//! Next: lazy-load video preview, headless mode API verification, smoke test contra LWE fork.
//!
//! Pinned to `=0.8.0-alpha.0` per operator decision D2 (2026-07-28).
//! See MANIFEST-2026-07-28-dioxus-ecosystem-triage.md §D2 for rationale.

use dioxus::prelude::*;
// Crate names with `-` are referenced as `_` in Rust (Cargo convention).
// Aliased to `core` so clippy::single_component_path_imports stays clean.
use paperforge_core as core;

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

    tracing::info!("paperforge-gui starting (Dioxus 0.8.0-alpha.0)");

    // Verify the core lib integration works before launching the GUI.
    // Fase 6C will replace this with full UI binding to paperforge-core APIs.
    let paths = core::paths::default_paths();
    match core::paths::require_at_least_one(&paths) {
        Ok(()) => {
            tracing::info!(roots = ?paths.all().collect::<Vec<_>>(), "core lib OK — workshop paths detected");
        }
        Err(e) => {
            tracing::warn!(error = %e, "core lib paths detection failed (non-fatal in scaffold)");
        }
    }

    launch(App);
}

/// Root component. Hello-world scaffold; full UI lives in Fase 6C implementation.
///
/// `#[allow(non_snake_case)]` is required because Dioxus components follow
/// PascalCase by convention. The CI runs `RUSTFLAGS="-D warnings"` so this
/// allow is needed to keep the build green.
#[allow(non_snake_case)]
#[component]
fn App() -> Element {
    rsx! {
        document::Title { "paperforge — Dioxus GUI" }
        div {
            style: "padding: 2rem; font-family: system-ui, sans-serif;",
            h1 { "paperforge" }
            p {
                style: "color: #666;",
                "Fase 6C scaffold · Dioxus 0.8.0-alpha.0 · Rust 1.96"
            }
            p {
                style: "margin-top: 1rem;",
                "Próximos: lazy-load video preview, headless mode, playlist UI, monitor picker."
            }
        }
    }
}