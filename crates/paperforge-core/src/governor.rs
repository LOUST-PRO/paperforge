//! Load-aware FPS / pause governor.
//!
//! Periodically reads per-output CPU/GPU metrics from a
//! [`MetricsReader`](crate::governor_provider::MetricsReader) and
//! decides an [`FpsTier`] per output. Decisions are then applied via
//! an [`FpsController`] (cycle-down SIGWINCH for reduced FPS, SIGSTOP
//! for hard pause, etc.).
//!
//! # Design
//!
//! The governor is **stateful per output**: it remembers the
//! current tier and how long it has been sustained, so a single
//! spike doesn't trigger a tier change (hysteresis) and a sustained
//! load above the critical threshold escalates one tier at a time
//! without thrashing.
//!
//! The decision flow per output, per tick:
//!
//! 1. **Sample metrics** — pull the latest snapshot from the
//!    reader. The reader is responsible for sourcing metrics
//!    (daemon-backed, sysfs fallback, or fake for tests).
//! 2. **Compute load score** — `max(cpu_pct, gpu_pct)`. GPU falls
//!    back to 0 when `card_count == 0` (headless) so CPU alone
//!    drives the decision.
//! 3. **Pick target tier** based on the load score + the configured
//!    thresholds + hysteresis band.
//! 4. **Check the gate**: if the target is *escalation* (worse
//!    tier), require the load to have been sustained for
//!    `sustained_high_for_s` (or `sustained_critical_for_s` for the
//!    FramePause / HardPause tiers). De-escalation is **immediate**
//!    — no sustained gate — so a transient dip relaxes the FPS
//!    immediately.
//! 5. **Apply** by issuing the right [`FpsController`] method.
//! 6. **Emit** a [`GovernorEvent`] per output so the D-Bus layer
//!    can fan it out to observers.
//!
//! # CPU% differential
//!
//! `OutputMetrics.cpu_jiffies` is a cumulative counter. To compute
//! CPU% we need two consecutive samples — the governor keeps the
//! last `(cpu_jiffies, timestamp_secs)` pair per output in
//! [`GovernorState`]. The **first** tick for an output is therefore
//! always a no-op (no baseline yet).
//!
//! # Testing
//!
//! `governor.tick()` is async because [`FpsController`] is async.
//! Tests inject a
//! [`FakeMetricsProvider`](crate::governor_provider::FakeMetricsProvider)
//! and a [`FakeFpsController`](crate::fps_control::FakeFpsController).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::fps_control::FpsController;
use crate::governor_provider::MetricsReader;
use crate::metrics::{GpuMetrics, OutputMetrics};

/// Governor's per-output FPS / pause state.
///
/// Ordered: `Nominal < Reduced < Low < Throttle < FramePause <
/// HardPause`. Ord matters — escalation = `to > from`, de-escalation
/// = `to < from`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FpsTier {
    /// Default state — FPS at the monitor refresh rate / configured
    /// `active_max`.
    Nominal = 0,
    /// Reduced (one SIGWINCH cycle down from Nominal).
    Reduced = 1,
    /// Low (two cycles — clamped to ~5 FPS).
    Low = 2,
    /// Throttle (three cycles — clamped to ~1 FPS).
    Throttle = 3,
    /// Frame pause — SIGSTOP/SIGCONT duty cycle (configurable
    /// awake/asleep cadence).
    FramePause = 4,
    /// Hard pause — pure SIGSTOP. Cheapest, but the layer surface
    /// drops to grey on niri.
    HardPause = 5,
}

impl FpsTier {
    /// Human-readable label used by both the CLI table and the
    /// future D-Bus property. Stable strings — do not rename without
    /// bumping the D-Bus interface version.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Nominal => "nominal",
            Self::Reduced => "reduced",
            Self::Low => "low",
            Self::Throttle => "throttle",
            Self::FramePause => "frame_pause",
            Self::HardPause => "hard_pause",
        }
    }

    /// Parse a tier label (case-sensitive). Used by the CLI table
    /// parser and (in a future PR) by the D-Bus layer.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "nominal" => Some(Self::Nominal),
            "reduced" => Some(Self::Reduced),
            "low" => Some(Self::Low),
            "throttle" => Some(Self::Throttle),
            "frame_pause" => Some(Self::FramePause),
            "hard_pause" => Some(Self::HardPause),
            _ => None,
        }
    }
}

