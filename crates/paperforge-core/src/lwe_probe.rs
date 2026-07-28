//! LWE binary version probing.
//!
//! Different `linux-wallpaperengine` builds handle POSIX signals
//! differently. Upstream Almamu's build of LWE **does NOT install a
//! handler for `SIGUSR1`/`SIGUSR2`**. When the kernel delivers an
//! unhandled user signal, the default action is to terminate the
//! process. Sending `SIGUSR1` to upstream LWE therefore kills it.
//!
//! Before [`crate::audio::LweAudioController`] dispatches audio
//! signals, we need to know whether the running LWE binary handles
//! them. We probe by running `<binary> --version` and inspecting the
//! output for a fork marker. The louzt fork (and any future patches
//! that add signal handlers) prints a recognizable string.
//!
//! ## Detection strategy
//!
//! 1. Run `<binary> --version` with a 2-second timeout (LWE usually
//!    prints version and exits immediately; if it hangs, fall back
//!    to "unknown" rather than blocking).
//! 2. Scan the combined stdout/stderr for a known marker substring
//!    (`"signal handlers"` is the canonical marker we put in the
//!    louzt fork patch).
//! 3. If no marker is found, classify the build as "unknown" —
//!    callers should default to **no audio control** unless the
//!    operator explicitly opted in via
//!    [`crate::config::Config::lwe_supports_audio_signals`].

use std::{
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Result of probing a single LWE binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LweBuildKind {
    /// Louzt fork with explicit SIGUSR1/SIGUSR2 handlers (or any
    /// future fork that ships the "signal handlers" marker in its
    /// `--version` output).
    SupportedFork,
    /// Upstream Almamu build — SIGUSR handlers NOT installed. Sending
    /// signals will terminate the process.
    UpstreamNoSignals,
    /// We could not run the binary or its `--version` output did not
    /// match any known marker. Treat as unsafe to send signals.
    Unknown,
}

impl LweBuildKind {
    /// Whether audio signals can be safely dispatched to a binary
    /// classified as this kind.
    pub fn supports_audio_signals(&self) -> bool {
        matches!(self, Self::SupportedFork)
    }
}

/// Maximum time we wait for `<binary> --version` to produce output
/// before declaring the probe inconclusive.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Probe `binary` for its build kind. Sync (uses `Command::output`
/// with a manual timeout via `wait_timeout` semantics — Rust doesn't
/// have a portable kill-by-timeout so we use a thread + `try_wait`).
///
/// The `binary` argument may be an absolute path or a `PATH`-lookup
/// name. The function does NOT do PATH resolution itself — callers
/// that want PATH lookup should pre-resolve via
/// [`which`](https://docs.rs/which) or `Command::new` and then call
/// this with the resulting path.
pub fn probe_lwe_binary(binary: &Path) -> LweBuildKind {
    match run_with_timeout(binary, PROBE_TIMEOUT) {
        Some(stdout) => classify_output(&stdout),
        None => LweBuildKind::Unknown,
    }
}

/// Run `<binary> --version` and capture combined stdout/stderr up to
/// `timeout`. Returns `None` if the binary did not exit in time, was
/// not found, or any other I/O error occurred.
fn run_with_timeout(binary: &Path, timeout: Duration) -> Option<String> {
    let mut child = Command::new(binary)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let start = Instant::now();

    // Poll try_wait in a tight loop with sleeps so we can bail out
    // after `timeout` even if the child is wedged. This is portable
    // across Linux/macOS without requiring signal-based termination
    // (which would itself send SIGKILL — too destructive for a probe).
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                let mut out = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = stdout.read_to_string(&mut out);
                }
                let mut err = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut err);
                }
                if !err.is_empty() {
                    out.push('\n');
                    out.push_str(&err);
                }
                return Some(out);
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }
}

