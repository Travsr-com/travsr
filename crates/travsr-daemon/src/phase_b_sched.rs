//! Debounced, single-flight scheduler for background Phase B re-runs (#318 O3).
//!
//! `reindex_files` deliberately skips Phase B on every commit (PERF-002): a full
//! semantic (SCIP) re-run is expensive and would block the commit hot path. The
//! consequence is that `phase_b_commit` froze at init time, so O5 staleness
//! reporting only ever grew. This scheduler closes that gap: a commit *arms* a
//! re-run, a short debounce window coalesces bursts of commits into one run, and
//! a single-flight guard ensures at most one background pass runs at a time.
//!
//! The scheduler only tracks *timing and mutual exclusion*; the actual Phase B
//! work (and advancing `phase_b_commit`) lives in the daemon so it can hold the
//! store lock only for the final write batch. Package-scoped incremental re-runs
//! — the ideal — are gated on SCIP tools gaining sub-path invocation support; a
//! full re-run is the honest fallback until then, which is exactly why it is
//! debounced and single-flighted rather than run inline.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Coordinates background Phase B refreshes across the daemon's tasks.
pub struct PhaseBScheduler {
    /// `Some(deadline)` while a re-run is pending; `None` when idle or already
    /// claimed by an in-flight run. Re-arming pushes the deadline out so a burst
    /// of commits settles into one run.
    dirty: Mutex<Option<Instant>>,
    /// Single-flight guard: at most one background Phase B run at a time.
    running: AtomicBool,
    debounce: Duration,
    /// Consecutive all-crash failures. After MAX_FAILURES the scheduler backs off
    /// until the daemon is restarted, preventing an infinite crash-retry loop when
    /// all Phase B sidecars are broken (e.g. tools not installed).
    consecutive_failures: AtomicU32,
}

impl PhaseBScheduler {
    const MAX_FAILURES: u32 = 3;
}

impl PhaseBScheduler {
    pub fn new(debounce: Duration) -> Self {
        Self {
            dirty: Mutex::new(None),
            running: AtomicBool::new(false),
            debounce,
            consecutive_failures: AtomicU32::new(0),
        }
    }

    /// Reset the consecutive-failure counter. Called by `travsr daemon restart`
    /// so a user can recover without restarting the whole machine.
    #[allow(dead_code)]
    pub fn reset_failures(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// Arm (or re-arm) a re-run for `debounce` from now. Called after a
    /// commit-triggered reindex. Re-arming intentionally pushes the deadline
    /// forward so that a rapid series of commits triggers a single run once the
    /// activity settles.
    pub fn mark_dirty(&self) {
        let mut d = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        *d = Some(Instant::now() + self.debounce);
    }

    /// Arm for an immediate run with no debounce. Used by `phase_b_tick` on
    /// daemon startup when Phase B is pending from a deferred init. Unlike
    /// `mark_dirty`, this does NOT override an existing armed deadline — so it
    /// never interferes with a commit-triggered debounce window that is already
    /// counting down.
    pub fn arm_immediate(&self) {
        let mut d = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        if d.is_none() {
            // Set deadline in the past so the next try_claim() fires immediately.
            *d = Some(Instant::now());
        }
    }

    /// If a run is due (armed and past its deadline) and none is in flight,
    /// atomically claim the run slot and consume the pending mark. Returns
    /// `true` iff the caller should start a background run; the caller MUST call
    /// [`finish`](Self::finish) when that run completes.
    ///
    /// `now` is injectable for deterministic tests; production callers pass
    /// `Instant::now()` via [`try_claim`](Self::try_claim).
    pub fn try_claim_at(&self, now: Instant) -> bool {
        // Back off after repeated all-crash runs to avoid hammering broken tools.
        if self.consecutive_failures.load(Ordering::Relaxed) >= Self::MAX_FAILURES {
            return false;
        }
        let mut d = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        let due = matches!(*d, Some(deadline) if now >= deadline);
        if !due {
            return false;
        }
        // Claim the single-flight slot before consuming the mark so a concurrent
        // claimer cannot also start a run.
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            *d = None;
            true
        } else {
            false
        }
    }

