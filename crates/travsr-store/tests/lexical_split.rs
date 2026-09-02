//! #478 RFC-023 WS-3 — schema v21 (`nodes_fts_words` + `nodes.is_noise`)
//! integration tests: write-path population, retraction on delete, and
//! migration idempotency.

use travsr_core::{Node, VName};
use travsr_store::{SqliteStore, Store as _};

fn node(path: &str, sig: &str, kind: &str) -> Node {
    Node::new(VName::new("corpus", "root", path, "rust", sig), kind)
}

fn open() -> SqliteStore {
    SqliteStore::open_in_memory().expect("open_in_memory failed")
}

#[test]
fn put_node_populates_fts_words_map_word_segmented() {
    let mut store = open();
    let n = node(
        "crates/travsr-store/src/lib.rs",
        "fn:SqliteStore.exec_ddl",
        "method",
    );
    let id = store.put_node(&n).unwrap();

    let (sig_words, path_words) = store
        .fts_words_entry(id)
        .unwrap()
        .expect("nodes_fts_words_map row must exist after put_node");
    assert!(sig_words.split_whitespace().any(|t| t == "sqlite"));
    assert!(sig_words.split_whitespace().any(|t| t == "store"));
    assert!(sig_words.split_whitespace().any(|t| t == "exec"));
    assert!(path_words.split_whitespace().any(|t| t == "lib"));
}

#[test]
fn put_node_sets_is_noise_false_for_src_function() {
    let mut store = open();
    let n = node("crates/travsr-store/src/lib.rs", "fn:real_impl", "function");
    let id = store.put_node(&n).unwrap();
    assert_eq!(store.is_noise_flag(id).unwrap(), Some(false));
}

#[test]
fn put_node_sets_is_noise_true_for_test_path() {
    let mut store = open();
    let n = node(
        "crates/travsr-store/tests/fuzzy.rs",
        "fn:some_test",
        "function",
    );
    let id = store.put_node(&n).unwrap();
    assert_eq!(store.is_noise_flag(id).unwrap(), Some(true));
}

#[test]
fn put_node_sets_is_noise_true_for_crate_kind() {
    let mut store = open();
    let n = node("Cargo.toml", "pkg:serde@1.0", "crate");
    let id = store.put_node(&n).unwrap();
    assert_eq!(store.is_noise_flag(id).unwrap(), Some(true));
}

#[test]
fn fts_words_node_count_matches_node_count_after_writes() {
    let mut store = open();
    for i in 0..5 {
        let n = node(&format!("src/f{i}.rs"), &format!("fn:f{i}"), "function");
        store.put_node(&n).unwrap();
    }
    assert_eq!(store.node_count().unwrap(), 5);
    assert_eq!(store.fts_words_node_count().unwrap(), 5);
}

#[test]
fn reindex_replace_retracts_and_repopulates_fts_words() {
    let mut store = open();
    let old = node("src/a.rs", "fn:old_name", "function");
    let old_id = store.put_node(&old).unwrap();
    assert!(store.fts_words_entry(old_id).unwrap().is_some());

    let new = node("src/a.rs", "fn:new_name", "function");
    store
        .reindex_replace(
            "corpus",
            "src/a.rs",
            std::slice::from_ref(&new),
            &[],
            "newhash",
            None,
        )
        .unwrap();

    // Old node's word-map row is gone (retracted).
    assert!(store.fts_words_entry(old_id).unwrap().is_none());
    // New node has its own word-map row.
    let new_id = new.vname.id();
    let (sig_words, _) = store
        .fts_words_entry(new_id)
        .unwrap()
        .expect("new node must have a nodes_fts_words_map row");
    assert!(sig_words.split_whitespace().any(|t| t == "new"));
    assert_eq!(store.fts_words_node_count().unwrap(), 1);
}

#[test]
fn delete_file_retracts_fts_words_map() {
    let mut store = open();
    let n = node("src/gone.rs", "fn:gone", "function");
    let id = store.put_node(&n).unwrap();
    assert!(store.fts_words_entry(id).unwrap().is_some());

    store.delete_file("corpus", "src/gone.rs").unwrap();

    assert!(store.fts_words_entry(id).unwrap().is_none());
    assert_eq!(store.fts_words_node_count().unwrap(), 0);
}

#[test]
fn delete_nodes_for_path_retracts_fts_words_map() {
    let mut store = open();
    let n = node("src/gone2.rs", "fn:gone2", "function");
    let id = store.put_node(&n).unwrap();

    store.delete_nodes_for_path("src/gone2.rs").unwrap();

    assert!(store.fts_words_entry(id).unwrap().is_none());
    assert_eq!(store.fts_words_node_count().unwrap(), 0);
}

