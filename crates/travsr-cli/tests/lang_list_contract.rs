//! #755: the `lang list --json` contract the VS Code Languages panel renders,
//! and the two `--version` / `embed init` entry points the same issue reports,
//! exercised through the shipped binary.
//!
//! Deliberately end-to-end rather than a unit test on the format string. The
//! reported bug was not "a field is computed wrong" — it was "the JSON a real
//! binary prints does not carry the fields a real consumer reads", and only
//! running the binary and parsing its stdout can catch that. The unit tests in
//! `travsr-cli/src/lang.rs` pin the contract number; this pins the bytes.
//!
//! The extension's own copy of this list lives in
//! `packages/travsr-vscode/src/webviews.ts` (`LANG_CONTRACT_FIELDS`), and its
//! parser tests assert the same names. If the two ever drift, one side reports
//! a skew that does not exist and the other renders cells it had to guess at —
//! which is the whole failure mode.

use std::path::PathBuf;
use std::process::Command;

fn travsr() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_travsr"))
}

/// Every field `buildLanguagesHtml` reads and cannot re-derive. Must stay equal
/// to `LANG_CONTRACT_FIELDS` in the extension.
const CONTRACT_FIELDS: &[&str] = &[
    "language",
    "status",
    "statusLine",
    "repoState",
    "prerequisites",
    "builtin",
    "availableOnThisPlatform",
    "unavailableTarget",
];

/// `lang list --json` probes analyzer availability for every catalog entry, which
/// means a PATH sweep per language. Six tests reading it is six sweeps, and on
/// Windows that dominated this file's runtime — so run the binary once and share
/// the parsed value. The command is read-only, so there is nothing to isolate
/// between tests.
fn lang_list_json() -> &'static serde_json::Value {
    static ONCE: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    ONCE.get_or_init(lang_list_json_uncached)
}

fn lang_list_json_uncached() -> serde_json::Value {
    let out = Command::new(travsr())
        .args(["lang", "list", "--json"])
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .output()
        .expect("running `travsr lang list --json`");
    assert!(
        out.status.success(),
        "`lang list --json` must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("`lang list --json` must emit parseable JSON ({e}); got: {stdout}")
    })
}

/// The positive case: a current binary reports every field the panel reads, on
/// every row. This is the assertion that fails if a field is renamed or dropped
/// without the contract marker moving with it.
#[test]
fn every_row_carries_every_contract_field() {
    let parsed = lang_list_json();
    let rows = parsed
        .as_array()
        .expect("`lang list --json` must be an array");
    assert!(!rows.is_empty(), "the catalog must not be empty");
    for row in rows {
        let obj = row.as_object().expect("every row must be an object");
        let lang = obj
            .get("language")
            .and_then(|v| v.as_str())
            .unwrap_or("<unnamed>");
        for field in CONTRACT_FIELDS {
            assert!(
                obj.contains_key(*field),
                "row '{lang}' omits '{field}' — the VS Code Languages panel reads \
                 it and has nothing to fall back on"
            );
        }
    }
}

/// The contract marker itself. A consumer cannot use the version string to spot
/// a stale binary (the npm build and a local build both self-report the same
/// version), so this number is the only positive signal it has.
#[test]
fn every_row_carries_the_contract_revision() {
    let parsed = lang_list_json();
    for row in parsed.as_array().expect("array") {
        assert_eq!(
            row.get("contract").and_then(|v| v.as_u64()),
            Some(1),
            "every row must state the contract revision it was emitted at"
        );
    }
}

/// `status` and `repoState` are rendered as enum tags. A tag outside the known
/// set would land in the panel's placeholder branch and read as "unknown", so
/// the CLI must only ever emit tags the panel knows.
#[test]
fn status_and_repo_state_only_use_known_tags() {
    const STATUS: &[&str] = &[
        "active",
        "partial",
        "needs_approval",
        "needs_consent",
        "unsupported",
    ];
    const REPO_STATE: &[&str] = &[
        "always_on",
        "enabled",
        "needs_analyzer",
        "not_enabled",
        "no_repo",
    ];
    for row in lang_list_json().as_array().expect("array") {
        let lang = row["language"].as_str().unwrap_or("<unnamed>");
        let status = row["status"].as_str().expect("status must be a string");
        assert!(
            STATUS.contains(&status),
            "{lang}: unknown status '{status}'"
        );
        let repo = row["repoState"]
            .as_str()
            .expect("repoState must be a string");
        assert!(
            REPO_STATE.contains(&repo),
            "{lang}: unknown repoState '{repo}'"
        );
    }
}