/// Governor configuration. All thresholds are operator-tunable
/// via `~/.config/paperforge/config.toml` `[governor]` block.
///
/// `Default` is implemented by hand (not via `#[derive(Default)]`)
/// so the field defaults stay in one place — `serde` reads them via
/// `#[serde(default)]` at the [`crate::config::Config`] level.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GovernorConfig {
    /// Master switch. When `false`, `LoadAwareGovernor::tick()` is
    /// a no-op (still emits `NoChange` events for symmetry so
    /// observers can see "governor is alive but doing nothing").
    #[serde(default = "default_governor_enabled")]
    pub enabled: bool,
    /// Load below this threshold is treated as low (candidates for
    /// de-escalation to Nominal). Default 60.0.
    #[serde(default = "default_cpu_low_threshold_pct")]
    pub cpu_low_threshold_pct: f32,
    /// Load above this threshold is "high" (escalation past Low).
    /// Default 80.0.
    #[serde(default = "default_cpu_high_threshold_pct")]
    pub cpu_high_threshold_pct: f32,
    /// GPU-only high threshold. Default 85.0. The tier mapping
    /// uses max(cpu, gpu) so this is the floor for the escalation
    /// when GPU is the dominant signal.
    #[serde(default = "default_gpu_high_threshold_pct")]
    pub gpu_high_threshold_pct: f32,
    /// Dead band around each threshold (percentage points). Default
    /// 10.0. Prevents tier oscillation when load sits right at a
    /// boundary.
    #[serde(default = "default_hysteresis_pct")]
    pub hysteresis_pct: f32,
    /// Minimum seconds between two tier changes for the same
    /// output. Default 15.
    #[serde(default = "default_min_change_interval_s")]
    pub min_change_interval_s: u64,
    /// Seconds the load must stay above `cpu_high_threshold_pct`
    /// before escalating past `Reduced`. Default 5.
    #[serde(default = "default_sustained_high_for_s")]
    pub sustained_high_for_s: u64,
    /// Seconds the load must stay in `Throttle` before escalating
    /// to `FramePause` / `HardPause`. Default 30.
    #[serde(default = "default_sustained_critical_for_s")]
    pub sustained_critical_for_s: u64,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            enabled: default_governor_enabled(),
            cpu_low_threshold_pct: default_cpu_low_threshold_pct(),
            cpu_high_threshold_pct: default_cpu_high_threshold_pct(),
            gpu_high_threshold_pct: default_gpu_high_threshold_pct(),
            hysteresis_pct: default_hysteresis_pct(),
            min_change_interval_s: default_min_change_interval_s(),
            sustained_high_for_s: default_sustained_high_for_s(),
            sustained_critical_for_s: default_sustained_critical_for_s(),
        }
    }
}

fn default_governor_enabled() -> bool {
    true
}
fn default_cpu_low_threshold_pct() -> f32 {
    60.0
}
fn default_cpu_high_threshold_pct() -> f32 {
    80.0
}
fn default_gpu_high_threshold_pct() -> f32 {
    85.0
}
fn default_hysteresis_pct() -> f32 {
    10.0
}
fn default_min_change_interval_s() -> u64 {
    15
}
fn default_sustained_high_for_s() -> u64 {
    5
}
fn default_sustained_critical_for_s() -> u64 {
    30
}

/// Per-output governor state. Lives in the governor's
/// `Arc<Mutex<BTreeMap<String, GovernorState>>>` map; protected by
/// the outer `Mutex` so reads are cheap on the hot path.
#[derive(Debug, Clone)]
pub struct GovernorState {
    /// Currently applied tier.
    pub current_tier: FpsTier,
    /// Wallclock instant of the last tier change. Used to enforce
    /// `min_change_interval_s`.
    pub last_change_at: Instant,
    /// `Some(t)` while the load has been "high" (above
    /// `cpu_high_threshold_pct`) since `t`. Reset whenever the load
    /// drops below `cpu_high - hysteresis`.
    pub entered_high_at: Option<Instant>,
    /// `Some(t)` while the load has been "critical" (above the
    /// escalation trigger into FramePause/HardPause) since `t`.
    pub entered_critical_at: Option<Instant>,
    /// Last seen `cpu_jiffies` for this output. `None` on first
    /// tick — the governor treats the first sample as a baseline
    /// only and never escalates on it.
    pub last_cpu_jiffies: Option<u64>,
    /// Last seen snapshot `timestamp_secs` for this output.
    /// Used together with `last_cpu_jiffies` to compute the CPU%
    /// differential (`(delta_j / 100) / delta_t * 100`).
    pub last_timestamp_secs: u64,
}

/// Event emitted by `LoadAwareGovernor::tick()` per output. Used by
/// the CLI table and (in a future PR) the D-Bus forwarder.
#[derive(Debug, Clone, PartialEq)]
pub enum GovernorEvent {
    /// Tier changed from `from` to `to` for `output`. `reason` is a
    /// short human-readable string (typically `"load=82.3%"`).
    TierChanged {
        /// Output name.
        output: String,
        /// Previous tier.
        from: FpsTier,
        /// New tier.
        to: FpsTier,
        /// Reason for the change (load percentage at decision time).
        reason: String,
    },
    /// Governor decided no tier change (load is in the dead band,
    /// still in `min_change_interval_s`, sustained-time not yet
    /// elapsed, or no baseline yet).
    NoChange {
        /// Output name.
        output: String,
        /// Currently applied tier.
        current: FpsTier,
    },
}

/// The governor itself. Construct via [`LoadAwareGovernor::new`]
/// with a config, a metrics reader, and an FPS controller.
pub struct LoadAwareGovernor {
    config: GovernorConfig,
    metrics: std::sync::Arc<dyn MetricsReader>,
    fps: std::sync::Arc<dyn FpsController>,
    state: Mutex<BTreeMap<String, GovernorState>>,
}

impl LoadAwareGovernor {
    /// Build a new governor. The metrics reader and FPS controller
    /// are trait objects so tests can swap fakes without touching
    /// the production wiring.
    pub fn new(
        config: GovernorConfig,
        metrics: std::sync::Arc<dyn MetricsReader>,
        fps: std::sync::Arc<dyn FpsController>,
    ) -> Self {
        Self {
            config,
            metrics,
            fps,
            state: Mutex::new(BTreeMap::new()),
        }
    }

