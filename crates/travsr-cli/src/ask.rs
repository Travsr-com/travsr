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

    // A question about travsr, or about the repository as a whole, is not a
    // question the graph can answer. `ask` seeds from the words in the query, so
    // "what is this repo written in?" matches `var:REPO` and returns a screen of
    // bench files with confident-looking scores. That is worse than abstaining:
    // the answer exists, one command away, and the result looks like a real one.
    //
    // Caught before retrieval rather than after, because retrieval succeeds here.
    // There is no low-confidence signal to hang an abstention on.
    if let Some(redirect) = meta_question_redirect(query_str) {
        println!("{}", redirect.answer);
        if !redirect.command.is_empty() {
            use std::io::IsTerminal as _;
            let pal = crate::progress::Palette::for_stream(std::io::stdout().is_terminal());
            println!("  {} {}", pal.dim("$"), pal.ident(redirect.command));
        }
        return Ok(());
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
        // where they are least sure.
        //
        // A static list of examples is barely better, because the user still has
        // to translate their own question into one of them. So the suggestions
        // below are built from what they actually typed: the nearest real symbol
        // in this repo, and the command that matches the intent their wording
        // signals. Falls back to the catalogue only when nothing specific can be
        // said, rather than leading with it.
        println!("no grounded match for '{display_query}' in this repo");
        let suggestions = suggest_next(&db_path, display_query);
        use std::io::IsTerminal as _;
        if suggestions.is_empty() {
            println!("run `travsr ask --examples` to see what travsr can answer");
        } else {
            let pal = crate::progress::Palette::for_stream(std::io::stdout().is_terminal());
            println!("\ntry one of these:");
            for s in &suggestions {
                println!("  {}", paint_command(pal, &s.command, &s.ident));
                println!("      {}", pal.dim(&s.why));
            }
            println!("\n{}", pal.dim("or `travsr ask --examples` for more"));
        }
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

/// The questions travsr answers, as templates over a symbol.
///
/// A flat list rather than grouped sections with notes: an earlier version read
/// as a manual page, and the point is to show what can be typed, not to document
/// the CLI. Each entry is a question a reader might actually have, paired with
/// the one command that answers it.
///
/// `(question, command)`. `{sym}` is filled with a real symbol from the reader's
/// own repository, `{term}` with a lowercased form for text search.
///
/// Diagnostics are deliberately absent. `travsr explain` describes itself as
/// "a diagnostic for tuning search; not part of normal use", and `fsck` reports
/// ghost nodes and orphan edges. Both answer questions about travsr's own
/// internals rather than about the reader's code, and a list of "questions you
/// can ask" is not where someone should first meet them.
///
/// Only `ask` shapes measured to return results are included. `ask` is
/// graph-grounded, so it answers when the question is anchored to something in
/// the code and abstains when it is not: "what breaks if I change X" returns
/// nothing today, which is the known conceptual-query gap, so that question is
/// routed to `graph` instead of being suggested as an `ask`.
///
/// "how does X work" was dropped for the same reason after testing it: it
/// returns results for some symbols and abstains for others, and a suggestion
/// that works only sometimes is worse than one fewer. A bare symbol never
/// abstains, so that is the shape offered for `ask`.
const FAQ: &[(&str, &str)] = &[
    // Orientation: what someone asks before they know any symbol.
    ("what is this repo written in?", "travsr lang list"),
    ("is the index ready to query?", "travsr status"),
    // Finding things.
    ("where is {sym} defined?", "travsr ask \"{sym}\""),
    (
        "where does the text \"TODO\" appear?",
        "travsr pattern \"TODO\"",
    ),
    // Structure. These are the questions the graph exists for.
    (
        "what calls {sym}?",
        "travsr graph {sym} --direction callers",
    ),
    (
        "what does {sym} depend on?",
        "travsr graph {sym} --direction deps",
    ),
    (
        "what breaks if I change {sym}?",
        "travsr graph {sym} --direction both",
    ),
    (
        "where is {sym} used, before a rename?",
        "travsr references {sym}",
    ),
];

/// Print the questions, using symbols from the reader's own repository.
pub fn print_examples(db_path: Option<&std::path::Path>) {
    use std::io::IsTerminal as _;
    let pal = crate::progress::Palette::for_stream(std::io::stdout().is_terminal());

    // Several symbols, rotated. One name repeated down the page reads as a
    // template with a variable substituted, which is what it would be.
    let symbols = example_symbols(db_path, FAQ.len());
    let grounded = !symbols.is_empty();
    let symbols = if grounded {
        symbols
    } else {
        vec!["PaymentService".to_string()]
    };

    println!("{}", pal.bold("Questions you can ask"));
    println!(
        "{}",
        pal.dim(&if grounded {
            "using symbols from this repo, so these run as they are".to_string()
        } else {
            "run `travsr init` first and these fill in with your own symbols".to_string()
        })
    );
    println!();

    for (i, (question, command)) in FAQ.iter().enumerate() {
        let sym = &symbols[i % symbols.len()];
        let term = sym.to_lowercase();
        let q = question.replace("{sym}", sym).replace("{term}", &term);
        let c = command.replace("{sym}", sym).replace("{term}", &term);
        println!("  {q}");
        println!("      {}", paint_command(pal, &c, sym));
    }
}

/// A question `ask` cannot answer, and the command that can.
pub(crate) struct MetaRedirect {
    pub answer: &'static str,
    pub command: &'static str,
}

/// Phrases that mean the question is about travsr or the repository as a whole,
/// paired with what actually answers them.
///
/// Matched as whole phrases rather than keywords, deliberately. A bare "repo" or
/// "language" appears in plenty of legitimate code questions, and hijacking those
/// would be a worse failure than the one being fixed: a user asking about a
/// symbol named `Language` must still get their search.
const META_QUESTIONS: &[(&[&str], MetaRedirect)] = &[
    (
        &[
            "what is this repo written in",
            "what languages",
            "what language is this",
            "which languages",
            "tech stack",
            "what is this codebase written in",
        ],
        MetaRedirect {
            answer: "That is a question about the repository rather than about a symbol in it.",
            command: "travsr lang list",
        },
    ),
    (
        &[
            "how big is",
            "how many files",
            "how many nodes",
            "is the index",
            "is my index",
            "is the graph fresh",
            "is it up to date",
        ],
        MetaRedirect {
            answer: "That is a question about the index rather than about the code.",
            command: "travsr status",
        },
    ),
    (
        &[
            "what is travsr",
            "how does travsr work",
            "what does travsr do",
            "how do i install",
            "how to install",
            "who made travsr",
        ],
        MetaRedirect {
            answer: "That is a question about travsr itself.",
            command: "travsr faq",
        },
    ),
];

/// Route a question about travsr or the repo away from graph retrieval.
///
/// Returns `None` for anything else, so an ordinary code question is untouched.
pub(crate) fn meta_question_redirect(query: &str) -> Option<&'static MetaRedirect> {
    let q = query.to_lowercase();
    META_QUESTIONS
        .iter()
        .find(|(phrases, _)| phrases.iter().any(|p| q.contains(p)))
        .map(|(_, r)| r)
}

