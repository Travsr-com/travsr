//! `travsr status` — index and graph health summary.
//!
//! Data acquisition is shared with the daemon via `travsr_mcp::query`
//! (#318 O1): a running daemon answers from its warm store; otherwise the
//! store is opened directly (read-only fast path).

use anyhow::Context as _;
use travsr_mcp::query::{self, StatusPayload};

use crate::daemon_client;
use crate::repo::find_git_root;

pub fn run() -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;

    let db_path = repo_root.join(".travsr").join("graph.db");

    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    let payload: StatusPayload =
        match daemon_client::try_query(&repo_root, "status", serde_json::json!({})) {
            Some(p) => p,
            None => {
                let store = daemon_client::open_read_store(&db_path)
                    .with_context(|| format!("opening graph database at {}", db_path.display()))?;
                query::status_query(&store)?
            }
        };

    let last_commit = payload.last_commit.as_deref().unwrap_or("(none)");
    // M7: compare last_commit vs phase_b_commit to show Phase B freshness.
    let phase_b_state = match payload.phase_b_commit.as_deref() {
        Some(pb) if !pb.is_empty() && Some(pb) == payload.last_commit.as_deref() => "complete",
        Some(pb) if !pb.is_empty() => "pending",
        _ => "not run",
    };
    // RFC-021 P5: reranker state. Old daemons omit the field (serde default
    // empty) — suppress the segment then so mixed CLI/daemon versions stay clean.
    let rerank_segment = if payload.rerank.is_empty() {
        String::new()
    } else {
        format!(" | rerank: {}", payload.rerank)
    };
    println!(
        "nodes: {} | edges: {} | schema: v{} | journal: {} | last_commit: {} | phase_b: {}{}",
        payload.nodes,
        payload.edges,
        payload.schema,
        payload.journal,
        last_commit,
        phase_b_state,
        rerank_segment
    );

    // RFC-014 #317 re-index policy: surface signature-format skew so the user
    // knows the graph was built with an older format and a re-index is due.
    let sig_v = payload.signature_format_version;
    if sig_v != travsr_core::SIGNATURE_FORMAT_VERSION {
        eprintln!(
            "warning: signature format v{sig_v} != current v{} — graph built with an older format; run `travsr init` to re-index",
            travsr_core::SIGNATURE_FORMAT_VERSION
        );
    }

    // L11: detect FTS/nodes skew — indicates a partial write or corrupt FTS index.
    let fts = payload.fts_nodes;
    if fts > 0 && fts != payload.nodes {
        eprintln!(
            "warning: FTS index has {fts} rows but graph has {} nodes — run `travsr init` to rebuild",
            payload.nodes
        );
    }

    // H3: surface Phase B warnings so the user knows about crashed/mismatched
    // analyzers without having to re-read the init output.
    if let Some(warnings) = &payload.phase_b_warnings {
        if !warnings.is_empty() {
            for warn in warnings.split(',') {
                let parts: Vec<&str> = warn.splitn(2, ':').collect();
                match parts.as_slice() {
                    ["crashed", lang] => eprintln!(
                        "warning: phase B analyzer for '{lang}' crashed — re-run `travsr init --semantic` to retry"
                    ),
                    ["version_mismatch", rest] => {
                        let v: Vec<&str> = rest.splitn(3, ':').collect();
                        if let [lang, expected, got] = v.as_slice() {
                            eprintln!(
                                "warning: '{lang}' sidecar protocol v{got} != expected v{expected} — run `travsr lang install {lang}`"
                            );
                        }
                    }
                    ["needs_approval", lang] => eprintln!(
                        "warning: '{lang}' requires elevated sandbox approval — run `travsr lang approve {lang}`"
                    ),
                    // #449: languages present in the repo whose Phase B sidecar
                    // never ran, previously a silent skip that left the user
                    // with "0 references" and no explanation.
                    ["skipped_unregistered", lang] => eprintln!(
                        "warning: '{lang}' sources found but semantic indexing is not set up. Run `travsr lang install {lang}`"
                    ),
                    ["skipped_no_analyzer", lang] => eprintln!(
                        "warning: '{lang}' is registered but its analyzer binary is missing. Run `travsr lang install {lang}`"
                    ),
                    // E6: SCIP definitions that did not unify onto their Phase A
                    // tree-sitter node — their references attribute to an orphaned
                    // duplicate node instead. `rate` is missed/attempted.
                    ["scip_unification_misses", rate] => eprintln!(
                        "warning: {rate} SCIP definitions did not unify onto their tree-sitter nodes — some references may resolve to a duplicate node. Re-run `travsr init --semantic` if it persists."
                    ),
                    _ => {}
                }
            }
        }
    }

    // M1: warn when Rust semantic edges are degraded due to sandbox unavailability.
    if let Some(reason) = &payload.rust_lsif_degraded {
        if reason == "sandbox_unavailable" {
            eprintln!(
                "warning: Rust semantic edges degraded — rust-analyzer LSIF was \
                 skipped because the OS sandbox (bubblewrap/sandbox-exec) is \
                 unavailable. Install bubblewrap, or re-run \
                 `travsr init --allow-unsandboxed-lsif` if you trust this repo."
            );
        }
    }

    Ok(())
}
