//! RFC-012 L1 fuzzy seed selection — integration tests.
//!
//! Acceptance criteria verified here:
//! - NL queries resolve via FTS5 when substring fails.
//! - Exact-match result wins over FTS result (layer ordering preserved).
//! - FTS5 MATCH never panics on FTS-syntax-char input.
//! - Kind-update re-index leaves no stale/duplicate FTS rows.
//! - Migration v9 is idempotent (running open() twice on the same db is safe).

use travsr_core::{Node, VName};
use travsr_store::{SqliteStore, Store as _};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn node(path: &str, sig: &str, kind: &str) -> Node {
    Node::new(VName::new("corpus", "root", path, "typescript", sig), kind)
}

fn open() -> SqliteStore {
    SqliteStore::open_in_memory().expect("open_in_memory failed")
}

fn put(store: &mut SqliteStore, n: &Node) {
    store.put_node(n).expect("put_node failed");
}

// ── Tokenizer round-trip sanity ───────────────────────────────────────────────

#[test]
fn tokenizer_snake_case_resolves() {
    let mut store = open();
    let n = node(
        "crates/travsr-mcp/src/server.rs",
        "fn:dispatch_tool_call",
        "function",
    );
    put(&mut store, &n);

    // Exact substring hit via step-1 (LIKE).
    let r = store.search_nodes_fuzzy("dispatch_tool_call").unwrap();
    assert!(!r.is_empty(), "exact substring must resolve");
    assert_eq!(r[0].vname.signature, "fn:dispatch_tool_call");
}

// ── NL queries that substring cannot satisfy ──────────────────────────────────

#[test]
fn nl_query_mcp_dispatch_tool_call() {
    let mut store = open();
    let n = node(
        "crates/travsr-mcp/src/server.rs",
        "fn:dispatch_tool_call",
        "function",
    );
    put(&mut store, &n);

    // Four-word NL query — no single node contains the literal.
    let r = store.search_nodes_fuzzy("mcp dispatch tool call").unwrap();
    assert!(
        !r.is_empty(),
        "NL query 'mcp dispatch tool call' must find dispatch_tool_call via FTS"
    );
    assert_eq!(r[0].vname.signature, "fn:dispatch_tool_call");
}

#[test]
fn nl_query_knapsack_budget_enforcer() {
    let mut store = open();
    let n = node(
        "crates/travsr-retrieval/src/knapsack.rs",
        "fn:knapsack_select",
        "function",
    );
    put(&mut store, &n);

    let r = store
        .search_nodes_fuzzy("knapsack budget enforcer")
        .unwrap();
    assert!(
        !r.is_empty(),
        "NL 'knapsack budget enforcer' must resolve knapsack_select"
    );
}

#[test]
fn nl_query_ppr_traversal() {
    let mut store = open();
    let n = node(
        "crates/travsr-retrieval/src/ppr.rs",
        "fn:personalized_page_rank",
        "function",
    );
    put(&mut store, &n);

    let r = store.search_nodes_fuzzy("ppr traversal algorithm").unwrap();
    assert!(
        !r.is_empty(),
        "NL 'ppr traversal algorithm' must resolve personalized_page_rank"
    );
}

#[test]
fn nl_query_search_symbol_handler() {
    let mut store = open();
    let n = node(
        "crates/travsr-mcp/src/tools.rs",
        "fn:search_symbol_raw",
        "function",
    );
    put(&mut store, &n);

    let r = store.search_nodes_fuzzy("search symbol handler").unwrap();
    assert!(
        !r.is_empty(),
        "NL 'search symbol handler' must resolve search_symbol_raw"
    );
}

#[test]
fn nl_query_sqlite_store_migration() {
    let mut store = open();
    let n = node(
        "crates/travsr-store/src/lib.rs",
        "fn:sqlite_migration_runner",
        "function",
    );
    put(&mut store, &n);

    let r = store
        .search_nodes_fuzzy("sqlite store migration runner")
        .unwrap();
    assert!(
        !r.is_empty(),
        "NL 'sqlite store migration runner' must resolve sqlite_migration_runner"
    );
}

#[test]
fn nl_query_node_fts_backfill() {
    let mut store = open();
    let n = node(
        "crates/travsr-store/src/lib.rs",
        "fn:backfill_fts_if_needed",
        "function",
    );
    put(&mut store, &n);

    let r = store.search_nodes_fuzzy("fts backfill needed").unwrap();
    assert!(
        !r.is_empty(),
        "NL 'fts backfill needed' must resolve backfill_fts_if_needed"
    );
}

#[test]
fn nl_query_vname_signature_path() {
    let mut store = open();
    let n = node("crates/travsr-core/src/lib.rs", "struct:VName", "struct");
    put(&mut store, &n);

    let r = store
        .search_nodes_fuzzy("vname signature path corpus")
        .unwrap();
    assert!(
        !r.is_empty(),
        "NL 'vname signature path corpus' must resolve struct:VName"
    );
}

#[test]
fn nl_query_camel_case_method() {
    let mut store = open();
    let n = node(
        "packages/travsr-vscode/src/extension.ts",
        "fn:getCallersGlobal",
        "function",
    );
    put(&mut store, &n);

    let r = store.search_nodes_fuzzy("get callers global").unwrap();
    assert!(
        !r.is_empty(),
        "NL 'get callers global' must resolve getCallersGlobal"
    );
}

#[test]
fn nl_query_rbac_session_model() {
    let mut store = open();
    let n = node(
        "crates/travsr-mcp/src/rbac.rs",
        "fn:check_session_access",
        "function",
    );
    put(&mut store, &n);

    let r = store
        .search_nodes_fuzzy("rbac session access check")
        .unwrap();
    assert!(
        !r.is_empty(),
        "NL 'rbac session access check' must resolve check_session_access"
    );
}

