//! Root component — top-level layout that composes every panel.
//!
//! PR 2 ships a placeholder layout. PR 3 swaps the placeholder for
//! the real `Sidebar | Bindings | Status` panel grid.

use dioxus::prelude::*;

use crate::app::AppState;
use crate::ui::theme::{self, FONT_STACK, PANEL_BORDER, PANEL_PADDING};

/// Root component. Mounts [`AppState`] into context so every
/// descendant can read it via `use_context::<AppState>()`.
///
/// `#[allow(non_snake_case)]` is required because Dioxus components
/// follow PascalCase by convention. The CI runs
/// `RUSTFLAGS="-D warnings"` so this allow is needed to keep the
/// build green.
#[allow(non_snake_case)]
#[component]
pub fn Root() -> Element {
    // `use_context_provider` mounts `AppState::new()` into the
    // Dioxus context tree once at the root. The closure runs once
    // (during the initial render). Children call
    // `use_context::<AppState>()` to read it.
    use_context_provider(AppState::new);

    let state = use_context::<AppState>();

    rsx! {
        document::Title { "paperforge — Dioxus GUI" }
        div {
            style: "padding: 1rem 1.5rem; font-family: {FONT_STACK}; background: #0d1117; color: #e6edf3; min-height: 100vh;",
            header {
                style: "display: flex; align-items: center; gap: 0.75rem; margin-bottom: 1rem;",
                h1 {
                    style: "font-size: 1.25rem; margin: 0;",
                    "paperforge"
                }
                span {
                    style: "color: #8b949e; font-size: 0.875rem;",
                    "Fase 6C.0 · Dioxus 0.8.0-alpha.0"
                }
            }
            if !state.path_warnings.is_empty() {
                div {
                    style: "background: #5a1f1f; color: #ffdcd7; padding: 0.5rem 1rem; border-radius: 4px; margin-bottom: 1rem;",
                    "Path detection warning: {state.path_warnings[0]}"
                }
            }
            div {
                style: "{PANEL_PADDING} {PANEL_BORDER} background: #161b22;",
                p { "Skeleton placeholder — PR 3 wires the sidebar, bindings, and status panels." }
                p {
                    style: "color: #8b949e; margin-top: 0.5rem;",
                    "Detected roots: "
                    for root in state.paths.all() {
                        span {
                            style: "display: inline-block; background: #21262d; padding: 0.125rem 0.5rem; border-radius: 3px; margin: 0 0.25rem 0.25rem 0; font-family: monospace;",
                            "{root.display()}"
                        }
                    }
                }
            }
            // Theme module is wired even though no panel currently
            // uses its helpers — keeps the navigation graph stable.
            div { style: "display: none;", "{theme::state_color(paperforge_core::backend::BackendState::Running)}" }
        }
    }
}
