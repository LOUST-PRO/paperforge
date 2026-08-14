//! Library entry point for `paperforge-gui`.
//!
//! `paperforge-gui` is a [Cargo](https://doc.rust-lang.org/cargo/)
//! package with both a binary target (`src/main.rs`) and a library
//! target (`src/lib.rs`). The binary target is the Dioxus desktop
//! GUI; the library target exposes the non-UI modules so that
//! integration tests under `tests/` and future consumers (e.g. a
//! CLI tool that wants to reuse the GUI's playlist save helper)
//! can hit the public surface without depending on Dioxus internals.
//!
//! ## Which modules are exposed?
//!
//! - `data` — local-filesystem data helpers (`refresh_inventory`,
//!   `refresh_playlists`, `save_playlist`, `set_binding`,
//!   `unset_binding`, `apply_playlist`). No Dioxus types in the
//!   signatures, so it's safe to import from any context.
//! - `error` — `GuiError` enum and `Severity` produced by the
//!   data helpers.
//!
//! UI modules (`ui/*`, `ipc/*`) stay private to the binary. They
//! pull in Dioxus 0.8-alpha and zbus which are heavy and unrelated
//! to the data layer's contract.
//!
//! ## Why a lib at all?
//!
//! Rust's `tests/` directory compiles each file as a separate
//! crate that imports the library target. Without a `lib.rs`,
//! integration tests can't reach the binary's modules — you'd be
//! forced to embed everything in `#[cfg(test)] mod tests` even
//! when the test path is genuinely cross-file / fixture-backed.
//! The split keeps unit tests close to the implementation (idiomatic
//! Rust) while letting integration tests live in `tests/` where they
//! belong.

#![allow(clippy::incompatible_msrv)]

pub mod data;
pub mod error;
pub mod ipc;
