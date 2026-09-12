//! #878: a `travsr` binary run from outside its build or install layout could
//! not find `travsr-lsif-ts` (discovery is anchored on `current_exe`), skipped
//! the TypeScript LSIF pass, and still exited 0 with `semantic analysis
//! produced symbols for: typescript` and `semantic: complete`. The index had
//! lost most of the language's `ref/call` edges and nothing said so.
//!
//! These tests hold the shipped binary to one invariant: a TypeScript
//! `init --semantic` whose LSIF pass did not run must disclose it, on the
//! `init` summary, in `phase_b_warnings`, and in `travsr status`. They never
//! require the emitter to be present: CI does not build
//! `packages/travsr-lsif-ts/dist/`, so the in-place run is itself the
//! "emitter missing" case there, and the tests assert on what the binary
//! *says* about the run it did, not on which run it got.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use travsr_store::SqliteStore;

/// The line `init` prints when the LSIF pass was skipped (progress.rs).
const INCOMPLETE_LINE: &str = "typescript semantic analysis is incomplete";

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git must be runnable")
        .success();
    assert!(ok, "git {args:?} failed");
}

/// A TypeScript repo with a root `tsconfig.json` and cross-file calls, so the
/// LSIF pass is due and, when it runs, has edges to add over the native pass.
fn seed_ts_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    for dir in ["ts-callers", "lang-sanity/typescript"] {
        for entry in std::fs::read_dir(fixtures.join(dir)).unwrap() {
            let entry = entry.unwrap();
            if entry.path().extension().is_some_and(|e| e == "ts") {
                std::fs::copy(entry.path(), root.join("src").join(entry.file_name())).unwrap();
            }
        }
    }
    std::fs::copy(
        fixtures.join("ts-small/tsconfig.json"),
        root.join("tsconfig.json"),
    )
    .unwrap();
    git(root, &["-c", "init.defaultBranch=main", "init", "-q"]);
    git(root, &["config", "user.email", "qa@travsr.test"]);
    git(root, &["config", "user.name", "QA Bot"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "seed"]);
    tmp
}

fn in_place_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_travsr"))
}

/// Run `bin <args>` in `dir`. `TRAVSR_LSIF_TS` is cleared unless `env` sets it,
/// so a developer's shell override cannot decide what discovery does here.
fn run(bin: &Path, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(bin);
    cmd.env("TRAVSR_DISABLE_REGISTRY", "1")
        .env_remove("TRAVSR_LSIF_TS")
        .env_remove("RUST_LOG")
        .current_dir(dir)
        .args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("travsr must be runnable")
}

fn init_semantic(bin: &Path, dir: &Path, env: &[(&str, &str)]) -> Output {
    let _ = std::fs::remove_dir_all(dir.join(".travsr"));
    run(bin, dir, &["init", "--semantic", "--no-connect"], env)
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn db(dir: &Path) -> PathBuf {
    dir.join(".travsr/graph.db")
}

fn refcall_edges(dir: &Path) -> usize {
    SqliteStore::open(&db(dir))
        .unwrap()
        .all_edges()
        .unwrap()
        .iter()
        .filter(|(_, _, kind, _)| kind == "ref/call")
        .count()
}

fn meta(dir: &Path, key: &str) -> String {
    SqliteStore::open(&db(dir))
        .unwrap()
        .get_meta(key)
        .unwrap()
        .unwrap_or_default()
}

fn warnings(dir: &Path) -> String {
    meta(dir, "phase_b_warnings")
}

fn has_emitter_warning(warnings: &str) -> bool {
    warnings
        .split(',')
        .any(|w| w == "emitter_missing:typescript" || w == "emitter_failed:typescript")
}

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The monorepo emitter, if it has been built on this machine.
fn bundled_emitter() -> Option<PathBuf> {
    let p =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/travsr-lsif-ts/dist/index.js");
    p.is_file().then_some(p)
}

/// Whether discovery's last rung (a bare `travsr-lsif-ts` on PATH) can succeed
/// here, which is the only way a *relocated* binary legitimately finds the
/// emitter.
fn emitter_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        ["travsr-lsif-ts", "travsr-lsif-ts.cmd", "travsr-lsif-ts.exe"]
            .iter()
            .any(|name| dir.join(name).is_file())
    })
}

fn stub_emitter(dir: &Path, body: &str) -> String {
    let p = dir.join("stub-emitter.js");
    std::fs::write(&p, body).unwrap();
    p.to_string_lossy().into_owned()
}