/// `statusLine` is rendered as a tooltip, so an empty one leaves the user with a
/// badge and no explanation.
#[test]
fn every_row_carries_a_non_empty_status_line() {
    for row in lang_list_json().as_array().expect("array") {
        let lang = row["language"].as_str().unwrap_or("<unnamed>");
        assert!(
            !row["statusLine"].as_str().unwrap_or("").is_empty(),
            "{lang}: statusLine is the badge's tooltip and must say something"
        );
    }
}

/// `status: "unsupported"` and `availableOnThisPlatform: false` come from one
/// CLI predicate, and the panel checks both before deciding not to offer an
/// install. They must never disagree, or the panel offers an install that
/// dead-ends (or hides one that would work).
#[test]
fn unsupported_status_agrees_with_platform_availability() {
    for row in lang_list_json().as_array().expect("array") {
        let lang = row["language"].as_str().unwrap_or("<unnamed>");
        let unsupported = row["status"] == "unsupported";
        let available = row["availableOnThisPlatform"]
            .as_bool()
            .expect("availableOnThisPlatform must be a bool");
        assert_eq!(
            unsupported, !available,
            "{lang}: status and availableOnThisPlatform must not disagree"
        );
        if unsupported {
            assert!(
                row["unavailableTarget"].is_string(),
                "{lang}: an unsupported row must name the OS it is unsupported on"
            );
        }
    }
}

// ── Part B item 2: `--version` for a hash-pinned language ────────────────────

/// The reported behaviour, on a language whose analyzer is hash-pinned on every
/// platform. `lang install <lang> --version <other>` must be refused **before**
/// any download: the old flow fetched the wrapper at `latest` first and only then
/// failed on the analyzer, leaving a wrapper version the user never asked for.
///
/// kotlin rather than the issue's ruby because scip-ruby publishes no Windows
/// binary, so on Windows ruby is (correctly) refused one check earlier for a
/// different reason — which would test the platform gate, not this one. kotlin's
/// `kotlin-language-server` vendors a checksum on every platform.
#[test]
fn version_override_on_a_pinned_language_is_refused_up_front() {
    let out = Command::new(travsr())
        .args([
            "lang",
            "install",
            "kotlin",
            "--version",
            "9.9.9-not-a-real-tag",
            "--no-interactive",
            "--yes",
        ])
        .stdin(std::process::Stdio::null())
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        // Belt and braces: if the refusal ever regresses, this stops the test
        // from reaching out to GitHub instead of quietly downloading a wrapper.
        .env("TRAVSR_SKIP_DOWNLOAD", "1")
        .output()
        .expect("running `travsr lang install`");
    assert!(
        !out.status.success(),
        "installing a pinned language at another version must fail, not warn"
    );
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        msg.contains("9.9.9-not-a-real-tag"),
        "the message must echo the version that was refused; got: {msg}"
    );
    assert!(
        msg.contains("1.3.13"),
        "the message must name the one tag that IS installable; got: {msg}"
    );
    assert!(
        msg.contains("Nothing was downloaded"),
        "the message must state that no partial install was left behind; got: {msg}"
    );
    assert!(
        !msg.contains("Installing travsr-lang-kotlin"),
        "the refusal must land before the wrapper download, not after it; got: {msg}"
    );
}

/// The platform gate still wins. On a host with no analyzer build for a language,
/// "there is no binary for this platform" is the more fundamental fact and must
/// not be replaced by a version complaint — the user cannot install it at ANY
/// version there.
#[test]
fn a_platform_gap_is_reported_before_a_version_pin() {
    // scip-clang publishes no Windows binary; on other platforms cpp is
    // installable and this case does not arise, so only assert where it does.
    if !cfg!(windows) {
        return;
    }
    let out = Command::new(travsr())
        .args([
            "lang",
            "install",
            "cpp",
            "--version",
            "9.9.9-not-a-real-tag",
            "--no-interactive",
            "--yes",
        ])
        .stdin(std::process::Stdio::null())
        .env("NO_COLOR", "1")
        .env("TRAVSR_SKIP_DOWNLOAD", "1")
        .output()
        .expect("running `travsr lang install`");
    assert!(!out.status.success());
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        msg.contains("no prebuilt binary for this platform"),
        "the platform gap is the reason to report; got: {msg}"
    );
    assert!(
        !msg.contains("supply-chain integrity"),
        "a version pin is beside the point when nothing is installable; got: {msg}"
    );
}

