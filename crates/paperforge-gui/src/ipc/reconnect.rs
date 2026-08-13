//! Exponential reconnect backoff for the D-Bus connection.
//!
//! Schedule (lifted from the operator's convention; matches the
//! pattern `lzt-hub-sync` uses for in-band sync retries):
//!
//! | Attempt | Delay |
//! |---------|-------|
//! | 1       | 5 s   |
//! | 2       | 10 s  |
//! | 3       | 20 s  |
//! | 4..∞    | 30 s  |
//!
//! The first three attempts are fast so a daemon that just started
//! catches up quickly; from attempt 4 onward the delay is capped at
//! 30 s so a long-running GUI doesn't hammer the session bus when
//! the daemon has crashed.
//!
//! Pure math — no I/O, no async. Easy to unit-test.

use std::time::Duration;

/// Backoff schedule for reconnect attempts. `attempt` is 1-based:
/// `next_backoff(1) = 5s`, `next_backoff(2) = 10s`, …
pub fn next_backoff(attempt: u32) -> Duration {
    match attempt {
        0 => Duration::from_secs(5),
        1 => Duration::from_secs(10),
        2 => Duration::from_secs(20),
        _ => Duration::from_secs(30),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_matches_expected_schedule() {
        assert_eq!(next_backoff(0), Duration::from_secs(5));
        assert_eq!(next_backoff(1), Duration::from_secs(10));
        assert_eq!(next_backoff(2), Duration::from_secs(20));
        assert_eq!(next_backoff(3), Duration::from_secs(30));
        assert_eq!(next_backoff(4), Duration::from_secs(30));
        assert_eq!(next_backoff(100), Duration::from_secs(30));
    }

    #[test]
    fn backoff_caps_at_30s() {
        // Property: once we've hit the cap, every subsequent attempt
        // waits the same. This is the regression guard against
        // accidentally doubling the schedule on a refactor.
        let cap = next_backoff(3);
        for n in 3..=50 {
            assert_eq!(next_backoff(n), cap, "attempt {n} must equal cap");
        }
    }

    #[test]
    fn backoff_is_non_decreasing() {
        // Property: the schedule never goes backwards. Important
        // because the GUI's reconnect loop sleeps for this duration
        // and any regression that makes it shorter would risk busy
        // looping the session bus.
        let mut prev = Duration::ZERO;
        for n in 0..=10 {
            let cur = next_backoff(n);
            assert!(
                cur >= prev,
                "attempt {n} went backwards: {prev:?} -> {cur:?}"
            );
            prev = cur;
        }
    }
}