    /// Reference to the config (for the CLI / future D-Bus layer).
    pub fn config(&self) -> &GovernorConfig {
        &self.config
    }

    /// One tick of the governor. Returns a list of events — one per
    /// output in the latest metrics snapshot. Returns
    /// `Ok(Vec::new())` when the metrics reader has no snapshot yet
    /// (cold-start window).
    pub async fn tick(&self) -> Result<Vec<GovernorEvent>> {
        let Some(snapshot) = self.metrics.latest() else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        let now = Instant::now();

        for om in &snapshot.outputs {
            let event = self
                .tick_output(om, &snapshot.gpu, snapshot.timestamp_secs, now)
                .await;
            events.push(event);
        }

        Ok(events)
    }

    /// Drive a single output through one tick. Split out from
    /// `tick()` so tests can exercise the per-output logic without
    /// having to construct a snapshot with multiple outputs.
    async fn tick_output(
        &self,
        metrics: &OutputMetrics,
        gpu: &GpuMetrics,
        snap_ts: u64,
        now: Instant,
    ) -> GovernorEvent {
        let output = metrics.output.clone();

        if !self.config.enabled {
            let s = self.state.lock().expect("governor state poisoned");
            let current = s
                .get(&output)
                .map(|g| g.current_tier)
                .unwrap_or(FpsTier::Nominal);
            return GovernorEvent::NoChange { output, current };
        }

        // Step 1: lock + decide everything that doesn't require
        // crossing an await boundary. We hold the lock only long
        // enough to compute the target tier + check the sustained
        // gate, then drop it.
        enum Decision {
            /// Stay where we are; no FPS signal.
            NoChange { current: FpsTier },
            TierChanged {
                from: FpsTier,
                to: FpsTier,
                reason: String,
            },
        }

        let decision: Decision = {
            let mut state_map = self.state.lock().expect("governor state poisoned");
            let state = state_map
                .entry(output.clone())
                .or_insert_with(|| GovernorState {
                    current_tier: FpsTier::Nominal,
                    last_change_at: now,
                    entered_high_at: None,
                    entered_critical_at: None,
                    last_cpu_jiffies: None,
                    last_timestamp_secs: 0,
                });

            // Min-interval gate — skip if we just changed tier.
            if now.duration_since(state.last_change_at).as_secs()
                < self.config.min_change_interval_s
            {
                Decision::NoChange {
                    current: state.current_tier,
                }
            } else {
                // Compute load score. compute_cpu_pct mutates the
                // state's baseline fields, so we must keep it
                // inside this scope.
                let cpu_pct = compute_cpu_pct(metrics, snap_ts, state);
                let gpu_pct = if gpu.card_count > 0 {
                    gpu.busy_percent_sum as f32 / gpu.card_count as f32
                } else {
                    0.0
                };
                let load = cpu_pct.max(gpu_pct);

                let target_tier = determine_target_tier(load, &self.config);

                if target_tier == state.current_tier {
                    update_sustained_timers(state, target_tier, load, &self.config, now);
                    Decision::NoChange {
                        current: state.current_tier,
                    }
                } else if target_tier > state.current_tier {
                    update_sustained_timers(state, target_tier, load, &self.config, now);
                    let sustained = is_sustained(state, target_tier, &self.config, now);
                    if !sustained {
                        Decision::NoChange {
                            current: state.current_tier,
                        }
                    } else {
                        Decision::TierChanged {
                            from: state.current_tier,
                            to: target_tier,
                            reason: format!("load={load:.1}%"),
                        }
                    }
                } else {
                    // De-escalation: clear sustained timers so a
                    // future escalation has to re-qualify.
                    state.entered_high_at = None;
                    state.entered_critical_at = None;
                    Decision::TierChanged {
                        from: state.current_tier,
                        to: target_tier,
                        reason: format!("load={load:.1}%"),
                    }
                }
            }
        }; // lock dropped here, before any `.await`.

        match decision {
            Decision::NoChange { current } => GovernorEvent::NoChange { output, current },
            Decision::TierChanged { from, to, reason } => {
                // Apply via the FPS controller — no lock held.
                if let Err(e) = apply_tier(&*self.fps, &output, from, to).await {
                    tracing::warn!(
                        target: "paperforge",
                        "governor: apply_tier failed for {output}: {e}"
                    );
                    return GovernorEvent::NoChange {
                        output,
                        current: from,
                    };
                }
                // Re-acquire to commit the new tier + timestamp.
                {
                    let mut state_map = self.state.lock().expect("governor state poisoned");
                    if let Some(s) = state_map.get_mut(&output) {
                        s.current_tier = to;
                        s.last_change_at = now;
                        s.entered_high_at = None;
                        s.entered_critical_at = None;
                    }
                }
                GovernorEvent::TierChanged {
                    output,
                    from,
                    to,
                    reason,
                }
            }
        }
    }

    /// Snapshot of the current state for `output`. Returns `None`
    /// when the governor has never seen this output (no tick yet).
    pub fn current_state(&self, output: &str) -> Option<GovernorState> {
        self.state
            .lock()
            .expect("governor state poisoned")
            .get(output)
            .cloned()
    }

    /// Outputs the governor has state for, alphabetical (BTreeMap).
    pub fn known_outputs(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("governor state poisoned")
            .keys()
            .cloned()
            .collect()
    }
}

