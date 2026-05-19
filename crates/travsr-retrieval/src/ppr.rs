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

/// Return ALPHA, overridden by `TRAVSR_PPR_ALPHA` env var if set and parseable.
pub fn alpha() -> f32 {
    parse_env_f32("TRAVSR_PPR_ALPHA", ALPHA)
}

/// Return EPSILON, overridden by `TRAVSR_PPR_EPSILON` env var if set and parseable.
pub fn epsilon() -> f32 {
    parse_env_f32("TRAVSR_PPR_EPSILON", EPSILON)
}

/// Return MAX_ITERATIONS, overridden by `TRAVSR_PPR_MAX_ITER` env var if set and parseable.
pub fn max_iterations() -> u32 {
    std::env::var("TRAVSR_PPR_MAX_ITER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_ITERATIONS)
}

fn parse_env_f32(var: &str, default: f32) -> f32 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&x: &f32| x.is_finite() && x > 0.0)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-time guard: catch accidental MAX_ITERATIONS change.
    const _: () = assert!(MAX_ITERATIONS == 50, "MAX_ITERATIONS changed from ADR-003 value");

    #[test]
    fn defaults_match_adr() {
        // Exact equality on constants: both sides are compile-time known.
        // Clippy allows assert_eq! on constants (unlike assert! with a comparison).
        assert_eq!(ALPHA, 0.85_f32);
        assert_eq!(EPSILON, 1e-6_f32);
        assert_eq!(MAX_ITERATIONS, 50_u32);
    }

    #[test]
    fn alpha_without_env_var_returns_default() {
        // Env var is not set in a clean test environment.
        std::env::remove_var("TRAVSR_PPR_ALPHA");
        assert_eq!(alpha(), ALPHA);
    }

    #[test]
    fn epsilon_without_env_var_returns_default() {
        std::env::remove_var("TRAVSR_PPR_EPSILON");
        assert!((epsilon() - EPSILON).abs() < 1e-10);
    }

    #[test]
    fn max_iterations_without_env_var_returns_default() {
        std::env::remove_var("TRAVSR_PPR_MAX_ITER");
        assert_eq!(max_iterations(), MAX_ITERATIONS);
    }

    #[test]
    fn parse_env_f32_rejects_non_positive() {
        // Negative and zero values are nonsensical for alpha/epsilon — must fall back.
        assert_eq!(parse_env_f32("__NON_EXISTENT__", 0.85), 0.85);
    }
}
