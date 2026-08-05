//! Process detach helpers for `LweBackend::set_per_output_with_fps`.
//!
//! Lives in its own module so the crate-wide `#![deny(unsafe_code)]`
//! can be relaxed here. The unsafe is scoped to a single function
//! (`pre_exec_setsid`) that runs in the forked child between
//! `fork()` and `exec()`, a textbook and safe `pre_exec` use case.

/// Run `setsid(2)` in the forked child before exec. Used by
/// `LweBackend::set_per_output_with_fps` so the LWE process is in
/// its own session and process group, isolated from the spawning
/// shell's signal mask. Without this, a `timeout 60 paperforge …`
/// in the operator's shell cascades SIGTERM to the LWE and leaves
/// the monitor grey the moment the timeout fires.
#[allow(unsafe_code)]
pub fn pre_exec_setsid(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if let Err(e) = nix::unistd::setsid() {
                return Err(std::io::Error::from_raw_os_error(e as i32));
            }
            Ok(())
        });
    }
}
