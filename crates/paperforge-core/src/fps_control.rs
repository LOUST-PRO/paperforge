//! FPS control abstraction for LWE wallpaper processes.
//!
//! Sits between the [`crate::governor`] (load-aware tier decision)
//! and the underlying [`crate::backend::LweBackend`]. The trait is
//! async because LWE's POSIX signal sends are async (the
//! `nix::sys::signal::kill` calls are sync, but they live inside
//! `LweBackendOps` which is async). Keeping the trait async also
//! matches the rest of the daemon's surface so future backends
//! (swww / hyprpaper / mpvpaper) can hook in without contortion.
//!
//! # Tier mapping
//!
//! The governor's [`FpsTier`](crate::governor::FpsTier) enum maps to
//! the trait's method surface:
//!
//! | Tier            | Method      | Notes                          |
//! |-----------------|-------------|--------------------------------|
//! | Nominal         | (none)      | Default — no signal sent       |
//! | Reduced         | `cycle_down`| One SIGWINCH (wraps to lower)  |
//! | Low             | `cycle_down`| Two SIGWINCH cumulative        |
//! | Throttle        | `cycle_down`| Three SIGWINCH cumulative      |
//! | FramePause      | `pause_frame`| SIGSTOP/SIGCONT duty cycle    |
//! | HardPause       | `pause_hard`| Pure SIGSTOP                  |
//!
//! Resuming from FramePause / HardPause uses `resume_hard`
//! (SIGCONT). Wrap-around on the way back up (Throttle → Nominal)
//! is the SIGWINCH minimal-approach caveat documented in the PR
//! body — see `crates/paperforge-core/src/governor.rs` for the
//! full discussion.
//!
//! # Limitations
//!
//! `LweFpsController` is a **stub** in this commit: it documents the
//! intended wiring (`LweBackendOps::pool_pid(output) -> pid` lookup
//! followed by `nix::sys::signal::kill(pid, SIGWINCH)`) but does not
//! actually call the kernel because the operator's LWE build has not
//! shipped SIGWINCH support yet. Once LWE merges the cycle-FPS
//! handler, replace the `todo!()` with the real `kill()` call.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::backend::LweBackend;
use crate::error::Result;

/// Backend-agnostic FPS / pause controller. All methods are
/// idempotent: re-issuing the same command while already in the
/// requested state is a no-op (the daemon relies on this to make
/// repeated `governor.tick()` calls safe).
#[async_trait]
pub trait FpsController: Send + Sync {
    /// Send one SIGWINCH-equivalent "cycle down" signal. After
    /// calling this four times the FPS should wrap back to the
    /// Nominal tier (for the SIGWINCH minimal approach).
    async fn cycle_down(&self, output: &str) -> Result<()>;

    /// Send one SIGWINCH-equivalent "cycle up" signal. The SIGWINCH
    /// minimal approach does NOT support going up — the LWE cycle
    /// handler only goes one direction. Default impl returns Ok(())
    /// so callers (like the governor) can treat cycle_up as a
    /// soft-fail / no-op.
    async fn cycle_up(&self, output: &str) -> Result<()> {
        let _ = output;
        Ok(())
    }

    /// Pure SIGSTOP. The cheapest pause — no frames are rendered.
    /// The layer-shell surface drops to grey on niri because the
    /// compositor stops receiving frames.
    async fn pause_hard(&self, output: &str) -> Result<()>;

    /// SIGCONT for an output that was paused with `pause_hard` or
    /// `pause_frame`. Idempotent: SIGCONT on a running process is a
    /// kernel no-op.
    async fn resume_hard(&self, output: &str) -> Result<()>;

    /// SIGSTOP/SIGCONT duty cycle — see
    /// [`crate::config::PauseMode::Frame`] for the awake/asleep
    /// cadence. The governor uses this for the `FramePause` tier.
    async fn pause_frame(&self, output: &str) -> Result<()>;
}

/// Recorded side-effect from a [`FakeFpsController`]. Each variant
/// carries the output name (and any extra args) so tests can assert
/// on the exact sequence of commands the governor issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FpsCall {
    /// `cycle_down(output)`.
    CycleDown(String),
    /// `cycle_up(output)`.
    CycleUp(String),
    /// `pause_hard(output)`.
    PauseHard(String),
    /// `resume_hard(output)`.
    ResumeHard(String),
    /// `pause_frame(output)`.
    PauseFrame(String),
}