/// Blocking per the RFC-023 acceptance criteria: v21 migration is idempotent.
#[test]
fn v21_migration_is_idempotent_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("graph.db");

    {
        let mut store = SqliteStore::open(&db_path).unwrap();
        let n = node("src/a.rs", "fn:a", "function");
        store.put_node(&n).unwrap();
    }
    // Reopen — migration runner + backfill_fts_words_if_needed must be no-ops.
    let store2 = SqliteStore::open(&db_path).unwrap();
    assert_eq!(store2.node_count().unwrap(), 1);
    assert_eq!(store2.fts_words_node_count().unwrap(), 1);

    // A third open must still be a safe no-op (idempotency, not just "twice").
    let store3 = SqliteStore::open(&db_path).unwrap();
    assert_eq!(store3.fts_words_node_count().unwrap(), 1);
}

/// Pre-v21 rows (simulated: is_noise defaults to 0 via ALTER TABLE) must be
/// correctly recomputed by `backfill_fts_words_if_needed`, not left at the
/// column default.
#[test]
fn backfill_recomputes_is_noise_for_preexisting_test_path_row() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("graph.db");

    let test_node_id;
    {
        let mut store = SqliteStore::open(&db_path).unwrap();
        let n = node("tests/some_test.rs", "fn:some_test", "function");
        test_node_id = store.put_node(&n).unwrap();
        // First write already computes is_noise correctly (WS-3 write path).
        assert_eq!(store.is_noise_flag(test_node_id).unwrap(), Some(true));
    }
    // Reopening must not corrupt the already-correct value.
    let store2 = SqliteStore::open(&db_path).unwrap();
    assert_eq!(store2.is_noise_flag(test_node_id).unwrap(), Some(true));
}

#[test]
fn bulk_init_path_populates_fts_words_and_is_noise() {
    use travsr_store::FileGraph;

    let mut store = open();
    store.begin_staging_tables().unwrap();

    let n = node("src/bulk.rs", "fn:bulk_written", "function");
    let batch = [FileGraph {
        vname_path: "src/bulk.rs".to_string(),
        new_hash: "h1".to_string(),
        nodes: vec![n.clone()],
        edges: vec![],
    }];
    store.write_file_graphs_batch(&batch, true).unwrap();
    store.flush_staging_to_production().unwrap();

    // nodes_fts_words_map was written directly during the staging loop
    // (decoupled from nodes_stage), independent of the flush.
    let id = n.vname.id();
    let (sig_words, _) = store
        .fts_words_entry(id)
        .unwrap()
        .expect("bulk-inited node must have a nodes_fts_words_map row");
    assert!(sig_words.split_whitespace().any(|t| t == "bulk"));
    assert_eq!(store.is_noise_flag(id).unwrap(), Some(false));

    // rebuild_fts_from_map finalizes nodes_fts_words from the map for pending ids.
    store.rebuild_fts_from_map().unwrap();
    assert_eq!(store.fts_words_node_count().unwrap(), 1);
}

// ── symbol_frequency (WS-4) ─────────────────────────────────────────────────

/// Found via CI, not local dev (an installed embed backend was masking it):
/// `symbol_frequency` must be given the token's *original case* to segment it
/// correctly. `ident::segments` finds "isPrime"'s word boundary via the
/// lower→upper transition; if the caller lowercases to "isprime" first (as
/// `travsr-mcp::seed::tokenize_query` used to, before the #478 fix), that
/// transition is gone permanently and `segments("isprime")` returns one
/// fused, unsplittable segment that can never match the index's real
/// "prime" vocabulary entry — `symbol_frequency` then reports the token as
/// absent, and callers wrongly treat it as maximally generic. This test
/// pins `symbol_frequency`'s own contract (correct given proper case); the
/// end-to-end regression this was causing (a real query collapsing to total
/// abstention) is pinned separately in `travsr-mcp`'s
/// `camel_case_exact_name_query_reaches_strong_confidence_on_the_fts_only_path`.
#[test]
fn symbol_frequency_finds_camel_case_token_when_case_is_preserved() {
    let mut store = open();
    store
        .put_node(&node("prime.ts", "fn:isPrime", "function"))
        .unwrap();
    // "is" is dropped (< 3 bytes); "prime" is the real indexed segment.
    // doc=1: one row (this node) has "prime" in its word index, regardless
    // of it appearing in both the sig and path columns of that one row.
    assert_eq!(store.symbol_frequency("isPrime").unwrap(), Some(1));
    // The all-lowercase fused form is a different, unindexed word — proves
    // this isn't accidentally passing via a substring/prefix match.
    assert_eq!(store.symbol_frequency("isprime").unwrap(), None);
}

#[test]
fn symbol_frequency_wal_does_not_count_walk() {
    let mut store = open();
    store
        .put_node(&node("src/walker.ts", "fn:walk", "function"))
        .unwrap();
    // "wal" is not a word segment of "fn:walk" / "src/walker.ts" — absent.
    assert_eq!(store.symbol_frequency("wal").unwrap(), None);
    assert_eq!(store.symbol_frequency("walk").unwrap(), Some(1));
}

#[test]
fn symbol_frequency_works_does_not_count_workspace() {
    let mut store = open();
    store
        .put_node(&node(
            "src/ws.ts",
            "fn:provideWorkspaceChatContext",
            "function",
        ))
        .unwrap();
    assert_eq!(store.symbol_frequency("works").unwrap(), None);
    assert_eq!(store.symbol_frequency("workspace").unwrap(), Some(1));
}

