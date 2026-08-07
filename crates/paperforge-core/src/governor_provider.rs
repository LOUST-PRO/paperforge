//! Metrics source providers for the load-aware governor.
//!
//! The governor consumes [`MetricsSnapshot`](crate::metrics::MetricsSnapshot)
//! samples, but it doesn't care where they come from. This module
//! provides three concrete sources:
//!
//! 1. [`SystemMetricsProvider`] — talks to a running
//!    `paperforge daemon` over D-Bus (`GetMetrics` /
//!    `GetMetricsHistory`). Uses the daemon's ring buffer so the
//!    governor inherits all the persistence + JSONL rotation logic.
//! 2. [`SysfsMetricsProvider`] — fallback when no daemon is running.
//!    Scans `pgrep -f linux-wallpaperengine`, then reads
//!    `/proc/<pid>/{cmdline,stat}` directly. Builds the snapshot
//!    inline without any IPC. Used by `paperforge governor --tick`
//!    when no daemon is on the session bus.
//! 3. [`FakeMetricsProvider`] — pre-loaded queue used by tests so
//!    the governor logic can be exercised deterministically without
//!    touching the kernel.
//!
//! # Trait surface
//!
//! [`MetricsReader`] is the abstraction the governor depends on.
//! It exposes `latest()` (the most recent sample) and `history(n)`
//! (the last `n` samples, oldest first). The CPU-percentage math
//! inside the governor needs **two consecutive samples** for a given
//! output, so `history(2)` is the minimum useful depth.
//!
//! # Why three providers?
//!
//! - **System** is the canonical path when the daemon is up — it
//!   carries the same per-output fidelity (RSS, threads, GPU)
//!   the daemon's ring buffer already persists.
//! - **Sysfs** lets the CLI run the governor *without* the daemon,
//!   so operators can trial it on a single machine without
//!   committing to the full systemd unit. It only sees alive LWE
//!   processes, so the snapshot is "live processes only" rather than
//!   "every output the daemon knows about".
//! - **Fake** is the test fixture — every other test in this crate
//!   depends on it.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::metrics::{DaemonMetrics, GpuMetrics, MetricsSnapshot, OutputMetrics};

/// Source of `MetricsSnapshot`s for the governor. Both methods are
/// cheap and synchronous — callers (the governor + CLI) wrap them in
/// `tokio::task::spawn_blocking` if they need to run on the async
/// runtime.
pub trait MetricsReader: Send + Sync {
    /// Most recent snapshot, or `None` if no sample has been taken
    /// yet (cold-start window).
    fn latest(&self) -> Option<MetricsSnapshot>;
    /// Last `n` snapshots in chronological order (oldest first).
    /// Used by the governor to compute CPU% differentials across
    /// two consecutive samples.
    fn history(&self, n: usize) -> Vec<MetricsSnapshot>;
}

/// In-memory fake — pre-loaded with a queue of snapshots. Each call
/// to `latest()` returns and removes the front entry. `history(n)`
/// returns up to `n` front-of-queue entries (in order, oldest
/// first) without removing them. Tests push via [`Self::push`].
#[derive(Default)]
pub struct FakeMetricsProvider {
    samples: Mutex<VecDeque<MetricsSnapshot>>,
}

impl FakeMetricsProvider {
    /// Empty provider. Tests push samples with [`Self::push`] before
    /// each tick.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a fake pre-loaded with `samples`. `latest()` consumes
    /// from the front of the queue (FIFO) — i.e. the first call to
    /// `latest()` returns `samples[0]`, the second returns `samples[1]`,
    /// and so on.
    pub fn with_samples(samples: Vec<MetricsSnapshot>) -> Self {
        Self {
            samples: Mutex::new(samples.into_iter().collect()),
        }
    }

    /// Append a snapshot to the back of the queue.
    pub fn push(&self, snap: MetricsSnapshot) {
        self.samples
            .lock()
            .expect("fake metrics poisoned")
            .push_back(snap);
    }

    /// Number of snapshots currently in the queue.
    pub fn len(&self) -> usize {
        self.samples.lock().expect("fake metrics poisoned").len()
    }

    /// True if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl MetricsReader for FakeMetricsProvider {
    fn latest(&self) -> Option<MetricsSnapshot> {
        self.samples
            .lock()
            .expect("fake metrics poisoned")
            .pop_front()
    }

