//! D-Bus IPC layer for paperforge-gui.
//!
//! Three responsibilities:
//!
//! 1. [`client::IpcClient`] — typed wrapper around `PaperforgeClient`
//!    that exposes a signal stream (the 3 `org.louzt.Paperforge1`
//!    signals) and turns raw `Message`s into a typed
//!    [`SignalEvent`] enum.
//! 2. [`reconnect::next_backoff`] — exponential schedule (5s, 10s,
//!    20s, capped 30s) consumed by `ui::root` when the daemon is
//!    unreachable.
//! 3. [`ConnectionStatus`] — connection state for the title-bar dot
//!    and the banner UX.
//!
//! Why one module per file: each has a distinct test surface
//! (`client` needs the bus, `reconnect` is pure math, the enum is a
//! tiny discriminant). The flat module layout keeps the imports
//! readable across the GUI.

use std::fmt;

pub mod client;
pub mod reconnect;

/// Connection state for the live D-Bus subscription.
///
/// Surfaced in the title-bar dot (`theme::connection_color`) and the
/// keep-stale-data banner. Transitions:
///
/// ```text
/// Disconnected ──▶ Reconnecting{1..} ──▶ Connected
///        ▲                                       │
///        └────────────── (any failure) ──────────┘
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionStatus {
    /// No connection yet, or last `connect()` failed without a
    /// pending retry. The title bar shows red.
    #[default]
    Disconnected,
    /// Background loop is trying to reconnect. `attempt` is the
    /// retry index (1-based) so the operator can tell whether the
    /// reconnect is in the "fast first try" phase (5s, 10s) or the
    /// "stable retry" phase (30s).
    Reconnecting { attempt: u32 },
    /// Live connection + signal subscription active.
    Connected,
}

impl fmt::Display for ConnectionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectionStatus::Disconnected => write!(f, "disconnected"),
            ConnectionStatus::Reconnecting { attempt } => {
                write!(f, "reconnecting (attempt {attempt})")
            }
            ConnectionStatus::Connected => write!(f, "connected"),
        }
    }
}
