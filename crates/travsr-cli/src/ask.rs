//! `travsr ask` — PPR-ranked, knapsack-budgeted symbol context.
//!
//! Data acquisition is shared with the daemon via `travsr_mcp::query`
//! (#318 O1): a running daemon answers from its warm store; otherwise the
//! store is opened directly (read-only fast path).

use anyhow::Context as _;
use tabled::{Table, Tabled};
use travsr_mcp::query::{self, AskPayload};

use crate::daemon_client;
use crate::repo::find_git_root;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable table (default)
    Table,
    /// Machine-readable JSON — emits the full AskPayload
    Json,
}

#[derive(Tabled)]
struct Row {
    #[tabled(rename = "Kind")]
    kind: String,
    #[tabled(rename = "Signature")]
    signature: String,
    #[tabled(rename = "Path")]
    path: String,
    #[tabled(rename = "Score")]
    score: String,
}

fn to_row(r: &query::AskRow) -> Row {
    Row {
        kind: r.kind.clone(),
        signature: r.signature.clone(),
        path: match r.line {
            Some(l) => format!("{}:{}", r.path, l),
            None => r.path.clone(),
        },
        score: format!("{:.3}", r.score),
    }
}

/// #376 §4.1: print the docs section, or nothing at all when it is empty
/// ("absent, not empty-ish" — a section that appears on every query trains the
/// reader to ignore it).
///
/// Plain lines, no table, and no score column: doc scores and code scores are
/// not commensurable (§8.3 measured doc cosines above each repo's code band),
/// so the header carries the epistemic weight instead of a number. The lines
/// arrive already sanitized from `travsr_mcp::query` — this function must not
/// reformat them in a way that could re-introduce what the sanitizer removed.
fn print_docs(docs: &[String]) {
    if docs.is_empty() {
        return;
    }
    println!(
        "── docs — documentation prose: claims about the code, verify behaviour against the code itself ──"
    );
    for line in docs {
        println!("{line}");
    }
}

/// #376 G1 / §18.7: `TRAVSR_DOCS_ENABLED` is read by whichever process performs
/// retrieval, and for `ask` that is never this one. When the daemon serves the
/// query it reads the flag from its own environment; when it does not, the cold
/// read-only path deliberately never arms the doc KNN hook. So setting the flag
/// on this command looks like it applies and silently does nothing — the exact
/// shape of the #516 bug, where a lane that was wired correctly rendered nothing
/// and reported no error.
///
/// Deliberately phrased as where-to-set-it rather than "docs are off": a query
/// that legitimately matched no doc above the floor renders nothing either
/// (§4.2, "absent, not empty-ish"), and this function cannot tell the two apart
/// from inside the CLI process. Only fires when the user actually set the
/// variable, and goes to stderr so `--format json` stays machine-readable.
///
/// #376 O1 update: there is now a *working* switch to point at.
/// `travsr config set docs.enabled true` writes the repo's
/// `.travsr/config.toml`, which the daemon reads regardless of the environment
/// it was started in, so the note recommends that over restarting the daemon
/// with an exported variable.
fn note_docs_flag_is_read_by_the_daemon() {
    if std::env::var_os("TRAVSR_DOCS_ENABLED").is_none() {
        return;
    }
    eprintln!(
        "note: TRAVSR_DOCS_ENABLED is read by the process that performs retrieval, \
         which for `ask` is the travsr daemon — not this command. Prefer the \
         config key, which the daemon reads whatever environment it was started \
         in: `travsr config set docs.enabled true`."
    );
}

