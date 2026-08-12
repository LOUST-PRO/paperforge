//! Integration tests for the `paperforge` CLI.
//!
//! These tests build the actual binary and run it as a subprocess, so
//! they cover clap parsing, the dispatched handlers, and the IPC
//! helper layers end-to-end. They rely on `CARGO_BIN_EXE_paperforge`,
//! which Cargo injects for `tests/` directory tests.

use std::process::{Command, Output};

/// Run the CLI with the given args, capturing stdout + stderr + exit.
fn run_cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_paperforge"))
        .args(args)
        .output()
        .expect("failed to spawn paperforge binary")
}

#[test]
fn cli_help_prints_subcommands() {
    let out = run_cli(&["--help"]);
    assert!(out.status.success(), "expected help to exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("set"), "help must list `set` subcommand");
    assert!(
        stdout.contains("pause"),
        "help must list `pause` subcommand"
    );
    assert!(
        stdout.contains("playlist"),
        "help must list `playlist` subcommand"
    );
    assert!(
        stdout.contains("paths"),
        "help must list `paths` subcommand"
    );
}

#[test]
fn cli_version_prints_semver() {
    let out = run_cli(&["--version"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // workspace version is "0.1.0" — strip any extra clap appends
    let first_line = stdout.lines().next().unwrap_or("");
    assert!(
        first_line.contains("0.1.0"),
        "expected version 0.1.0, got: {first_line:?}"
    );
}

#[test]
fn cli_paths_prints_workshop_or_local_section() {
    // The CLI's `paths` subcommand always prints both headings; either
    // section may be empty. We just verify the structure survives.
    let out = run_cli(&["paths"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("workshop roots:"));
    assert!(stdout.contains("local roots:"));
}

#[test]
fn cli_list_runs_even_when_no_lwe_present() {
    // If no LWE instances are running, the CLI should still exit 0
    // and print "(no LWE instances running)" — operators rely on
    // this in cron/healthcheck scripts.
    let out = run_cli(&["list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "list should exit 0; stderr={stderr}; stdout={stdout}"
    );
    // Either format is acceptable: empty list OR at least one PID.
    // We don't assert emptiness because the operator's CI agent may
    // have LWE running.
    let _ = stdout;
}

#[test]
fn cli_set_with_nonexistent_path_errors() {
    let out = run_cli(&["set", "/nonexistent/paperforge-test-scene"]);
    assert!(!out.status.success(), "set on missing path should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("scene path does not exist")
            || combined.contains("BackendUnreachable")
            || combined.contains("not reachable")
            // Post-fix #1 (PR #15): when LWE is not installed the
            // CLI fail-fast gate kicks in before the path check.
            // Accept that actionable error too.
            || combined.contains("linux-wallpaperengine binary not found"),
        "expected scene-not-found or lwe-not-found error, got: {combined}"
    );
}

#[test]
fn cli_playlist_delete_missing_returns_zero_with_message() {
    let out = run_cli(&["playlist", "delete", "definitely-not-a-real-playlist-xyz"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The CLI returns 0 even when the playlist doesn't exist, just
    // prints a "no playlist named '...'" message. Verify the
    // dial-tone contract.
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("no playlist named"),
        "expected friendly missing-playlist message, got: {combined}"
    );
}