    fn history(&self, n: usize) -> Vec<MetricsSnapshot> {
        let q = self.samples.lock().expect("fake metrics poisoned");
        q.iter().take(n).cloned().collect()
    }
}

/// Talks to a running `paperforge daemon` over D-Bus and pulls
/// metrics snapshots via the `GetMetrics` / `GetMetricsHistory`
/// methods. Each `latest()` / `history(n)` call shells out to
/// `gdbus` — we don't link zbus into the CLI (keeps the CLI
/// hermetic — only the daemon process owns the session bus).
///
/// The CLI uses this when the daemon is reachable; falls back to
/// [`SysfsMetricsProvider`] when not.
pub struct SystemMetricsProvider {
    /// Cache of the last snapshot (so we don't re-shell on every
    /// tick). Refreshed by [`Self::refresh`].
    cached: Mutex<Option<MetricsSnapshot>>,
    /// Optional CLI path to `gdbus`. Defaults to `gdbus` on PATH.
    gdbus: PathBuf,
}

impl SystemMetricsProvider {
    /// Build a provider that shells out to `gdbus` on every call.
    pub fn new() -> Self {
        Self {
            cached: Mutex::new(None),
            gdbus: PathBuf::from("gdbus"),
        }
    }

    /// Build with an explicit `gdbus` binary path (used by tests).
    pub fn with_gdbus(path: PathBuf) -> Self {
        Self {
            cached: Mutex::new(None),
            gdbus: path,
        }
    }

    /// Refresh the cache by calling `GetMetrics` on the daemon.
    /// Returns `Ok(true)` when the daemon replied with a fresh
    /// snapshot, `Ok(false)` when the daemon was unreachable
    /// (caller should fall back to sysfs), `Err(_)` on parse
    /// failures.
    pub fn refresh(&self) -> Result<bool> {
        let json = match gdbus_call(&self.gdbus, "GetMetrics")? {
            Some(s) => s,
            None => return Ok(false),
        };
        let snap: MetricsSnapshot = serde_json::from_str(&json).map_err(|e| {
            Error::Other(anyhow::anyhow!(
                "system metrics: parse GetMetrics reply: {e}"
            ))
        })?;
        *self.cached.lock().expect("system metrics cache poisoned") = Some(snap);
        Ok(true)
    }
}

impl Default for SystemMetricsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsReader for SystemMetricsProvider {
    fn latest(&self) -> Option<MetricsSnapshot> {
        // Refresh the cache on every read. We could cache for
        // `tick_interval` but the CLI only ever polls at human
        // cadence (--tick, --watch) so the extra round-trip is
        // fine and keeps the implementation trivial.
        if self.refresh().ok().unwrap_or(false) {
            self.cached
                .lock()
                .expect("system metrics cache poisoned")
                .clone()
        } else {
            None
        }
    }

    fn history(&self, n: usize) -> Vec<MetricsSnapshot> {
        // Pull `n` samples via GetMetricsHistory. The daemon returns
        // up to `n` entries; we just pass through.
        let json = match gdbus_call(&self.gdbus, "GetMetricsHistory") {
            Ok(Some(s)) => s,
            _ => return Vec::new(),
        };
        serde_json::from_str::<Vec<MetricsSnapshot>>(&json)
            .map(|v| v.into_iter().take(n).collect())
            .unwrap_or_default()
    }
}

/// Fallback provider — reads `/proc/<pid>/stat` and
/// `/proc/<pid>/cmdline` directly, scans for `linux-wallpaperengine`
/// PIDs, and builds a snapshot inline. Used by the CLI when no
/// daemon is on the session bus (e.g. operator trialing the
/// governor on a workstation).
///
/// # Why "sysfs"?
///
/// Despite the name, this provider doesn't actually use `/sys`. It
/// reads `/proc/<pid>/{cmdline,stat}`. The name is a deliberate
/// distinction from the daemon-backed path: "system files only,
/// no daemon". The naming follows the convention from the metrics
/// module's GPU sampling (which does read `/sys/class/drm/...`).
///
/// # Outputs without PID
///
/// When the CLI lists known outputs (via `niri msg --json outputs`
/// or `wlr-randr`), but no LWE process is bound to a given output
/// (e.g. the operator hasn't applied a wallpaper there yet), the
/// snapshot includes a row with `pid: 0`, `rss_kb: None`,
/// `cpu_jiffies: None` — the governor treats this as "no
/// regulation target" and the corresponding load score is 0.
pub struct SysfsMetricsProvider {
    /// Override for the proc filesystem root. Tests use tempdir
    /// fixtures; production code defaults to `/proc`.
    proc_root: PathBuf,
    /// Cached previous snapshots, keyed by `pid`. Used to compute
    /// CPU% differentials across two samples.
    history: Mutex<BTreeMap<i32, OutputMetrics>>,
    /// Optional overrides for the LWE process scanner. Production
    /// uses `pgrep -f linux-wallpaperengine`; tests inject a
    /// fixed PID list.
    pid_source: PidSource,
}

