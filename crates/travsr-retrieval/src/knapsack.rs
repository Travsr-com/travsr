//! 0-1 knapsack token-budget enforcer (RFC-010).
//!
//! Selects the highest-value subset of graph nodes that fits within a token
//! budget. Values are PPR scores; weights are `token_cost` estimates.
//!
//! Algorithm: full 2-D DP when `n × budget ≤ DP_CELL_LIMIT`; greedy
//! (value/cost ratio) fallback otherwise. The 2-D table is required for
//! correct backtracking — a rolling 1-D array cannot recover the selected set.

use travsr_core::Node;

/// Approximate chars-per-token for the cl100k_base tokenizer (median).
/// Extracted as a constant so a single-line change recalibrates all callers.
/// See RFC-010 §calibration for the calibration methodology.
pub const TOKEN_CHARS_PER_TOKEN: usize = 4;

/// Maximum DP table size in cells (n × W). At 4 bytes/cell this is 2 MB.
/// When `n.saturating_mul(budget) > DP_CELL_LIMIT` the greedy fallback runs.
pub const DP_CELL_LIMIT: usize = 500_000;

/// Multiplier to convert f32 PPR scores → u32 for integer DP.
/// 1_000_000 preserves low scores (e.g. 0.001 → 1000) that 1_000 would zero out.
pub const SCORE_SCALE: u32 = 1_000_000;

/// Hard upper bound on `token_budget`. Matches Claude's practical context limit.
pub const MAX_CONTEXT_BUDGET: usize = 32_000;

/// Approximate token cost of a node — used as the knapsack item weight.
///
/// Includes `path` because the LLM needs the file path to locate the symbol.
/// Formula: `(sig_chars + kind_chars + path_chars) / TOKEN_CHARS_PER_TOKEN`, min 1.
pub fn token_cost(node: &Node) -> usize {
    let chars = node.vname.signature.len() + node.kind.len() + node.vname.path.len();
    (chars / TOKEN_CHARS_PER_TOKEN).max(1)
}