/// Colour a rendered command so its shape reads at a glance.
///
/// Three roles, because they answer three different questions for the reader:
/// which tool (constant, so dimmed), which action (the verb, so it leads), and
/// which part is theirs to replace (the identifier). Without that, the line is a
/// uniform run of words and the reader has to parse it before they can use it.
///
/// Everything routes through `Palette`, so `NO_COLOR`, `CLICOLOR_FORCE` and a
/// non-tty stdout all fall back to plain text with the spacing unchanged.
fn paint_command(pal: crate::progress::Palette, cmd: &str, ident: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for (i, word) in cmd.split(' ').enumerate() {
        let painted = if i == 0 {
            // `travsr` is on every line, so it carries no information.
            pal.dim(word)
        } else if i == 1 {
            // The subcommand is the verb: what this line actually does.
            pal.green(word)
        } else if word.starts_with("--") {
            pal.dim(word)
        } else if word.contains(ident) {
            // The part the reader swaps for their own symbol.
            pal.ident(word)
        } else {
            // A flag's value (`callers`, `both`) belongs with its flag.
            pal.dim(word)
        };
        out.push(painted);
    }
    out.join(" ")
}

/// Up to `want` symbols worth putting in examples: real, in this repo, not noise.
///
/// Ranked by in-degree, because an example that resolves to a leaf with no
/// callers demonstrates the command without demonstrating an answer. Returns
/// several so the printed list varies; one name repeated down the page reads as a
/// filled-in template rather than as questions about this codebase.
fn example_symbols(db_path: Option<&std::path::Path>, want: usize) -> Vec<String> {
    let Some(db_path) = db_path else {
        return Vec::new();
    };
    let Ok(store) = crate::daemon_client::open_read_store(db_path) else {
        return Vec::new();
    };

    // Classes and functions only: a file or module node is a valid graph node but
    // a confusing thing to put in `travsr graph <symbol>`.
    let mut candidates: Vec<travsr_core::Node> = Vec::new();
    for kind in ["class", "function", "method", "struct"] {
        if let Ok(mut ns) = store.nodes_by_kind(kind) {
            candidates.append(&mut ns);
        }
    }
    candidates.retain(|n| {
        !travsr_core::noise::is_structural_noise(n)
            // Very short names make confusing examples even when well connected.
            && travsr_core::ident::leaf_of(&n.vname.signature).len() >= 4
    });
    if candidates.is_empty() {
        return Vec::new();
    }

    let ids: Vec<travsr_core::NodeId> = candidates.iter().map(|n| n.id).collect();
    let degrees = store.in_degrees(&ids).unwrap_or_default();
    candidates.sort_by_key(|n| std::cmp::Reverse(degrees.get(&n.id).copied().unwrap_or(0)));

    let mut out: Vec<String> = Vec::new();
    for n in candidates {
        let leaf = travsr_core::ident::leaf_of(&n.vname.signature).to_string();
        // Distinct names only: the same leaf can appear on several nodes, and a
        // repeated one defeats the reason for collecting more than one.
        if !out.contains(&leaf) {
            out.push(leaf);
        }
        if out.len() >= want {
            break;
        }
    }
    out
}