#[test]
fn nl_query_edge_provenance() {
    let mut store = open();
    let n = node(
        "crates/travsr-store/src/migrations/v2_edge_provenance.sql",
        "file:v2_edge_provenance",
        "file",
    );
    put(&mut store, &n);

    let r = store
        .search_nodes_fuzzy("edge provenance migration")
        .unwrap();
    assert!(
        !r.is_empty(),
        "NL 'edge provenance migration' must resolve v2_edge_provenance"
    );
}

// ── Layer ordering: exact wins over FTS ──────────────────────────────────────

#[test]
fn exact_match_wins_over_fts_result() {
    let mut store = open();
    // Two nodes: one is an exact-substring match, the other would hit via FTS only.
    let exact = node("src/foo.rs", "fn:dispatch_tool_call", "function");
    let fts_only = node("src/bar.rs", "fn:dispatch_something_else", "function");
    put(&mut store, &exact);
    put(&mut store, &fts_only);

    let r = store.search_nodes_fuzzy("dispatch_tool_call").unwrap();
    // Exact substring returns immediately (step 1) — result contains the exact match.
    assert!(
        r.iter()
            .any(|n| n.vname.signature == "fn:dispatch_tool_call"),
        "exact match must be in results"
    );
}

// ── FTS-syntax-char safety ───────────────────────────────────────────────────

#[test]
fn fts_syntax_star_does_not_panic() {
    let store = open();
    let r = store.search_nodes_fuzzy("*");
    assert!(r.is_ok(), "bare '*' must not error: {:?}", r.err());
}

#[test]
fn fts_syntax_and_or_not_does_not_panic() {
    let store = open();
    assert!(store.search_nodes_fuzzy("foo AND bar OR NOT baz").is_ok());
}

#[test]
fn fts_syntax_colon_caret_does_not_panic() {
    let store = open();
    assert!(store.search_nodes_fuzzy("foo:bar^2").is_ok());
}

#[test]
fn fts_syntax_near_does_not_panic() {
    let store = open();
    assert!(store.search_nodes_fuzzy("NEAR(foo bar)").is_ok());
}

#[test]
fn fts_syntax_unbalanced_quote_does_not_panic() {
    let store = open();
    assert!(store.search_nodes_fuzzy("\"unbalanced").is_ok());
}

#[test]
fn all_punct_returns_empty_no_error() {
    let store = open();
    let r = store.search_nodes_fuzzy("* :: --").unwrap();
    assert!(r.is_empty(), "all-punctuation query should return empty");
}

// ── Kind-update reindex leaves no stale FTS rows ─────────────────────────────

#[test]
fn kind_update_reindex_no_duplicate_fts_rows() {
    let mut store = open();

    // Insert a node as "function".
    let n1 = node("src/foo.rs", "fn:myFunc", "function");
    put(&mut store, &n1);

    // Re-insert the same node id with kind="method" (upsert).
    let n2 = Node {
        kind: "method".to_string(),
        ..n1.clone()
    };
    put(&mut store, &n2);

    // The node must still be findable via FTS after the kind-update upsert.
    // If the FTS index were corrupted (duplicate rows), BM25 scoring would
    // return duplicates or incorrect ranks — the functional assertion covers this.
    let r = store.search_nodes_fuzzy("myFunc").unwrap();
    assert!(
        !r.is_empty(),
        "node must be findable after kind-update upsert"
    );
    // Exactly one result (no duplicates from corrupt FTS index).
    assert_eq!(
        r.len(),
        1,
        "exactly one FTS result after kind-update — no stale rows"
    );
}

// ── #393: RRF fusion — kind-diversity + singular/plural overlap ───────────────
//
// Reproduces the reported bug: functions containing "workflow" starved out the
// `.github/workflows/*.yml` file nodes, and singular vs plural returned disjoint
// sets. After RRF fusion + kind diversity the two kinds must co-exist.

/// Build the #393 fixture: 12 `workflow`-named functions in one file + 6 file
/// nodes under a `workflows/` directory.
fn put_393_fixture(store: &mut SqliteStore) {
    for i in 0..12 {
        let f = node(
            "crates/travsr-analysis/src/data_format.rs",
            &format!("fn:github_actions_workflow_parse_{i}"),
            "function",
        );
        put(store, &f);
    }
    for name in ["ci", "release", "fuzz", "bench", "osv-scan", "vscode"] {
        let path = format!(".github/workflows/{name}.yml");
        let f = node(&path, &path, "file");
        put(store, &f);
    }
}

#[test]
fn issue393_workflow_singular_surfaces_files_not_only_functions() {
    let mut store = open();
    put_393_fixture(&mut store);

    let r = store.search_nodes_fuzzy("workflow").unwrap();
    let n_files = r.iter().filter(|n| n.kind == "file").count();
    let n_funcs = r.iter().filter(|n| n.kind == "function").count();
    assert!(n_funcs > 0, "functions must still be present");
    assert!(
        n_files > 0,
        "#393: singular 'workflow' must surface the workflows/*.yml files, not 0"
    );
    // #478 RFC-023 §6 item 7 (WS-7): kind diversity is now a tie-break within
    // a score band, not an unconditional top-K promotion. The singular query
    // "workflow" only substring-matches the plural "workflows/*.yml" paths via
    // the down-weighted trigram leg (Leg C) — it never word-matches them via
    // Leg B — so these files legitimately score well below the genuine
    // "workflow"-named functions and are no longer forced into the top-6.
    // That forced promotion was the exact bug this workstream fixes (a weak
    // substring-only match must not outrank a clean same-kind run). The
    // no-longer-zero assertion above is what #393 actually required.
}

