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

pub fn run(query_str: &str, format: OutputFormat) -> anyhow::Result<()> {
    if query_str.trim().is_empty() {
        anyhow::bail!("search query must not be empty — try: travsr ask \"PaymentService\"");
    }
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