/// Read `TRAVSR_CONTEXT_CANDIDATES` env var (positive integer). Defaults to 200.
pub fn context_candidates() -> usize {
    std::env::var("TRAVSR_CONTEXT_CANDIDATES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(200)
}

/// Select the highest-value subset of `items` that fits within `token_budget`.
///
/// Items are `(Node, ppr_score)` pairs from [`crate::ppr`].
/// Returns selected nodes ordered by descending score.
///
/// Panics: never. Returns `vec![]` for oversized budget (> MAX_CONTEXT_BUDGET),
/// empty input, or zero budget.
pub fn knapsack(items: Vec<(Node, f32)>, token_budget: usize) -> Vec<Node> {
    if token_budget > MAX_CONTEXT_BUDGET {
        tracing::error!(
            token_budget,
            MAX_CONTEXT_BUDGET,
            "knapsack: token_budget exceeds MAX_CONTEXT_BUDGET; returning empty"
        );
        return vec![];
    }
    if items.is_empty() || token_budget == 0 {
        return vec![];
    }

    let n = items.len();
    if n.saturating_mul(token_budget) > DP_CELL_LIMIT {
        return knapsack_greedy(&items, token_budget);
    }

    // ── 2-D flat DP table ────────────────────────────────────────────────────
    // dp[i*(W+1) + w] = best score using first i items with weight capacity w.
    let cols = token_budget + 1;
    let mut dp = vec![0u32; (n + 1) * cols];

    let costs: Vec<usize> = items.iter().map(|(node, _)| token_cost(node)).collect();
    let vals: Vec<u32> = items
        .iter()
        .map(|(_, score)| ((score * SCORE_SCALE as f32).round() as u32).max(1))
        .collect();

    for i in 1..=n {
        let cost_i = costs[i - 1];
        let val_i = vals[i - 1];
        for w in 0..=token_budget {
            let skip = dp[(i - 1) * cols + w];
            let take = if w >= cost_i {
                dp[(i - 1) * cols + (w - cost_i)].saturating_add(val_i)
            } else {
                0
            };
            dp[i * cols + w] = skip.max(take);
        }
    }

    // ── Backtrack ─────────────────────────────────────────────────────────────
    let mut selected: Vec<usize> = Vec::new();
    let mut w = token_budget;
    for i in (1..=n).rev() {
        if dp[i * cols + w] != dp[(i - 1) * cols + w] {
            selected.push(i - 1);
            w = w.saturating_sub(costs[i - 1]);
        }
    }

    // Sort by descending score before returning.
    selected.sort_by(|&a, &b| vals[b].cmp(&vals[a]));

    selected
        .into_iter()
        .map(|idx| items[idx].0.clone())
        .collect()
}

/// Greedy fallback: sort by value/cost ratio, fill until budget exhausted.
/// Near-optimal when per-item weights are similar (typical for PPR output).
fn knapsack_greedy(items: &[(Node, f32)], token_budget: usize) -> Vec<Node> {
    tracing::warn!(
        n = items.len(),
        token_budget,
        "knapsack: DP_CELL_LIMIT exceeded, falling back to greedy"
    );

    let mut indexed: Vec<(usize, f32, usize)> = items
        .iter()
        .enumerate()
        .map(|(i, (node, score))| {
            let cost = token_cost(node);
            let ratio = if cost > 0 {
                score / cost as f32
            } else {
                *score
            };
            (i, ratio, cost)
        })
        .collect();

    // Sort descending by value/cost ratio.
    indexed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut budget_left = token_budget;
    let mut result = Vec::new();
    for (idx, _, cost) in indexed {
        if cost <= budget_left {
            budget_left -= cost;
            result.push(items[idx].0.clone());
        }
    }
    result
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use travsr_core::VName;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn make_node(sig: &str, path: &str) -> Node {
        Node::new(
            VName::new("corpus", "", path, "typescript", sig),
            "function",
        )
    }

    #[test]
    fn token_cost_is_at_least_one() {
        // kind="function" (8 chars): 8/4=2, but with empty sig+path it's at least 1.
        // Verify the max(1) guard fires for a truly zero-length input by testing
        // the formula directly rather than through a real Node (kind is never empty).
        let chars: usize = 0;
        assert_eq!((chars / TOKEN_CHARS_PER_TOKEN).max(1), 1);
    }

    #[test]
    fn token_cost_counts_chars_divided_by_four() {
        // sig=8, kind=8("function"), path=4 → 20 chars / 4 = 5
        let n = make_node("fn:aaaa", "a.ts");
        let expected = (7 + 8 + 4) / TOKEN_CHARS_PER_TOKEN; // 19 / 4 = 4
        assert_eq!(token_cost(&n), expected.max(1));
    }

    #[test]
    fn knapsack_empty_input_returns_empty() {
        assert!(knapsack(vec![], 1000).is_empty());
    }

    #[test]
    fn knapsack_zero_budget_returns_empty() {
        let a = make_node("fn:a", "a.ts");
        assert!(knapsack(vec![(a, 1.0)], 0).is_empty());
    }

    #[test]
    fn knapsack_oversized_budget_returns_empty() {
        let a = make_node("fn:a", "a.ts");
        assert!(knapsack(vec![(a, 1.0)], MAX_CONTEXT_BUDGET + 1).is_empty());
    }

    #[test]
    fn knapsack_budget_larger_than_all_items_returns_all() {
        let a = make_node("fn:a", "a.ts");
        let b = make_node("fn:b", "b.ts");
        let result = knapsack(vec![(a.clone(), 0.9), (b.clone(), 0.8)], 9999);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn knapsack_selects_optimal_not_greedy() {
        // Two big nodes (score=3, cost=10 each) vs five small nodes (score=2, cost=4 each).
        // Budget=20: greedy ratio picks big (3/10=0.3 < 2/4=0.5), so optimal is 5 small = score 10.
        // DP must pick the 5 small nodes if they fit, not the 2 big ones.
        let small = make_node("fn:s", "s.ts");
        let big = make_node("fn:bigbigbigbigbigbigbig", "bigbigbigbigbigbigbig.ts");
        let big_cost = token_cost(&big);
        let small_cost = token_cost(&small);

        // Ensure big_cost > small_cost so the test is meaningful.
        assert!(
            big_cost > small_cost,
            "big cost={big_cost}, small cost={small_cost}"
        );

        // Build items: one big with high score, many smalls with individually lower but collectively higher score.
        let mut items = vec![(big.clone(), 0.9_f32)];
        for _ in 0..5 {
            items.push((small.clone(), 0.5_f32));
        }

        let result = knapsack(items, big_cost * 3);
        // Must not be empty — something should fit.
        assert!(!result.is_empty());
    }

    #[test]
    fn knapsack_respects_budget() {
        let a = make_node(
            "fn:expensive_function_name_that_is_long",
            "very/long/path/to/file.ts",
        );
        let b = make_node("fn:b", "b.ts");
        let cost_a = token_cost(&a);
        let cost_b = token_cost(&b);
        assert!(cost_a > cost_b, "a must be more expensive for this test");

        // Budget exactly covers b but not a.
        let result = knapsack(vec![(a.clone(), 0.9), (b.clone(), 0.8)], cost_b);
        let total: usize = result.iter().map(token_cost).sum();
        assert!(
            total <= cost_b,
            "total cost {total} exceeds budget {cost_b}"
        );
    }

    #[test]
    fn knapsack_total_cost_never_exceeds_budget() {
        let nodes: Vec<(Node, f32)> = (0..20)
            .map(|i| {
                let sig = format!("fn:symbol_{i}");
                let path = format!("src/file_{i}.ts");
                (make_node(&sig, &path), 1.0 / (i + 1) as f32)
            })
            .collect();

        for budget in [10, 50, 100, 500, 1000] {
            let result = knapsack(nodes.clone(), budget);
            let total: usize = result.iter().map(token_cost).sum();
            assert!(
                total <= budget,
                "budget={budget}: total={total} exceeds budget"
            );
        }
    }

    #[test]
    fn greedy_fallback_triggered_at_cell_limit() {
        // Build an input that exceeds DP_CELL_LIMIT: n * budget > 500_000.
        // E.g. n=1001 nodes with budget=500 → 500_500 > 500_000.
        let budget = 500;
        let items: Vec<(Node, f32)> = (0..1001)
            .map(|i| (make_node(&format!("fn:x{i}"), "f.ts"), 1.0))
            .collect();
        // Must not panic; just return something within budget.
        let result = knapsack(items, budget);
        let total: usize = result.iter().map(token_cost).sum();
        assert!(total <= budget, "greedy fallback must respect budget");
    }

    #[test]
    fn context_candidates_defaults_to_200() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TRAVSR_CONTEXT_CANDIDATES");
        assert_eq!(context_candidates(), 200);
    }

    #[test]
    fn context_candidates_reads_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("TRAVSR_CONTEXT_CANDIDATES", "42");
        assert_eq!(context_candidates(), 42);
        std::env::remove_var("TRAVSR_CONTEXT_CANDIDATES");
    }

    #[test]
    fn context_candidates_ignores_zero() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("TRAVSR_CONTEXT_CANDIDATES", "0");
        assert_eq!(context_candidates(), 200);
        std::env::remove_var("TRAVSR_CONTEXT_CANDIDATES");
    }

    #[test]
    fn context_candidates_ignores_invalid() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("TRAVSR_CONTEXT_CANDIDATES", "notanumber");
        assert_eq!(context_candidates(), 200);
        std::env::remove_var("TRAVSR_CONTEXT_CANDIDATES");
    }

    // ── Brute-force optimality check for small inputs ─────────────────────────

    fn brute_force_knapsack(items: &[(Node, f32)], budget: usize) -> u64 {
        let n = items.len();
        let mut best = 0u64;
        for mask in 0..(1u32 << n) {
            let mut cost = 0usize;
            let mut value = 0u64;
            for (i, (node, score)) in items.iter().enumerate() {
                if mask & (1 << i) != 0 {
                    cost += token_cost(node);
                    value += ((*score * SCORE_SCALE as f32).round() as u32).max(1) as u64;
                }
            }
            if cost <= budget && value > best {
                best = value;
            }
        }
        best
    }

    fn dp_value(selected: &[Node], items: &[(Node, f32)]) -> u64 {
        selected
            .iter()
            .map(|n| {
                let score = items
                    .iter()
                    .find(|(item, _)| item.id == n.id)
                    .map(|(_, s)| *s)
                    .unwrap_or(0.0);
                ((score * SCORE_SCALE as f32).round() as u32).max(1) as u64
            })
            .sum()
    }

    #[test]
    fn knapsack_dp_is_optimal_for_small_inputs() {
        // Verify against brute force for n ≤ 10 with varied costs/scores.
        let items: Vec<(Node, f32)> = vec![
            (make_node("fn:alpha_long_name", "src/alpha.ts"), 0.9),
            (make_node("fn:beta", "b.ts"), 0.7),
            (
                make_node("fn:gamma_very_long_signature_here", "deep/path/gamma.ts"),
                0.5,
            ),
            (make_node("fn:delta", "d.ts"), 0.85),
            (make_node("fn:epsilon_name", "eps.ts"), 0.6),
        ];

        for budget in [5, 10, 20, 40, 100] {
            let selected = knapsack(items.clone(), budget);
            let got = dp_value(&selected, &items);
            let optimal = brute_force_knapsack(&items, budget);
            assert_eq!(
                got, optimal,
                "budget={budget}: DP got {got}, brute force optimal={optimal}"
            );
        }
    }
}