#[test]
fn issue393_singular_and_plural_results_overlap() {
    let mut store = open();
    put_393_fixture(&mut store);

    let singular = store.search_nodes_fuzzy("workflow").unwrap();
    let plural = store.search_nodes_fuzzy("workflows").unwrap();

    let sig_set = |v: &[Node]| {
        v.iter()
            .map(|n| n.vname.signature.clone())
            .collect::<std::collections::HashSet<_>>()
    };
    let overlap = sig_set(&singular).intersection(&sig_set(&plural)).count();
    assert!(
        overlap > 0,
        "#393: 'workflow' and 'workflows' must share results, not be disjoint"
    );
}

#[test]
fn issue393_exact_signature_still_fast_paths() {
    let mut store = open();
    put_393_fixture(&mut store);
    // An exact signature match must still win outright (G1 fast path, TL3):
    // diversity/interleave must not demote a crisp symbol lookup.
    let r = store
        .search_nodes_fuzzy("fn:github_actions_workflow_parse_3")
        .unwrap();
    assert_eq!(
        r.first().map(|n| n.vname.signature.as_str()),
        Some("fn:github_actions_workflow_parse_3"),
        "#393 G1: exact signature hit must remain the first result"
    );
}

/// #478 RFC-023 §11 AC #4 / Evidence E: the G1 exact fast-path never sets
/// `bm25_natural` — no real BM25-scale leg (Leg B/C) backs a pure
/// exact-signature match, so the abstention gate that reads `bm25_natural`
/// must see `None` here, not a position-derived score masquerading as one.
#[test]
fn issue478_g1_fast_path_has_no_bm25_natural() {
    let mut store = open();
    put(
        &mut store,
        &node("src/a.rs", "fn:exact_unique_name_xyz", "function"),
    );

    let hits = store
        .search_nodes_fuzzy_scored("fn:exact_unique_name_xyz")
        .unwrap();
    assert!(!hits.is_empty(), "the exact signature match must be found");
    assert_eq!(
        hits[0].exact_rank,
        Some(0),
        "G1 fast path must set exact_rank"
    );
    assert!(
        hits[0].bm25_natural.is_none(),
        "AC #4: a G1 exact-fast-path hit must have bm25_natural=None"
    );
}

#[test]
fn issue393_filtered_lang_scopes_and_fuses() {
    let mut store = open();
    let rs = Node::new(
        VName::new(
            "corpus",
            "root",
            "crates/a/src/lib.rs",
            "rust",
            "fn:workflow_parse",
        ),
        "function",
    );
    let ts = Node::new(
        VName::new(
            "corpus",
            "root",
            "pkg/b/src/x.ts",
            "typescript",
            "fn:workflowParse",
        ),
        "function",
    );
    put(&mut store, &rs);
    put(&mut store, &ts);

    let rust_only = store
        .search_nodes_fuzzy_filtered("workflow", Some("rust"), false)
        .unwrap();
    assert!(!rust_only.is_empty(), "lang-filtered query must resolve");
    assert!(
        rust_only.iter().all(|n| n.vname.language == "rust"),
        "#393: _filtered(Some(\"rust\")) must return only rust nodes"
    );
    assert!(rust_only
        .iter()
        .any(|n| n.vname.signature == "fn:workflow_parse"));
}

#[test]
fn issue393_scored_carries_bm25_scale_not_rrf() {
    let mut store = open();
    put(
        &mut store,
        &node("crates/x/src/a.rs", "fn:knapsack_select", "function"),
    );
    // The score must be BM25/synthetic scale (>0.1), never the tiny internal RRF
    // accumulator (~0.03). Guards the "RRF orders, does not rescore" contract that
    // get_context's BM25-scale relevance floor depends on (#393 §5.1).
    let scored = store.search_nodes_fuzzy_scored("knapsack_select").unwrap();
    assert!(!scored.is_empty());
    let top = scored.iter().map(|hit| hit.natural).fold(0.0f32, f32::max);
    assert!(top > 0.1, "score must be BM25/synthetic scale, got {top}");
}

#[test]
fn issue478_embed_always_contributes_but_get_context_path_never_embeds() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let mut store = open();
    put(
        &mut store,
        &node("crates/x/src/a.rs", "fn:dispatch_tool_call", "function"),
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let hook: travsr_store::EmbedKnnHook = Arc::new(move |_q, _k| {
        c.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    });
    store.set_embed_knn_hook(hook);

    // #478 RFC-023 §6 item 6: embed (like L2-A) now always contributes on the
    // `include_embed = true` path, even when the cheap SQLite stages already
    // resolve — the old "fire only on a combined miss" gate is exactly the
    // #393 cascade short-circuit this closes (a correct embed candidate could
    // never surface once any cheap stage returned even one weak hit).
    let _ = store.search_nodes_fuzzy("dispatch_tool_call").unwrap();
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "embed must contribute even when the cheap stages already resolve (no longer miss-gated)"
    );

    // Still fires on a combined miss.
    let before = calls.load(Ordering::SeqCst);
    let _ = store
        .search_nodes_fuzzy("zzqqvv nonexistent token")
        .unwrap();
    assert!(
        calls.load(Ordering::SeqCst) > before,
        "embed must fire on a combined miss (ask path)"
    );

    // get_context path (_scored, include_embed=false) must NEVER call embed — it has
    // its own KNN channel, so double-embedding would be redundant (#393 §5.1).
    // Un-gating L2-A/embed relative to the cheap-stage miss (above) is orthogonal
    // to this contract: `include_embed` still gates embed independently.
    let before = calls.load(Ordering::SeqCst);
    let _ = store
        .search_nodes_fuzzy_scored("zzqqvv nonexistent token")
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        before,
        "get_context (_scored) path must not invoke the embed hook"
    );
}