#[test]
fn symbol_frequency_counts_sqlite_on_sqlitestore_fixture() {
    let mut store = open();
    store
        .put_node(&node(
            "crates/travsr-store/src/lib.rs",
            "fn:SqliteStore.exec_ddl",
            "method",
        ))
        .unwrap();
    store
        .put_node(&node(
            "crates/travsr-store/src/lib.rs",
            "fn:SqliteStore.open",
            "method",
        ))
        .unwrap();
    // Both nodes' signature AND path contribute "sqlite"/"store" segments —
    // fts5vocab's `doc` counts documents (rows), not raw occurrences, so two
    // nodes each containing the term once still count as 2.
    assert_eq!(store.symbol_frequency("sqlite").unwrap(), Some(2));
}

#[test]
fn symbol_frequency_none_for_short_token() {
    let store = open();
    // < 3 bytes never enters the word vocab (ident::segments drops it), and with
    // no node named "ab" the #778 exact-leaf-name fallback is 0, so the token is
    // still unmeasurable -> None (stays generic / abstains).
    assert_eq!(store.symbol_frequency("ab").unwrap(), None);
}

#[test]
fn symbol_frequency_short_token_grounds_on_exact_symbol() {
    // #778: a 2-char symbol (`UI`) is absent from the word vocab (min segment
    // len 3), but it is a real, unique symbol in this repo. The exact-leaf-name
    // fallback measures it as rare (Some(1)) rather than returning None and
    // letting the seed path fabricate `freq = n_total` and abstain on the exact
    // rank-0 match.
    let mut store = open();
    store
        .put_node(&node("app/ui.rb", "class:UI", "class"))
        .unwrap();
    assert_eq!(store.symbol_frequency("UI").unwrap(), Some(1));
    // A qualified leaf named exactly the token counts too (`method:Foo.ui` ->
    // leaf `ui`), case-insensitively.
    store
        .put_node(&node("app/foo.rb", "method:Foo.ui", "method"))
        .unwrap();
    assert_eq!(store.symbol_frequency("UI").unwrap(), Some(2));
    // A different short token with no matching symbol stays unmeasurable.
    assert_eq!(store.symbol_frequency("QZ").unwrap(), None);
}

#[test]
fn symbol_frequency_short_token_counts_qualified_members_as_generic() {
    // #778 regression for PR #791 review: the earlier boundary-count fallback
    // measured a short token on a DIFFERENT scale than the segment-vocab path it
    // falls back from. `NAME_MATCH_BOUNDARY`'s tail forms cannot see a qualified
    // member (`method:WidgetN.id`), so a corpus of hundreds of `.id` members plus
    // one `class:Id` counted as 1 -> `freq <= rare_anchor_max` -> the strongest
    // trust signal in the system, purely because `id` is 2 chars. The
    // exact-leaf-name count instead counts every `*.id` member (leaf `id`), so a
    // common member name reads as generic, exactly as the 3-char segment vocab
    // would count the identical `.key` corpus.
    let mut store = open();
    for i in 0..40 {
        store
            .put_node(&node(
                &format!("app/widget_{i}.rb"),
                &format!("method:Widget{i}.id"),
                "method",
            ))
            .unwrap();
    }
    store
        .put_node(&node("app/id.rb", "class:Id", "class"))
        .unwrap();
    // 40 members + the class, all named exactly `id` at their leaf: measured, not
    // fabricated as 1. Far above any rare-anchor bar, so it grounds nothing.
    assert_eq!(store.symbol_frequency("id").unwrap(), Some(41));
}

#[test]
fn symbol_frequency_short_token_saturates_at_cap() {
    // #778 PR #791 review follow-up: when a short leaf name is common enough to
    // hit LEAF_NAME_COUNT_CAP (4096), the LIMIT truncates the scan. The truncated
    // count must NOT be reported verbatim: 4096 still reads as "specific" through
    // idf_weight (which does not saturate near it), which would let a name borne
    // by tens of thousands of nodes clear the anchor-emit cut. The count is
    // instead saturated to the corpus size so IDF floors to generic.
    let mut store = open();
    // 4096 nodes named exactly `xy` at their leaf -> the leaf-count scan hits the
    // LIMIT. One extra non-matching node makes the corpus 4097, so a saturated
    // result is distinguishable from the truncated 4096.
    for i in 0..4096 {
        store
            .put_node(&node(
                &format!("app/w_{i}.rb"),
                &format!("method:W{i}.xy"),
                "method",
            ))
            .unwrap();
    }
    store
        .put_node(&node("app/other.rb", "class:Other", "class"))
        .unwrap();
    // Saturated to the corpus size (4097), not the truncated cap (4096): proves
    // the LIMIT value is not reported as-is.
    assert_eq!(store.symbol_frequency("xy").unwrap(), Some(4097));
}

#[test]
fn symbol_frequency_none_for_absent_token() {
    let mut store = open();
    store
        .put_node(&node("src/a.rs", "fn:hello", "function"))
        .unwrap();
    assert_eq!(store.symbol_frequency("zzzznotpresent").unwrap(), None);
}
