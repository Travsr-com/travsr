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

/// Outcome of a background Phase B run, reported via
/// [`PhaseBScheduler::finish_with_outcome`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunOutcome {
    /// No language crashed — `phase_b_commit` advanced (or was already fresh).
    Success,
    /// At least one language produced results but another crashed.
    /// `phase_b_commit` did NOT advance (C4), so the daemon's tick re-arms the
    /// scheduler immediately and, without a gate, Phase B would re-run every
    /// ~5–30 s — each run writing graph.db, which since #464 also bumps
    /// `data_version` and thrashes the warm query cache. Triggers exponential
    /// back-off instead of the all-crash retry cap: partial progress means the
    /// toolchain still works and retrying (slowly) is worthwhile.
    Partial,
    /// Every sidecar crashed. Counts toward the `MAX_FAILURES` retry cap.
    AllCrashed,
}

/// Pending-run state (FT-M6). `deadline` is the debounce target (pushed forward
/// by each commit); `first_arm` is when the *current* pending window opened and
/// is NOT pushed forward: it anchors the max-staleness ceiling so a fast commit
/// stream cannot defer Phase B forever.
#[derive(Clone, Copy)]
struct DirtyState {
    deadline: Instant,
    first_arm: Instant,
}

/// Coordinates background Phase B refreshes across the daemon's tasks.
pub struct PhaseBScheduler {
    /// `Some(state)` while a re-run is pending; `None` when idle or already
    /// claimed by an in-flight run. Re-arming pushes `deadline` forward but
    /// keeps `first_arm` fixed so a burst of commits still triggers a run.
    dirty: Mutex<Option<DirtyState>>,
    /// Single-flight guard: at most one background Phase B run at a time.
    running: AtomicBool,
    debounce: Duration,
    /// Force a run once the pending window has been open this long, regardless
    /// of ongoing debounce re-arming (FT-M6).
    ///
    /// #511: this ceiling governs the healthy-commit-stream regime only. After a
    /// partial-crash run the partial back-off gate in `try_claim_at` sits ahead
    /// of this check and deliberately outranks it: re-running a partially broken
    /// toolchain only reproduces the crash and re-thrashes graph.db + the warm
    /// query cache, so the ceiling must not force a run there. Staleness in that
    /// regime is bounded by `PARTIAL_BACKOFF_CAP` (900 s), not by `max_defer`.
    max_defer: Duration,
    /// Consecutive all-crash failures. After MAX_FAILURES the scheduler backs off
    /// until the daemon is restarted, preventing an infinite crash-retry loop when
    /// all Phase B sidecars are broken (e.g. tools not installed).
    consecutive_failures: AtomicU32,
    /// Consecutive partial-crash runs (some languages ran, ≥1 crashed). Drives
    /// the exponential re-run back-off; reset by the first clean run.
    consecutive_partials: AtomicU32,
    /// Claim gate set after a partial-crash run: `try_claim` refuses to start a
    /// run before this instant. Cleared on a clean run.
    backoff_until: Mutex<Option<Instant>>,
}

impl PhaseBScheduler {
    const MAX_FAILURES: u32 = 3;
    /// Default ceiling when constructed via `new`. 10× the 30 s debounce.
    const DEFAULT_MAX_DEFER: Duration = Duration::from_secs(300);
    /// First delay after a partial-crash run; doubles per consecutive partial.
    const PARTIAL_BACKOFF_BASE: Duration = Duration::from_secs(60);
    /// Ceiling for the partial-crash back-off (15 min): keep retrying so a
    /// fixed toolchain is picked up, but stop thrashing graph.db and the warm
    /// query cache in the meantime.
    const PARTIAL_BACKOFF_CAP: Duration = Duration::from_secs(900);
}

impl PhaseBScheduler {
    pub fn new(debounce: Duration) -> Self {
        Self::with_max_defer(debounce, Self::DEFAULT_MAX_DEFER)
    }

    /// Explicit ceiling, used by FT-M6 tests to exercise the starvation guard
    /// deterministically without waiting 300 s.
    pub fn with_max_defer(debounce: Duration, max_defer: Duration) -> Self {
        Self {
            dirty: Mutex::new(None),
            running: AtomicBool::new(false),
            debounce,
            max_defer,
            consecutive_failures: AtomicU32::new(0),
            consecutive_partials: AtomicU32::new(0),
            backoff_until: Mutex::new(None),
        }
    }

