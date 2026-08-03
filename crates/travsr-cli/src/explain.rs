//! `travsr explain` — diagnostic seed-building trace for one query/symbol pair.
//!
//! #478 RFC-023 §6.1/WS-8: the instrument for the §9 threshold sweep. A local
//! CLI diagnostic (RFC-023 §4) — always uses the direct (cold) store-open
//! path, never the daemon control socket, since it is not performance-
//! sensitive and does not need daemon-warm state to be useful.

use anyhow::Context as _;
use travsr_mcp::query::{explain_query, ExplainDisposition, ExplainLeg, ExplainReport};

use crate::daemon_client;
use crate::repo::find_git_root;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable text (default)
    Text,
    /// Machine-readable JSON — emits the full ExplainReport
    Json,
}

fn print_leg(name: &str, leg: Option<&ExplainLeg>) {
    match leg {
        Some(l) => println!(
            "    {name:<8} rank {:<4} raw_score {:.4}",
            l.rank, l.raw_score
        ),
        None => println!("    {name:<8} (no match)"),
    }
}

fn print_disposition(label: &str, d: &ExplainDisposition) {
    println!("  {label}:");
    println!("    confidence:  {}", d.confidence);
    if d.in_seed_set {
        println!(
            "    in seed set: yes (rank {}, weight {:.4}, source {})",
            d.seed_rank.unwrap_or(0),
            d.weight.unwrap_or(0.0),
            d.source.unwrap_or("?")
        );
        match d.rerank_score {
            Some(r) => println!("    rerank score: {r:.4}"),
            None => println!("    rerank score: (none — not reranked)"),
        }
    } else {
        println!("    in seed set: no");
    }
}

pub fn run(query_str: &str, symbol: &str, format: OutputFormat) -> anyhow::Result<()> {
    if query_str.trim().is_empty() {
        anyhow::bail!("query must not be empty");
    }
    if symbol.trim().is_empty() {
        anyhow::bail!("symbol must not be empty");
    }
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;
    let db_path = repo_root.join(".travsr/graph.db");
    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    let mut store = daemon_client::open_read_store(&db_path)?;
    travsr_daemon::try_inject_embed_hook_readonly(&mut store, &db_path);
    let knn = store.embed_knn_fn();
    let knn_ref = knn
        .as_ref()
        .map(|f| f as &dyn Fn(&str, u32) -> Vec<(travsr_core::NodeId, f32)>);
    let report: ExplainReport = explain_query(&store, query_str, symbol, knn_ref);

    if matches!(format, OutputFormat::Json) {
        println!("{}", serde_json::to_string(&report)?);
        return Ok(());
    }

    println!("query:  \"{}\"", report.query);
    println!("symbol: \"{}\"", report.symbol);
    if !report.node_found {
        println!("\nsymbol not found in the graph (no node matches this name)");
        return Ok(());
    }
    println!(
        "node:   {} — {}",
        report.target_signature.as_deref().unwrap_or("?"),
        report.target_path.as_deref().unwrap_or("?")
    );
    println!("is_noise: {}", report.is_noise);
    match report.oracle_cosine {
        Some(c) => println!("oracle cosine: {c:.4}"),
        None => println!("oracle cosine: (not scored)"),
    }

    println!("\nthresholds:");
    println!(
        "  idf_coverage_min:   {:.3}",
        report.thresholds.idf_coverage_min
    );
    println!(
        "  anchor_emit_cut:    {:.3}",
        report.thresholds.anchor_emit_cut
    );
    println!(
        "  bm25_strong_floor:  {:.3}",
        report.thresholds.bm25_strong_floor
    );
    println!(
        "  scope_strong_floor: {:.3}",
        report.thresholds.scope_strong_floor
    );

    println!("\nquery tokens:");
    for t in &report.tokens {
        println!(
            "  \"{}\"  freq={}  idf={:.3}  resolved={}  anchor_emit={}{}",
            t.token,
            t.symbol_freq,
            t.idf_w,
            t.resolved,
            t.is_anchor_emit,
            t.top_node_signature
                .as_ref()
                .map(|s| format!("  top_node={s}"))
                .unwrap_or_default()
        );
    }

    if let Some(legs) = &report.legs {
        println!("\nper-leg raw match (pre-fusion):");
        print_leg("exact", legs.exact.as_ref());
        print_leg("word", legs.word.as_ref());
        print_leg("trigram", legs.trigram.as_ref());
        print_leg("l2a", legs.l2a.as_ref());
        print_leg("embed", legs.embed.as_ref());
    }

    println!("\ndisposition:");
    print_disposition("live", &report.live);
    print_disposition("fts-only (embeddings disabled)", &report.fts_only);

    Ok(())
}
