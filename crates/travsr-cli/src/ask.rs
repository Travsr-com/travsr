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

/// The match-source lanes the grouped human table renders, in backend
/// `trust_rank` order (seed.rs `MatchSource`): exact -> semantic -> docs ->
/// tests -> relevant. Every lane the backend can stamp on a row MUST appear
/// here: the per-tag filter in `run` drops any row whose `match_source` matches
/// no listed tag, so a missing lane is silently dropped from the table (the #479
/// bug: `tests` was absent, so 24 test rows including the top-scored node
/// vanished; the JSON `rows` still carried them). `docs` renders from
/// `payload.docs`, not from `rows`.
const SECTION_TAGS: [&str; 5] = ["exact", "semantic", "docs", "tests", "relevant"];

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
    // Before either path: `ask` ranks over call edges, so an incomplete Phase B
    // changes the answer, not just its completeness.
    daemon_client::warn_if_call_graph_degraded(&db_path);

    let mut served_cold_path = false;
    let payload: AskPayload = match daemon_client::try_query(
        &repo_root,
        "ask",
        serde_json::json!({ "query": query_str }),
    ) {
        Some(p) => p,
        None => {
            served_cold_path = true;
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

    // UX-010: only nudge about the missing doc index when the cold path actually
    // produced a grounded answer — a real result a docs section could have
    // augmented. On an abstention or an empty result (including nonsense queries)
    // a docs section would not have helped, so the note was pure recurring noise.
    if served_cold_path && payload.matched && !payload.no_results {
        note_cold_path_cannot_render_docs(&repo_root);
    }

    if matches!(format, OutputFormat::Json) {
        println!("{}", serde_json::to_string(&payload)?);
        return Ok(());
    }

    let display_query = query_str.strip_prefix(':').unwrap_or(query_str).trim();
    if !payload.matched {
        // `ask` is natural-language, graph-grounded retrieval — not a symbol-name
        // lookup — so an abstention means "no confidently relevant code found",
        // not "that symbol does not exist". Word it that way to avoid the misread.
        // "try rephrasing" is not actionable on its own: a user who does not
        // already know what travsr answers cannot tell what to rephrase towards,
        // and this path fires most often on exactly the conceptual questions
        // where they are least sure. Name the catalogue instead.
        println!(
            "no grounded match for '{display_query}' in this repo\n\
             try a symbol name directly, or run `travsr ask --examples` to see \
             what travsr can answer"
        );
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
    // print one table per Exact → Semantic → Docs → Tests → Relevant section.
    // Otherwise a single flat table (unchanged default). This never reorders the
    // JSON `rows` (that path returned above); it only regroups the human table.
    let grouped = n > 4 && payload.rows.iter().any(|r| r.match_source.is_some());
    if grouped {
        for tag in SECTION_TAGS {
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
            // #479: cap the tests lane in the human table the same way
            // `get_context`'s `assemble_context_body` does (TESTS_CAP = 3) so a
            // test lane never dominates the summary. The JSON `rows` keep every row.
            if tag == "tests" {
                section.truncate(3);
            }
            let rows: Vec<Row> = section.iter().map(|r| to_row(r)).collect();
            // U3: titles describe WHAT the group is, not the internal mechanism.
            // The old "semantic — cross-encoder ranked" asserted a reranker pass
            // that may not have run (e.g. KNN timed out → lexical fallback),
            // contradicting the degraded note printed below.
            let header = match tag {
                "exact" => "── exact matches — literal symbol / text ──",
                "semantic" => "── related — ranked by relevance ──",
                "tests" => "── tests — test entry points & fixtures ──",
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
    if !payload.degraded_note.is_empty() {
        println!("{}", payload.degraded_note);
    }
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

    /// #479 regression: every match-source lane the backend can stamp on a row
    /// must be rendered by the grouped table. The backend stamps `tests` (and
    /// `docs`, #376) alongside `exact`/`semantic`/`relevant`; the grouped renderer
    /// filters rows per `SECTION_TAGS`, so a lane missing from that list is
    /// silently dropped from the human table (the #479 bug dropped 24 `tests`
    /// rows, including the top-scored node, while the JSON path still carried
    /// them). Pin the full lane set so a future edit cannot re-introduce the drop.
    #[test]
    fn section_tags_cover_every_backend_match_source_lane() {
        for lane in ["exact", "semantic", "docs", "tests", "relevant"] {
            assert!(
                super::SECTION_TAGS.contains(&lane),
                "match-source lane {lane:?} missing from SECTION_TAGS -> its rows would be silently dropped from the ask table"
            );
        }
        // And the order matches the backend trust_rank so sections read
        // most-trusted first.
        assert_eq!(
            super::SECTION_TAGS,
            ["exact", "semantic", "docs", "tests", "relevant"]
        );
    }
}

/// The question shapes `travsr ask` and its sibling commands can answer.
///
/// Written because the abstention path told users to "try rephrasing" without
/// saying what to rephrase *towards*. A user who does not already know the
/// product cannot act on that, and `ask` abstains most often on exactly the
/// conceptual questions where they are least sure what is supported.
///
/// Every entry is a real command against real capabilities. Nothing aspirational:
/// an example here that does not work would be worse than no catalogue, because
/// it would send someone to a dead end while looking authoritative.
struct QuestionShape {
    /// What the user is actually trying to find out, in their words.
    intent: &'static str,
    /// A runnable command, using a placeholder the user substitutes.
    example: &'static str,
    /// Why this command rather than `ask`, when that is the better route.
    note: &'static str,
}

const QUESTION_CATALOGUE: &[(&str, &[QuestionShape])] = &[
    (
        "Find something",
        &[
            QuestionShape {
                intent: "Where is this defined?",
                example: "travsr ask \"<symbol or description>\"",
                note: "",
            },
            QuestionShape {
                intent: "Find a symbol by name",
                example: "travsr ask \"PaymentService\"",
                note: "a bare name is accepted, not just a question",
            },
            QuestionShape {
                intent: "Find text the graph does not model",
                example: "travsr pattern \"<regex>\"",
                note: "log strings, TODOs, config keys",
            },
        ],
    ),
    (
        "Understand impact",
        &[
            QuestionShape {
                intent: "What calls this?",
                example: "travsr graph <symbol> --direction callers",
                note: "",
            },
            QuestionShape {
                intent: "What does this depend on?",
                example: "travsr graph <symbol> --direction deps",
                note: "",
            },
            QuestionShape {
                intent: "What breaks if I change this?",
                example: "travsr graph <symbol> --direction both",
                note: "callers and dependencies together",
            },
            QuestionShape {
                intent: "Every use site, with path:line",
                example: "travsr references <symbol>",
                note: "wider than callers: includes types and assignments",
            },
        ],
    ),
    (
        "Check the index itself",
        &[
            QuestionShape {
                intent: "Is the graph fresh and healthy?",
                example: "travsr status",
                note: "",
            },
            QuestionShape {
                intent: "Are there ghost nodes?",
                example: "travsr fsck",
                note: "",
            },
            QuestionShape {
                intent: "Which languages have semantic analysis?",
                example: "travsr lang status",
                note: "an inactive language means thin results for it",
            },
            QuestionShape {
                intent: "Why did ask rank it that way?",
                example: "travsr explain \"<query>\" <symbol>",
                note: "shows which terms matched and which thresholds failed",
            },
        ],
    ),
];

/// Print the catalogue. Grouped by intent rather than by command, because a user
/// who does not know the commands cannot look one up by name.
pub fn print_examples() {
    println!("What you can ask travsr\n");
    for (group, shapes) in QUESTION_CATALOGUE {
        println!("{group}");
        for s in *shapes {
            println!("  {}", s.intent);
            if s.note.is_empty() {
                println!("    {}", s.example);
            } else {
                println!("    {}   ({})", s.example, s.note);
            }
        }
        println!();
    }
    println!("`ask` is graph-grounded: it answers from the indexed code, and");
    println!("abstains rather than guessing when nothing is confidently relevant.");
    println!("A question about intent or history is usually a question for git,");
    println!("not for the graph.");
}

#[cfg(test)]
mod catalogue_tests {
    use super::QUESTION_CATALOGUE;

    /// Every example must name a subcommand that exists. A catalogue that sends
    /// users to a command travsr does not have is worse than no catalogue: #727
    /// was exactly that failure, where the docs told agents to run `travsr lang
    /// status` while the subcommand did not exist.
    #[test]
    fn every_example_names_a_real_subcommand() {
        // Taken from clap itself rather than a hand-maintained list, so a renamed
        // or removed subcommand fails here instead of drifting. A hardcoded list
        // would have to be remembered, which is the same weakness that let #727
        // ship: the docs named `travsr lang status` while the CLI did not have it.
        use clap::CommandFactory as _;
        let cmd = crate::Cli::command();
        let known: Vec<String> = cmd
            .get_subcommands()
            .flat_map(|c| {
                std::iter::once(c.get_name().to_string())
                    .chain(c.get_all_aliases().map(str::to_string))
            })
            .collect();
        assert!(!known.is_empty(), "clap reported no subcommands");
        for (group, shapes) in QUESTION_CATALOGUE {
            for s in *shapes {
                let mut words = s.example.split_whitespace();
                assert_eq!(words.next(), Some("travsr"), "{group}: {}", s.example);
                let sub = words.next().unwrap_or("");
                assert!(
                    known.iter().any(|k| k == sub),
                    "{group}: `{}` names unknown subcommand {sub:?}; known: {known:?}",
                    s.example
                );
            }
        }
    }

    /// A placeholder the user is meant to replace must look like one. Without
    /// this, a copy-pasted example silently searches for the literal text.
    #[test]
    fn placeholders_are_visibly_placeholders() {
        for (_, shapes) in QUESTION_CATALOGUE {
            for s in *shapes {
                if s.example.contains('<') {
                    assert!(
                        s.example.contains('>'),
                        "unclosed placeholder in `{}`",
                        s.example
                    );
                }
            }
        }
    }

    #[test]
    fn the_catalogue_is_not_empty_and_every_entry_is_filled_in() {
        assert!(!QUESTION_CATALOGUE.is_empty());
        for (group, shapes) in QUESTION_CATALOGUE {
            assert!(!group.is_empty());
            assert!(!shapes.is_empty(), "{group} has no entries");
            for s in *shapes {
                assert!(!s.intent.is_empty(), "{group}: empty intent");
                assert!(!s.example.is_empty(), "{group}: empty example");
            }
        }
    }
}