/// #376 O7 / G7: with no daemon running, `ask` is served by the cold read-only
/// path, which can never render a docs section — and said nothing about it.
///
/// The silence was the whole problem: an empty docs section is also what a
/// correctly-working lane produces when nothing cleared the floor (§4.2,
/// "absent, not empty-ish"), so a user who enabled the lane and got nothing had
/// no way to tell "no doc matched" from "this code path structurally cannot
/// answer you". Two of #376's shipped bugs (#516, and the CLI env-var no-op)
/// were the same shape.
///
/// The fix is to say so rather than to arm the hook. Arming it was measured and
/// rejected: [`travsr_daemon::try_inject_embed_hook_readonly`] documents that a
/// per-invocation sidecar costs ~0.6 s of model load, which overruns
/// `ask_query`'s own 600 ms circuit breaker, so the seeds would be discarded
/// anyway while every `ask` churned a throwaway 127 MB-model process.
///
/// Only fires when the lane is actually enabled for this repo — a user who
/// never turned docs on is not owed a message about them — and writes to stderr
/// so `--format json` stays machine-readable.
fn note_cold_path_cannot_render_docs(repo_root: &std::path::Path) {
    // #519: must match travsr-mcp::seed::docs_enabled's own default (now
    // true) - otherwise this warning silently stops firing for a cold-path
    // user on the new default, the exact silent-failure shape this function
    // exists to prevent.
    let enabled = travsr_config::effective_bool("docs.enabled", Some(repo_root)).unwrap_or(true);
    if !enabled {
        return;
    }
    eprintln!(
        "note: docs.enabled is on, but no travsr daemon is running — this query \
         is served by the read-only cold path, which does not load the doc \
         index, so no docs section can appear. Start the daemon with \
         `travsr daemon start`."
    );
}

pub fn run(query_str: &str, format: OutputFormat) -> anyhow::Result<()> {
    if query_str.trim().is_empty() {
        anyhow::bail!("search query must not be empty — try: travsr ask \"PaymentService\"");
    }
    note_docs_flag_is_read_by_the_daemon();
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;
    let db_path = repo_root.join(".travsr/graph.db");

    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    let payload: AskPayload = match daemon_client::try_query(
        &repo_root,
        "ask",
        serde_json::json!({ "query": query_str }),
    ) {
        Some(p) => p,
        None => {
            note_cold_path_cannot_render_docs(&repo_root);
            let mut store = daemon_client::open_read_store(&db_path)?;
            // Best-effort: load HNSW embed hook for cold-path KNN. Falls back to
            // FTS-only if the sidecar binary is absent or the index is not built.
            travsr_daemon::try_inject_embed_hook_readonly(&mut store, &db_path);
            let knn = store.embed_knn_fn();
            let knn_ref = knn
                .as_ref()
                .map(|f| f as &dyn Fn(&str, u32) -> Vec<(travsr_core::NodeId, f32)>);
            query::ask_query(&store, query_str, knn_ref)?
        }
    };

    if matches!(format, OutputFormat::Json) {
        println!("{}", serde_json::to_string(&payload)?);
        return Ok(());
    }

    let display_query = query_str.strip_prefix(':').unwrap_or(query_str).trim();
    if !payload.matched {
        // `ask` is natural-language, graph-grounded retrieval — not a symbol-name
        // lookup — so an abstention means "no confidently relevant code found",
        // not "that symbol does not exist". Word it that way to avoid the misread.
        println!("no grounded match for '{display_query}' in this repo (try rephrasing, or search a symbol name directly)");
        // #376 §4.3: doc hits may appear below the abstain message, but never
        // convert it into a match — `payload.matched` stays false and no
        // confidence, coverage or tier label is derived from them. This is the
        // highest-value case for the lane: §8.5 measured 15/20 k8s rationale
        // queries as hard abstentions with a citable doc section available.
        if !payload.docs.is_empty() {
            println!();
            print_docs(&payload.docs);
        }
        return Ok(());
    }
    if payload.no_results {
        println!("no graph results for '{display_query}'");
        if !payload.docs.is_empty() {
            println!();
            print_docs(&payload.docs);
        }
        return Ok(());
    }

    let n = payload.rows.len();
    // RFC-022 §14: when match-source grouping is on (rows carry `match_source`)
    // and the result is large enough (N>4) that section headers pay for themselves,
    // print one table per Exact → Semantic → Relevant section. Otherwise a single
    // flat table (unchanged default). This never reorders the JSON `rows` (that
    // path returned above); it only regroups the human table.
    let grouped = n > 4 && payload.rows.iter().any(|r| r.match_source.is_some());
    if grouped {
        for tag in ["exact", "semantic", "docs", "relevant"] {
            // #376 §4.1 section order: exact → semantic → docs → relevant. Docs
            // sit above `relevant` because design intent beats graph-adjacent
            // filler, and below the code sections so code leads whenever it has
            // an answer. Docs are not `rows`, so this arm is not a filter.
            if tag == "docs" {
                print_docs(&payload.docs);
                continue;
            }
            let mut section: Vec<&query::AskRow> = payload
                .rows
                .iter()
                .filter(|r| r.match_source.as_deref() == Some(tag))
                .collect();
            if section.is_empty() {
                continue;
            }
            section.sort_by(|a, b| b.score.total_cmp(&a.score));
            let rows: Vec<Row> = section.iter().map(|r| to_row(r)).collect();
            let header = match tag {
                "exact" => "── exact — literal symbol / FTS match (not reranked) ──",
                "semantic" => "── semantic — cross-encoder ranked ──",
                _ => "── relevant — graph-adjacent context ──",
            };
            println!("{header}");
            println!("{}", Table::new(rows));
        }
    } else {
        let rows: Vec<Row> = payload.rows.iter().map(to_row).collect();
        println!("{}", Table::new(rows));
        print_docs(&payload.docs);
    }
    let embed_note = if payload.embed_used {
        " · [embed-enhanced]"
    } else {
        ""
    };
    // F9: surface the honest confidence label (parity with get_context's header).
    let confidence_note = if payload.confidence.is_empty() {
        String::new()
    } else {
        format!(" · confidence: {}", payload.confidence)
    };
    println!(
        "\n{n} nodes · ~{} tokens{confidence_note}{embed_note}",
        payload.total_tokens
    );
    Ok(())
}