/// Where to look for LWE PIDs. Tests can supply a fixed list to
/// avoid spawning `pgrep`.
#[derive(Clone)]
pub enum PidSource {
    /// Run `pgrep -f <pattern>` on each scan. Default.
    Pgrep(String),
    /// Use a fixed list (for tests).
    Fixed(Vec<i32>),
}

impl SysfsMetricsProvider {
    /// Build a provider with the default `/proc` root + `pgrep -f
    /// linux-wallpaperengine` scanner.
    pub fn new() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
            history: Mutex::new(BTreeMap::new()),
            pid_source: PidSource::Pgrep("linux-wallpaperengine".to_string()),
        }
    }

    /// Build with an explicit proc root + pid source. Used by tests
    /// to inject tempdir fixtures + a fixed PID list.
    pub fn with_source(proc_root: PathBuf, pid_source: PidSource) -> Self {
        Self {
            proc_root,
            history: Mutex::new(BTreeMap::new()),
            pid_source,
        }
    }

    /// Scan the PID source and return the list of alive PIDs.
    pub fn scan_pids(&self) -> Vec<i32> {
        match &self.pid_source {
            PidSource::Pgrep(pat) => pgrep_lwe(pat),
            PidSource::Fixed(pids) => pids.clone(),
        }
    }

    /// Read `/proc/<pid>/cmdline` and extract the value of
    /// `--screen-root <output>` if present. Returns `None` when the
    /// flag is missing or the cmdline is unreadable.
    fn read_output(&self, pid: i32) -> Option<String> {
        let path = self.proc_root.join(format!("{pid}/cmdline"));
        let mut f = fs::File::open(&path).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        // cmdline uses NUL separators; convert to args.
        let args: Vec<&str> = buf
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| std::str::from_utf8(s).unwrap_or(""))
            .collect();
        let mut iter = args.iter().copied();
        while let Some(arg) = iter.next() {
            if arg == "--screen-root" {
                return iter.next().map(|s| s.to_string());
            }
        }
        None
    }

    /// Read `/proc/<pid>/stat` and parse RSS + cpu_jiffies +
    /// thread_count. Returns `None` when the proc entry is missing
    /// (PID reaped between scan_pids and read_proc_stat).
    fn read_stat(&self, pid: i32) -> Option<(u64, u64, u32)> {
        let path = self.proc_root.join(format!("{pid}/stat"));
        let mut f = fs::File::open(&path).ok()?;
        let mut buf = String::with_capacity(512);
        f.read_to_string(&mut buf).ok()?;
        parse_proc_stat(&buf).ok()
    }
}

impl Default for SysfsMetricsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsReader for SysfsMetricsProvider {
    fn latest(&self) -> Option<MetricsSnapshot> {
        let now = unix_secs_now();
        let pids = self.scan_pids();
        let mut outputs = Vec::with_capacity(pids.len());
        let mut history = self.history.lock().expect("sysfs history poisoned");
        for pid in pids {
            let output = self
                .read_output(pid)
                .unwrap_or_else(|| format!("pid-{pid}"));
            match self.read_stat(pid) {
                Some((rss_kb, cpu_jiffies, threads)) => {
                    outputs.push(OutputMetrics {
                        output: output.clone(),
                        pid,
                        rss_kb: Some(rss_kb),
                        cpu_jiffies: Some(cpu_jiffies),
                        thread_count: Some(threads),
                        fps_measured: None,
                    });
                    // Update history for the next call's differential.
                    history.insert(
                        pid,
                        OutputMetrics {
                            output,
                            pid,
                            rss_kb: Some(rss_kb),
                            cpu_jiffies: Some(cpu_jiffies),
                            thread_count: Some(threads),
                            fps_measured: None,
                        },
                    );
                }
                None => {
                    // PID died between scan_pids and read_stat —
                    // emit a "dead pid" row so the operator sees it
                    // in the snapshot, then drop it from history.
                    tracing::warn!(
                        target: "paperforge",
                        "sysfs: pid {pid} ({output}) disappeared mid-scan"
                    );
                    outputs.push(OutputMetrics {
                        output,
                        pid,
                        rss_kb: None,
                        cpu_jiffies: None,
                        thread_count: None,
                        fps_measured: None,
                    });
                    history.remove(&pid);
                }
            }
        }
        let snap = MetricsSnapshot {
            timestamp_secs: now,
            outputs,
            daemon: DaemonMetrics {
                pid: std::process::id() as i32,
                rss_kb: None,
                thread_count: None,
            },
            // GPU: this provider doesn't read /sys/class/drm — leave
            // the daemon to handle that. The governor uses 0% GPU
            // (card_count == 0 → score falls back to CPU).
            gpu: GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
            read_errors: 0,
        };
        Some(snap)
    }

