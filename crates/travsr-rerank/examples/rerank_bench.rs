use std::time::Instant;
use travsr_rerank::{Reranker, TractReranker};

fn main() {
    let dir = std::env::args().nth(1).expect("model dir arg");
    let reranker = TractReranker::load(&dir).expect("load");

    let query = "how is the knapsack budget optimizer implemented";
    let candidate = "fn:knapsack_budget_optimizer computes a 0-1 knapsack over token budget for context assembly\nfn body line 1\nfn body line 2\nfn body line 3\nfn body line 4\nfn body line 5\nfn body line 6\nfn body line 7\nfn body line 8\nfn body line 9\nfn body line 10";

    // warm-up
    let _ = reranker.rerank(query, &[candidate]).unwrap();

    for k in [1, 5, 10, 20, 30] {
        let cands: Vec<&str> = (0..k).map(|_| candidate).collect();
        let t = Instant::now();
        let scores = reranker.rerank(query, &cands).unwrap();
        println!("K={k}: {:?} (n_scores={})", t.elapsed(), scores.len());
    }
    println!("rayon threads: {}", rayon::current_num_threads());
}