#[cfg(test)]
mod meta_question_tests {
    use super::{meta_question_redirect, META_QUESTIONS};

    /// The reported failure: `ask "what is this repo written in?"` matched the
    /// word "repo" against `var:REPO` and returned a screen of bench files with
    /// confident scores. Retrieval succeeds there, so there is no low-confidence
    /// signal to abstain on; it has to be caught before the search runs.
    #[test]
    fn the_reported_question_is_redirected() {
        let r = meta_question_redirect("what is this repo written in?")
            .expect("must be recognised as a question about the repo");
        assert_eq!(r.command, "travsr lang list");
    }

    #[test]
    fn questions_about_travsr_itself_go_to_the_faq() {
        for q in [
            "what is travsr",
            "how does travsr work",
            "how do I install travsr",
        ] {
            let r = meta_question_redirect(q).unwrap_or_else(|| panic!("{q} not matched"));
            assert_eq!(r.command, "travsr faq", "{q}");
        }
    }

    #[test]
    fn questions_about_the_index_go_to_status() {
        for q in [
            "how big is the graph",
            "is my index ready",
            "is it up to date",
        ] {
            let r = meta_question_redirect(q).unwrap_or_else(|| panic!("{q} not matched"));
            assert_eq!(r.command, "travsr status", "{q}");
        }
    }

    /// The failure mode that would be worse than the bug. A user searching for a
    /// symbol whose name happens to contain "repo" or "language" must still get
    /// their search: hijacking a real query is a silent wrong answer, where the
    /// original problem was at least visibly noisy.
    #[test]
    fn real_code_questions_are_never_hijacked() {
        for q in [
            "repo_languages",
            "language_distribution",
            "where is Language defined",
            "what calls repo_root",
            "RepoStats",
            "normalize_repo_root",
            "how does the parser handle a repo path",
        ] {
            assert!(
                meta_question_redirect(q).is_none(),
                "`{q}` is a code question and must reach retrieval"
            );
        }
    }