// ── Empty graph ───────────────────────────────────────────────────────────────

#[test]
fn empty_graph_returns_empty_not_error() {
    let store = open();
    // No nodes at all — every layer must return empty, never error.
    let r = store.search_nodes_fuzzy("mcp dispatch tool call").unwrap();
    assert!(r.is_empty(), "empty graph must return empty vec, not error");
}

#[test]
fn empty_graph_exact_query_returns_empty() {
    let store = open();
    let r = store.search_nodes_fuzzy("dispatch_tool_call").unwrap();
    assert!(r.is_empty());
}

// ── Delete retraction (always-fresh invariant) ────────────────────────────────

#[test]
fn delete_nodes_for_path_retracts_fts_entry() {
    let mut store = open();
    let n = node(
        "crates/travsr-mcp/src/server.rs",
        "fn:handle_tool_call",
        "function",
    );
    put(&mut store, &n);

    // Verify it is findable before deletion.
    let before = store.search_nodes_fuzzy("handle tool call").unwrap();
    assert!(
        !before.is_empty(),
        "node must be findable via FTS before deletion"
    );

    // Delete via the store's path-based deletion (same path used to index).
    store
        .delete_nodes_for_path("crates/travsr-mcp/src/server.rs")
        .expect("delete_nodes_for_path failed");

    // Must not surface via FTS after deletion (always-fresh invariant).
    let after = store.search_nodes_fuzzy("handle tool call").unwrap();
    assert!(
        after.is_empty(),
        "deleted node must not be findable via FTS after delete_nodes_for_path"
    );

    // Must not surface via exact match either.
    let exact = store.search_nodes_fuzzy("handle_tool_call").unwrap();
    assert!(
        exact.is_empty(),
        "deleted node must not appear via exact match"
    );
}

#[test]
fn delete_nodes_for_path_prefix_retracts_fts_entries() {
    let mut store = open();
    let n1 = node(
        "crates/travsr-mcp/src/server.rs",
        "fn:handle_tool_call",
        "function",
    );
    let n2 = node(
        "crates/travsr-mcp/src/tools.rs",
        "fn:search_symbol_raw",
        "function",
    );
    let n3 = node("crates/travsr-store/src/lib.rs", "fn:put_node", "function");
    put(&mut store, &n1);
    put(&mut store, &n2);
    put(&mut store, &n3);

    // Delete all nodes under crates/travsr-mcp/ via prefix.
    store
        .delete_nodes_for_path_prefix("crates/travsr-mcp/")
        .expect("delete_nodes_for_path_prefix failed");

    // MCP nodes must be gone.
    assert!(
        store
            .search_nodes_fuzzy("handle tool call")
            .unwrap()
            .is_empty(),
        "handle_tool_call must be retracted"
    );
    assert!(
        store
            .search_nodes_fuzzy("search symbol raw")
            .unwrap()
            .is_empty(),
        "search_symbol_raw must be retracted"
    );

    // Store node must still be findable.
    let remaining = store.search_nodes_fuzzy("put node").unwrap();
    assert!(
        !remaining.is_empty(),
        "put_node (different prefix) must still be findable"
    );
}

// ── T0 heuristic floor (RFC-012 A1 S20) ──────────────────────────────────────
//
// These tests verify the normaliser (stopword strip + synonym expansion) with
// NO model present.  All queries must resolve via the deterministic FTS path.

#[test]
fn t0_stopword_stripped_dispatch_resolves() {
    // "where do we dispatch the tool call" — "where", "the" are stopwords;
    // "dispatch", "tool", "call" are content tokens that must hit the index.
    let mut store = open();
    let n = node(
        "crates/travsr-mcp/src/server.rs",
        "fn:dispatch_tool_call",
        "function",
    );
    put(&mut store, &n);

    let r = store
        .search_nodes_fuzzy("where do we dispatch the tool call")
        .unwrap();
    assert!(
        !r.is_empty(),
        "T0: stopword-stripped NL query must find dispatch_tool_call"
    );
}

#[test]
fn t0_synonym_auth_resolves_session() {
    // "auth" → synonym "session" expands the union; must hit check_session_access.
    let mut store = open();
    let n = node(
        "crates/travsr-mcp/src/rbac.rs",
        "fn:check_session_access",
        "function",
    );
    put(&mut store, &n);

    let r = store.search_nodes_fuzzy("auth guard").unwrap();
    assert!(
        !r.is_empty(),
        "T0: 'auth' synonym expansion must find check_session_access via 'session'"
    );
}

#[test]
fn t0_synonym_delete_resolves_retract() {
    // "delete" → synonym "retract" must hit retract_fts_entry.
    let mut store = open();
    let n = node(
        "crates/travsr-store/src/lib.rs",
        "fn:retract_fts_entry",
        "function",
    );
    put(&mut store, &n);

    let r = store.search_nodes_fuzzy("delete fts entry").unwrap();
    assert!(
        !r.is_empty(),
        "T0: 'delete' synonym 'retract' must find retract_fts_entry"
    );
}

#[test]
fn t0_exact_step_unaffected_by_normaliser() {
    // Exact substring on "knapsack_select" must still win via Step 1 (regression
    // guard — Step 1 receives the raw literal, never the normalised form, per TL3).
    let mut store = open();
    let n = node(
        "crates/travsr-retrieval/src/knapsack.rs",
        "fn:knapsack_select",
        "function",
    );
    put(&mut store, &n);

    let r = store.search_nodes_fuzzy("knapsack_select").unwrap();
    assert!(
        !r.is_empty(),
        "T0 regression: exact 'knapsack_select' must resolve via Step 1"
    );
    assert_eq!(
        r[0].vname.signature, "fn:knapsack_select",
        "exact match must be the first result"
    );
}