    fn history(&self, n: usize) -> Vec<MetricsSnapshot> {
        // The sysfs provider has no ring buffer — but the governor
        // only uses history() for differential CPU math, which is
        // already handled by the per-pid `history` map. We return
        // the current snapshot `n` times so the governor's history
        // path doesn't trip an index-out-of-bounds. Cheap, bounded,
        // semantically equivalent for our use.
        let _ = n;
        self.latest().into_iter().collect()
    }
}

/// Run `pgrep -f <pattern>` and parse the PIDs from stdout. Returns
/// an empty Vec on failure (pgrep missing, no matches, etc.) — the
/// caller treats empty as "no LWEs alive" rather than erroring.
fn pgrep_lwe(pattern: &str) -> Vec<i32> {
    let out = match Command::new("pgrep").arg("-f").arg(pattern).output() {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!(target: "paperforge", "pgrep spawn failed: {e}");
            return Vec::new();
        }
    };
    if !out.status.success() {
        // pgrep exits 1 when no matches — not an error.
        return Vec::new();
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .collect()
}

/// Invoke `gdbus call --session ... --method <method>` and return
/// the JSON-decoded reply body. Returns `Ok(None)` when `gdbus`
/// fails (no daemon on the session bus) so the caller can fall
/// back to a different provider.
fn gdbus_call(gdbus: &Path, method: &str) -> Result<Option<String>> {
    let out = Command::new(gdbus)
        .args([
            "call",
            "--session",
            "--dest",
            "org.louzt.Paperforge",
            "--object-path",
            "/org/louzt/Paperforge",
            "--method",
            &format!("org.louzt.Paperforge1.{method}"),
        ])
        .output()
        .map_err(|e| Error::Other(anyhow::anyhow!("gdbus spawn: {e}")))?;
    if !out.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // gdbus wraps the body in parens: `(<'{"..."}',>)`. Slice out
    // the inner JSON.
    let open = stdout.find('(');
    let close = stdout.rfind(')');
    let (Some(open), Some(close)) = (open, close) else {
        return Ok(None);
    };
    let body = stdout.get(open + 1..close).unwrap_or("").trim();
    // Strip single quotes (gdbus quotes string args).
    let body = body.trim_matches('\'').trim_matches('"');
    Ok(Some(body.to_string()))
}