/// Classify the captured output of `binary --version`.
fn classify_output(stdout: &str) -> LweBuildKind {
    let lower = stdout.to_ascii_lowercase();
    // The louzt fork (and any future patch that adds signal handlers)
    // MUST print "signal handlers" in its --version output. This is a
    // contractual marker; without it we cannot trust the binary.
    if lower.contains("signal handlers") {
        LweBuildKind::SupportedFork
    } else if lower.contains("linux-wallpaperengine") || lower.contains("wallpaper engine") {
        // Looks like upstream Almamu's --version, which is just the
        // version string. Default to "no signals" rather than
        // "unknown" because we have positive evidence this is an
        // upstream build.
        LweBuildKind::UpstreamNoSignals
    } else {
        LweBuildKind::Unknown
    }
}

/// Probe + return whether the binary can safely receive audio
/// signals. Convenience wrapper used by [`crate::audio`] and CLI.
pub fn lwe_supports_audio_signals(binary: &Path) -> bool {
    probe_lwe_binary(binary).supports_audio_signals()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn classify_detects_supported_fork_marker() {
        let s = "linux-wallpaperengine 0.10.1 (louzt fork, signal handlers enabled)";
        assert_eq!(classify_output(s), LweBuildKind::SupportedFork);
    }

    #[test]
    fn classify_detects_upstream_version_only() {
        let s = "linux-wallpaperengine 0.10.1";
        assert_eq!(classify_output(s), LweBuildKind::UpstreamNoSignals);
    }

    #[test]
    fn classify_handles_case_insensitive_marker() {
        let s = "Linux-WallpaperEngine 0.10.1\nBuild with SIGNAL HANDLERS";
        assert_eq!(classify_output(s), LweBuildKind::SupportedFork);
    }

    #[test]
    fn classify_unknown_for_garbage() {
        let s = "random shell output that does not match anything";
        assert_eq!(classify_output(s), LweBuildKind::Unknown);
    }

    #[test]
    fn classify_empty_string_is_unknown() {
        assert_eq!(classify_output(""), LweBuildKind::Unknown);
    }

    #[test]
    fn supports_audio_signals_predicate() {
        assert!(LweBuildKind::SupportedFork.supports_audio_signals());
        assert!(!LweBuildKind::UpstreamNoSignals.supports_audio_signals());
        assert!(!LweBuildKind::Unknown.supports_audio_signals());
    }

    fn write_fake_binary(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let script = dir.join(name);
        let mut f = std::fs::File::create(&script).unwrap();
        write!(f, "#!/bin/sh\n{body}\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[test]
    fn probe_a_real_script() {
        // Build a tiny shell script that prints the marker, then
        // probe it. This validates the full path: spawn, read, classify.
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_binary(
            tmp.path(),
            "fake-lwe.sh",
            "echo 'linux-wallpaperengine 0.10.1 (signal handlers)'",
        );

        let kind = probe_lwe_binary(&script);
        assert_eq!(kind, LweBuildKind::SupportedFork);
    }

    #[test]
    fn probe_a_real_upstream_script() {
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_binary(
            tmp.path(),
            "upstream-lwe.sh",
            "echo 'linux-wallpaperengine 0.10.1'",
        );

        let kind = probe_lwe_binary(&script);
        assert_eq!(kind, LweBuildKind::UpstreamNoSignals);
    }

    #[test]
    fn probe_returns_unknown_for_nonexistent_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let kind = probe_lwe_binary(&missing);
        assert_eq!(kind, LweBuildKind::Unknown);
    }

    #[test]
    fn probe_returns_unknown_for_hanging_binary() {
        // A script that sleeps forever. Probe should bail out at
        // PROBE_TIMEOUT and return Unknown. To keep the test fast we
        // do not use the full 2-second timeout — instead, we override
        // PROBE_TIMEOUT for this single run by calling the internal
        // helper directly with a 200ms cap.
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_binary(tmp.path(), "hanging-lwe.sh", "sleep 60");

        let out = run_with_timeout(&script, Duration::from_millis(200));
        assert!(
            out.is_none(),
            "hanging binary should produce None from run_with_timeout"
        );
    }
}