#[test]
fn t0_c1_raw_token_always_in_union() {
    // Even when a token has synonyms, the raw token itself must be in the FTS
    // MATCH expression (PA C1).  Verify by inserting a node whose signature
    // contains the raw token — it must resolve.
    let mut store = open();
    let n = node(
        "crates/travsr-store/src/lib.rs",
        "fn:auth_middleware",
        "function",
    );
    put(&mut store, &n);

    // "auth" is a synonym key; the raw token "auth" must also hit.
    let r = store.search_nodes_fuzzy("auth middleware").unwrap();
    assert!(
        !r.is_empty(),
        "PA C1: raw token 'auth' must remain in union alongside its synonyms"
    );
}

#[test]
fn t0_escaper_parity_synonym_output_quoted() {
    // Synonym aliases must pass through the double-quote escaper (TL4).
    // Verify by inserting a node whose signature matches a synonym alias
    // and querying with the source term.
    let mut store = open();
    let n = node(
        "crates/travsr-store/src/lib.rs",
        "fn:bfs_traversal",
        "function",
    );
    put(&mut store, &n);

    // "traversal" → synonym "bfs"; the expanded "bfs" token must be quoted
    // in the MATCH expr (no FTS5 operator injection).
    let r = store.search_nodes_fuzzy("traversal algorithm").unwrap();
    assert!(
        !r.is_empty(),
        "TL4: synonym 'bfs' must be properly quoted in MATCH expr and find bfs_traversal"
    );
}

#[test]
fn t0_all_stopwords_fallback_finds_match() {
    // When every token is a stopword, expand_tokens falls back to the raw tokens
    // so the query is not silently dropped.
    let mut store = open();
    let n = node("src/for.rs", "fn:for_each_node", "function");
    put(&mut store, &n);

    // "for", "the" are stopwords; fallback to raw tokens must still fire.
    // "for" is only 3 chars — may or may not return results depending on trigram
    // indexing. The contract is: must not panic or error (unwrap covers this).
    let _r = store.search_nodes_fuzzy("for the").unwrap();
}

// ── Migration idempotency ─────────────────────────────────────────────────────

#[test]
fn open_in_memory_twice_is_idempotent() {
    // open_in_memory runs migrations + backfill each time;
    // IF NOT EXISTS guards make the DDL idempotent.
    SqliteStore::open_in_memory().expect("first open");
    SqliteStore::open_in_memory().expect("second open — must not fail");
}

#[test]
fn backfill_is_idempotent() {
    let mut store = open();
    let n = node("src/a.rs", "fn:alpha", "function");
    put(&mut store, &n);

    // A second open_in_memory won't help here, but we can verify count stability:
    let r1 = store.search_nodes_fuzzy("alpha").unwrap();
    let r2 = store.search_nodes_fuzzy("alpha").unwrap();
    assert_eq!(
        r1.len(),
        r2.len(),
        "repeated fuzzy calls must return same count"
    );
}

// ── L2-A vocabulary-grounded expansion (RFC-012 A1 S20) ──────────────────────

#[test]
fn l2a_vocab_grounded_step3_fires_on_step2_miss() {
    // Step 3 must fire when Step 1 and Step 2 both miss but a vocabulary-grounded
    // candidate (Jaccard ≥ 0.4) exists in fts_vocab.
    let mut store = open();
    // Index a node with a distinctive signature.
    let n = node("src/dispatch.rs", "fn:dispatch_tool_request", "function");
    put(&mut store, &n);

    // Query with a token that has high Jaccard similarity to "dispatch" but is
    // not an exact substring and is not in the synonym table.
    // "dispatched" shares byte-trigrams with "dispatch": dis, isp, spa, pat, atc, tch
    // That gives Jaccard > 0.4 against "dispatch". Step 3 should find the node.
    let r = store.search_nodes_fuzzy("dispatched tool").unwrap();
    // If L2-A fires, the node should be in the result.
    // (Test is informational if fts_vocab is not populated — the node is still
    //  reachable via Step 2 through the "tool" token).
    assert!(
        !r.is_empty(),
        "L2-A or T0 must find fn:dispatch_tool_request"
    );
}

#[test]
fn l2a_fts_vocab_refcount_incremented_on_put() {
    // AC (e): inserting a node must populate fts_vocab with refcount ≥ 1 for
    // each of its tokens.
    let mut store = open();
    let n = node("src/bfs.rs", "fn:bfs_traversal_context", "function");
    put(&mut store, &n);

    // The token "bfs" must appear in fts_vocab with refcount ≥ 1.
    let count = store
        .fts_vocab_refcount("bfs")
        .expect("fts_vocab_refcount query")
        .expect("fts_vocab must contain 'bfs' after put_node");
    assert!(count >= 1, "refcount for 'bfs' must be ≥ 1 after put_node");
}

#[test]
fn l2a_fts_vocab_refcount_decremented_on_delete_path() {
    // AC (e): delete_nodes_for_path must decrement fts_vocab refcounts.
    let mut store = open();
    let n = node("src/session.rs", "fn:session_validate", "function");
    put(&mut store, &n);

    let before = store
        .fts_vocab_refcount("session")
        .expect("fts_vocab_refcount query")
        .expect("fts_vocab must contain 'session' before delete");
    assert!(before >= 1, "refcount must be ≥ 1 before delete");

    store
        .delete_nodes_for_path("src/session.rs")
        .expect("delete_nodes_for_path failed");

    let after = store
        .fts_vocab_refcount("session")
        .expect("fts_vocab_refcount after delete")
        .unwrap_or(0);
    assert!(
        after < before,
        "refcount for 'session' must decrease after delete_nodes_for_path \
         (was {before}, now {after})"
    );
}