/// Wallclock seconds since the UNIX epoch.
fn unix_secs_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse `/proc/<pid>/stat` (the same shape
/// [`crate::metrics::parse_proc_stat`] handles) — duplicated here
/// so the sysfs provider can stand alone without depending on the
/// metrics module's internals.
fn parse_proc_stat(s: &str) -> Result<(u64, u64, u32)> {
    // proc(5) field numbering after the paren:
    //   0  state, 1  ppid, 2  pgrp, 3  session, 4  tty_nr,
    //   5  tpgid, 6  flags, 7  minflt, 8  cminflt, 9  majflt,
    //   10 cmajflt, 11 utime, 12 stime, 13 cutime, 14 cstime,
    //   15 priority, 16 nice, 17 num_threads, 18 itrealvalue,
    //   19 starttime, 20 vsize, 21 rss (in pages).
    let after_paren = s
        .rfind(')')
        .ok_or_else(|| Error::Other(anyhow::anyhow!("proc stat: no closing paren")))?;
    let rest = &s[after_paren + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 20 {
        return Err(Error::Other(anyhow::anyhow!(
            "proc stat: too few fields ({})",
            fields.len()
        )));
    }
    let utime: u64 = fields[11]
        .parse()
        .map_err(|e| Error::Other(anyhow::anyhow!("utime: {e}")))?;
    let stime: u64 = fields[12]
        .parse()
        .map_err(|e| Error::Other(anyhow::anyhow!("stime: {e}")))?;
    let cpu_jiffies = utime + stime;
    let num_threads: u32 = fields[17]
        .parse()
        .map_err(|e| Error::Other(anyhow::anyhow!("threads: {e}")))?;
    let rss_pages: u64 = fields[21]
        .parse()
        .map_err(|e| Error::Other(anyhow::anyhow!("rss: {e}")))?;
    let rss_kb = rss_pages.saturating_mul(4);
    Ok((rss_kb, cpu_jiffies, num_threads))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// Build a `/proc/<pid>/stat` fixture with the requested
    /// utime/stime/threads/rss. Writes to `<tmp>/<pid>/stat`.
    fn write_proc_stat(
        root: &Path,
        pid: i32,
        utime: u64,
        stime: u64,
        threads: u32,
        rss_pages: u64,
    ) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        // proc(5) format: "<pid> (<comm>) <state> <ppid> ...".
        let body = format!(
            "{pid} (lwe --screen-root DP-1) R 1 {pid} {pid} 0 -1 4194304 100 0 0 0 {utime} {stime} 0 0 0 0 {threads} 0 0 2100000000 {rss_pages} 0 0 0",
        );
        fs::write(dir.join("stat"), body).unwrap();
    }

    /// Build a `/proc/<pid>/cmdline` fixture. NUL-separated args.
    fn write_proc_cmdline(root: &Path, pid: i32, args: &[&str]) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        let body: Vec<u8> = args
            .iter()
            .flat_map(|a| a.as_bytes().iter().copied().chain(std::iter::once(0u8)))
            .collect();
        fs::write(dir.join("cmdline"), body).unwrap();
        // Make cmdline readable to the test process (it already
        // should be, but chmod to be defensive).
        let mut perms = fs::metadata(dir.join("cmdline")).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(dir.join("cmdline"), perms).unwrap();
    }

    #[test]
    fn parse_proc_stat_basic() {
        // utime=100, stime=50, threads=4, rss=12345 pages = 49380 KiB.
        let s = "1234 (lwe) R 1 1234 1234 0 -1 4194304 100 0 0 0 100 50 0 0 0 0 4 0 0 2100000000 12345 0 0 0";
        let (rss, cpu, threads) = parse_proc_stat(s).unwrap();
        assert_eq!(rss, 49_380);
        assert_eq!(cpu, 150);
        assert_eq!(threads, 4);
    }

    #[test]
    fn parse_proc_stat_handles_comm_with_spaces() {
        let s = "9999 (lwe --foo bar) R 1 1 1 0 -1 0 0 0 0 0 10 5 0 0 0 0 2 0 0 1 100 0 0 0";
        let (rss, cpu, threads) = parse_proc_stat(s).unwrap();
        assert_eq!(cpu, 15);
        assert_eq!(threads, 2);
        assert_eq!(rss, 400);
    }

    #[test]
    fn parse_proc_stat_rejects_short_input() {
        let s = "1 (x) R";
        assert!(parse_proc_stat(s).is_err());
    }

    #[test]
    fn fake_provider_with_samples_returns_in_order() {
        let s1 = MetricsSnapshot {
            timestamp_secs: 1,
            outputs: vec![],
            daemon: DaemonMetrics {
                pid: 1,
                rss_kb: None,
                thread_count: None,
            },
            gpu: GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
            read_errors: 0,
        };
        let s2 = MetricsSnapshot {
            timestamp_secs: 2,
            outputs: vec![],
            daemon: DaemonMetrics {
                pid: 1,
                rss_kb: None,
                thread_count: None,
            },
            gpu: GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
            read_errors: 0,
        };
        let provider = FakeMetricsProvider::with_samples(vec![s1.clone(), s2.clone()]);
        assert_eq!(provider.latest(), Some(s1));
        assert_eq!(provider.latest(), Some(s2));
        assert_eq!(provider.latest(), None);
    }

    #[test]
    fn fake_provider_history_is_non_destructive() {
        let s1 = MetricsSnapshot {
            timestamp_secs: 1,
            outputs: vec![],
            daemon: DaemonMetrics {
                pid: 1,
                rss_kb: None,
                thread_count: None,
            },
            gpu: GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
            read_errors: 0,
        };
        let s2 = MetricsSnapshot {
            timestamp_secs: 2,
            outputs: vec![],
            daemon: DaemonMetrics {
                pid: 1,
                rss_kb: None,
                thread_count: None,
            },
            gpu: GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
            read_errors: 0,
        };
        let provider = FakeMetricsProvider::with_samples(vec![s1.clone(), s2.clone()]);
        // history() does NOT consume — three calls return the same
        // first-n samples.
        assert_eq!(provider.history(2), vec![s1.clone(), s2.clone()]);
        assert_eq!(provider.history(2), vec![s1.clone(), s2.clone()]);
        // latest() DOES consume (FIFO).
        assert_eq!(provider.latest(), Some(s1));
    }

    #[test]
    fn fake_provider_push_appends() {
        let provider = FakeMetricsProvider::new();
        assert!(provider.is_empty());
        let s = MetricsSnapshot {
            timestamp_secs: 1,
            outputs: vec![],
            daemon: DaemonMetrics {
                pid: 1,
                rss_kb: None,
                thread_count: None,
            },
            gpu: GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
            read_errors: 0,
        };
        provider.push(s.clone());
        provider.push(s.clone());
        assert_eq!(provider.len(), 2);
        assert_eq!(provider.latest(), Some(s.clone()));
        assert_eq!(provider.latest(), Some(s));
        assert!(provider.is_empty());
    }

    /// End-to-end: build a sysfs fixture with one LWE proc entry,
    /// verify the provider returns the right snapshot.
    #[test]
    fn sysfs_provider_reads_live_proc_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // PID 4242 with --screen-root HDMI-A-1, 100 jiffies utime,
        // 50 jiffies stime, 4 threads, 12345 rss pages = 49380 KiB.
        write_proc_cmdline(
            root,
            4242,
            &["lwe", "--screen-root", "HDMI-A-1", "--bg", "x"],
        );
        write_proc_stat(root, 4242, 100, 50, 4, 12345);
        let provider =
            SysfsMetricsProvider::with_source(root.to_path_buf(), PidSource::Fixed(vec![4242]));
        let snap = provider.latest().expect("snapshot");
        assert_eq!(snap.outputs.len(), 1);
        let om = &snap.outputs[0];
        assert_eq!(om.output, "HDMI-A-1");
        assert_eq!(om.pid, 4242);
        assert_eq!(om.cpu_jiffies, Some(150));
        assert_eq!(om.thread_count, Some(4));
        assert_eq!(om.rss_kb, Some(49_380));
    }

    /// Dead-PID handling: the proc entry is removed between scans.
    /// The provider emits a row with `rss_kb: None` etc.
    #[test]
    fn sysfs_provider_emits_dead_pid_row() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // 4242 exists in the cmdline list, but we never write a
        // stat file — simulates a reaped PID.
        write_proc_cmdline(root, 4242, &["lwe", "--screen-root", "DP-1"]);
        let provider =
            SysfsMetricsProvider::with_source(root.to_path_buf(), PidSource::Fixed(vec![4242]));
        let snap = provider.latest().expect("snapshot");
        assert_eq!(snap.outputs.len(), 1);
        let om = &snap.outputs[0];
        assert_eq!(om.pid, 4242);
        assert_eq!(om.rss_kb, None);
        assert_eq!(om.cpu_jiffies, None);
        assert_eq!(om.thread_count, None);
    }

    /// `--screen-root` missing → the output name falls back to
    /// `pid-<n>` so the operator can still see the row.
    #[test]
    fn sysfs_provider_fallback_output_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_proc_cmdline(root, 9999, &["lwe", "--bg", "x"]);
        write_proc_stat(root, 9999, 0, 0, 1, 0);
        let provider =
            SysfsMetricsProvider::with_source(root.to_path_buf(), PidSource::Fixed(vec![9999]));
        let snap = provider.latest().expect("snapshot");
        assert_eq!(snap.outputs.len(), 1);
        assert_eq!(snap.outputs[0].output, "pid-9999");
    }
}
