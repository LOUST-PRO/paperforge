//! LWE orphan process cleanup.
//!
//! Scans `/proc` for processes matching
//! [`BackendKind::process_pattern`] that are NOT tracked by
//! `LweBackend`. These are processes that survived a daemon crash
//! or were spawned outside paperforge's supervision (operator by
//! hand, leftover from a previous daemon lifetime, leftover from an
//! emergency `kill -9` that skipped the daemon's normal cleanup).
//!
//! Without this module, a paperforge restart leaves the previous LWE
//! pool running, consuming GPU + journal space indefinitely. The
//! operator wouldn't know unless they noticed via `nvidia-smi` or
//! `ls /proc/*/cmdline | grep linux-wallpaperengine`.
//!
//! # Design
//!
//! Two pure functions + one async kill:
//!
//! - [`find_orphans_in`] scans a fake `/proc` directory. Pure
//!   function — no real `/proc` access, no subprocess spawn. Used
//!   by tests with `tempfile::tempdir()`.
//! - [`find_orphans`] wraps [`find_orphans_in`] with
//!   `Path::new("/proc")` + the current wallclock. Production entry
//!   point.
//! - [`kill_orphan`] sends SIGTERM via `/usr/bin/kill`, waits the
//!   grace period, escalates to SIGKILL via the same path if the
//!   process is still alive (probed via
//!   [`crate::backend::pid_state_quick`]).
//!
//! No new dependencies: `kill(1)` is a POSIX builtin available on
//! every Linux install paperforge targets.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::backend::{pid_state_quick, BackendKind, BackendState};
use crate::error::{Error, Result};

/// One orphan candidate found in `/proc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanLwe {
    /// Process ID.
    pub pid: i32,
    /// Best-effort reconstruction of `/proc/<pid>/cmdline`. NUL
    /// separators are replaced with spaces so the field is
    /// single-line + grep-friendly. Empty string if the cmdline
    /// file was unreadable.
    pub cmdline: String,
    /// Wallclock seconds since the process was started, derived
    /// from `/proc/<pid>/stat` field 22 (starttime, in jiffies,
    /// divided by `USER_HZ=100`). `0` if the stat file was
    /// unreadable or malformed.
    pub age_secs: u64,
}

/// Pure-function variant of [`find_orphans`]. Scans `proc_dir` for
/// entries whose `cmdline` contains `backend.process_pattern()` and
/// whose numeric pid is NOT in `tracked_pids`. Skips non-numeric
/// directory names (e.g. `self`, `thread-self`, `fs/`).
///
/// Returns a `BTreeMap<pid, OrphanLwe>` so the test assertions can
/// be deterministic (no map-iteration randomness) and so duplicate
/// pid entries in `/proc` are impossible.
pub fn find_orphans_in(
    proc_dir: &Path,
    backend: BackendKind,
    tracked_pids: &[i32],
    now_unix_secs: u64,
) -> BTreeMap<i32, OrphanLwe> {
    let mut out = BTreeMap::new();
    let pattern = backend.process_pattern();
    let entries = match std::fs::read_dir(proc_dir) {
        Ok(e) => e,
        Err(_) => return out, // no /proc — test env or sandbox.
    };
    for entry in entries.flatten() {
        let Some(name_str) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name_str.parse::<i32>() else {
            continue;
        };
        if tracked_pids.contains(&pid) {
            continue;
        }
        let cmdline_path = entry.path().join("cmdline");
        let Ok(mut cmdline_bytes) = std::fs::read(&cmdline_path) else {
            continue;
        };
        // cmdline is NUL-separated; replace NULs with spaces for
        // display + grep.
        for byte in cmdline_bytes.iter_mut() {
            if *byte == 0 {
                *byte = b' ';
            }
        }
        let cmdline = String::from_utf8_lossy(&cmdline_bytes).to_string();
        if !cmdline.contains(pattern) {
            continue;
        }
        // Compute age from /proc/<pid>/stat field 22 (starttime).
        // Returns 0 on any read/parse failure — the orphan is still
        // reported, just without an age.
        let stat_path = entry.path().join("stat");
        let age_secs = std::fs::read_to_string(&stat_path)
            .ok()
            .and_then(|s| parse_starttime_secs(&s))
            .map(|start_secs| now_unix_secs.saturating_sub(start_secs))
            .unwrap_or(0);
        out.insert(
            pid,
            OrphanLwe {
                pid,
                cmdline,
                age_secs,
            },
        );
    }
    out
}

