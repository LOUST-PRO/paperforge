//! GUI error type.
//!
//! All fallible operations in the GUI return or propagate a
//! [`GuiError`]. Variants cover each subsystem that can fail:
//!
//! - [`GuiError::Ipc`] — D-Bus call to the paperforge daemon failed
//!   (daemon not running, signature mismatch, transport error).
//! - [`GuiError::DaemonResponse`] — the daemon responded with an
//!   error string (the only payload zbus can transport for an
//!   error).
//! - [`GuiError::Core`] — a `paperforge-core` call failed (file IO,
//!   config parse, daemon lifecycle).
//! - [`GuiError::Image`] — thumbnail decode/encode error.
//! - [`GuiError::Io`] — raw `std::io::Error` wrapped.
//! - [`GuiError::Config`] — config file parse / validation.
//! - [`GuiError::Notice`] — non-fatal, dismissable. The UI shows
//!   it as a yellow banner and the user clicks ✕ to clear.
//!
//! Every variant is `Clone + Display + Debug` so it can flow through
//! Dioxus signals (which require `Clone + PartialEq + 'static`)
//! and render into the status banner via [`std::fmt::Display`].
//!
//! The `keep-stale-data on error` UX policy (lifted from the TUI)
//! means most errors here are **non-fatal** for snapshot data —
//! the GUI keeps the previous snapshot and surfaces the error in
//! the banner. Only `Ipc` write failures invalidate the optimistic
//! update.

use std::fmt;

use paperforge_core::error::Error as CoreError;

/// GUI-level error union.
///
/// One variant per subsystem. New variants are added (not
/// refactored) to keep the `match` arms in `ui/status.rs` cheap.
#[allow(dead_code)] // consumed by ui/status.rs in PR 3+
#[derive(Debug, Clone, PartialEq)]
pub enum GuiError {
    /// D-Bus IPC failure (connection, call_method, signature).
    Ipc { kind: &'static str, message: String },
    /// Daemon returned an error string from a method call.
    DaemonResponse(String),
    /// `paperforge-core` returned an error.
    Core(String),
    /// Thumbnail decode/encode error (image crate).
    Image(String),
    /// ffmpeg subprocess error (thumbnail extraction for LooseVideo).
    Ffmpeg(String),
    /// Raw `std::io::Error` wrapped.
    Io(String),
    /// Config file parse / validation error.
    Config(String),
    /// Non-fatal, dismissable notice.
    Notice(String),
}

impl GuiError {
    /// Short, single-word source label for the banner.
    #[allow(dead_code)] // consumed by ui/status.rs in PR 3+
    pub fn source(&self) -> &'static str {
        match self {
            GuiError::Ipc { .. } => "ipc",
            GuiError::DaemonResponse(_) => "daemon",
            GuiError::Core(_) => "core",
            GuiError::Image(_) => "image",
            GuiError::Ffmpeg(_) => "ffmpeg",
            GuiError::Io(_) => "io",
            GuiError::Config(_) => "config",
            GuiError::Notice(_) => "notice",
        }
    }

    /// Severity hint. `Notice` is the only "soft" variant; the
    /// others paint a red/yellow banner.
    #[allow(dead_code)] // consumed by ui/status.rs in PR 3+
    pub fn severity(&self) -> Severity {
        match self {
            GuiError::Notice(_) => Severity::Notice,
            GuiError::Ipc { .. } | GuiError::DaemonResponse(_) => Severity::Error,
            GuiError::Core(_)
            | GuiError::Image(_)
            | GuiError::Ffmpeg(_)
            | GuiError::Io(_)
            | GuiError::Config(_) => Severity::Error,
        }
    }

    /// Construct from a `paperforge-core` error.
    #[allow(dead_code)] // consumed by ipc/client.rs in PR 4
    pub fn from_core(e: CoreError) -> Self {
        GuiError::Core(format!("{e}"))
    }
}

/// Banner severity.
#[allow(dead_code)] // consumed by ui/status.rs in PR 3+
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Yellow banner, dismissable. The app keeps running normally.
    Notice,
    /// Red banner. Action required.
    Error,
}

impl fmt::Display for GuiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GuiError::Ipc { kind, message } => {
                write!(f, "IPC error ({kind}): {message}")
            }
            GuiError::DaemonResponse(m) => write!(f, "daemon error: {m}"),
            GuiError::Core(m) => write!(f, "core error: {m}"),
            GuiError::Image(m) => write!(f, "image error: {m}"),
            GuiError::Ffmpeg(m) => write!(f, "ffmpeg error: {m}"),
            GuiError::Io(m) => write!(f, "io error: {m}"),
            GuiError::Config(m) => write!(f, "config error: {m}"),
            GuiError::Notice(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for GuiError {}

impl From<std::io::Error> for GuiError {
    fn from(e: std::io::Error) -> Self {
        GuiError::Io(e.to_string())
    }
}

impl From<CoreError> for GuiError {
    fn from(e: CoreError) -> Self {
        GuiError::Core(format!("{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paperforge_core::error::Error as CoreError;

    #[test]
    fn source_labels_are_distinct() {
        let variants = [
            GuiError::Ipc {
                kind: "connect",
                message: "x".into(),
            },
            GuiError::DaemonResponse("x".into()),
            GuiError::Core("x".into()),
            GuiError::Image("x".into()),
            GuiError::Ffmpeg("x".into()),
            GuiError::Io("x".into()),
            GuiError::Config("x".into()),
            GuiError::Notice("x".into()),
        ];
        let labels: Vec<&'static str> = variants.iter().map(|e| e.source()).collect();
        let mut sorted = labels.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "source labels must be unique");
    }

    #[test]
    fn notice_is_only_soft_variant() {
        let variants = vec![
            GuiError::Ipc {
                kind: "x",
                message: "x".into(),
            },
            GuiError::DaemonResponse("x".into()),
            GuiError::Core("x".into()),
            GuiError::Image("x".into()),
            GuiError::Ffmpeg("x".into()),
            GuiError::Io("x".into()),
            GuiError::Config("x".into()),
        ];
        for v in variants {
            assert_eq!(v.severity(), Severity::Error, "{v:?} should be Error");
        }
        assert_eq!(GuiError::Notice("x".into()).severity(), Severity::Notice);
    }

    #[test]
    fn display_includes_source_kind() {
        let e = GuiError::Ipc {
            kind: "connect",
            message: "no session bus".into(),
        };
        assert!(format!("{e}").contains("connect"));
        assert!(format!("{e}").contains("no session bus"));
    }

    #[test]
    fn from_io_preserves_message() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing file");
        let g: GuiError = io.into();
        assert!(matches!(g, GuiError::Io(_)));
        assert!(format!("{g}").contains("missing file"));
    }

    #[test]
    fn from_core_wraps_message() {
        // Use `CoreError::Config(String)` to avoid pulling anyhow
        // into this crate's dev-dependencies.
        let c = CoreError::Config("bad path".into());
        let g: GuiError = c.into();
        assert!(matches!(g, GuiError::Core(_)));
        assert!(format!("{g}").contains("bad path"));
    }
}
