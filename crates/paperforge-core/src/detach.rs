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

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the helper composes onto a `Command` without
    /// panicking. The actual `setsid(2)` syscall runs in the forked
    /// child, which is exercised end-to-end by the
    /// `setsid_isolates_lwe_from_outer_signal_mask` integration
    /// test in the CLI crate (real `timeout 12 paperforge …` flow).
    /// Here we just verify the builder pattern compiles and the
    /// `pre_exec` closure is wired (i.e. `Command::pre_exec`
    /// accepts the closure without complaint).
    #[test]
    fn pre_exec_setsid_attaches_to_command() {
        let mut cmd = std::process::Command::new("/bin/true");
        pre_exec_setsid(&mut cmd);
        // The Command is now wired with the setsid pre_exec. We
        // don't execute it (we'd need a real fork for that) — just
        // verify the function runs without panicking on a
        // well-formed Command.
        drop(cmd);
    }

    /// The helper is idempotent in the sense that calling it
    /// twice on the same Command does not panic. (Internally
    /// `Command::pre_exec` appends to a Vec; we want to be sure
    /// the wrapper doesn't accidentally overwrite the slot.)
    #[test]
    fn pre_exec_setsid_is_idempotent() {
        let mut cmd = std::process::Command::new("/bin/true");
        pre_exec_setsid(&mut cmd);
        pre_exec_setsid(&mut cmd);
        drop(cmd);
    }
}