/// Production entry point: scan the real `/proc` for orphans of the
/// given backend. `tracked_pids` is the set of pids the daemon
/// currently supervises — typically `LweBackend::list_per_output_pids()`
/// for the v0.1 per-output path or `[pool.current_pid()]` for the
/// v0.2 shared-pool path.
pub fn find_orphans(backend: BackendKind, tracked_pids: &[i32]) -> BTreeMap<i32, OrphanLwe> {
    let now_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    find_orphans_in(Path::new("/proc"), backend, tracked_pids, now_unix_secs)
}

/// Send SIGTERM via `/usr/bin/kill`, wait `grace`, then SIGKILL if
/// the process is still alive (probed via
/// [`crate::backend::pid_state_quick`]).
///
/// Returns the [`KillOutcome`] describing which signal was the
/// terminal one. Errors propagate from `kill(1)` itself (e.g. EPERM
/// when the daemon doesn't own the pid, ESRCH when the pid was
/// already reaped between scan + kill).
pub async fn kill_orphan(pid: i32, grace: Duration) -> Result<KillOutcome> {
    // SIGTERM first. Use tokio's process wrapper so we don't block
    // the executor on a slow `kill(1)`.
    let term_out = tokio::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .output()
        .await;
    match term_out {
        Ok(o) if !o.status.success() => {
            // Non-zero exit from `kill` means we couldn't deliver
            // the signal (e.g. ESRCH, EPERM). Don't escalate to
            // SIGKILL on a missing pid — that would just produce a
            // noisy second error. Surface the failure verbatim.
            return Err(Error::Other(anyhow::anyhow!(
                "kill -TERM {pid} exited with {} (stderr: {})",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            )));
        }
        Err(e) => {
            return Err(Error::Other(anyhow::anyhow!("kill -TERM {pid}: {e}")));
        }
        _ => {}
    }

    // Wait the grace period before the liveness probe. If the
    // process is well-behaved it will exit cleanly on SIGTERM and we
    // never escalate. If it's stuck (decoder loop frozen, GPU hang),
    // SIGKILL follows.
    tokio::time::sleep(grace).await;

    let alive = match pid_state_quick(pid, BackendKind::LinuxWallpaperEngine) {
        Ok(BackendState::Running) | Ok(BackendState::Paused) => true,
        // NotRunning + read errors both mean "process is gone" —
        // SIGTERM was enough.
        Ok(BackendState::NotRunning) | Err(_) => false,
    };
    if !alive {
        return Ok(KillOutcome::Terminated);
    }

    // Escalate. Same error semantics as the SIGTERM branch.
    let kill_out = tokio::process::Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .output()
        .await;
    match kill_out {
        Ok(o) if !o.status.success() => Err(Error::Other(anyhow::anyhow!(
            "kill -KILL {pid} exited with {} (stderr: {})",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        ))),
        Err(e) => Err(Error::Other(anyhow::anyhow!("kill -KILL {pid}: {e}"))),
        _ => Ok(KillOutcome::Escalated),
    }
}

/// Reap a single orphan end-to-end: kill it (SIGTERM then SIGKILL
/// after `grace`) AND emit a structured tracing event with
/// `pid`, `cmdline`, `age_secs`, `outcome` fields. Returns the
/// [`KillOutcome`].
///
/// This is the operator-queryable entry point. Journald gets
/// `event=lwe_orphan_killed pid=<N> cmdline="..." age_secs=<N>
/// outcome=terminated|escalated` so `journalctl -u paperforge |
/// grep event=lwe_orphan_killed` works without parsing prose.
pub async fn reap_orphan(orphan: &OrphanLwe, grace: Duration) -> Result<KillOutcome> {
    let pid = orphan.pid;
    let cmdline = orphan.cmdline.clone();
    let age_secs = orphan.age_secs;
    let outcome = kill_orphan(pid, grace).await?;
    let outcome_str = match outcome {
        KillOutcome::Terminated => "terminated",
        KillOutcome::Escalated => "escalated",
    };
    tracing::warn!(
        target: "paperforge",
        event = "lwe_orphan_killed",
        pid = pid,
        cmdline = cmdline.as_str(),
        age_secs = age_secs,
        outcome = outcome_str,
        "killed lwe orphan process not tracked by paperforge"
    );
    Ok(outcome)
}