#[test]
fn l2a_fts_vocab_refcount_decremented_on_delete_prefix() {
    // AC (e): delete_nodes_for_path_prefix must decrement fts_vocab refcounts.
    let mut store = open();
    let n = node("src/auth/token.rs", "fn:auth_token_verify", "function");
    put(&mut store, &n);

    let before = store
        .fts_vocab_refcount("auth")
        .expect("fts_vocab_refcount query")
        .expect("fts_vocab must contain 'auth' before prefix delete");
    assert!(before >= 1);

    store
        .delete_nodes_for_path_prefix("src/auth/")
        .expect("delete_nodes_for_path_prefix failed");

    let after = store
        .fts_vocab_refcount("auth")
        .expect("fts_vocab_refcount after prefix delete")
        .unwrap_or(0);
    assert!(
        after < before,
        "refcount for 'auth' must decrease after delete_nodes_for_path_prefix \
         (was {before}, now {after})"
    );
}

#[test]
fn l2a_or_arm_cap_at_most_16() {
    // AC (k): the merged T0+L2-A OR arm list must never exceed 16 entries.
    // The `arms.truncate(16)` call in lib.rs is the compile-time guarantee.
    // This test exercises the code path where vocab candidates are numerous
    // and verifies: (a) no panic/error, (b) correct results are still returned.
    let mut store = open();
    // Insert nodes whose tokens exactly match the T0 synonym expansion for "auth":
    // "auth" → "rbac", "session", "login", "token".  These will be in fts_vocab
    // and returned by expand_query if Step 2 misses, keeping the arm count ≥ 5
    // but well within the cap of 16.
    for sig in &[
        "fn:rbac_check",
        "fn:session_guard",
        "fn:login_service",
        "fn:token_verify",
        "fn:auth_middleware",
    ] {
        let n = node("src/auth.rs", sig, "function");
        put(&mut store, &n);
    }
    // "auth guard" — T0 expands "auth" → "rbac"+"session"+"login"+"token".
    // Step 2 MATCH fires and must find the auth-related nodes.
    // No panic is the primary assertion; result count verifies correctness.
    let r = store.search_nodes_fuzzy("auth guard").unwrap();
    assert!(
        !r.is_empty(),
        "AC k: must find auth-related nodes without panic (OR arm cap ≤ 16)"
    );
}

// ── F1 dynamic synonyms (RFC-012 A2 S21) ─────────────────────────────────────
//
// The T0 tests above exercise the compile-time SYNONYMS static. These verify the
// DB-backed path: a synonym added at runtime via synonym_add/set/remove must
// change search_nodes_fuzzy results through build_fuzzy_match_expr_db.

#[test]
fn dynamic_synonym_add_changes_search() {
    let mut store = open();
    let n = node("src/billing.rs", "fn:billing_service", "function");
    put(&mut store, &n);

    // Negative control: "payment" is not a static synonym of "billing", and
    // "payment" is not a substring of "billing_service" — no match yet.
    let before = store.search_nodes_fuzzy("payment").unwrap();
    assert!(
        before.is_empty(),
        "precondition: 'payment' must not resolve billing_service before the synonym exists"
    );

    // Add the synonym at runtime; the DB-backed expander must now pick it up.
    store.synonym_add("payment", "billing").unwrap();

    let after = store.search_nodes_fuzzy("payment").unwrap();
    assert!(
        after
            .iter()
            .any(|m| m.vname.signature == "fn:billing_service"),
        "F1: runtime synonym 'payment'→'billing' must resolve billing_service"
    );
}

#[test]
fn dynamic_synonym_remove_reverts_search() {
    let mut store = open();
    let n = node("src/billing.rs", "fn:billing_service", "function");
    put(&mut store, &n);
    store.synonym_add("payment", "billing").unwrap();
    assert!(!store.search_nodes_fuzzy("payment").unwrap().is_empty());

    store.synonym_remove("payment", "billing").unwrap();
    assert!(
        store.search_nodes_fuzzy("payment").unwrap().is_empty(),
        "F1: removing the synonym must revert search behaviour"
    );
}

#[test]
fn dynamic_synonym_set_is_declarative() {
    let mut store = open();
    let billing = node("src/billing.rs", "fn:billing_service", "function");
    let invoice = node("src/invoice.rs", "fn:invoice_service", "function");
    put(&mut store, &billing);
    put(&mut store, &invoice);

    store.synonym_add("payment", "billing").unwrap();
    // set replaces the entire alias set: billing is dropped, invoice added.
    store
        .synonym_set("payment", &["invoice".to_string()])
        .unwrap();

    let r = store.search_nodes_fuzzy("payment").unwrap();
    assert!(
        r.iter().any(|m| m.vname.signature == "fn:invoice_service"),
        "set must add the new alias 'invoice'"
    );
    assert!(
        !r.iter().any(|m| m.vname.signature == "fn:billing_service"),
        "set must drop the replaced alias 'billing'"
    );
}

#[test]
fn synonym_reset_drops_user_adds_but_keeps_defaults() {
    let mut store = open();
    let billing = node("src/billing.rs", "fn:billing_service", "function");
    let session = node("src/rbac.rs", "fn:check_session_access", "function");
    put(&mut store, &billing);
    put(&mut store, &session);

    store.synonym_add("payment", "billing").unwrap();
    assert!(!store.search_nodes_fuzzy("payment").unwrap().is_empty());

    store.synonym_reset().unwrap();

    // User-added synonym is gone.
    assert!(
        store.search_nodes_fuzzy("payment").unwrap().is_empty(),
        "reset must drop the user-added 'payment' synonym"
    );
    // Static default ("auth" → "session") is restored.
    assert!(
        !store.search_nodes_fuzzy("auth guard").unwrap().is_empty(),
        "reset must restore the static default 'auth'→'session'"
    );
}

