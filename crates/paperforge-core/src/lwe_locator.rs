//! LWE binary path resolution.
//!
//! The `linux-wallpaperengine` binary lives in different places depending
//! on how it was installed:
//! - `cargo install --path` puts it in `$CARGO_HOME/bin`, which on most
//!   distros is `$HOME/.local/bin` (Rust's default since 1.71).
//! - The Almamu/louzt clone-and-build convention puts it at
//!   `$HOME/linux-wallpaperengine/build/output/linux-wallpaperengine`.
//! - System packages (Debian, AUR) put it in `/usr/bin/linux-wallpaperengine`.
//!
//! Before this module existed, the backend hardcoded
//! `PathBuf::from("linux-wallpaperengine")` and relied on `$PATH`
//! resolution by `Command::spawn`. That broke whenever the operator
//! ran paperforge as a systemd service without a `$PATH` that included
//! `~/.local/bin` (the canonical case — see the 2026-08-10 incident).
//!
//! `resolve()` checks the 3 well-known absolute paths FIRST, then
//! falls back to a `$PATH` scan, returning `Err(Error::LweBinaryNotFound)`
//! with all paths tried included.

use std::path::PathBuf;
use std::sync::Mutex;

#[cfg(test)]
use std::path::Path;

use crate::error::{Error, Result};

const BINARY_BASENAME: &str = "linux-wallpaperengine";

/// Static cache of the resolved path. Avoids re-scanning PATH on every
/// `LweBackend::with_binary()` call (one per `set()`). Wrapped in a
/// `Mutex` so we can clear it via [`clear_cache`] (used by the future
/// `--rescan` flag).
///
/// We only cache the success path. The `Error` enum does not implement
/// `Clone` (it embeds `anyhow::Error`), so negative results are
/// recomputed on every call — which is OK because missing-binary
/// scenarios are operator-attention events anyway and re-scanning
/// is cheap (<1 ms).
static RESOLVED: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Resolve the LWE binary path. Returns the first existing path, or
/// `Err(Error::LweBinaryNotFound)` listing every path tried.
///
/// Result is cached per-process. Pass [`clear_cache`] to invalidate
/// after the operator installs the binary (e.g. CLI `--rescan` flag).
pub fn resolve() -> Result<PathBuf> {
    {
        let guard = RESOLVED.lock().expect("lwe locator cache mutex poisoned");
        if let Some(cached) = guard.as_ref() {
            return Ok(cached.clone());
        }
    }
    let resolved = compute()?;
    let mut guard = RESOLVED.lock().expect("lwe locator cache mutex poisoned");
    *guard = Some(resolved.clone());
    Ok(resolved)
}

/// Drop the cached resolution. The next `resolve()` call re-scans.
pub fn clear_cache() {
    let mut guard = RESOLVED.lock().expect("lwe locator cache mutex poisoned");
    *guard = None;
}

fn compute() -> Result<PathBuf> {
    let mut paths_tried: Vec<PathBuf> = Vec::with_capacity(16);

    // Tier 1: well-known absolute paths (deterministic, not env-dependent).
    for candidate in well_known_paths() {
        paths_tried.push(candidate.clone());
        if candidate.is_file() {
            tracing::debug!(
                target: "paperforge",
                "lwe locator: found binary at {}",
                candidate.display()
            );
            return Ok(candidate);
        }
    }

    // Tier 2: $PATH scan. Walk each directory and look for the basename.
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_env) {
            let candidate = dir.join(BINARY_BASENAME);
            paths_tried.push(candidate.clone());
            if candidate.is_file() {
                tracing::debug!(
                    target: "paperforge",
                    "lwe locator: found binary on PATH at {}",
                    candidate.display()
                );
                return Ok(candidate);
            }
        }
    }

    tracing::error!(
        target: "paperforge",
        "lwe locator: binary not found in {} candidate paths",
        paths_tried.len()
    );
    Err(Error::LweBinaryNotFound { paths_tried })
}

fn well_known_paths() -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(3);
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        out.push(home.join(".local/bin").join(BINARY_BASENAME));
        out.push(
            home.join("linux-wallpaperengine/build/output")
                .join(BINARY_BASENAME),
        );
    }
    out.push(PathBuf::from("/usr/bin").join(BINARY_BASENAME));
    out
}

/// Pure-function variant for tests — accepts the env vars and PATH
/// string instead of reading from the process environment. The test
/// module calls this with a temp dir + a custom PATH to exercise each
/// tier without polluting the real env.
#[cfg(test)]
fn compute_with(
    home: Option<&Path>,
    path_env: Option<&str>,
    file_exists: &dyn Fn(&Path) -> bool,
) -> Result<PathBuf> {
    let mut paths_tried: Vec<PathBuf> = Vec::with_capacity(16);

    for candidate in well_known_paths_with(home) {
        paths_tried.push(candidate.clone());
        if file_exists(&candidate) {
            return Ok(candidate);
        }
    }

    if let Some(path_env) = path_env {
        for dir in std::env::split_paths(path_env) {
            let candidate = dir.join(BINARY_BASENAME);
            paths_tried.push(candidate.clone());
            if file_exists(&candidate) {
                return Ok(candidate);
            }
        }
    }

    Err(Error::LweBinaryNotFound { paths_tried })
}