    /// Arm (or re-arm) a re-run for `debounce` from now. Called after a
    /// commit-triggered reindex. Re-arming pushes `deadline` forward so a burst
    /// of commits settles into one run, but keeps `first_arm` fixed so a fast
    /// commit stream cannot defer Phase B past `max_defer` (FT-M6).
    pub fn mark_dirty(&self) {
        let now = Instant::now();
        let mut d = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        match d.as_mut() {
            // Re-arm: push debounce deadline, KEEP first_arm (ceiling anchor).
            Some(st) => st.deadline = now + self.debounce,
            None => {
                *d = Some(DirtyState {
                    deadline: now + self.debounce,
                    first_arm: now,
                })
            }
        }
    }

    /// Arm for an immediate run with no debounce. Used by `phase_b_tick` on
    /// daemon startup when Phase B is pending from a deferred init. Unlike
    /// `mark_dirty`, this does NOT override an existing armed deadline — so it
    /// never interferes with a commit-triggered debounce window that is already
    /// counting down.
    pub fn arm_immediate(&self) {
        let now = Instant::now();
        let mut d = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        if d.is_none() {
            *d = Some(DirtyState {
                deadline: now,
                first_arm: now,
            });
        }
    }

    /// If a run is due (armed and past its deadline OR past max_defer) and none
    /// is in flight, atomically claim the run slot and consume the pending mark.
    /// Returns `true` iff the caller should start a background run; the caller
    /// MUST call [`finish`](Self::finish) when that run completes.
    ///
    /// `now` is injectable for deterministic tests; production callers pass
    /// `Instant::now()` via [`try_claim`](Self::try_claim).
    pub fn try_claim_at(&self, now: Instant) -> bool {
        // Back off after repeated all-crash runs to avoid hammering broken tools.
        if self.consecutive_failures.load(Ordering::Relaxed) >= Self::MAX_FAILURES {
            return false;
        }
        // Partial-crash back-off (#464 follow-up): after a partial run
        // `phase_b_commit` lags HEAD, so the daemon tick re-arms every 5 s —
        // without this gate Phase B would re-run continuously, each run
        // writing graph.db and invalidating the warm query cache.
        //
        // #511: this gate is intentionally ahead of the `max_defer` ceiling
        // below and outranks it. Re-running a partially broken toolchain only
        // reproduces the crash, so the ceiling must not force a run here;
        // back-off staleness is bounded by `PARTIAL_BACKOFF_CAP` instead.
        if let Some(until) = *self.backoff_until.lock().unwrap_or_else(|e| e.into_inner()) {
            if now < until {
                return false;
            }
        }
        let mut d = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        let due = match *d {
            Some(st) => {
                now >= st.deadline || now.saturating_duration_since(st.first_arm) >= self.max_defer
            }
            None => false,
        };
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
            *d = None; // consumes the mark → first_arm resets on next mark_dirty
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
    /// Use [`finish_with_outcome`](Self::finish_with_outcome) when the caller
    /// can report how the run went.
    #[allow(dead_code)]
    pub fn finish(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// Release the slot and record a binary success/failure. Kept for callers
    /// that cannot distinguish partial crashes; maps to
    /// [`RunOutcome::Success`] / [`RunOutcome::AllCrashed`].
    #[allow(dead_code)]
    pub fn finish_with_result(&self, succeeded: bool) {
        self.finish_with_outcome(if succeeded {
            RunOutcome::Success
        } else {
            RunOutcome::AllCrashed
        });
    }

    /// Release the slot and record the run's [`RunOutcome`].
    pub fn finish_with_outcome(&self, outcome: RunOutcome) {
        self.finish_with_outcome_at(outcome, Instant::now());
    }

    /// [`finish_with_outcome`](Self::finish_with_outcome) with an injectable
    /// clock for deterministic tests.
    pub fn finish_with_outcome_at(&self, outcome: RunOutcome, now: Instant) {
        match outcome {
            RunOutcome::Success => {
                self.consecutive_failures.store(0, Ordering::Relaxed);
                self.consecutive_partials.store(0, Ordering::Relaxed);
                *self.backoff_until.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            RunOutcome::Partial => {
                // Partial progress means the toolchain is not entirely broken:
                // keep the all-crash retry cap out of the way, but gate the
                // next claim behind an exponentially growing delay (60 s,
                // 120 s, … capped at 15 min) so the re-armed run cannot
                // thrash graph.db and the warm query cache.
                self.consecutive_failures.store(0, Ordering::Relaxed);
                let n = self.consecutive_partials.fetch_add(1, Ordering::Relaxed);
                let delay = Self::PARTIAL_BACKOFF_BASE
                    .saturating_mul(1u32 << n.min(31))
                    .min(Self::PARTIAL_BACKOFF_CAP);
                *self.backoff_until.lock().unwrap_or_else(|e| e.into_inner()) = Some(now + delay);
            }
            RunOutcome::AllCrashed => {
                self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
            }
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
    fn partial_crash_backs_off_exponentially() {
        // #464 follow-up: a partial crash leaves phase_b_commit stale, so the
        // daemon tick re-arms immediately — the back-off gate must refuse the
        // claim until the delay elapses, doubling per consecutive partial.
        // Virtual clock anchored in the future so mark_dirty's real-clock
        // debounce deadline (0 s) is always already past.
        let s = PhaseBScheduler::new(Duration::from_secs(0));
        let t0 = Instant::now() + Duration::from_secs(1000);

        s.mark_dirty();
        assert!(s.try_claim_at(t0));
        s.finish_with_outcome_at(RunOutcome::Partial, t0);

        // First back-off: 60 s.
        s.mark_dirty();
        assert!(
            !s.try_claim_at(t0 + Duration::from_secs(59)),
            "claim gated during first back-off"
        );
        let t1 = t0 + Duration::from_secs(60);
        assert!(s.try_claim_at(t1), "claimable once back-off elapses");
        s.finish_with_outcome_at(RunOutcome::Partial, t1);

        // Second consecutive partial: 120 s.
        s.mark_dirty();
        assert!(
            !s.try_claim_at(t1 + Duration::from_secs(119)),
            "back-off doubles on the second consecutive partial"
        );
        assert!(s.try_claim_at(t1 + Duration::from_secs(120)));
    }

    #[test]
    fn partial_backoff_is_capped() {
        let s = PhaseBScheduler::new(Duration::from_secs(0));
        let t0 = Instant::now() + Duration::from_secs(1000);
        // Drive the counter far past the doubling range.
        for _ in 0..10 {
            s.mark_dirty();
            // Claim far in the future so any pending back-off has elapsed.
            assert!(s.try_claim_at(t0 + Duration::from_secs(100_000)));
            s.finish_with_outcome_at(RunOutcome::Partial, t0);
        }
        // Even after many partials the gate lifts at the 900 s cap.
        s.mark_dirty();
        assert!(!s.try_claim_at(t0 + Duration::from_secs(899)));
        assert!(s.try_claim_at(t0 + Duration::from_secs(900)));
    }

    #[test]
    fn clean_run_clears_partial_backoff() {
        let s = PhaseBScheduler::new(Duration::from_secs(0));
        let t0 = Instant::now() + Duration::from_secs(1000);
        s.mark_dirty();
        assert!(s.try_claim_at(t0));
        s.finish_with_outcome_at(RunOutcome::Partial, t0);
        // A later clean run (e.g. the user installed the missing tool and the
        // capped retry succeeded) clears the gate and the counter.
        s.mark_dirty();
        assert!(s.try_claim_at(t0 + Duration::from_secs(60)));
        s.finish_with_outcome_at(RunOutcome::Success, t0 + Duration::from_secs(60));
        s.mark_dirty();
        assert!(
            s.try_claim_at(t0 + Duration::from_secs(61)),
            "no residual back-off after a clean run"
        );
    }

    #[test]
    fn partial_does_not_trip_all_crash_cap() {
        // Partials must never accumulate into the MAX_FAILURES hard stop —
        // they throttle retries, not disable them.
        let s = PhaseBScheduler::new(Duration::from_secs(0));
        let t0 = Instant::now() + Duration::from_secs(1000);
        for i in 0..(PhaseBScheduler::MAX_FAILURES + 2) {
            s.mark_dirty();
            assert!(
                s.try_claim_at(t0 + Duration::from_secs(100_000 * (i as u64 + 1))),
                "partial #{i} must still be claimable after back-off"
            );
            s.finish_with_outcome_at(RunOutcome::Partial, t0);
        }
        assert_eq!(s.consecutive_failures(), 0);
    }

    #[test]
    fn partial_backoff_outranks_max_defer_ceiling() {
        // #511: after a partial crash the back-off gate deliberately outranks the
        // max_defer staleness ceiling — forcing a re-run of a broken toolchain
        // only reproduces the crash and re-thrashes graph.db + the warm cache.
        // max_defer=50s is below PARTIAL_BACKOFF_BASE=60s, so the very first
        // back-off already lasts longer than the ceiling would allow.
        let s = PhaseBScheduler::with_max_defer(Duration::from_secs(30), Duration::from_secs(50));
        let t0 = Instant::now() + Duration::from_secs(1000);

        // A first run partial-crashes at t0, arming a 60 s back-off.
        *s.dirty.lock().unwrap() = Some(DirtyState {
            deadline: t0,
            first_arm: t0,
        });
        assert!(s.try_claim_at(t0));
        s.finish_with_outcome_at(RunOutcome::Partial, t0);

        // Re-arm with first_arm anchored at t0 so the 50 s ceiling is already
        // past at t0+55s — yet the 60 s back-off must still refuse the claim.
        *s.dirty.lock().unwrap() = Some(DirtyState {
            deadline: t0 + Duration::from_secs(1000),
            first_arm: t0,
        });
        assert!(
            !s.try_claim_at(t0 + Duration::from_secs(55)),
            "partial back-off outranks the ceiling even after max_defer has elapsed"
        );
        // Once the back-off lifts (60 s) the claim — long past the ceiling — succeeds.
        assert!(s.try_claim_at(t0 + Duration::from_secs(60)));
    }

    // ── TC-FT-M6: max-staleness ceiling ──────────────────────────────────────

    #[test]
    fn ceiling_forces_run_despite_continuous_rearm() {
        // debounce=30s, max_defer=100s. Directly inject DirtyState so first_arm
        // and deadline are both anchored to t0 (avoids wall-clock vs virtual-time
        // mismatch that arises when mark_dirty() captures its own Instant::now()).
        let s = PhaseBScheduler::with_max_defer(Duration::from_secs(30), Duration::from_secs(100));
        let t0 = Instant::now();
        // Simulate: first armed at t0, then re-armed to push deadline to t0+129s.
        *s.dirty.lock().unwrap() = Some(DirtyState {
            deadline: t0 + Duration::from_secs(129),
            first_arm: t0,
        });
        // At t0+99s: deadline not hit (99 < 129), ceiling not hit (99 < 100).
        assert!(!s.try_claim_at(t0 + Duration::from_secs(99)));
        // At t0+100s: ceiling fires (elapsed from first_arm == max_defer).
        assert!(s.try_claim_at(t0 + Duration::from_secs(100)));
    }

    #[test]
    fn first_arm_resets_after_claim() {
        // After a ceiling-forced claim+finish, the next mark opens a fresh window
        // whose ceiling is measured from its own first_arm, not the old one.
        let s = PhaseBScheduler::with_max_defer(Duration::from_secs(30), Duration::from_secs(100));
        let t0 = Instant::now();
        // First window: first_arm=t0, deadline=t0+129s.
        *s.dirty.lock().unwrap() = Some(DirtyState {
            deadline: t0 + Duration::from_secs(129),
            first_arm: t0,
        });
        assert!(s.try_claim_at(t0 + Duration::from_secs(100)));
        s.finish();
        // Second window: first_arm=t1, deadline=t1+30s. Inject directly so the
        // virtual t1 is precisely t0+105s.
        let t1 = t0 + Duration::from_secs(105);
        *s.dirty.lock().unwrap() = Some(DirtyState {
            deadline: t1 + Duration::from_secs(30),
            first_arm: t1,
        });
        // At t1+29s: debounce not hit (29 < 30), ceiling not hit (29 < 100).
        assert!(!s.try_claim_at(t1 + Duration::from_secs(29)));
        // At t1+100s: ceiling fires on the new window.
        assert!(s.try_claim_at(t1 + Duration::from_secs(100)));
    }
}