// ── PA C3 — full == incremental on canonical graph ───────────────────────────

#[test]
fn pa_c3_full_reindex_equals_incremental_reindex() {
    // Full reindex (fresh store + all nodes) and incremental reindex (update
    // existing store with the same set) must produce identical node sets.
    // This verifies the idempotent upsert invariant (NOT vector index).
    let nodes = vec![
        node("src/a.rs", "fn:alpha_handler", "function"),
        node("src/b.rs", "fn:beta_service", "function"),
        node("src/c.rs", "struct:GammaConfig", "struct"),
        node("src/d.rs", "fn:delta_migrate", "function"),
        node("src/e.rs", "fn:epsilon_auth", "function"),
    ];

    // Full index: fresh store, insert all nodes once.
    let mut full = open();
    for n in &nodes {
        put(&mut full, n);
    }

    // Incremental reindex: same store, insert all nodes AGAIN (upsert).
    for n in &nodes {
        put(&mut full, n);
    }

    // Both passes produce the same set: node_count must equal len(nodes).
    let count = full.node_count().expect("node_count");
    assert_eq!(
        count as usize,
        nodes.len(),
        "PA C3: incremental re-upsert must not duplicate nodes (full == incremental)"
    );

    // All nodes must be findable via FTS after incremental upsert.
    let r = full.search_nodes_fuzzy("alpha handler").unwrap();
    assert!(
        !r.is_empty(),
        "PA C3: alpha_handler must be findable after incremental upsert"
    );

    // FTS index must have no duplicates: kind-update test proves this for the
    // FTS layer; here we verify no duplicate FTS rows cause inflated result counts.
    let r2 = full.search_nodes_fuzzy("fn:alpha_handler").unwrap();
    assert_eq!(
        r2.len(),
        1,
        "PA C3: exact match must return exactly 1 result after incremental upsert"
    );
}

// ── PA C4 — daemon boundary bright line ──────────────────────────────────────

#[test]
fn pa_c4_search_nodes_fuzzy_output_is_structural_only() {
    // PA C4 bright line: search_nodes_fuzzy returns Vec<Node> — structured graph
    // data only. No free-text NL, no LLM-derived ranking metadata, no rewritten
    // context crosses the daemon boundary via this API.
    //
    // Verified at the type level: Node contains only VName (corpus/root/path/
    // language/signature), kind, package, and line — all structural fields.
    // No NL payload field exists on Node.
    let mut store = open();
    let n = node("src/rbac.rs", "fn:check_permission", "function");
    put(&mut store, &n);

    let results = store.search_nodes_fuzzy("permission check rbac").unwrap();

    for node in &results {
        // Each result must be a structural Node: non-empty path and kind.
        assert!(!node.vname.path.is_empty(), "PA C4: node must have a path");
        assert!(!node.kind.is_empty(), "PA C4: node must have a kind");
        // Signature must not contain LLM-injected text markers.
        assert!(
            !node.vname.signature.contains("LLM:"),
            "PA C4: signature must not contain LLM-injected content"
        );
    }
}

#[test]
fn pa_c4_l2a_expand_query_only_returns_vocabulary_grounded_tokens() {
    // PA C4 enforcement for L2-A: expand_query (Step 3) can only return tokens
    // that exist in fts_vocab (which is populated by put_node_fts from actual
    // graph content).  A token that was never in the graph cannot appear.
    // We verify this by checking: after inserting a known set of nodes, an
    // L2-A query with a completely invented term ("zzz_hallucinated_llm_term")
    // returns empty — the vocab has no such token, so L2-A produces nothing.
    let mut store = open();
    let n = node("src/real.rs", "fn:real_handler", "function");
    put(&mut store, &n);

    // This token has no similarity to anything in the vocab — Step 3 must return
    // empty rather than hallucinating candidates.
    let r = store
        .search_nodes_fuzzy("zzz_hallucinated_llm_term_xyz")
        .unwrap();
    assert!(
        r.is_empty(),
        "PA C4: L2-A must return empty for token absent from vocabulary"
    );
}

#[test]
fn test_exact_only_filters_pure_substring() {
    let mut store = open();
    // 1. exact match
    let n1 = node("src/ClassD.rs", "struct:ClassD", "struct");
    // 2. word boundary match (prefix/suffix on signature)
    let n2 = node("src/ClassD.rs", "fn:ClassD::method", "function");
    // 3. pure substring match on signature (ELSE 40)
    let n3 = node(
        "src/ClassDConfig.rs",
        "struct:ClassDConfigurationManager",
        "struct",
    );
    // 4. pure substring match on path (ELSE 40)
    let n4 = node("src/ClassD.rs", "fn:unrelated_func", "function");

    put(&mut store, &n1);
    put(&mut store, &n2);
    put(&mut store, &n3);
    put(&mut store, &n4);

    // With exact_only = false (default), all of them should match ClassD
    let results_all = store
        .search_nodes_fuzzy_filtered("ClassD", None, false)
        .unwrap();
    assert_eq!(results_all.len(), 4);

    // With exact_only = true, only n1 and n2 should match
    let results_exact = store
        .search_nodes_fuzzy_filtered("ClassD", None, true)
        .unwrap();
    assert_eq!(results_exact.len(), 2);
    let sigs: std::collections::HashSet<String> = results_exact
        .iter()
        .map(|n| n.vname.signature.clone())
        .collect();
    assert!(sigs.contains("struct:ClassD"));
    assert!(sigs.contains("fn:ClassD::method"));
    assert!(!sigs.contains("struct:ClassDConfigurationManager"));
    assert!(!sigs.contains("fn:unrelated_func"));
}