#[cfg(test)]
fn well_known_paths_with(home: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(3);
    if let Some(home) = home {
        out.push(home.join(".local/bin").join(BINARY_BASENAME));
        out.push(
            home.join("linux-wallpaperengine/build/output")
                .join(BINARY_BASENAME),
        );
    }
    out.push(PathBuf::from("/usr/bin").join(BINARY_BASENAME));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Unique temp dir per test invocation. Uses an atomic counter
    /// keyed by PID + test index so tests that share a process can't
    /// pollute each other's `~/.local/bin` and `build/output` siblings.
    fn tmp_dir(test_name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "paperforge-lwe-locator-{}-{}-{}",
            std::process::id(),
            test_name,
            n,
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Touch a file to simulate the binary existing.
    fn touch(path: &Path) {
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// Tests use the pure-function variant `compute_with` so we don't
    /// pollute or depend on the real $HOME / $PATH.

    #[test]
    fn resolves_from_home_local_bin_first() {
        let tmp = tmp_dir("local_bin_first");
        let well_known = tmp.join(".local/bin").join(BINARY_BASENAME);
        std::fs::create_dir_all(well_known.parent().unwrap()).unwrap();
        touch(&well_known);

        let result = compute_with(Some(&tmp), None, &|p: &Path| p.exists());
        assert!(
            matches!(result, Ok(ref p) if p == &well_known),
            "got: {result:?}"
        );
    }

    #[test]
    fn resolves_from_home_lwe_build_output_second() {
        let tmp = tmp_dir("build_output_second");
        let well_known = tmp
            .join("linux-wallpaperengine/build/output")
            .join(BINARY_BASENAME);
        std::fs::create_dir_all(well_known.parent().unwrap()).unwrap();
        touch(&well_known);

        let result = compute_with(Some(&tmp), None, &|p: &Path| p.exists());
        assert!(
            matches!(result, Ok(ref p) if p == &well_known),
            "got: {result:?}"
        );
    }

    #[test]
    fn falls_back_to_path_env() {
        let tmp = tmp_dir("path_env");
        let path_dir = tmp.join("path_env_bin");
        std::fs::create_dir_all(&path_dir).unwrap();
        let on_path = path_dir.join(BINARY_BASENAME);
        touch(&on_path);

        // HOME is set to a dir with NO well-known locations populated.
        let empty_home = tmp.join("empty_home");
        std::fs::create_dir_all(&empty_home).unwrap();

        let result = compute_with(
            Some(&empty_home),
            Some(path_dir.to_str().unwrap()),
            &|p: &Path| p.exists(),
        );
        assert!(
            matches!(result, Ok(ref p) if p == &on_path),
            "got: {result:?}"
        );
    }

    #[test]
    fn returns_error_when_nothing_found() {
        let tmp = tmp_dir("nothing_found");
        let empty_home = tmp.join("empty_home");
        std::fs::create_dir_all(&empty_home).unwrap();
        let empty_path_dir = tmp.join("empty_path");
        std::fs::create_dir_all(&empty_path_dir).unwrap();

        let result = compute_with(
            Some(&empty_home),
            Some(empty_path_dir.to_str().unwrap()),
            &|_p: &Path| false,
        );
        match result {
            Err(Error::LweBinaryNotFound { paths_tried }) => {
                assert!(!paths_tried.is_empty(), "paths_tried must be populated");
                // Must include all 3 well-known paths.
                assert!(
                    paths_tried
                        .iter()
                        .any(|p| p.ends_with(".local/bin/linux-wallpaperengine")),
                    "missing ~/.local/bin candidate: {paths_tried:?}"
                );
                assert!(
                    paths_tried
                        .iter()
                        .any(|p| p.ends_with("build/output/linux-wallpaperengine")),
                    "missing build/output candidate: {paths_tried:?}"
                );
                assert!(
                    paths_tried
                        .iter()
                        .any(|p| p.ends_with("/usr/bin/linux-wallpaperengine")),
                    "missing /usr/bin candidate: {paths_tried:?}"
                );
            }
            other => panic!("expected LweBinaryNotFound, got: {other:?}"),
        }
    }

    #[test]
    fn prefers_well_known_over_path_env() {
        let tmp = tmp_dir("well_known_over_path");
        // Both well-known AND path candidate exist; well-known wins.
        let well_known = tmp.join(".local/bin").join(BINARY_BASENAME);
        std::fs::create_dir_all(well_known.parent().unwrap()).unwrap();
        touch(&well_known);

        let path_dir = tmp.join("path_env_bin");
        std::fs::create_dir_all(&path_dir).unwrap();
        touch(&path_dir.join(BINARY_BASENAME));

        let result = compute_with(Some(&tmp), Some(path_dir.to_str().unwrap()), &|p: &Path| {
            p.exists()
        });
        assert!(
            matches!(result, Ok(ref p) if p == &well_known),
            "got: {result:?}"
        );
    }
}