/// Everything the user can see after a skipped LSIF pass, asserted together so
/// one surface cannot quietly stop agreeing with the others.
fn assert_disclosed(bin: &Path, repo: &Path, out: &Output, class: &str) {
    let combined = text(out);
    assert!(
        combined.contains(INCOMPLETE_LINE),
        "init must say the TypeScript analysis is incomplete at default verbosity:\n{combined}"
    );
    assert!(
        combined.contains("travsr init --semantic --force"),
        "init must name the retry:\n{combined}"
    );
    assert!(
        warnings(repo).split(',').any(|w| w == class),
        "phase_b_warnings must record {class}, got {:?}",
        warnings(repo)
    );
    let status = text(&run(bin, repo, &["status"], &[]));
    assert!(
        status.contains("semantic: partial (incomplete: typescript)"),
        "status must downgrade the semantic field:\n{status}"
    );
    assert!(
        status.contains("warning: full 'typescript' analysis is incomplete"),
        "status must explain the downgrade:\n{status}"
    );
}

// ── Case B: the required emitter is unavailable ─────────────────────────────

/// The override names a file that does not exist, which deterministically
/// reproduces "the emitter cannot be started" on every machine, with or
/// without node, with or without a built `dist/`. Before #878 the override was
/// silently ignored and discovery ran instead.
#[test]
fn missing_emitter_is_disclosed_on_init_in_meta_and_in_status() {
    let repo = seed_ts_repo();
    let bin = in_place_binary();
    let missing = repo.path().join("nowhere").join("index.js");
    let env = [("TRAVSR_LSIF_TS", missing.to_str().unwrap())];

    let out = init_semantic(&bin, repo.path(), &env);
    // Exit 0 is deliberate and matches a crashed sidecar: Phase B disclosure
    // lives in the summary, the meta and `status`, and the native pass did
    // produce a usable (if partial) graph. What must never happen is the
    // pre-#878 combination of exit 0 AND silence.
    assert!(
        out.status.success(),
        "init must still complete: {}",
        text(&out)
    );
    assert_disclosed(&bin, repo.path(), &out, "emitter_missing:typescript");
    let combined = text(&out);
    assert!(
        combined.contains("TRAVSR_LSIF_TS"),
        "the summary must name the override that is wrong:\n{combined}"
    );
    assert!(
        combined.contains("semantic analysis produced symbols for: typescript"),
        "the native pass did run and may still be reported, just not alone:\n{combined}"
    );
    // The marker advances (the native pass is current at HEAD), exactly as it
    // does for a crashed sidecar under #712; `status` is what says "partial".
    assert_eq!(
        meta(repo.path(), "phase_b_commit"),
        meta(repo.path(), "last_commit")
    );

    // The machine-readable summary must agree with `status`.
    let json_out = run(
        &bin,
        repo.path(),
        &["init", "--semantic", "--force", "--json", "--no-connect"],
        &env,
    );
    let summary: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&json_out.stdout).trim())
            .unwrap_or_else(|e| panic!("init --json must print JSON: {e}\n{}", text(&json_out)));
    assert_eq!(
        summary["phase_b"], "partial",
        "--json must not report a skipped LSIF pass as complete: {summary}"
    );
}

/// An emitter that starts and then fails is a different fault with a different
/// fix, and must be classified as such rather than as "not installed".
#[test]
fn failing_emitter_is_disclosed_as_failed() {
    if !node_available() {
        eprintln!("SKIP: node not available");
        return;
    }
    let repo = seed_ts_repo();
    let bin = in_place_binary();
    let stub = stub_emitter(
        repo.path(),
        "process.stderr.write('stub emitter: boom\\n'); process.exit(2);",
    );

    let out = init_semantic(&bin, repo.path(), &[("TRAVSR_LSIF_TS", &stub)]);
    assert!(out.status.success(), "{}", text(&out));
    assert_disclosed(&bin, repo.path(), &out, "emitter_failed:typescript");
    let combined = text(&out);
    assert!(
        combined.contains("failed") && combined.contains("boom"),
        "the summary must carry the emitter's own error:\n{combined}"
    );
}

// ── Case D: the explicit override is honoured ────────────────────────────────