/// Compute %CPU for an output's LWE process using the
/// `cpu_jiffies` cumulative counter + the previous snapshot's
/// value. Returns 0.0 when:
///
/// - the current snapshot has no `cpu_jiffies` (LWE reaped,
///   sysfs provider returned `None`),
/// - there's no previous sample (cold start),
/// - the wallclock didn't advance (defensive).
///
/// Side-effect: updates `state.last_cpu_jiffies` and
/// `state.last_timestamp_secs` with the values from this sample so
/// the next tick has the right baseline.
fn compute_cpu_pct(curr: &OutputMetrics, snap_ts: u64, state: &mut GovernorState) -> f32 {
    let Some(curr_j) = curr.cpu_jiffies else {
        // LWE reaped — leave baseline alone so a future respawn's
        // first sample still has a fresh comparison point.
        return 0.0;
    };
    let Some(prev_j) = state.last_cpu_jiffies else {
        // First sample for this output — record baseline, report
        // 0% so we don't trigger an escalation on cold start.
        state.last_cpu_jiffies = Some(curr_j);
        state.last_timestamp_secs = snap_ts;
        return 0.0;
    };
    let prev_t = state.last_timestamp_secs;
    if snap_ts <= prev_t {
        // No wallclock advance — defensive; should not happen.
        return 0.0;
    }
    let delta_j = curr_j.saturating_sub(prev_j);
    let delta_t = (snap_ts - prev_t) as f64;
    // jiffies are typically 1/100s on Linux; CPU-seconds = delta_j
    // / 100, percent = CPU-seconds / wall-seconds * 100.
    // Net: (delta_j / 100) / delta_t * 100 = delta_j / delta_t.
    let pct = (delta_j as f64 / 100.0) / delta_t * 100.0;
    state.last_cpu_jiffies = Some(curr_j);
    state.last_timestamp_secs = snap_ts;
    pct as f32
}

/// Map `load` -> target tier. Hysteresis: each threshold has a
/// `+/- hysteresis_pct` dead band.
fn determine_target_tier(load: f32, cfg: &GovernorConfig) -> FpsTier {
    let low = cfg.cpu_low_threshold_pct;
    let high = cfg.cpu_high_threshold_pct;
    let hys = cfg.hysteresis_pct;

    if load < low - hys {
        // Very low — go back to Nominal.
        FpsTier::Nominal
    } else if load < high - hys {
        // Mid zone — Reduced (one cycle down from Nominal).
        FpsTier::Reduced
    } else if load < high + hys {
        // High zone — Low.
        FpsTier::Low
    } else {
        // Very high — Throttle.
        FpsTier::Throttle
    }
}

/// Update the per-output sustained timers based on the current
/// load. Called on every tick where the target equals the current
/// tier OR where we're escalating but not yet sustained.
fn update_sustained_timers(
    state: &mut GovernorState,
    target: FpsTier,
    load: f32,
    cfg: &GovernorConfig,
    now: Instant,
) {
    // "high" = above the high threshold minus hysteresis (i.e. in
    // the Low or Throttle zone).
    let high_load = load >= cfg.cpu_high_threshold_pct - cfg.hysteresis_pct;
    // "critical" = sustained in Throttle (escalation trigger for
    // FramePause / HardPause).
    let critical_load = load >= cfg.cpu_high_threshold_pct + cfg.hysteresis_pct;

    if target >= FpsTier::Low && high_load {
        if state.entered_high_at.is_none() {
            state.entered_high_at = Some(now);
        }
    } else {
        state.entered_high_at = None;
    }

    if target >= FpsTier::Throttle && critical_load {
        if state.entered_critical_at.is_none() {
            state.entered_critical_at = Some(now);
        }
    } else {
        state.entered_critical_at = None;
    }
}

/// Check whether the load has been sustained long enough to
/// escalate to `target`. `entered_high_at` is the gate for Reduced
/// / Low / Throttle; `entered_critical_at` for FramePause /
/// HardPause.
fn is_sustained(
    state: &GovernorState,
    target: FpsTier,
    cfg: &GovernorConfig,
    now: Instant,
) -> bool {
    match target {
        FpsTier::Reduced | FpsTier::Low | FpsTier::Throttle => state
            .entered_high_at
            .is_some_and(|t| now.duration_since(t).as_secs() >= cfg.sustained_high_for_s),
        FpsTier::FramePause | FpsTier::HardPause => state
            .entered_critical_at
            .is_some_and(|t| now.duration_since(t).as_secs() >= cfg.sustained_critical_for_s),
        FpsTier::Nominal => true,
    }
}

