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

use std::sync::atomic::{AtomicBool, Ordering};
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
}

impl PhaseBScheduler {
    pub fn new(debounce: Duration) -> Self {
        Self {
            dirty: Mutex::new(None),
            running: AtomicBool::new(false),
            debounce,
        }
    }

    /// Arm (or re-arm) a re-run for `debounce` from now. Called after a
    /// commit-triggered reindex. Re-arming intentionally pushes the deadline
    /// forward so that a rapid series of commits triggers a single run once the
    /// activity settles.
    pub fn mark_dirty(&self) {
        let mut d = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        *d = Some(Instant::now() + self.debounce);
    }

    /// If a run is due (armed and past its deadline) and none is in flight,
    /// atomically claim the run slot and consume the pending mark. Returns
    /// `true` iff the caller should start a background run; the caller MUST call
    /// [`finish`](Self::finish) when that run completes.
    ///
    /// `now` is injectable for deterministic tests; production callers pass
    /// `Instant::now()` via [`try_claim`](Self::try_claim).
    pub fn try_claim_at(&self, now: Instant) -> bool {
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
    pub fn finish(&self) {
        self.running.store(false, Ordering::Release);
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
}
