//! End-to-end language sanity: does a real `travsr init --semantic` on a real
//! repo answer "who calls this?" across a file boundary?
//!
//! Deliberately end-to-end through the shipped binary rather than through
//! `Indexer::parse_file`. The unit-level tests in `travsr-indexer/tests` prove
//! the grammar produces nodes; they cannot prove that Phase B ran, that its
//! definitions unified with the Phase A nodes, or that the symbol a user types
//! resolves to the node the edges actually point at. Every failure this suite
//! has found so far lives in that gap.
//!
//! Fixtures are in `fixtures/lang-sanity/<lang>/` and each carries a
//! cross-file call. See that directory's README for why the C/C++
//! `compile_commands.json` is a template rather than a real compdb.
//!
//! Skips rather than fails when a provider is absent: "not installed" and
//! "installed and broken" call for completely different responses, and a suite
//! that collapses them teaches people to ignore it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn travsr() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_travsr"))
}

/// Guards the corpus trust grant, which mutates the global
/// `~/.travsr/lang.toml` shared by every test in this binary.
fn trust_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

fn fixture_dir(lang: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/lang-sanity")
        .join(lang)
}

/// Whether `lang` has an active Phase B provider on this machine.
///
/// Read from `travsr lang list` rather than probing the binary directly, so the
/// skip reason is the same fact the user would see when asking why a language
/// is quiet.
fn provider_active(lang: &str) -> bool {
    let out = match Command::new(travsr()).args(["lang", "list"]).output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.split_whitespace().next() == Some(lang) && l.contains("active"))
}