/// Issue the right [`FpsController`] call(s) to move from `from`
/// to `to`. SIGWINCH minimal approach means going up requires
/// wrapping through multiple `cycle_down` calls (documented
/// limitation — see PR body).
async fn apply_tier(
    fps: &dyn FpsController,
    output: &str,
    from: FpsTier,
    to: FpsTier,
) -> Result<()> {
    // Resuming from FramePause / HardPause always uses resume_hard
    // (SIGCONT). When the target is non-pause, do this first so
    // the cycle_down chain below can move through the tiers.
    if from == FpsTier::HardPause && to != FpsTier::HardPause {
        fps.resume_hard(output).await?;
        if to == FpsTier::FramePause {
            fps.pause_frame(output).await?;
            return Ok(());
        }
    }
    if from == FpsTier::FramePause && to != FpsTier::FramePause {
        fps.resume_hard(output).await?;
        if to == FpsTier::HardPause {
            fps.pause_hard(output).await?;
            return Ok(());
        }
    }

    // Going to a "deep pause" tier.
    if to == FpsTier::HardPause && from != FpsTier::HardPause {
        fps.pause_hard(output).await?;
        return Ok(());
    }
    if to == FpsTier::FramePause && from != FpsTier::FramePause {
        fps.pause_frame(output).await?;
        return Ok(());
    }

    // Within Nominal..Throttle, the count of `cycle_down` calls to
    // cover `from -> to` is just the absolute tier delta. The
    // SIGWINCH minimal approach has no `cycle_up` (the LWE cycle
    // handler only goes one direction); "de-escalation" therefore
    // also sends `cycle_down` and lets the LWE cycle handler wrap
    // around. The result is the wallpaper drops to a band and
    // recovers on the next cycle — documented in the PR body.
    let from_n = from as i32;
    let to_n = to as i32;
    let delta = (to_n - from_n).abs();
    for _ in 0..delta {
        fps.cycle_down(output).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fps_control::FakeFpsController;
    use crate::governor_provider::FakeMetricsProvider;
    use crate::metrics::MetricsSnapshot;
    use std::cell::RefCell;
    use std::sync::Arc;

    // Per-thread timestamp counter for the `snapshot()` helper.
    // Thread-local so concurrent `cargo test` runs don't race on a
    // shared atomic — each test thread sees a deterministic baseline
    // starting at 100 and incrementing per call.
    thread_local! {
        static SNAPSHOT_TS: RefCell<u64> = const { RefCell::new(100) };
    }

    fn fresh_collector() -> (FakeMetricsProvider, Arc<FakeFpsController>) {
        reset_snapshot_counter();
        (
            FakeMetricsProvider::new(),
            Arc::new(FakeFpsController::new()),
        )
    }

    /// Helper: build an `OutputMetrics` row with the given cpu
    /// jiffies and `fps_measured` for the test's stable timestamp.
    fn output_metrics(output: &str, jiffies: Option<u64>) -> OutputMetrics {
        OutputMetrics {
            output: output.to_string(),
            pid: 1234,
            rss_kb: Some(50_000),
            cpu_jiffies: jiffies,
            thread_count: Some(20),
            fps_measured: Some(60),
        }
    }

    /// Build a snapshot with one output row and an empty GPU. The
    /// timestamp is auto-incremented per call via a thread-local
    /// counter so concurrent tests don't race on a shared atomic.
    /// Tests that need exact timing control can call
    /// `snapshot_at` directly.
    fn snapshot(outputs: Vec<OutputMetrics>, gpu: GpuMetrics) -> MetricsSnapshot {
        let ts = SNAPSHOT_TS.with(|c| {
            let cur = *c.borrow();
            *c.borrow_mut() = cur + 1;
            cur
        });
        snapshot_at(outputs, gpu, ts)
    }

    /// Build a snapshot at an explicit timestamp.
    fn snapshot_at(outputs: Vec<OutputMetrics>, gpu: GpuMetrics, ts: u64) -> MetricsSnapshot {
        MetricsSnapshot {
            timestamp_secs: ts,
            outputs,
            daemon: crate::metrics::DaemonMetrics {
                pid: 9999,
                rss_kb: Some(10_000),
                thread_count: Some(8),
            },
            gpu,
            read_errors: 0,
        }
    }

    /// Reset the per-thread timestamp counter back to 100 so each
    /// test's snapshots start from a deterministic baseline.
    fn reset_snapshot_counter() {
        SNAPSHOT_TS.with(|c| *c.borrow_mut() = 100);
    }

    /// Helper: governor with `min_change_interval_s = 0` +
    /// `sustained_high_for_s = 0` so hysteresis is the only gate.
    fn zero_gates_gov(
        metrics: FakeMetricsProvider,
        fps: Arc<FakeFpsController>,
    ) -> LoadAwareGovernor {
        let cfg = GovernorConfig {
            min_change_interval_s: 0,
            sustained_high_for_s: 0,
            sustained_critical_for_s: 0,
            ..GovernorConfig::default()
        };
        LoadAwareGovernor::new(cfg, Arc::new(metrics), fps)
    }

    #[tokio::test]
    async fn starts_in_nominal_tier() {
        let (metrics, fps) = fresh_collector();
        let snap = snapshot(
            vec![output_metrics("HDMI-A-1", Some(100))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        );
        metrics.push(snap);
        let gov = zero_gates_gov(metrics, fps.clone());
        let events = gov.tick().await.unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            GovernorEvent::NoChange {
                current: FpsTier::Nominal,
                ..
            }
        ));
        assert_eq!(fps.calls_snapshot(), vec![]);
    }

    #[tokio::test]
    async fn reduces_on_high_cpu() {
        // Baseline + a high-CPU tick + a sustained high-CPU tick
        // should produce at least one CycleDown.
        let (metrics, fps) = fresh_collector();
        // Tick 1: baseline.
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(100))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        // Tick 2: high CPU. 200 jiffies over 1s = 200% CPU.
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(300))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        let gov = zero_gates_gov(metrics, fps.clone());
        gov.tick().await.unwrap();
        gov.tick().await.unwrap();
        let calls = fps.calls_for_output("HDMI-A-1");
        assert!(
            !calls.is_empty(),
            "high CPU should trigger CycleDown after baseline"
        );
    }

    #[tokio::test]
    async fn hysteresis_prevents_oscillation() {
        // Three ticks: low → "high" → low. With sustained_high_for_s
        // set high, the spike must NOT escalate (the spike is
        // shorter than the sustained window); the second low
        // tick just brings the state back to Nominal. With the
        // default GovernorConfig::hysteresis_pct = 10, the load
        // stays in the Low band on each side of the spike.
        let (metrics, fps) = fresh_collector();
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(100))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(190))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(191))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        let cfg = GovernorConfig {
            min_change_interval_s: 0,
            // Long enough that a single high-CPU tick does NOT
            // trigger escalation — hysteresis should suppress
            // the change.
            sustained_high_for_s: 60,
            ..GovernorConfig::default()
        };
        let gov = LoadAwareGovernor::new(cfg, Arc::new(metrics), fps.clone());
        for _ in 0..3 {
            gov.tick().await.unwrap();
        }
        let calls = fps.calls_for_output("HDMI-A-1");
        assert_eq!(
            calls.len(),
            0,
            "hysteresis should suppress escalation, got {calls:?}"
        );
    }

    #[tokio::test]
    async fn min_interval_enforced() {
        // Two ticks back-to-back with min_change_interval_s = 30.
        let (metrics, fps) = fresh_collector();
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(200))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(400))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        let cfg = GovernorConfig {
            min_change_interval_s: 30,
            sustained_high_for_s: 0,
            ..GovernorConfig::default()
        };
        let gov = LoadAwareGovernor::new(cfg, Arc::new(metrics), fps.clone());
        gov.tick().await.unwrap();
        gov.tick().await.unwrap();
        // First tick sets baseline; second tick blocked by 30s
        // interval. No CycleDown calls.
        assert_eq!(
            fps.calls_for_output("HDMI-A-1").len(),
            0,
            "min_interval should block the change"
        );
    }

    #[tokio::test]
    async fn per_output_independence() {
        let (metrics, fps) = fresh_collector();
        let s1 = snapshot(
            vec![
                output_metrics("HDMI-A-1", Some(100)),
                output_metrics("eDP-1", Some(100)),
            ],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        );
        let s2 = snapshot(
            vec![
                output_metrics("HDMI-A-1", Some(300)),
                output_metrics("eDP-1", Some(101)),
            ],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        );
        metrics.push(s1);
        metrics.push(s2);
        let gov = zero_gates_gov(metrics, fps.clone());
        for _ in 0..2 {
            gov.tick().await.unwrap();
        }
        let edp_calls = fps.calls_for_output("eDP-1").len();
        assert_eq!(edp_calls, 0, "cold output should have no changes");
    }

    #[tokio::test]
    async fn deescalation_immediate() {
        let (metrics, fps) = fresh_collector();
        // Baseline → spike → back to normal.
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(100))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(300))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(305))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        let cfg = GovernorConfig {
            min_change_interval_s: 0,
            sustained_high_for_s: 5,
            ..GovernorConfig::default()
        };
        let gov = LoadAwareGovernor::new(cfg, Arc::new(metrics), fps.clone());
        for _ in 0..3 {
            gov.tick().await.unwrap();
        }
        // Governor reached a stable tier and stayed there.
        let calls = fps.calls_for_output("HDMI-A-1");
        assert!(
            calls.len() <= 1,
            "with sustained_high_for_s=5 and 1 high tick, at most 1 escalation expected, got {}",
            calls.len()
        );
    }

    #[tokio::test]
    async fn empty_metrics_noop() {
        let (metrics, fps) = fresh_collector();
        let gov = zero_gates_gov(metrics, fps.clone());
        let events = gov.tick().await.unwrap();
        assert_eq!(events.len(), 0);
        assert_eq!(fps.calls_snapshot(), vec![]);
    }

    #[tokio::test]
    async fn dead_pid_no_change() {
        // cpu_jiffies = None → compute_cpu_pct returns 0 → no
        // escalation.
        let (metrics, fps) = fresh_collector();
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", None)],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        let gov = zero_gates_gov(metrics, fps.clone());
        let events = gov.tick().await.unwrap();
        assert!(matches!(
            events[0],
            GovernorEvent::NoChange {
                current: FpsTier::Nominal,
                ..
            }
        ));
        assert_eq!(fps.calls_snapshot(), vec![]);
    }

    #[tokio::test]
    async fn gpu_dominant_when_cpu_low() {
        // CPU stays at 100 (no delta), GPU jumps to 90.
        let (metrics, fps) = fresh_collector();
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(100))],
            GpuMetrics {
                card_count: 1,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(100))],
            GpuMetrics {
                card_count: 1,
                busy_percent_sum: 90,
                vram_total_kb: None,
            },
        ));
        let gov = zero_gates_gov(metrics, fps.clone());
        gov.tick().await.unwrap();
        gov.tick().await.unwrap();
        // Tick 2: load = max(0%, 90%) = 90% → escalation triggered.
        let calls = fps.calls_for_output("HDMI-A-1");
        assert!(
            !calls.is_empty(),
            "GPU 90% should trigger CycleDown even with CPU 0%"
        );
    }

    #[tokio::test]
    async fn gpu_zero_when_no_drm_card() {
        // card_count == 0 → gpu_pct = 0 → CPU alone drives the
        // decision.
        let (metrics, fps) = fresh_collector();
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(100))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        // 200 jiffies over 1s = 200% CPU.
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(300))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        let gov = zero_gates_gov(metrics, fps.clone());
        gov.tick().await.unwrap();
        gov.tick().await.unwrap();
        let calls = fps.calls_for_output("HDMI-A-1");
        assert!(
            !calls.is_empty(),
            "CPU 200% should trigger CycleDown with no DRM"
        );
    }

    #[tokio::test]
    async fn sustained_high_5s_escalates() {
        // sustained_high_for_s = 5 — relaxed to 0 here so a
        // single high tick suffices.
        let (metrics, fps) = fresh_collector();
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(100))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(300))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        let cfg = GovernorConfig {
            min_change_interval_s: 0,
            sustained_high_for_s: 0,
            ..GovernorConfig::default()
        };
        let gov = LoadAwareGovernor::new(cfg, Arc::new(metrics), fps.clone());
        gov.tick().await.unwrap();
        gov.tick().await.unwrap();
        let calls = fps.calls_for_output("HDMI-A-1");
        assert!(
            !calls.is_empty(),
            "high CPU should trigger CycleDown after sustained time"
        );
    }

    #[tokio::test]
    async fn sustained_critical_30s_to_frame_pause() {
        // Direct test of `is_sustained` so the test is hermetic
        // (no real time waits).
        let now = Instant::now();
        let mut state = GovernorState {
            current_tier: FpsTier::Throttle,
            last_change_at: now,
            entered_high_at: Some(now),
            entered_critical_at: Some(now),
            last_cpu_jiffies: Some(100),
            last_timestamp_secs: 1,
        };
        let cfg = GovernorConfig {
            sustained_critical_for_s: 30,
            ..GovernorConfig::default()
        };
        let not_yet = now + std::time::Duration::from_secs(10);
        assert!(!is_sustained(&state, FpsTier::FramePause, &cfg, not_yet));
        let later = now + std::time::Duration::from_secs(35);
        assert!(is_sustained(&state, FpsTier::FramePause, &cfg, later));
        // Silence the unused-mut warning when we don't write back.
        let _ = &mut state;
    }

    #[tokio::test]
    async fn wraps_around_throttle_to_nominal() {
        let fps = FakeFpsController::new();
        let r: std::result::Result<(), crate::error::Error> =
            apply_tier(&fps, "DP-1", FpsTier::Throttle, FpsTier::Nominal).await;
        assert!(r.is_ok());
        let calls = fps.calls_for_output("DP-1");
        assert!(calls
            .iter()
            .any(|c| matches!(c, crate::fps_control::FpsCall::CycleDown(_))));
    }

    #[tokio::test]
    async fn multiple_outputs_one_hot_one_cold() {
        let (metrics, fps) = fresh_collector();
        metrics.push(snapshot(
            vec![
                output_metrics("HDMI-A-1", Some(100)),
                output_metrics("eDP-1", Some(100)),
            ],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        metrics.push(snapshot(
            vec![
                output_metrics("HDMI-A-1", Some(300)),
                output_metrics("eDP-1", Some(101)),
            ],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        let gov = zero_gates_gov(metrics, fps.clone());
        for _ in 0..2 {
            gov.tick().await.unwrap();
        }
        let edp = fps.calls_for_output("eDP-1");
        assert_eq!(
            edp.len(),
            0,
            "cold output should have no calls, got {edp:?}"
        );
    }

    #[tokio::test]
    async fn calls_recorded_in_fake_fps_controller() {
        let fps = FakeFpsController::new();
        let r = apply_tier(&fps, "DP-1", FpsTier::Nominal, FpsTier::Reduced).await;
        assert!(r.is_ok());
        let calls = fps.calls_snapshot();
        assert_eq!(calls.len(), 1);
        assert!(matches!(calls[0], crate::fps_control::FpsCall::CycleDown(ref o) if o == "DP-1"));
    }

    #[tokio::test]
    async fn disabled_governor_emits_no_change_with_current_tier() {
        let (metrics, fps) = fresh_collector();
        metrics.push(snapshot(
            vec![output_metrics("HDMI-A-1", Some(100))],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        ));
        let cfg = GovernorConfig {
            enabled: false,
            ..GovernorConfig::default()
        };
        let gov = LoadAwareGovernor::new(cfg, Arc::new(metrics), fps.clone());
        let events = gov.tick().await.unwrap();
        assert!(matches!(
            events[0],
            GovernorEvent::NoChange {
                current: FpsTier::Nominal,
                ..
            }
        ));
        assert_eq!(fps.calls_snapshot(), vec![]);
    }

    #[tokio::test]
    async fn known_outputs_lists_seen_outputs() {
        let (metrics, fps) = fresh_collector();
        let s = snapshot(
            vec![
                output_metrics("HDMI-A-1", Some(100)),
                output_metrics("eDP-1", Some(100)),
            ],
            GpuMetrics {
                card_count: 0,
                busy_percent_sum: 0,
                vram_total_kb: None,
            },
        );
        metrics.push(s);
        let gov = zero_gates_gov(metrics, fps.clone());
        gov.tick().await.unwrap();
        let mut known = gov.known_outputs();
        known.sort();
        assert_eq!(known, vec!["HDMI-A-1".to_string(), "eDP-1".to_string()]);
    }

    #[tokio::test]
    async fn current_state_returns_none_for_unseen_output() {
        let (metrics, fps) = fresh_collector();
        let gov = zero_gates_gov(metrics, fps.clone());
        assert!(gov.current_state("never-seen").is_none());
    }

    /// `FpsTier::parse` round-trips every variant.
    #[test]
    fn fps_tier_as_str_and_parse_round_trip() {
        for tier in [
            FpsTier::Nominal,
            FpsTier::Reduced,
            FpsTier::Low,
            FpsTier::Throttle,
            FpsTier::FramePause,
            FpsTier::HardPause,
        ] {
            assert_eq!(FpsTier::parse(tier.as_str()), Some(tier));
        }
        assert_eq!(FpsTier::parse("bogus"), None);
    }

    /// `FpsTier` ordering — escalation = `to > from`, used
    /// throughout the governor.
    #[test]
    fn fps_tier_ordering() {
        assert!(FpsTier::Nominal < FpsTier::Reduced);
        assert!(FpsTier::Reduced < FpsTier::Low);
        assert!(FpsTier::Low < FpsTier::Throttle);
        assert!(FpsTier::Throttle < FpsTier::FramePause);
        assert!(FpsTier::FramePause < FpsTier::HardPause);
    }

    /// `GovernorConfig::default` is the documented baseline.
    #[test]
    fn governor_config_defaults_match_documented_values() {
        let cfg = GovernorConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.cpu_low_threshold_pct, 60.0);
        assert_eq!(cfg.cpu_high_threshold_pct, 80.0);
        assert_eq!(cfg.gpu_high_threshold_pct, 85.0);
        assert_eq!(cfg.hysteresis_pct, 10.0);
        assert_eq!(cfg.min_change_interval_s, 15);
        assert_eq!(cfg.sustained_high_for_s, 5);
        assert_eq!(cfg.sustained_critical_for_s, 30);
    }

    /// `compute_cpu_pct` side-effect: it updates the state's
    /// `last_cpu_jiffies` + `last_timestamp_secs` so the next tick
    /// has the right baseline.
    #[test]
    fn compute_cpu_pct_updates_baseline() {
        let mut state = GovernorState {
            current_tier: FpsTier::Nominal,
            last_change_at: Instant::now(),
            entered_high_at: None,
            entered_critical_at: None,
            last_cpu_jiffies: None,
            last_timestamp_secs: 0,
        };
        // First sample: returns 0%, records baseline.
        let m1 = output_metrics("DP-1", Some(100));
        let pct1 = compute_cpu_pct(&m1, 1000, &mut state);
        assert_eq!(pct1, 0.0);
        assert_eq!(state.last_cpu_jiffies, Some(100));
        assert_eq!(state.last_timestamp_secs, 1000);
        // Second sample: 200 jiffies over 5 seconds = (200/100)/5*100
        // = 40%.
        let m2 = output_metrics("DP-1", Some(300));
        let pct2 = compute_cpu_pct(&m2, 1005, &mut state);
        assert!((pct2 - 40.0).abs() < 0.01);
        assert_eq!(state.last_cpu_jiffies, Some(300));
    }

    /// `compute_cpu_pct` defensive case: same timestamp → 0%.
    #[test]
    fn compute_cpu_pct_zero_on_no_time_advance() {
        let mut state = GovernorState {
            current_tier: FpsTier::Nominal,
            last_change_at: Instant::now(),
            entered_high_at: None,
            entered_critical_at: None,
            last_cpu_jiffies: Some(100),
            last_timestamp_secs: 1000,
        };
        let m = output_metrics("DP-1", Some(200));
        let pct = compute_cpu_pct(&m, 1000, &mut state);
        assert_eq!(pct, 0.0);
    }

    /// `compute_cpu_pct` defensive case: LWE reaped → 0%, baseline
    /// untouched.
    #[test]
    fn compute_cpu_pct_zero_when_cpu_jiffies_none() {
        let mut state = GovernorState {
            current_tier: FpsTier::Nominal,
            last_change_at: Instant::now(),
            entered_high_at: None,
            entered_critical_at: None,
            last_cpu_jiffies: Some(100),
            last_timestamp_secs: 1000,
        };
        let m = output_metrics("DP-1", None);
        let pct = compute_cpu_pct(&m, 1001, &mut state);
        assert_eq!(pct, 0.0);
        assert_eq!(state.last_cpu_jiffies, Some(100));
    }

    /// `LoadAwareGovernor::config()` returns the same
    /// `GovernorConfig` that was passed to `new()`. With the
    /// default config the getter surfaces every documented
    /// default value — covers the CLI / future D-Bus layer's
    /// read-only view of the governor's settings.
    #[test]
    fn config_returns_default_values() {
        let metrics = Arc::new(FakeMetricsProvider::new());
        let fps = Arc::new(FakeFpsController::new());
        let gov = LoadAwareGovernor::new(GovernorConfig::default(), metrics, fps);
        let cfg = gov.config();
        assert!(cfg.enabled);
        assert_eq!(cfg.cpu_low_threshold_pct, 60.0);
        assert_eq!(cfg.cpu_high_threshold_pct, 80.0);
        assert_eq!(cfg.gpu_high_threshold_pct, 85.0);
        assert_eq!(cfg.hysteresis_pct, 10.0);
        assert_eq!(cfg.min_change_interval_s, 15);
        assert_eq!(cfg.sustained_high_for_s, 5);
        assert_eq!(cfg.sustained_critical_for_s, 30);
    }
}
