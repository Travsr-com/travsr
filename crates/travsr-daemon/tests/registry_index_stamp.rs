//! #454: `init_repo` stamps the registry on the success path, so a graph.db
//! that is later deleted reads as `IndexMissing` ("built, then gone") rather
//! than `NotIndexed` ("never built"). The db file cannot carry this itself:
//! it is created at the *start* of init and deleting it takes the evidence
//! with it, which is why the registry records the completion separately.
//!
//! This lives in its own test binary on purpose. The in-crate unit tests force
//! the registry OFF through a process-global env var (their shared `git_init`
//! sets `TRAVSR_DISABLE_REGISTRY=1` so temp repos never touch the developer's
//! real `~/.travsr`). This test needs it ON, and env vars are process-global,
//! so running alongside those tests let a parallel `git_init` flip the flag
//! mid-`init_repo` and skip the stamp. As the sole test in its process the flag
//! stays unset and the assertion is race-free. HOME is redirected to a tempdir
//! so the registry write lands there, never in the real home.

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git must be runnable")
        .success();
    assert!(ok, "git {args:?} failed");
}

fn git_init(dir: &Path) {
    git(dir, &["-c", "init.defaultBranch=main", "init", "-q"]);
    git(dir, &["config", "user.email", "test@test.com"]);
    git(dir, &["config", "user.name", "Test"]);
}

#[test]
fn init_repo_records_the_completed_index_in_the_registry() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());
    std::fs::write(tmp.path().join("app.ts"), "export class App {}").unwrap();

    // Redirect the registry's home so the write never touches the real
    // ~/.travsr. `registry::home_dir` reads HOME then USERPROFILE, so set both
    // for cross-platform coverage. The registry is left enabled (this process
    // never sets TRAVSR_DISABLE_REGISTRY).
    let home_tmp = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home_tmp.path());
    std::env::set_var("USERPROFILE", home_tmp.path());

    travsr_daemon::init_repo(tmp.path()).unwrap();
    let entries = travsr_store::registry::all_entries().unwrap();

    let (_, entry) = entries.iter().next().expect("repo must be registered");
    assert_eq!(
        entry.index_status(),
        travsr_store::registry::IndexStatus::Indexed
    );
    std::fs::remove_file(&entry.db_path).unwrap();
    assert_eq!(
        entry.index_status(),
        travsr_store::registry::IndexStatus::IndexMissing,
        "a deleted graph.db must not read as 'never indexed'"
    );
}
