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

/// One construct under test: the symbol a user would type, and a `path:line`
/// that must appear among its uses.
struct Probe {
    symbol: &'static str,
    site: &'static str,
    /// What language feature this pins, for the failure message.
    construct: &'static str,
    /// `Some(reason)` when this construct is known **not** to resolve today.
    ///
    /// Recorded as an expectation rather than deleted or `#[ignore]`d, so the
    /// suite stays green on a known gap *and* goes red the moment someone
    /// fixes it. A gap that silently starts working is how coverage rots: the
    /// probe would keep passing for the wrong reason and nobody would learn
    /// the limitation is gone.
    gap: Option<&'static str>,
}

/// Index `lang` once, then run every probe against it.
///
/// One index per language rather than one per construct: indexing dominates the
/// runtime (scip-clang alone is tens of seconds), and a per-probe repo would
/// make broad coverage too slow to run, which is how a suite stops being run.
///
/// Reports **every** failing probe rather than stopping at the first, so one run
/// tells you the whole state of a language instead of one construct at a time.
fn assert_probes(lang: &str, probes: &[Probe]) {
    if !provider_active(lang) {
        eprintln!("SKIP {lang}: Phase B provider not active (travsr lang install {lang})");
        return;
    }
    let repo = indexed_repo(lang);
    let mut failures = Vec::new();
    for p in probes {
        let refs = references(repo.path(), p.symbol);
        let resolved = refs.contains(p.site);
        match (p.gap, resolved) {
            // Works, as expected.
            (None, true) => {}
            // Regression: something that used to resolve no longer does.
            (None, false) => failures.push(format!(
                "  [{}] `{}` should be used at {}\n      got: {}",
                p.construct,
                p.symbol,
                p.site,
                refs.lines().take(2).collect::<Vec<_>>().join(" / ")
            )),
            // Known gap, still a gap.
            (Some(_), false) => {}
            // Known gap that now works: the expectation is stale and hiding
            // real coverage. Fail so it gets removed rather than left to rot.
            (Some(reason), true) => failures.push(format!(
                "  [{}] `{}` now resolves at {} but is still marked a known gap ({reason}).\n      \
                 Drop `gap` from this probe.",
                p.construct, p.symbol, p.site
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{lang}: {} of {} constructs did not resolve:\n{}",
        failures.len(),
        probes.len(),
        failures.join("\n")
    );
}

#[test]
fn c_constructs_resolve_end_to_end() {
    assert_probes(
        "c",
        &[
            Probe {
                symbol: "add_numbers",
                site: "main.c:5",
                construct: "header decl + .c definition",
                gap: None,
            },
            Probe {
                symbol: "scale_value",
                site: "main.c:6",
                construct: "cross-file call",
                gap: None,
            },
            Probe {
                symbol: "clamp_low",
                site: "math_util.c:9",
                construct: "static (file-local) function",
                gap: None,
            },
            Probe {
                symbol: "apply_op",
                site: "main.c:7",
                construct: "function-pointer parameter",
                gap: None,
            },
            Probe {
                symbol: "point_sum",
                site: "main.c:10",
                construct: "struct pointer parameter",
                gap: None,
            },
            Probe {
                symbol: "enum_width",
                site: "main.c:11",
                construct: "enum parameter",
                gap: None,
            },
        ],
    );
}

#[test]
fn cpp_constructs_resolve_end_to_end() {
    assert_probes(
        "cpp",
        &[
            Probe {
                symbol: "draw",
                site: "main.cpp:6",
                construct: "out-of-line member (the #698 shape)",
                gap: None,
            },
            Probe {
                symbol: "area",
                site: "main.cpp:7",
                construct: "virtual override, out-of-line",
                gap: None,
            },
            Probe {
                symbol: "inlineSize",
                site: "main.cpp:8",
                construct: "inline member defined in the header",
                gap: None,
            },
            Probe {
                symbol: "build_default",
                site: "main.cpp:12",
                construct: "free function in a nested namespace",
                gap: None,
            },
            Probe {
                symbol: "instances",
                site: "main.cpp:13",
                construct: "static member function",
                gap: None,
            },
            Probe {
                symbol: "twice",
                site: "main.cpp:14",
                construct: "function template",
                gap: Some(
                    "scip-clang attributes the call to an instantiation, not the template definition",
                ),
            },
            Probe {
                symbol: "unwrap",
                site: "main.cpp:16",
                construct: "class template, out-of-class member",
                gap: Some(
                    "template member: the instantiation problem plus the out-of-class split",
                ),
            },
            Probe {
                symbol: "describe",
                site: "main.cpp:17",
                construct: "inherited base-class method",
                gap: None,
            },
            Probe {
                symbol: "label_of",
                site: "main.cpp:18",
                construct: "free function taking a base reference",
                gap: Some(
                    "the argument needs a derived-to-base conversion at the call site",
                ),
            },
        ],
    );
}

#[test]
fn typescript_constructs_resolve_end_to_end() {
    assert_probes(
        "typescript",
        &[
            Probe {
                symbol: "makeGreeter",
                site: "main.ts:5",
                construct: "factory function via a barrel re-export",
                gap: None,
            },
            Probe {
                symbol: "speak",
                site: "main.ts:6",
                construct: "class method implementing an abstract base",
                gap: None,
            },
            Probe {
                symbol: "speakLater",
                site: "main.ts:7",
                construct: "async method",
                gap: None,
            },
            Probe {
                symbol: "describe",
                site: "main.ts:8",
                construct: "inherited method on an abstract class",
                gap: Some("call is on a subclass instance; the method is defined on the base"),
            },
            Probe {
                symbol: "firstOf",
                site: "main.ts:13",
                construct: "generic function",
                gap: None,
            },
            Probe {
                symbol: "formatGreeting",
                site: "main.ts:15",
                construct: "arrow function behind a type alias",
                gap: None,
            },
        ],
    );
}

#[test]
fn javascript_constructs_resolve_end_to_end() {
    assert_probes(
        "javascript",
        &[
            Probe {
                symbol: "addNumbers",
                site: "main.mjs:4",
                construct: "ESM named export via a barrel",
                gap: None,
            },
            Probe {
                symbol: "scaleValue",
                site: "main.mjs:7",
                construct: "ESM default export",
                gap: None,
            },
            Probe {
                symbol: "addLater",
                site: "main.mjs:6",
                construct: "ESM async export",
                gap: None,
            },
            Probe {
                symbol: "sumLegacy",
                site: "legacy.cjs:5",
                construct: "CommonJS require + module.exports",
                gap: None,
            },
            Probe {
                symbol: "plainHelper",
                site: "plain.js:7",
                construct: "plain .js ambient script",
                gap: None,
            },
        ],
    );
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