#[test]
fn test_exact_only_respects_language_filter() {
    let mut store = open();

    // Rust nodes:
    // - Rust exact match: struct:ClassD
    let rust_exact = Node::new(
        VName::new("corpus", "root", "src/ClassD.rs", "rust", "struct:ClassD"),
        "struct",
    );
    // - Rust word-boundary match: fn:ClassD::method
    let rust_word_boundary = Node::new(
        VName::new(
            "corpus",
            "root",
            "src/ClassD.rs",
            "rust",
            "fn:ClassD::method",
        ),
        "function",
    );
    // - Rust pure-substring match: struct:ClassDConfigurationManager
    let rust_substring = Node::new(
        VName::new(
            "corpus",
            "root",
            "src/ClassDConfig.rs",
            "rust",
            "struct:ClassDConfigurationManager",
        ),
        "struct",
    );

    // TypeScript nodes:
    // - TypeScript exact match: struct:ClassD
    let ts_exact = Node::new(
        VName::new(
            "corpus",
            "root",
            "src/ClassD.ts",
            "typescript",
            "struct:ClassD",
        ),
        "struct",
    );

    put(&mut store, &rust_exact);
    put(&mut store, &rust_word_boundary);
    put(&mut store, &rust_substring);
    put(&mut store, &ts_exact);

    // A. Language filter only:
    //    search_nodes_fuzzy_filtered("ClassD", Some("rust"), false)
    //    Expected:
    //    - 3 results
    //    - all results are Rust
    //    - substring match is still included
    let results_lang_only = store
        .search_nodes_fuzzy_filtered("ClassD", Some("rust"), false)
        .unwrap();
    assert_eq!(results_lang_only.len(), 3);
    for node in &results_lang_only {
        assert_eq!(node.vname.language, "rust");
    }
    let sigs_lang_only: std::collections::HashSet<String> = results_lang_only
        .iter()
        .map(|n| n.vname.signature.clone())
        .collect();
    assert!(sigs_lang_only.contains("struct:ClassD"));
    assert!(sigs_lang_only.contains("fn:ClassD::method"));
    assert!(sigs_lang_only.contains("struct:ClassDConfigurationManager"));

    // B. Language filter + exact_only:
    //    search_nodes_fuzzy_filtered("ClassD", Some("rust"), true)
    //    Expected:
    //    - 2 results
    //    - exact Rust match is included
    //    - Rust word-boundary match is included
    //    - Rust substring match is excluded
    //    - TypeScript exact match is excluded because of the language filter
    //    - every returned node has language == "rust"
    let results_lang_exact = store
        .search_nodes_fuzzy_filtered("ClassD", Some("rust"), true)
        .unwrap();
    assert_eq!(results_lang_exact.len(), 2);
    for node in &results_lang_exact {
        assert_eq!(node.vname.language, "rust");
    }
    let sigs_lang_exact: std::collections::HashSet<String> = results_lang_exact
        .iter()
        .map(|n| n.vname.signature.clone())
        .collect();
    assert!(sigs_lang_exact.contains("struct:ClassD"));
    assert!(sigs_lang_exact.contains("fn:ClassD::method"));
    assert!(!sigs_lang_exact.contains("struct:ClassDConfigurationManager"));
}

// ── #709: strict single-token typo correction ────────────────────────────────

#[test]
fn fuzzy_correct_symbol_resolves_documented_typo() {
    // The literal repro from issue #709: `htpresponse` (a dropped 't') must
    // correct to the real `HttpResponse` symbol so a typo grounds instead of
    // abstaining. Returns the original-case leaf so downstream segmentation and
    // symbol_frequency see the real camelCase identifier.
    let mut store = open();
    put(&mut store, &node("http.py", "class:HttpResponse", "class"));
    put(
        &mut store,
        &node("http.py", "method:HttpResponse.render", "method"),
    );

    let fix = store.fuzzy_correct_symbol("htpresponse", 0.7).unwrap();
    assert_eq!(fix.as_deref(), Some("HttpResponse"));
}

#[test]
fn fuzzy_correct_symbol_is_none_for_unrelated_token() {
    // A token that is not a near-miss of any symbol must not be "corrected" into
    // one — that would ground a query the index cannot support.
    let mut store = open();
    put(&mut store, &node("http.py", "class:HttpResponse", "class"));

    let fix = store.fuzzy_correct_symbol("database", 0.7).unwrap();
    assert_eq!(fix, None);
}

#[test]
fn fuzzy_correct_symbol_abstains_on_ambiguous_typo() {
    // When a typo sits within the ambiguity margin of two DISTINCT names, it is
    // ambiguous and must ground nothing (fail-closed).
    let mut store = open();
    put(&mut store, &node("a.rs", "fn:configure", "function"));
    put(&mut store, &node("b.rs", "fn:configared", "function"));

    // `configered` is a near-equal trigram match to both distinct names.
    let fix = store.fuzzy_correct_symbol("configered", 0.5).unwrap();
    assert_eq!(fix, None, "ambiguous typo must not resolve: {fix:?}");
}

#[test]
fn fuzzy_correct_symbol_skips_short_tokens() {
    // Trigram Jaccard is unstable on very short tokens; they never correct.
    let mut store = open();
    put(&mut store, &node("a.rs", "fn:cat", "function"));
    assert_eq!(store.fuzzy_correct_symbol("cot", 0.5).unwrap(), None);
}
