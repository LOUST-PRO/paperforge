//! Local data fetches.
//!
//! Each submodule exposes `refresh_*` async functions that take inputs
//! and return `(T, Option<GuiError>)`. The root coroutine drives them
//! on independent timers (outputs 2s, playlists 10s, inventory 30s)
//! and writes the result into the corresponding `AppState` signal.
//!
//! Why one function per panel (not a single batched refresh):
//! - Independent failure modes — a broken playlist does not hide the
//!   inventory.
//! - Independent cadences — outputs change quickly, inventory rarely.
//! - Trivial test surface — each refresh is a thin async wrapper over
//!   a single `paperforge_core` call.
//!
//! D-Bus backed panels (running pids, bindings, daemon state) live
//! under `ipc/` and are wired in PR 4. Bindings get a placeholder
//! here so the UI module can compile against the read-only shape.

pub mod bindings;
pub mod inventory;
pub mod outputs;
pub mod playlists;
