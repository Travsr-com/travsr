//! #645 WS-A: the mass-delete circuit breaker in `SqliteStore::reconcile` is
//! the guard that keeps a truncated/empty walked set (e.g. a broken
//! `git ls-files`) from wiping the whole graph. `reconcile_head_drift` relies
//! on it, so pin the abort behaviour here where the `SafetyPolicy` is
//! injectable (the daemon path hardcodes `min: 100`, which would need a
//! 100+ file fixture to trip).

use travsr_core::SafetyPolicy;
use travsr_store::SqliteStore;

#[test]
fn reconcile_aborts_and_deletes_nothing_when_ghosts_exceed_ceiling() {
    let mut store = SqliteStore::open_in_memory().expect("open_in_memory");
    for p in ["a.ts", "b.ts", "c.ts"] {
        store.put_file_hash(p, "deadbeef").unwrap();
    }

    // Empty walked set ⇒ all three are ghosts. A ceiling of max(1, 3*0.5)=1 is
    // exceeded, so the breaker must abort before any delete.
    let policy = SafetyPolicy {
        mass_delete_ceiling_pct: 0.5,
        mass_delete_ceiling_min: 1,
        toctou_recheck: false,
    };
    let tmp = tempfile::tempdir().unwrap();
    let report = store
        .reconcile(
            &std::collections::HashSet::new(),
            &policy,
            tmp.path(),
            "corpus",
        )
        .unwrap();

    assert!(report.aborted, "circuit breaker must trip");
    assert!(report.abort_reason.is_some(), "abort must carry a reason");
    assert!(
        report.ghost_paths.is_empty(),
        "an aborted reconcile must delete nothing"
    );
    assert_eq!(
        store.get_all_file_hashes().unwrap().len(),
        3,
        "all file rows must survive the abort"
    );
}