/// Recording fake — used by tests so we can assert on the exact
/// commands the governor issued without touching the kernel.
///
/// `FakeFpsController::new()` starts with an empty queue. Every
/// `cycle_down` / `pause_hard` etc. pushes an entry into
/// `calls_snapshot`. Tests inspect via `calls_snapshot()` or
/// `calls_for_output(name)`.
#[derive(Default, Clone)]
pub struct FakeFpsController {
    inner: Arc<Mutex<VecDeque<FpsCall>>>,
}

impl FakeFpsController {
    /// Build a fresh fake with an empty call log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of all calls recorded so far, oldest first.
    pub fn calls_snapshot(&self) -> Vec<FpsCall> {
        self.inner
            .lock()
            .expect("fps fake poisoned")
            .iter()
            .cloned()
            .collect()
    }

    /// Subset of `calls_snapshot()` filtered to a single output.
    pub fn calls_for_output(&self, output: &str) -> Vec<FpsCall> {
        self.calls_snapshot()
            .into_iter()
            .filter(|c| match c {
                FpsCall::CycleDown(o)
                | FpsCall::CycleUp(o)
                | FpsCall::PauseHard(o)
                | FpsCall::ResumeHard(o)
                | FpsCall::PauseFrame(o) => o == output,
            })
            .collect()
    }
}

#[async_trait]
impl FpsController for FakeFpsController {
    async fn cycle_down(&self, output: &str) -> Result<()> {
        self.inner
            .lock()
            .expect("fps fake poisoned")
            .push_back(FpsCall::CycleDown(output.to_string()));
        Ok(())
    }

    async fn cycle_up(&self, output: &str) -> Result<()> {
        self.inner
            .lock()
            .expect("fps fake poisoned")
            .push_back(FpsCall::CycleUp(output.to_string()));
        Ok(())
    }

    async fn pause_hard(&self, output: &str) -> Result<()> {
        self.inner
            .lock()
            .expect("fps fake poisoned")
            .push_back(FpsCall::PauseHard(output.to_string()));
        Ok(())
    }

    async fn resume_hard(&self, output: &str) -> Result<()> {
        self.inner
            .lock()
            .expect("fps fake poisoned")
            .push_back(FpsCall::ResumeHard(output.to_string()));
        Ok(())
    }

    async fn pause_frame(&self, output: &str) -> Result<()> {
        self.inner
            .lock()
            .expect("fps fake poisoned")
            .push_back(FpsCall::PauseFrame(output.to_string()));
        Ok(())
    }
}

/// Production controller — wraps an [`LweBackend`] and resolves
/// `output -> lwe_pid` via the backend's pool map, then sends the
/// POSIX signal.
///
/// **Status: stub.** The actual `nix::sys::signal::kill()` call is
/// gated behind a `todo!()` because the operator's LWE build has
/// not merged the SIGWINCH handler yet (tracked in the LWE fork's
/// issue tracker). The intended wiring per method:
///
/// | Method        | Signal    |
/// |---------------|-----------|
/// | `cycle_down`  | SIGWINCH  |
/// | `cycle_up`    | SIGWINCH (unused — LWE minimal-cycle is one-way) |
/// | `pause_hard`  | SIGSTOP   |
/// | `resume_hard` | SIGCONT   |
/// | `pause_frame` | SIGSTOP/SIGCONT duty cycle (cadence from `Config.pause`) |
///
/// Once the backend exposes a `pool_pid(output) -> i32` accessor
/// (currently `per_output_pids` is private), the implementation is
/// roughly:
///
/// ```ignore
/// use nix::sys::signal::{kill, Signal};
/// use nix::unistd::Pid;
///
/// let pid = self.backend.pool_pid(output).await?;
/// if pid <= 0 {
///     return Err(Error::Other(anyhow!("no live LWE pid for {output}")));
/// }
/// kill(Pid::from_raw(pid), Signal::SIGSTOP)?;
/// ```
///
/// All methods currently `todo!()` so the governor can wire against
/// the trait surface end-to-end without sending stray signals to
/// LWE. The `#[allow(dead_code)]` keeps `clippy -D warnings` happy
/// for fields the operator will use once LWE merges SIGWINCH and
/// the pid accessor is made public.
#[allow(dead_code)]
pub struct LweFpsController {
    /// Backend handle used to resolve `output -> pid`. Held by Arc
    /// so the controller can live alongside the daemon without
    /// owning a clone of the backend.
    backend: Arc<LweBackend>,
}

