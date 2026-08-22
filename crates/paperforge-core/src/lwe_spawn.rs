//! LWE process spawn path with optional systemd cgroup isolation.
//!
//! When a systemd user instance is available and `Delegate=yes` is
//! set on paperforge.service, LWE spawns go through:
//!
//!   systemd-run --user --quiet --no-ask-password --scope \
//!     --slice=paperforge-lwe.slice --unit=<derived> -- \
//!     /path/to/lwe --screen-root <output> ...
//!
//! This puts LWE in its own cgroup slice with its own memory cap
//! (configured in `contrib/systemd/paperforge-lwe.slice`), so a
//! runaway LWE renderer can never exhaust paperforge's small
//! orchestrator budget.
//!
//! ## Why --scope and not --service?
//!
//! `systemd-run --service` daemonizes: the actual LWE is re-parented
//! to systemd's PID 1. Pool management breaks because `Child::id()`
//! returns systemd's daemon PID, not LWE's, and SIGCHLD fires when
//! systemd's daemon exits (never, until LWE finishes).
//!
//! `--scope` keeps LWE as a direct child of paperforge. PID
//! semantics, SIGTERM, and SIGCHLD all work normally. Component C's
//! `drain_pipe` logic continues to work: setting
//! `stdout=Stdio::piped()` on the systemd-run Command captures the
//! pipe at systemd-run's level, and after systemd-run exec's into
//! LWE, the pipe is LWE's stdout (inherited across exec).
//!
//! ## Fallback
//!
//! If systemd-run is unavailable (CI containers, sudo -u contexts
//! without systemd, missing binary), `systemd_run_available()`
//! returns false and `build_command` falls back to direct
//! `tokio::process::Command::new(binary)` with the legacy
//! `pre_exec_setsid` for terminal isolation. The fallback keeps LWE
//! in paperforge's cgroup — only enable it for test/dev where you
//! don't need the isolation.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use tokio::process::Command;

use crate::error::Result;

/// Name of the cgroup slice LWE renderer processes are placed in
/// when systemd-run --scope is available. Referenced by name from
/// `contrib/systemd/paperforge-lwe.slice`.
pub const SLICE_NAME: &str = "paperforge-lwe.slice";

/// Cached availability check. Resolved on first call.
static SYSTEMD_RUN_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Bundle of inputs for one LWE spawn. Caller fills the fields and
/// hands the struct to [`build_command`].
#[derive(Debug, Clone)]
pub struct SpawnConfig<'a> {
    /// Absolute path to the `linux-wallpaperengine` binary.
    pub binary: &'a Path,
    /// Compositor output name (e.g. `DP-1`, `HDMI-A-1`). Used to
    /// derive the systemd transient unit name and is also passed
    /// through to LWE as `--screen-root`.
    pub output: &'a str,
    /// Wallpaper Engine scene id (numeric `<id>` or `<id>_<variant>`).
    pub content_id: &'a str,
    /// FPS cap forwarded to LWE as `--fps`.
    pub fps: u32,
}

/// `true` if `systemd-run` is on `$PATH` and the user systemd
/// socket exists under `$XDG_RUNTIME_DIR/systemd/`. Cached after
/// the first call — cheap to query on every spawn.
///
/// Test escape hatch: under `cfg(test)` the env var
/// `PAPERFORGE_FORCE_NO_SYSTEMD=1` short-circuits to `false` so
/// `build_command` falls back to direct spawn. Direct spawn is
/// what the test wrappers expect — `set_per_output_with_fps`
/// records `child.id()` from `Command::spawn`, and with
/// `systemd-run` that PID is the transient systemd-run process,
/// not the LWE process it forks. The PID-recycling defense
/// (`pid_state_quick` + cmdline cross-check) would correctly
/// flag the systemd-run PID as non-LWE and the test would
/// falsely report the spawned output as dead. Set the env var
/// BEFORE the first `build_command` call inside the test.
///
/// The check happens before the `OnceLock` lookup so a parallel
/// test that doesn't set the env var can't poison the cache for
/// tests that do.
pub fn systemd_run_available() -> bool {
    if cfg!(test) && std::env::var_os("PAPERFORGE_FORCE_NO_SYSTEMD").is_some() {
        return false;
    }
    *SYSTEMD_RUN_AVAILABLE.get_or_init(detect_systemd_run)
}

fn detect_systemd_run() -> bool {
    which("systemd-run").is_some() && systemd_user_socket_exists()
}