/// An unknown language is still rejected on its own terms — the new up-front
/// `--version` check must not shadow the "unknown language" error.
#[test]
fn version_override_on_an_unknown_language_still_reports_the_unknown_language() {
    let out = Command::new(travsr())
        .args([
            "lang",
            "install",
            "not-a-language",
            "--version",
            "v1.2.3",
            "--no-interactive",
            "--yes",
        ])
        .env("NO_COLOR", "1")
        .output()
        .expect("running `travsr lang install`");
    assert!(!out.status.success());
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        msg.contains("Unknown language"),
        "the unknown-language error must win; got: {msg}"
    );
}

// ── Part B item 8: `embed init` must be answerable without a terminal ────────

/// `embed init` with no backend and no terminal used to print the menu and exit
/// 0, so a CI step "passed" with embeddings still off. It must fail, and name
/// both ways to answer.
#[test]
fn embed_init_without_a_backend_fails_non_interactively() {
    let out = Command::new(travsr())
        .args(["embed", "init"])
        .stdin(std::process::Stdio::null())
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .output()
        .expect("running `travsr embed init`");
    assert!(
        !out.status.success(),
        "exiting 0 without installing anything reads as success; it must fail"
    );
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        msg.contains("--yes"),
        "the error must name the non-interactive flag; got: {msg}"
    );
    assert!(
        msg.contains("--backend"),
        "the error must name the explicit-backend flag; got: {msg}"
    );
}

/// `--yes` must be a real flag on `embed init`, and `--no-interactive` must reach
/// the same one — a script that already passes `--no-interactive` to `lang
/// install` should not have to learn a second spelling.
#[test]
fn embed_init_accepts_yes_and_its_no_interactive_alias() {
    let help = Command::new(travsr())
        .args(["embed", "init", "--help"])
        .env("NO_COLOR", "1")
        .output()
        .expect("running `travsr embed init --help`");
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("--yes"), "got: {text}");
    assert!(text.contains("--no-interactive"), "got: {text}");
}

/// An unknown `--backend` is still rejected by name, and `--yes` must not paper
/// over it by silently installing the recommended model instead.
#[test]
fn embed_init_rejects_an_unknown_backend_even_with_yes() {
    let out = Command::new(travsr())
        .args(["embed", "init", "--backend", "not-a-model", "--yes"])
        .stdin(std::process::Stdio::null())
        .env("NO_COLOR", "1")
        .output()
        .expect("running `travsr embed init`");
    assert!(!out.status.success());
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        msg.contains("Unknown backend") && msg.contains("not-a-model"),
        "an explicit bad backend must be named, not swapped for the default; got: {msg}"
    );
}

// ── Part B item 7: `embed list` must say where "active" came from ────────────

/// `embed list` and `embed status` resolve identically; the reported confusion
/// was that `list` never said which layer it read. Either it names one, or no
/// model is active at all — but it must never mark a row active with no source.
#[test]
fn embed_list_names_the_layer_that_made_a_model_active() {
    let out = Command::new(travsr())
        .args(["embed", "list", "--json"])
        .env("NO_COLOR", "1")
        .output()
        .expect("running `travsr embed list --json`");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("`embed list --json` must be parseable ({e}); got: {stdout}"));
    for row in parsed.as_array().expect("array") {
        let id = row["id"].as_str().unwrap_or("<unnamed>");
        let active = row["active"].as_bool().expect("active must be a bool");
        let source = row["activeSource"]
            .as_str()
            .expect("every row must carry activeSource");
        if active {
            assert!(
                source == "repo" || source == "machine-default",
                "{id}: an active row must name the layer it came from, got '{source}'"
            );
        } else {
            assert_eq!(
                source, "none",
                "{id}: an inactive row must not credit a layer"
            );
        }
        assert!(
            row.get("machineDefault").is_some(),
            "{id}: the machine default must be reported so a consumer can see the \
             same pair `embed status` prints"
        );
    }
}