impl LweFpsController {
    /// Wrap a backend. Cheap — no I/O.
    pub fn new(backend: Arc<LweBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl FpsController for LweFpsController {
    async fn cycle_down(&self, output: &str) -> Result<()> {
        // Intended:
        //   let pid = self.backend.pool_pid(output).await?;
        //   nix::sys::signal::kill(
        //       nix::unistd::Pid::from_raw(pid),
        //       nix::sys::signal::Signal::SIGWINCH,
        //   )?;
        //
        // Until LWE merges the cycle-FPS handler (tracked in the
        // operator's local LWE fork), we surface the missing
        // capability as a structured error instead of panicking
        // — the daemon and CLI rely on `Result<()>` propagation
        // to make the governor safe to run end-to-end.
        let _ = (self, output);
        Err(crate::error::Error::Other(anyhow::anyhow!(
            "LweFpsController::cycle_down: pending LWE SIGWINCH handler merge \
             (see fork commit `737a230` and `lwe-sigwinch-local-fork-only` memory)"
        )))
    }

    async fn cycle_up(&self, output: &str) -> Result<()> {
        // SIGWINCH minimal approach does not support cycle_up —
        // LWE's cycle handler only goes one direction. Stays a
        // no-op even after the SIGWINCH merge; documented as such
        // at the trait level so callers know to expect Ok(()).
        let _ = (self, output);
        Ok(())
    }

    async fn pause_hard(&self, output: &str) -> Result<()> {
        // Intended: nix::sys::signal::kill(pid, Signal::SIGSTOP)
        // once `pool_pid` accessor is public. For now we can't
        // resolve pids, so we surface a clear error instead of
        // sending a stray SIGSTOP to PID 0 / -1 (which would
        // either no-op or kill the entire process group).
        let _ = (self, output);
        Err(crate::error::Error::Other(anyhow::anyhow!(
            "LweFpsController::pause_hard: pending backend pool_pid accessor \
             (LweBackend::per_output_pids is private)"
        )))
    }

    async fn resume_hard(&self, output: &str) -> Result<()> {
        // Intended: nix::sys::signal::kill(pid, Signal::SIGCONT)
        // once `pool_pid` accessor is public.
        let _ = (self, output);
        Err(crate::error::Error::Other(anyhow::anyhow!(
            "LweFpsController::resume_hard: pending backend pool_pid accessor"
        )))
    }

    async fn pause_frame(&self, output: &str) -> Result<()> {
        // Intended: SIGSTOP/SIGCONT duty cycle driven by
        // `Config.pause.clock_awake_ms` / `clock_asleep_ms`. Lives
        // in `LweBackendOps::pause_soft` today; the controller
        // shim would call it through the backend once accessible.
        let _ = (self, output);
        Err(crate::error::Error::Other(anyhow::anyhow!(
            "LweFpsController::pause_frame: pending backend pool_pid accessor"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default `cycle_up` impl in the trait returns Ok(())
    /// without recording — verify FakeFpsController still records.
    #[tokio::test]
    async fn fake_records_all_calls() {
        let fake = FakeFpsController::new();
        FpsController::cycle_down(&fake, "DP-1").await.unwrap();
        FpsController::cycle_up(&fake, "DP-1").await.unwrap();
        FpsController::pause_hard(&fake, "DP-1").await.unwrap();
        FpsController::resume_hard(&fake, "DP-1").await.unwrap();
        FpsController::pause_frame(&fake, "DP-1").await.unwrap();

        let calls = fake.calls_snapshot();
        assert_eq!(calls.len(), 5);
        assert!(matches!(calls[0], FpsCall::CycleDown(ref o) if o == "DP-1"));
        assert!(matches!(calls[1], FpsCall::CycleUp(ref o) if o == "DP-1"));
        assert!(matches!(calls[2], FpsCall::PauseHard(ref o) if o == "DP-1"));
        assert!(matches!(calls[3], FpsCall::ResumeHard(ref o) if o == "DP-1"));
        assert!(matches!(calls[4], FpsCall::PauseFrame(ref o) if o == "DP-1"));
    }

    /// `calls_for_output` filters by output name — used by governor
    /// tests to assert per-output behaviour in the per_output
    /// independence test.
    #[tokio::test]
    async fn calls_for_output_filters_correctly() {
        let fake = FakeFpsController::new();
        FpsController::cycle_down(&fake, "DP-1").await.unwrap();
        FpsController::cycle_down(&fake, "HDMI-A-1").await.unwrap();
        FpsController::cycle_down(&fake, "DP-1").await.unwrap();

        let dp1 = fake.calls_for_output("DP-1");
        let hdmi = fake.calls_for_output("HDMI-A-1");
        assert_eq!(dp1.len(), 2);
        assert_eq!(hdmi.len(), 1);
    }

    /// The trait's default `cycle_up` is Ok(()). The fake records
    /// regardless. This is mostly a compile-time check that
    /// `dyn FpsController` resolves through both default and
    /// overridden methods.
    #[tokio::test]
    async fn trait_object_default_cycle_up_is_ok() {
        // Build a fake that does NOT override cycle_up — the
        // blanket default kicks in. Since FakeFpsController DOES
        // override it, we use a tiny test-only type here.
        struct DefaultOnly;

        #[async_trait]
        impl FpsController for DefaultOnly {
            async fn cycle_down(&self, _: &str) -> Result<()> {
                Ok(())
            }
            async fn pause_hard(&self, _: &str) -> Result<()> {
                Ok(())
            }
            async fn resume_hard(&self, _: &str) -> Result<()> {
                Ok(())
            }
            async fn pause_frame(&self, _: &str) -> Result<()> {
                Ok(())
            }
        }

        let ctrl: Arc<dyn FpsController> = Arc::new(DefaultOnly);
        assert!(ctrl.cycle_up("ignored").await.is_ok());
    }

    /// `pause_hard` records into the fake's call log — verify it
    /// emits the `PauseHard(output)` variant in isolation, separate
    /// from the combined `fake_records_all_calls` smoke test.
    #[tokio::test]
    async fn fake_records_pause_hard() {
        let fake = FakeFpsController::new();
        FpsController::pause_hard(&fake, "HDMI-A-1").await.unwrap();
        let calls = fake.calls_snapshot();
        assert_eq!(calls, vec![FpsCall::PauseHard("HDMI-A-1".to_string())]);
    }

    /// `resume_hard` records into the fake's call log — verify it
    /// emits the `ResumeHard(output)` variant in isolation.
    #[tokio::test]
    async fn fake_records_resume_hard() {
        let fake = FakeFpsController::new();
        FpsController::resume_hard(&fake, "HDMI-A-1").await.unwrap();
        let calls = fake.calls_snapshot();
        assert_eq!(calls, vec![FpsCall::ResumeHard("HDMI-A-1".to_string())]);
    }

    /// `pause_frame` records into the fake's call log — verify it
    /// emits the `PauseFrame(output)` variant in isolation.
    #[tokio::test]
    async fn fake_records_pause_frame() {
        let fake = FakeFpsController::new();
        FpsController::pause_frame(&fake, "eDP-1").await.unwrap();
        let calls = fake.calls_snapshot();
        assert_eq!(calls, vec![FpsCall::PauseFrame("eDP-1".to_string())]);
    }

    /// `LweFpsController` is a stub today but must construct +
    /// route through `Arc<dyn FpsController>`. The trait surface
    /// has 5 methods; `cycle_up` returns `Ok(())` and the
    /// kernel-touching methods return `Err` with a structured
    /// reason instead of panicking. The test exercises the Ok
    /// path and the construction surface — the structured-error
    /// contract is enforced by `cargo build` (any caller that
    /// ignores the `Result<()>` will trip `must_use`).
    #[tokio::test]
    async fn lwe_fps_controller_constructs_and_cycle_up_works() {
        let backend = Arc::new(LweBackend::with_binary("/bin/true"));
        let ctrl = LweFpsController::new(backend);
        let dyn_ctrl: Arc<dyn FpsController> = Arc::new(ctrl);
        // cycle_up has a graceful Ok(()) path (SIGWINCH minimal
        // approach does not support going up).
        dyn_ctrl.cycle_up("DP-1").await.unwrap();
    }

    /// The four kernel-touching methods on `LweFpsController`
    /// must return a structured `Err` today (not panic). When
    /// LWE merges SIGWINCH + exposes `pool_pid`, these methods
    /// will start sending real signals — at which point these
    /// tests should be updated to assert on the Ok(()) path or
    /// removed entirely.
    #[tokio::test]
    async fn lwe_fps_controller_kernel_methods_return_err() {
        let backend = Arc::new(LweBackend::with_binary("/bin/true"));
        let dyn_ctrl: Arc<dyn FpsController> = Arc::new(LweFpsController::new(backend));

        assert!(dyn_ctrl.cycle_down("DP-1").await.is_err());
        assert!(dyn_ctrl.pause_hard("DP-1").await.is_err());
        assert!(dyn_ctrl.resume_hard("DP-1").await.is_err());
        assert!(dyn_ctrl.pause_frame("DP-1").await.is_err());
    }
}