/// Convenience: scan `/proc` and reap every orphan of the given
/// backend. `tracked_pids` is the set of pids the daemon currently
/// supervises — typically the result of
/// `LweBackend::list_per_output_pids()` for the v0.1 path, or
/// `[pool.current_pid()]` for the v0.2 path.
///
/// Returns the number of orphans successfully reaped (Terminated
///   + Escalated). Errors during `kill(1)` are propagated, not
///     silently counted.
pub async fn reap_all_orphans(
    backend: BackendKind,
    tracked_pids: &[i32],
    grace: Duration,
) -> Result<usize> {
    let orphans = find_orphans(backend, tracked_pids);
    let mut count = 0;
    for orphan in orphans.values() {
        reap_orphan(orphan, grace).await?;
        count += 1;
    }
    Ok(count)
}

/// What terminated the orphan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    /// SIGTERM was enough; the process exited cleanly within the
    /// grace period.
    Terminated,
    /// Process ignored SIGTERM; SIGKILL was needed.
    Escalated,
}

/// Parse `/proc/<pid>/stat` and return the process starttime in
/// seconds since boot. Field 22 (1-indexed: starttime in jiffies
/// since boot) — fields are 1-indexed in `man proc` but 0-indexed
/// in the post-`)` split below.
///
/// Returns `None` on malformed input.
fn parse_starttime_secs(stat: &str) -> Option<u64> {
    // /proc/<pid>/stat format: pid (comm) state ppid pgrp session ...
    // The comm field can contain spaces + parens (e.g. process
    // names like "(lwe) (parent)"). Find the LAST `)` to skip past
    // the comm field safely.
    let last_paren = stat.rfind(')')?;
    let after = &stat[last_paren + 1..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    // field index 0 (after ')') = state, 1 = ppid, 2 = pgrp,
    // 3 = session, 4 = tty_nr, 5 = tpgid, 6 = flags, 7 = minflt,
    // 8 = cminflt, 9 = majflt, 10 = cmajflt, 11 = utime, 12 = stime,
    // 13 = cutime, 14 = cstime, 15 = priority, 16 = nice,
    // 17 = num_threads, 18 = itrealvalue, 19 = starttime (this is
    // what we want).
    let jiffies: u64 = fields.get(19)?.parse().ok()?;
    // USER_HZ is 100 on Linux. Hardcoded; if a non-x86 platform
    // changes this, callers see stale ages — acceptable trade-off
    // since paperforge only targets Linux.
    Some(jiffies / 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: make `proc_dir/<name>` exist as a directory
    /// with no contents (used to skip the entry early in
    /// `find_orphans_in`).
    fn touch_dir(dir: &Path, name: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Convenience: make `proc_dir/<name>/cmdline` + `stat` files
    /// with the given content.
    fn touch_proc(dir: &Path, pid_name: &str, cmdline: &[u8], stat: &str) -> std::path::PathBuf {
        let p = dir.join(pid_name);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("cmdline"), cmdline).unwrap();
        std::fs::write(p.join("stat"), stat).unwrap();
        p
    }

    #[test]
    fn find_orphans_in_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let result = find_orphans_in(tmp.path(), BackendKind::LinuxWallpaperEngine, &[], 1000);
        assert!(result.is_empty());
    }

    #[test]
    fn find_orphans_in_finds_untracked_lwe() {
        let tmp = tempfile::tempdir().unwrap();
        // Fake /proc/1234/{cmdline,stat} for an LWE-shaped process.
        // The stat fields are aligned with `/proc/[pid]/stat` man
        // page: state(3), ppid(4), pgrp(5), session(6), tty_nr(7),
        // tpgid(8), flags(9), minflt(10), cminflt(11), majflt(12),
        // cmajflt(13), utime(14), stime(15), cutime(16), cstime(17),
        // priority(18), nice(19), num_threads(20), itrealvalue(21),
        // starttime(22). The starttime field is set to 200 jiffies
        // (= 2 seconds since boot). now_unix_secs = 200 means
        // age = 198 seconds.
        touch_proc(
            tmp.path(),
            "1234",
            b"linux-wallpaperengine\x00--bg\x00123",
            "1234 (lwe) S 1 1234 1234 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 200 0",
        );
        let result = find_orphans_in(tmp.path(), BackendKind::LinuxWallpaperEngine, &[], 200);
        assert!(result.contains_key(&1234), "got: {result:?}");
        let orphan = &result[&1234];
        assert!(orphan.cmdline.contains("linux-wallpaperengine"));
        assert!(orphan.cmdline.contains("--bg"), "NUL must become space");
        assert!(
            !orphan.cmdline.contains('\0'),
            "no NUL should survive cmdline reformat"
        );
        // starttime = 200 jiffies = 2 sec since boot.
        // age = 200 - 2 = 198 secs.
        assert_eq!(orphan.age_secs, 198);
    }

    #[test]
    fn find_orphans_in_skips_tracked_pids() {
        let tmp = tempfile::tempdir().unwrap();
        touch_proc(
            tmp.path(),
            "5678",
            b"linux-wallpaperengine\x00--bg\x00456",
            "5678 (lwe) S 1 5678 5678 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 100 0",
        );
        let result = find_orphans_in(tmp.path(), BackendKind::LinuxWallpaperEngine, &[5678], 200);
        assert!(
            !result.contains_key(&5678),
            "tracked pid should be excluded: {result:?}"
        );
    }

    #[test]
    fn find_orphans_in_skips_non_lwe_processes() {
        let tmp = tempfile::tempdir().unwrap();
        // A non-LWE process — even though we don't track its pid,
        // it must be skipped because its cmdline doesn't contain
        // `linux-wallpaperengine`.
        touch_proc(
            tmp.path(),
            "9999",
            b"some-other-process\x00--foo",
            "9999 (other) S 1 9999 9999 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 100 0",
        );
        let result = find_orphans_in(tmp.path(), BackendKind::LinuxWallpaperEngine, &[], 200);
        assert!(
            !result.contains_key(&9999),
            "non-lwe should be skipped: {result:?}"
        );
    }

    #[test]
    fn find_orphans_in_skips_non_numeric_dir_names() {
        // /proc has self/, thread-self/, fs/, ... entries that are
        // NOT pid directories. `parse::<i32>` should reject them
        // silently.
        let tmp = tempfile::tempdir().unwrap();
        touch_dir(tmp.path(), "self");
        touch_dir(tmp.path(), "thread-self");
        touch_dir(tmp.path(), "fs");
        let result = find_orphans_in(tmp.path(), BackendKind::LinuxWallpaperEngine, &[], 200);
        assert!(result.is_empty(), "got: {result:?}");
    }

    #[test]
    fn find_orphans_in_unreadable_cmdline_is_skipped() {
        // A pid dir without a cmdline file (e.g. kernel thread
        // briefly visible during a race) must not panic. The
        // process is dropped from the candidates.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("4242");
        std::fs::create_dir_all(&p).unwrap();
        // stat only, no cmdline. Field count is aligned with
        // /proc/[pid]/stat (state at index 0, starttime at index
        // 19).
        std::fs::write(
            p.join("stat"),
            "4242 (orphan) S 1 4242 4242 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 100 0",
        )
        .unwrap();
        let result = find_orphans_in(tmp.path(), BackendKind::LinuxWallpaperEngine, &[], 200);
        assert!(result.is_empty(), "got: {result:?}");
    }

    #[test]
    fn find_orphans_in_malformed_stat_yields_zero_age() {
        // If stat is unreadable, the orphan is still reported, just
        // with age_secs=0 so operators know it's "unknown age"
        // rather than seeing a fabricated number.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("7777");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("cmdline"), b"linux-wallpaperengine\x00--bg").unwrap();
        std::fs::write(p.join("stat"), b"not a real stat file").unwrap();
        let result = find_orphans_in(tmp.path(), BackendKind::LinuxWallpaperEngine, &[], 200);
        assert!(result.contains_key(&7777), "got: {result:?}");
        assert_eq!(result[&7777].age_secs, 0);
    }

    #[test]
    fn parse_starttime_secs_basic() {
        // /proc/<pid>/stat with starttime field 22 = 1500 jiffies.
        // 1500 / 100 (USER_HZ) = 15 seconds since boot. Field
        // count is aligned with /proc/[pid]/stat (state at index
        // 0, starttime at index 19).
        let stat = "1234 (lwe) S 1 1234 1234 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 1500 0";
        assert_eq!(parse_starttime_secs(stat), Some(15));
    }

    #[test]
    fn parse_starttime_secs_handles_spaces_in_comm() {
        // Process names with spaces + parens like "(lwe v2 (fork))"
        // must not break the field split. The last `)` anchors the
        // parser correctly.
        let stat = "9999 (lwe v2 (fork)) S 1 9999 9999 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 2500 0";
        assert_eq!(parse_starttime_secs(stat), Some(25));
    }

    #[test]
    fn parse_starttime_secs_returns_none_on_garbage() {
        assert_eq!(parse_starttime_secs("not a stat file"), None);
        assert_eq!(parse_starttime_secs(""), None);
        // Missing closing paren → no `)` to anchor on.
        assert_eq!(parse_starttime_secs("1234 (lwe S 1 ..."), None);
    }
}