fn systemd_user_socket_exists() -> bool {
    let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") else {
        return false;
    };
    std::path::Path::new(&xdg).join("systemd").exists()
}

/// Resolve a binary name to its first match on $PATH.
fn which(binary: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Pure-function variant for tests.
#[cfg(test)]
fn systemd_run_available_with(binary_found: bool, socket_exists: bool) -> bool {
    binary_found && socket_exists
}

/// Sanitize a compositor output name into a systemd unit-name suffix.
/// `DP-1` → `dp-1`, `HDMI-A-1` → `hdmi-a-1`, garbage → `unknown`.
pub fn unit_name_for(output: &str) -> String {
    let mut sanitized: String = output
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    // Collapse consecutive dashes
    let mut prev_dash = false;
    sanitized.retain(|c| {
        let dash = c == '-';
        let drop = dash && prev_dash;
        prev_dash = dash;
        !drop
    });
    let trimmed = sanitized.trim_matches('-');
    let suffix = if trimmed.is_empty() {
        "unknown"
    } else {
        trimmed
    };
    let capped = if suffix.len() > 32 {
        &suffix[..32]
    } else {
        suffix
    };
    format!("paperforge-lwe-{capped}")
}

/// Build the LWE spawn command. The caller is responsible for `.spawn()`.
///
/// Always sets `stdin=null`, `stdout=Stdio::piped()`,
/// `stderr=Stdio::piped()` so Component C's `drain_pipe` logic can
/// capture journald-bound logs.
///
/// When `systemd_run_available()` is true, the binary is invoked
/// through `systemd-run --user --scope` into `paperforge-lwe.slice`.
/// Otherwise, falls back to direct spawn with `pre_exec_setsid` for
/// terminal isolation (matches legacy behaviour).
pub fn build_command(cfg: &SpawnConfig<'_>) -> Result<Command> {
    let mut cmd = if systemd_run_available() {
        let mut c = Command::new("systemd-run");
        c.arg("--user")
            .arg("--quiet")
            .arg("--no-ask-password")
            .arg("--scope")
            .arg(format!("--slice={SLICE_NAME}"))
            .arg(format!("--unit={}", unit_name_for(cfg.output)))
            .arg("--property=Description=paperforge LWE renderer")
            .arg("--property=CPUQuota=80%") // don't starve compositor
            .arg("--");
        c.arg(cfg.binary);
        c
    } else {
        let mut c = Command::new(cfg.binary);
        crate::detach::pre_exec_setsid(c.as_std_mut());
        c
    };

    cmd.arg("--screen-root")
        .arg(cfg.output)
        .arg("--bg")
        .arg(cfg.content_id)
        .arg("--silent")
        .arg("--volume")
        .arg("0")
        .arg("--no-audio-processing")
        .arg("--noautomute")
        .arg("--disable-particles")
        .arg("--disable-mouse")
        .arg("--disable-parallax")
        .arg("--fullscreen-pause-only-active")
        .arg("--fps")
        .arg(cfg.fps.to_string());

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_name_sanitizes_uppercase() {
        assert_eq!(unit_name_for("DP-1"), "paperforge-lwe-dp-1");
    }

    #[test]
    fn unit_name_sanitizes_hyphens_and_digits() {
        assert_eq!(unit_name_for("HDMI-A-1"), "paperforge-lwe-hdmi-a-1");
        assert_eq!(unit_name_for("eDP-1"), "paperforge-lwe-edp-1");
    }

    #[test]
    fn unit_name_collapses_consecutive_specials() {
        assert_eq!(unit_name_for("Foo___Bar"), "paperforge-lwe-foo-bar");
    }

    #[test]
    fn unit_name_trims_leading_trailing_specials() {
        assert_eq!(unit_name_for("___DP-1___"), "paperforge-lwe-dp-1");
    }

    #[test]
    fn unit_name_handles_empty() {
        assert_eq!(unit_name_for(""), "paperforge-lwe-unknown");
    }

    #[test]
    fn unit_name_handles_pure_garbage() {
        assert_eq!(unit_name_for("___"), "paperforge-lwe-unknown");
    }

    #[test]
    fn unit_name_caps_length() {
        let long = "a".repeat(64);
        let name = unit_name_for(&long);
        assert!(name.len() <= 64, "name too long: {name} ({})", name.len());
    }

    #[test]
    fn systemd_run_available_pure() {
        assert!(systemd_run_available_with(true, true));
        assert!(!systemd_run_available_with(true, false));
        assert!(!systemd_run_available_with(false, true));
        assert!(!systemd_run_available_with(false, false));
    }
}