    /// Convenience wrapper over [`try_claim_at`](Self::try_claim_at) using the
    /// real clock.
    pub fn try_claim(&self) -> bool {
        self.try_claim_at(Instant::now())
    }

    /// Release the single-flight slot when a background run finishes.
    /// Use [`finish_with_result`] when the caller can report success/failure.
    #[allow(dead_code)]
    pub fn finish(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// Release the slot and record whether the run produced any output.
    /// `succeeded` should be `true` when at least one language ran without
    /// crashing — even partial success resets the failure counter.
    pub fn finish_with_result(&self, succeeded: bool) {
        if succeeded {
            self.consecutive_failures.store(0, Ordering::Relaxed);
        } else {
            self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        }
        self.running.store(false, Ordering::Release);
    }

    /// Number of consecutive all-crash failures. Exposed for `travsr status`.
    #[allow(dead_code)]
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    /// Whether a Phase B run is currently in flight.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Whether a Phase B run is armed and waiting for its debounce window.
    pub fn is_pending(&self) -> bool {
        self.dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_scheduler_never_claims() {
        let s = PhaseBScheduler::new(Duration::from_secs(30));
        assert!(!s.try_claim(), "nothing armed → no run");
    }

    #[test]
    fn debounce_defers_until_deadline() {
        let s = PhaseBScheduler::new(Duration::from_secs(30));
        let t0 = Instant::now();
        s.mark_dirty();
        // Before the deadline: not due.
        assert!(!s.try_claim_at(t0 + Duration::from_secs(1)));
        // After the deadline: due, claims once.
        assert!(s.try_claim_at(t0 + Duration::from_secs(31)));
    }

    #[test]
    fn single_flight_until_finish() {
        let s = PhaseBScheduler::new(Duration::from_secs(0));
        s.mark_dirty();
        assert!(s.try_claim(), "first claim succeeds");
        // A new commit arrives while the run is in flight.
        s.mark_dirty();
        assert!(!s.try_claim(), "no overlapping run while one is in flight");
        // Run finishes; the re-armed mark is now claimable.
        s.finish();
        assert!(
            s.try_claim(),
            "re-armed mark runs after the previous finishes"
        );
    }

    #[test]
    fn claim_consumes_the_mark() {
        let s = PhaseBScheduler::new(Duration::from_secs(0));
        s.mark_dirty();
        assert!(s.try_claim());
        s.finish();
        // No new mark → the consumed one does not re-fire.
        assert!(!s.try_claim());
    }

    #[test]
    fn retry_cap_blocks_after_max_failures() {
        let s = PhaseBScheduler::new(Duration::from_secs(0));
        for _ in 0..PhaseBScheduler::MAX_FAILURES {
            s.mark_dirty();
            assert!(s.try_claim(), "claim should succeed before cap");
            s.finish_with_result(false);
        }
        // Cap reached — further claims are rejected even with a fresh mark.
        s.mark_dirty();
        assert!(!s.try_claim(), "must be blocked after MAX_FAILURES");
    }

    #[test]
    fn partial_success_resets_failure_counter() {
        let s = PhaseBScheduler::new(Duration::from_secs(0));
        // Two failures …
        for _ in 0..2 {
            s.mark_dirty();
            assert!(s.try_claim());
            s.finish_with_result(false);
        }
        // … followed by a success → counter resets.
        s.mark_dirty();
        assert!(s.try_claim());
        s.finish_with_result(true);
        assert_eq!(s.consecutive_failures(), 0);
        // Scheduler is claimable again after a new mark.
        s.mark_dirty();
        assert!(s.try_claim());
    }

    #[test]
    fn reset_failures_unblocks_scheduler() {
        let s = PhaseBScheduler::new(Duration::from_secs(0));
        for _ in 0..PhaseBScheduler::MAX_FAILURES {
            s.mark_dirty();
            assert!(s.try_claim());
            s.finish_with_result(false);
        }
        s.reset_failures();
        s.mark_dirty();
        assert!(s.try_claim(), "must be claimable after reset_failures");
    }
}