    /// Whole phrases, not keywords. A single word like "repo" or "language" is
    /// too common in real queries to route on, and this is what keeps the test
    /// above passing rather than luck.
    #[test]
    fn every_trigger_is_a_phrase_not_a_bare_keyword() {
        for (phrases, redirect) in META_QUESTIONS {
            assert!(!phrases.is_empty());
            for p in *phrases {
                assert!(
                    p.contains(' '),
                    "`{p}` is a single word; routing on it would hijack code queries"
                );
                assert_eq!(p.to_lowercase(), *p, "`{p}` must be lowercase to match");
            }
            assert!(!redirect.answer.is_empty());
            assert!(!redirect.command.is_empty());
        }
    }

    /// Every redirect must name a command that exists, for the same reason the
    /// other lists do: #727 was a documented command that did not.
    #[test]
    fn every_redirect_names_a_real_subcommand() {
        use clap::CommandFactory as _;
        let cmd = crate::Cli::command();
        let known: Vec<String> = cmd
            .get_subcommands()
            .flat_map(|c| {
                std::iter::once(c.get_name().to_string())
                    .chain(c.get_all_aliases().map(str::to_string))
            })
            .collect();
        for (_, r) in META_QUESTIONS {
            let sub = r.command.split_whitespace().nth(1).unwrap_or("");
            assert!(
                known.iter().any(|k| k == sub),
                "`{}` names unknown subcommand {sub:?}",
                r.command
            );
        }
    }
}

#[cfg(test)]
mod faq_tests {
    use super::FAQ;

    /// Every entry must render with nothing left to substitute. A stray `{sym}`
    /// reaching the terminal would be pasted verbatim and search for that text.
    #[test]
    fn every_entry_renders_cleanly() {
        for (question, command) in FAQ {
            for (label, text) in [("question", question), ("command", command)] {
                let r = text.replace("{sym}", "Widget").replace("{term}", "widget");
                assert!(
                    !r.contains('{') && !r.contains('}'),
                    "{label} `{text}` left a slot unfilled: {r}"
                );
            }
        }
    }

    /// Each entry is scanned for by its question, so it has to read as one.
    /// An earlier version used command labels ("Find a symbol by name"), which
    /// describe the tool rather than the reader's problem.
    #[test]
    fn every_question_reads_as_a_question() {
        for (question, _) in FAQ {
            assert!(
                question.ends_with('?'),
                "`{question}` is a label, not a question"
            );
            assert!(
                question.chars().next().is_some_and(|c| c.is_lowercase()),
                "`{question}` should read as spoken text, not a heading"
            );
        }
    }

    /// Only commands that exist. A list naming a subcommand travsr does not have
    /// sends someone to a dead end while looking authoritative, which is exactly
    /// what #727 was. Read from clap rather than a hand-kept copy.
    #[test]
    fn every_command_names_a_real_subcommand() {
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

        for (_, command) in FAQ {
            let mut words = command.split_whitespace();
            assert_eq!(words.next(), Some("travsr"), "`{command}`");
            let sub = words.next().unwrap_or("");
            assert!(
                known.iter().any(|k| k == sub),
                "`{command}` names unknown subcommand {sub:?}"
            );
        }
    }

    /// Diagnostics stay out of a user-facing list. `explain` describes itself as
    /// "not part of normal use", and `fsck` reports ghost nodes and orphan edges:
    /// both are about travsr's own internals rather than the reader's code.
    /// Pinned because they are easy to add back, being genuinely useful commands.
    #[test]
    fn no_internal_diagnostics_are_offered() {
        for (question, command) in FAQ {
            for internal in ["explain", "fsck"] {
                assert!(
                    !command.contains(&format!("travsr {internal}")),
                    "`{question}` offers the {internal} diagnostic; it answers a \
                     question about travsr's internals, not about this codebase"
                );
            }
        }
    }

    /// `ask` abstains on questions it cannot ground. "what breaks if I change X"
    /// is the measured case: it returns nothing today, so it must be routed to
    /// `graph`, not offered as something to ask. Pins the routing rather than
    /// leaving it to whoever edits the list next.
    #[test]
    fn questions_ask_cannot_answer_are_not_routed_to_ask() {
        for (question, command) in FAQ {
            if question.to_lowercase().contains("what breaks") {
                assert!(
                    command.contains("graph") && command.contains("--direction both"),
                    "`{question}` must route to graph, not `{command}`"
                );
            }
        }
    }
}