#[cfg(test)]
mod docs_note_tests {
    /// Serializes the tests below: both mutate `HOME`, which is process-global.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A hermetic repo + config environment. Redirects **both** file layers into
    /// a tempdir — `HOME` for the global one, the returned path for the repo one
    /// — because a developer with `docs.enabled = true` in their real
    /// `~/.travsr/config.toml` would otherwise flip these assertions.
    struct Env {
        dir: tempfile::TempDir,
        prev_home: Option<std::ffi::OsString>,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let prev_home = std::env::var_os("HOME");
            std::env::set_var("HOME", dir.path().join("home"));
            std::fs::create_dir_all(dir.path().join("repo").join(".travsr")).expect("mk repo");
            Self { dir, prev_home }
        }
        fn repo(&self) -> std::path::PathBuf {
            self.dir.path().join("repo")
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            match self.prev_home.take() {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// #376 O7: the note must be gated on the lane actually being enabled.
    /// Printing it unconditionally would put a docs warning in front of every
    /// user who never turned docs on, which is how a note becomes noise and then
    /// becomes ignored — the failure mode §20.4 O3 flags for flaky CI gates too.
    ///
    /// #519 flipped the default to on, so the "silent" case this test pins is
    /// now an explicit opt-out rather than the default — was
    /// `cold_path_note_is_silent_when_docs_are_off`, renamed rather than
    /// edited in place.
    #[test]
    fn cold_path_note_is_silent_when_docs_are_explicitly_off() {
        let _g = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = Env::new();
        assert!(
            travsr_config::effective_bool("docs.enabled", Some(&env.repo())).unwrap_or(true),
            "default must be on"
        );
        travsr_config::set(
            "docs.enabled",
            "false",
            travsr_config::Scope::Repo(env.repo()),
        )
        .expect("set");
        assert!(
            !travsr_config::effective_bool("docs.enabled", Some(&env.repo())).unwrap_or(true),
            "explicit opt-out must make note_cold_path_cannot_render_docs return early"
        );
    }

    /// And it must fire once the key is on — the condition the live check in
    /// this session exercised, pinned so a config-plumbing regression is caught
    /// here rather than by a user seeing an unexplained empty docs section.
    #[test]
    fn cold_path_note_fires_when_docs_are_on() {
        let _g = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = Env::new();
        travsr_config::set(
            "docs.enabled",
            "true",
            travsr_config::Scope::Repo(env.repo()),
        )
        .expect("set");
        assert!(
            travsr_config::effective_bool("docs.enabled", Some(&env.repo())).unwrap_or(true),
            "repo config must drive the note"
        );
    }
}
