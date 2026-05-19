//! Personalized PageRank hyperparameters (ADR-003).
//!
//! Defaults are the Accepted values from ADR-003. Do NOT change them without
//! updating the ADR. Overridable at runtime via env vars for experimentation
//! (see ADR-003 §Overrides); these are NOT a production configuration surface.

/// Probability of following an outgoing edge at each PPR step.
///
/// The complementary probability (1 − ALPHA) is the teleportation weight
/// back to the seed nodes. Standard PageRank default; see ADR-003 §Decision.
pub const ALPHA: f32 = 0.85;

/// L₁-norm convergence threshold.
///
/// Iteration stops when `‖r_{t+1} − r_t‖₁ < EPSILON`. 1e-6 converges in
/// 15–30 iterations on code graphs ≤ 10M nodes at ALPHA = 0.85 (ADR-003).
pub const EPSILON: f32 = 1e-6;

/// Hard iteration cap — prevents runaway on degenerate graph topologies.
///
/// At ALPHA = 0.85, genuine convergence always occurs before this limit for
/// any connected component within the MVP node ceiling (ADR-003).
pub const MAX_ITERATIONS: u32 = 50;

// Compile-time guard: fires on every build (not just tests) so an accidental
// constant change is caught before release. Update the ADR before changing.
const _: () = assert!(
    MAX_ITERATIONS == 50,
    "MAX_ITERATIONS changed from ADR-003 value — update ADR-003 first"
);

/// Return ALPHA, overridden by `TRAVSR_PPR_ALPHA` env var if set and parseable.
///
/// Accepts only values in (0.0, 1.0) exclusive — values outside this range
/// are mathematically invalid for a damping factor and fall back to the default.
pub fn alpha() -> f32 {
    parse_env_f32_bounded("TRAVSR_PPR_ALPHA", ALPHA, 0.0, 1.0)
}

/// Return EPSILON, overridden by `TRAVSR_PPR_EPSILON` env var if set and parseable.
pub fn epsilon() -> f32 {
    parse_env_f32("TRAVSR_PPR_EPSILON", EPSILON)
}

/// Return MAX_ITERATIONS, overridden by `TRAVSR_PPR_MAX_ITER` env var if set and parseable.
///
/// Rejects `0` — a zero iteration cap produces an uninitialized score vector.
pub fn max_iterations() -> u32 {
    std::env::var("TRAVSR_PPR_MAX_ITER")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(MAX_ITERATIONS)
}

fn parse_env_f32(var: &str, default: f32) -> f32 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| x.is_finite() && x > 0.0)
        .unwrap_or(default)
}

/// Like `parse_env_f32` but also enforces an exclusive upper bound.
fn parse_env_f32_bounded(var: &str, default: f32, lo: f32, hi: f32) -> f32 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| x.is_finite() && x > lo && x < hi)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialise env-var mutation tests to avoid data races when the test
    // harness runs tests on multiple threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn defaults_match_adr() {
        // Exact equality on constants: both sides are compile-time known.
        assert_eq!(ALPHA, 0.85_f32);
        assert_eq!(EPSILON, 1e-6_f32);
        assert_eq!(MAX_ITERATIONS, 50_u32);
    }

    #[test]
    fn alpha_without_env_var_returns_default() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TRAVSR_PPR_ALPHA");
        assert_eq!(alpha(), ALPHA);
    }

    #[test]
    fn alpha_valid_override_is_used() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TRAVSR_PPR_ALPHA", "0.90");
        let v = alpha();
        std::env::remove_var("TRAVSR_PPR_ALPHA");
        assert!((v - 0.90_f32).abs() < 1e-6, "valid override must be used");
    }

    #[test]
    fn alpha_rejects_zero_and_falls_back() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TRAVSR_PPR_ALPHA", "0.0");
        let v = alpha();
        std::env::remove_var("TRAVSR_PPR_ALPHA");
        assert_eq!(v, ALPHA, "alpha=0.0 is invalid — must fall back to default");
    }

    #[test]
    fn alpha_rejects_one_and_above() {
        let _g = ENV_LOCK.lock().unwrap();
        for bad in ["1.0", "1.5", "99.0"] {
            std::env::set_var("TRAVSR_PPR_ALPHA", bad);
            let v = alpha();
            std::env::remove_var("TRAVSR_PPR_ALPHA");
            assert_eq!(v, ALPHA, "alpha={bad} >= 1.0 is invalid — must fall back");
        }
    }

    #[test]
    fn alpha_rejects_negative() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TRAVSR_PPR_ALPHA", "-0.5");
        let v = alpha();
        std::env::remove_var("TRAVSR_PPR_ALPHA");
        assert_eq!(v, ALPHA, "negative alpha must fall back to default");
    }

    #[test]
    fn epsilon_without_env_var_returns_default() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TRAVSR_PPR_EPSILON");
        assert!((epsilon() - EPSILON).abs() < 1e-10);
    }

    #[test]
    fn max_iterations_without_env_var_returns_default() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TRAVSR_PPR_MAX_ITER");
        assert_eq!(max_iterations(), MAX_ITERATIONS);
    }

    #[test]
    fn max_iterations_valid_override_is_used() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TRAVSR_PPR_MAX_ITER", "100");
        let n = max_iterations();
        std::env::remove_var("TRAVSR_PPR_MAX_ITER");
        assert_eq!(n, 100, "valid override must be used");
    }

    #[test]
    fn max_iterations_rejects_zero() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TRAVSR_PPR_MAX_ITER", "0");
        let n = max_iterations();
        std::env::remove_var("TRAVSR_PPR_MAX_ITER");
        assert_eq!(
            n, MAX_ITERATIONS,
            "max_iterations=0 must fall back to default"
        );
    }

    #[test]
    fn parse_env_f32_missing_var_returns_default() {
        // When the env var is absent the default must be returned unchanged.
        assert_eq!(parse_env_f32("__TRAVSR_TEST_NON_EXISTENT__", 0.85), 0.85);
    }
}