/// One concrete next step, phrased as a command the user can paste.
pub(crate) struct Suggestion {
    pub command: String,
    pub why: String,
    /// The part of `command` the reader is expected to swap. Carried rather than
    /// re-derived so the painter highlights exactly what was substituted.
    pub ident: String,
}

/// Intent keywords mapped to the command that actually answers them.
///
/// `ask` is graph-grounded retrieval, so several common questions are better
/// served by a different subcommand entirely. A user who phrases one of those as
/// a question gets an abstention today and no hint that the answer exists one
/// command over.
///
/// Ordered most specific first: "what breaks if" must win over the bare "what",
/// and "who calls" over "call".
const INTENT_ROUTES: &[(&[&str], &str, &str)] = &[
    (
        &[
            "what breaks",
            "blast radius",
            "impact of",
            "safe to change",
            "safe to remove",
        ],
        "travsr graph {sym} --direction both",
        "callers and dependencies together, which is what breaks",
    ),
    (
        &[
            "who calls",
            "what calls",
            "callers of",
            "used by",
            "call sites",
        ],
        "travsr graph {sym} --direction callers",
        "incoming call edges",
    ),
    (
        &[
            "depend on",
            "dependencies of",
            "imports",
            "what does it use",
        ],
        "travsr graph {sym} --direction deps",
        "outgoing dependency edges",
    ),
    (
        &["every use", "all uses", "references to", "rename"],
        "travsr references {sym}",
        "every use site with path:line, wider than callers",
    ),
    (
        &[
            "config",
            "setting",
            "option",
            "env var",
            "environment variable",
        ],
        "travsr config get <key>",
        "configuration is not in the code graph",
    ),
    (
        &["why did", "why is", "ranked", "scored", "not showing"],
        "travsr explain \"{q}\" <symbol>",
        "shows which terms matched and which thresholds failed",
    ),
];

/// Build next steps from the user's own question.
///
/// Two independent sources, because they fail differently. Symbol lookup finds a
/// real name in *this* repo, which is the strongest possible suggestion but only
/// works when the query contains something close to one. Intent routing works on
/// phrasing alone, which covers the case where the user described what they want
/// without naming anything that exists.
///
/// Returns empty rather than padding with generic advice, so the caller can fall
/// back to the catalogue instead of printing suggestions that suggest nothing.
pub(crate) fn suggest_next(db_path: &std::path::Path, query: &str) -> Vec<Suggestion> {
    let lower = query.to_lowercase();
    let mut out: Vec<Suggestion> = Vec::new();

    // The nearest real symbol, if the repo has one. Uses the same fuzzy search
    // `ask` itself uses, so a suggestion can never name something unindexed.
    let nearest = nearest_symbol(db_path, query);

    if let Some(sym) = nearest.as_deref() {
        out.push(Suggestion {
            command: format!("travsr ask \"{sym}\""),
            why: "closest symbol in this repo to what you typed".to_string(),
            ident: sym.to_string(),
        });
    }

    // Intent routing. `{sym}` is substituted when a real symbol was found;
    // otherwise a placeholder, so the shape of the answer is still visible.
    let sym_slot = nearest.clone().unwrap_or_else(|| "<symbol>".to_string());
    for (keys, template, why) in INTENT_ROUTES {
        if keys.iter().any(|k| lower.contains(k)) {
            out.push(Suggestion {
                command: template.replace("{sym}", &sym_slot).replace("{q}", query),
                why: (*why).to_string(),
                ident: sym_slot.clone(),
            });
            // One intent route is a hint; several is a menu the user has to
            // re-read. Stop at the most specific match.
            break;
        }
    }

    // The text escape hatch, offered only when there is a distinctive term to
    // search for. `pattern` answers questions the graph deliberately does not
    // model, which is a large share of what reaches this path.
    if let Some(term) = distinctive_term(query) {
        out.push(Suggestion {
            command: format!("travsr pattern \"{term}\""),
            why: "searches the text of tracked files, for things the graph does not model"
                .to_string(),
            ident: term.clone(),
        });
    }

    out
}