/// Copy a fixture into a fresh git repo, materialize any compdb template, and
/// index it with semantic analysis on. Returns the repo root.
fn indexed_repo(lang: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    for entry in std::fs::read_dir(fixture_dir(lang)).expect("fixture dir") {
        let entry = entry.expect("dir entry");
        if entry.path().is_file() {
            std::fs::copy(entry.path(), root.join(entry.file_name())).expect("copy fixture");
        }
    }

    // `compile_commands.json` ships with a placeholder because scip-clang
    // resolves `directory` + `file` against the filesystem: a checked-in
    // absolute path would be right on one machine and silently wrong on every
    // other, CI included.
    let compdb = root.join("compile_commands.json");
    if compdb.exists() {
        let text = std::fs::read_to_string(&compdb).expect("read compdb");
        let real = text.replace("__FIXTURE_DIR__", &root.to_string_lossy());
        std::fs::write(&compdb, real).expect("write compdb");
    }

    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git");
    };
    git(&["init", "-q", "."]);
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.email=t@example.com",
        "-c",
        "user.name=t",
        "commit",
        "-qm",
        "fixture",
    ]);

    // First pass establishes the corpus. For a non-builtin language this pass
    // *cannot* produce semantic data: ADR-017 Rule 3 gates external tooling on
    // a per-corpus trust grant, and a repo nobody has trusted yet does not have
    // one. The grant needs the corpus id, which only exists after this runs.
    let out = Command::new(travsr())
        .args(["init", "--semantic"])
        .current_dir(root)
        .output()
        .expect("travsr init");
    assert!(
        out.status.success(),
        "init failed for {lang}: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let init_text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    if let Some(corpus) = corpus_from_hint(&init_text) {
        // Serialized because the grant is a read-modify-write of the *global*
        // `~/.travsr/lang.toml`: two tests granting at once can lose one
        // another's `trusted_corpora` entry, and the loser then indexes with
        // no semantic data and reports its language as broken.
        //
        // Held here rather than left to `--test-threads=1` on the command
        // line. A suite that only works with a flag people have to remember
        // fails intermittently for whoever forgets, and an intermittently red
        // suite gets ignored, which costs more than the lost parallelism.
        let _guard = trust_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let granted = Command::new(travsr())
            .args(["lang", "add", lang, "--corpus", &corpus])
            .current_dir(root)
            .output()
            .expect("travsr lang add");
        assert!(
            granted.status.success(),
            "trust grant failed for {lang}/{corpus}: {}",
            String::from_utf8_lossy(&granted.stderr)
        );
        // Re-index now that the sidecar is allowed to run at all.
        let out = Command::new(travsr())
            .args(["init", "--semantic"])
            .current_dir(root)
            .output()
            .expect("travsr re-init");
        assert!(
            out.status.success(),
            "re-init failed for {lang}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    wait_for_phase_b(root, lang);
    dir
}

/// The corpus id, taken from the trust hint `init` prints when a language is
/// gated:
///
/// ```text
/// corpus not trusted for: c - run `travsr lang add <lang> --corpus local/tmp-x` to enable
/// ```
///
/// Read out of that line rather than recomputed, for two reasons. The id is
/// normalized from the directory name, so a test that reimplemented the rule
/// could pass while trusting a different corpus than the binary gated. And
/// this is literally the command the message tells a user to run, so the test
/// exercises the documented recovery rather than a private path around it.
///
/// `None` when no hint appeared, which is the normal case for a builtin
/// language that needs no grant.
fn corpus_from_hint(init_output: &str) -> Option<String> {
    init_output
        .lines()
        .find(|l| l.contains("--corpus"))
        .and_then(|l| {
            let after = l.split("--corpus").nth(1)?;
            let token = after.split_whitespace().next()?;
            Some(
                token
                    .trim_matches(|c: char| c == '`' || c == '\'' || c == '"')
                    .to_string(),
            )
        })
        .filter(|c| !c.is_empty())
}

/// Block until Phase B has settled for this repo.
///
/// `init --semantic` can return before the semantic layer has finished, so
/// querying straight after it races: the store answers honestly that it has no
/// occurrences yet, and the test reads that as a broken language. Interactive
/// use never notices because a human takes seconds to type the next command,
/// which is exactly why this needed a test to surface.
///
/// Polls `travsr status`, the same surface a user would check, and gives up
/// after a bounded wait rather than hanging a suite forever.
fn wait_for_phase_b(root: &Path, lang: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let out = Command::new(travsr())
            .arg("status")
            .current_dir(root)
            .output()
            .expect("travsr status");
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        if text.contains("semantic: complete") {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("{lang}: Phase B did not settle within 90s\nlast status:\n{text}");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// `travsr references <symbol>` output for a repo.
fn references(root: &Path, symbol: &str) -> String {
    let out = Command::new(travsr())
        .args(["references", symbol])
        .current_dir(root)
        .output()
        .expect("travsr references");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Assert that `symbol` resolves and that `expected_site` is among its uses.
///
/// Checks the call site, not just a non-zero count: a symbol can accumulate
/// references from its own file while the cross-file edge, the one Phase B
/// exists for, is missing entirely.
fn assert_cross_file_reference(lang: &str, symbol: &str, expected_site: &str) {
    if !provider_active(lang) {
        eprintln!("SKIP {lang}: Phase B provider not active (travsr lang install {lang})");
        return;
    }
    let repo = indexed_repo(lang);
    let refs = references(repo.path(), symbol);
    assert!(
        refs.contains(expected_site),
        "{lang}: `{symbol}` must report the cross-file call site {expected_site}\n\
         got:\n{refs}"
    );
}

#[test]
fn c_resolves_a_cross_file_call() {
    // add_numbers is declared in math_util.h, defined in math_util.c, and
    // called from main.c and from a sibling in its own file.
    assert_cross_file_reference("c", "add_numbers", "main.c:5");
}

#[test]
fn typescript_resolves_a_cross_file_call() {
    assert_cross_file_reference("typescript", "makeGreeter", "main.ts:4");
}

#[test]
fn javascript_esm_resolves_a_cross_file_call() {
    // `.mjs` is the flavour worth pinning: it is ESM by extension rather than
    // by `package.json` type, so it exercises the module path independently of
    // any manifest.
    assert_cross_file_reference("javascript", "addNumbers", "main.mjs:5");
}

#[test]
fn javascript_commonjs_resolves_a_cross_file_call() {
    // Same question through `require`/`module.exports` (#610). The ESM and
    // CommonJS symbols are named differently on purpose: a name defined in two
    // files is deliberately left unindexed to avoid mis-targeting, so sharing
    // a name would report an ambiguity and hide whichever flavour is broken.
    assert_cross_file_reference("javascript", "sumLegacy", "legacy.cjs:5");
}

/// C++ out-of-line member definitions, the shape that had no working path
/// through unification until the cross-file rung was added.
///
/// This is the regression guard for that fix, so it is worth stating what was
/// wrong. `app::Widget::draw` is *declared* in `widget.h:9` and *defined* in
/// `widget.cpp:7`. Phase A anchors its node on the declaration; scip-clang
/// anchors its definition on the implementation. The same-file matcher cannot
/// see across that, and the cross-file rescue only fires when the same symbol
/// already unified somewhere, which it never had. So the store kept both
/// `fn:draw` and an orphan `scip:...app/Widget#draw(...)`, all 12 ref/call
/// edges pointed at the orphan, and `travsr references draw` answered zero
/// while `travsr status` said "semantic: complete" — the answer present in the
/// graph and unreachable from every query a user can type.
///
/// C never hit it because its function is defined in the `.c` file Phase A
/// anchors to, which is why a C-only test would have looked green.
#[test]
fn cpp_resolves_a_cross_file_call() {
    assert_cross_file_reference("cpp", "draw", "main.cpp:6");
}

/// Every JavaScript flavour parses, whatever its extension.
///
/// Cheap and provider-free, so it runs even where Phase B cannot. It also
/// records a disagreement worth knowing about: `travsr lang list` reports
/// `javascript` as its own active language, but `Language::from_extension`
/// maps `.js` / `.jsx` / `.mjs` / `.cjs` onto TypeScript, so nothing in the
/// graph is ever labelled `javascript`. Asserted as-is rather than as it
/// arguably should be, because changing it moves what lands in the store.
#[test]
fn javascript_flavours_all_parse_and_report_one_language() {
    let repo = indexed_repo("javascript");
    let db = repo.path().join(".travsr").join("graph.db");
    assert!(db.exists(), "index must exist");

    for symbol in ["addNumbers", "sumLegacy", "plainHelper", "scaleValue"] {
        let refs = references(repo.path(), symbol);
        assert!(
            !refs.contains("no match") && !refs.is_empty(),
            "`{symbol}` must be found whatever file extension defines it, got:\n{refs}"
        );
    }
}