/// `TRAVSR_LSIF_TS` pointing at a working emitter must run it and record no
/// warning. The stub emits an empty (valid) dump so this holds on CI; when the
/// real emitter has been built locally it is exercised too, and must add the
/// compiler-derived edges the native pass alone cannot produce.
#[test]
fn explicit_override_runs_the_emitter_and_records_no_warning() {
    if !node_available() {
        eprintln!("SKIP: node not available");
        return;
    }
    let repo = seed_ts_repo();
    let bin = in_place_binary();

    // Baseline: what the native pass alone produces.
    let missing = repo.path().join("nowhere").join("index.js");
    let out = init_semantic(
        &bin,
        repo.path(),
        &[("TRAVSR_LSIF_TS", missing.to_str().unwrap())],
    );
    assert!(out.status.success(), "{}", text(&out));
    let native_only = refcall_edges(repo.path());

    let stub = stub_emitter(repo.path(), "process.exit(0);");
    let out = init_semantic(&bin, repo.path(), &[("TRAVSR_LSIF_TS", &stub)]);
    assert!(out.status.success(), "{}", text(&out));
    let combined = text(&out);
    assert!(
        !combined.contains(INCOMPLETE_LINE),
        "a working override must not be reported as incomplete:\n{combined}"
    );
    assert!(
        !has_emitter_warning(&warnings(repo.path())),
        "no emitter warning for a working override, got {:?}",
        warnings(repo.path())
    );
    let status = text(&run(&bin, repo.path(), &["status"], &[]));
    assert!(status.contains("semantic: complete"), "{status}");

    // Case A/D with the real emitter, when this machine has built it.
    match bundled_emitter() {
        Some(emitter) => {
            let out = init_semantic(
                &bin,
                repo.path(),
                &[("TRAVSR_LSIF_TS", emitter.to_str().unwrap())],
            );
            assert!(out.status.success(), "{}", text(&out));
            assert!(
                !has_emitter_warning(&warnings(repo.path())),
                "{:?}",
                warnings(repo.path())
            );
            let with_lsif = refcall_edges(repo.path());
            assert!(
                with_lsif > native_only,
                "the LSIF pass must add ref/call edges over the native pass \
                 (native-only {native_only}, with emitter {with_lsif})"
            );
        }
        None => eprintln!(
            "SKIP: packages/travsr-lsif-ts/dist/index.js not built; real-emitter half not run"
        ),
    }
}

// ── Cases A and C: in place vs relocated, the issue's own reproduction ────────

/// The byte-identical binary, run from its build location and from a copy
/// elsewhere. Whatever discovery finds in each place, the run that produced
/// fewer `ref/call` edges must have said so. That is the whole of #878.
#[test]
fn relocated_binary_never_degrades_silently() {
    let repo = seed_ts_repo();
    let in_place = in_place_binary();

    let relocated_dir = tempfile::tempdir().unwrap();
    let relocated = relocated_dir
        .path()
        .join("bin")
        .join(in_place.file_name().unwrap());
    std::fs::create_dir_all(relocated.parent().unwrap()).unwrap();
    std::fs::copy(&in_place, &relocated).unwrap();

    let out_in_place = init_semantic(&in_place, repo.path(), &[]);
    assert!(out_in_place.status.success(), "{}", text(&out_in_place));
    let edges_in_place = refcall_edges(repo.path());
    let warn_in_place = warnings(repo.path());
    let said_in_place = text(&out_in_place).contains(INCOMPLETE_LINE);

    let out_relocated = init_semantic(&relocated, repo.path(), &[]);
    assert!(out_relocated.status.success(), "{}", text(&out_relocated));
    let edges_relocated = refcall_edges(repo.path());
    let warn_relocated = warnings(repo.path());
    let said_relocated = text(&out_relocated).contains(INCOMPLETE_LINE);

    eprintln!(
        "in place: {edges_in_place} ref/call, warnings {warn_in_place:?}; \
         relocated: {edges_relocated} ref/call, warnings {warn_relocated:?}"
    );

    // The invariant: degradation is never silent. This is the exact pre-#878
    // failure (fewer edges, exit 0, no warning), stated as an assertion.
    if edges_relocated < edges_in_place {
        assert!(
            said_relocated && has_emitter_warning(&warn_relocated),
            "the relocated binary produced fewer ref/call edges ({edges_relocated} < \
             {edges_in_place}) and must disclose why; init said {said_relocated}, \
             phase_b_warnings {warn_relocated:?}:\n{}",
            text(&out_relocated)
        );
    }
    // Each run's own summary agrees with its own meta.
    assert_eq!(
        said_in_place,
        has_emitter_warning(&warn_in_place),
        "in place"
    );
    assert_eq!(
        said_relocated,
        has_emitter_warning(&warn_relocated),
        "relocated"
    );

    // A relocated binary that did NOT warn can only have found the emitter on
    // PATH (the `current_exe`-anchored rungs cannot succeed from a tempdir).
    if !has_emitter_warning(&warn_relocated) {
        assert!(
            emitter_on_path(),
            "relocated binary reported a full LSIF pass but travsr-lsif-ts is not on PATH"
        );
        assert_eq!(edges_relocated, edges_in_place);
    } else {
        assert_disclosed(
            &relocated,
            repo.path(),
            &out_relocated,
            "emitter_missing:typescript",
        );
    }

    // Case A: in place, normal discovery still works wherever the emitter has
    // been built (rung 3, the monorepo layout). Where it has not (CI), the
    // in-place run is itself a disclosed skip, never a silent one.
    if bundled_emitter().is_some() && node_available() {
        assert!(
            !has_emitter_warning(&warn_in_place),
            "in-place discovery must find the bundled emitter, got {warn_in_place:?}:\n{}",
            text(&out_in_place)
        );
        if !emitter_on_path() {
            assert!(
                edges_in_place > edges_relocated,
                "with the emitter bundled but not on PATH, relocating must lose LSIF edges \
                 (in place {edges_in_place}, relocated {edges_relocated}); if it did not, \
                 this test no longer reproduces #878"
            );
        }
    }
}