/// The closest indexed symbol to `query`, or `None` when nothing is close.
///
/// Deliberately reuses the store's own fuzzy search rather than a bespoke match,
/// so a suggestion is always a name that exists. Suggesting a symbol the repo
/// does not contain would repeat the mistake this whole path exists to fix.
fn nearest_symbol(db_path: &std::path::Path, query: &str) -> Option<String> {
    let store = crate::daemon_client::open_read_store(db_path).ok()?;
    let hits = store.search_nodes_fuzzy(query).ok()?;
    let leaf = hits
        .iter()
        .find(|n| !travsr_core::noise::is_structural_noise(n))
        .or_else(|| hits.first())
        .map(|n| travsr_core::ident::leaf_of(&n.vname.signature).to_string())?;
    (!leaf.is_empty()).then_some(leaf)
}

/// The longest word in the query that is not a stop word, used as a text search
/// term. Longest because it is the most selective: `pattern "the"` is noise.
fn distinctive_term(query: &str) -> Option<String> {
    const STOP: &[&str] = &[
        "what", "where", "which", "who", "why", "how", "does", "do", "did", "is", "are", "was",
        "the", "this", "that", "for", "from", "with", "and", "not", "can", "should", "would",
        "when", "there", "here", "into", "about", "code", "function", "class", "file",
    ];
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 4 && !STOP.contains(&w.to_lowercase().as_str()))
        .max_by_key(|w| w.len())
        .map(str::to_string)
}

#[cfg(test)]
mod suggestion_tests {
    use super::{distinctive_term, INTENT_ROUTES};

    /// Every route must produce a runnable command, not a template with an
    /// unsubstituted slot left in it.
    #[test]
    fn routes_have_no_unsubstituted_slots_after_rendering() {
        for (_, template, _) in INTENT_ROUTES {
            let rendered = template.replace("{sym}", "Foo").replace("{q}", "a query");
            assert!(
                !rendered.contains('{') && !rendered.contains('}'),
                "template `{template}` left a slot unfilled: {rendered}"
            );
        }
    }

    /// The specific phrasings must beat the general ones. "what breaks if I
    /// change X" must route to blast radius, not to the bare "what calls" rule,
    /// which is why order is load-bearing rather than incidental.
    #[test]
    fn the_most_specific_intent_wins() {
        let q = "what breaks if i change the ledger";
        let first = INTENT_ROUTES
            .iter()
            .find(|(keys, _, _)| keys.iter().any(|k| q.contains(k)))
            .expect("phrase must match some route");
        assert!(
            first.1.contains("--direction both"),
            "expected the blast-radius route, got `{}`",
            first.1
        );
    }

    #[test]
    fn caller_phrasings_route_to_callers() {
        for q in [
            "who calls payment service",
            "what calls this",
            "call sites of foo",
        ] {
            let hit = INTENT_ROUTES
                .iter()
                .find(|(keys, _, _)| keys.iter().any(|k| q.contains(k)))
                .unwrap_or_else(|| panic!("no route matched {q:?}"));
            assert!(hit.1.contains("callers"), "{q:?} routed to `{}`", hit.1);
        }
    }

    /// Configuration questions are the clearest case of "the answer exists, just
    /// not in the graph", so they must not route to a graph command.
    #[test]
    fn configuration_questions_route_away_from_the_graph() {
        let hit = INTENT_ROUTES
            .iter()
            .find(|(keys, _, _)| {
                keys.iter()
                    .any(|k| "how do i set the config option".contains(k))
            })
            .expect("must match");
        assert!(hit.1.starts_with("travsr config get"), "got `{}`", hit.1);
    }

    #[test]
    fn the_search_term_is_selective_not_a_stop_word() {
        assert_eq!(
            distinctive_term("where is the retry budget configured"),
            Some("configured".to_string())
        );
        // All stop words or too short: nothing worth searching for.
        assert_eq!(distinctive_term("what is it for"), None);
        assert_eq!(distinctive_term(""), None);
    }
}
