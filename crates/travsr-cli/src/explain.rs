//! `travsr explain` — diagnostic seed-building trace for one query/symbol pair.
//!
//! #478 RFC-023 §6.1/WS-8: the instrument for the §9 threshold sweep. A local
//! CLI diagnostic (RFC-023 §4) — always uses the direct (cold) store-open
//! path, never the daemon control socket, since it is not performance-
//! sensitive and does not need daemon-warm state to be useful.

use anyhow::Context as _;
use travsr_mcp::query::{
    explain_query, ExplainDisposition, ExplainLeg, ExplainReport, ExplainToken,
};

use crate::daemon_client;
use crate::repo::find_git_root;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable text (default)
    Text,
    /// Machine-readable JSON — emits the full ExplainReport
    Json,
}

/// #540: how many of a token's matches reached the anchor set.
fn anchor_note(t: &ExplainToken) -> Option<String> {
    // #540: only a token that reached the anchor stage can lose matches to
    // capacity. A token below `anchor_emit_cut` is suppressed before the
    // loop and emits nothing at all; its absence is a relevance decision,
    // so claiming "cut on capacity, not relevance" there states the exact
    // opposite of what happened. Same class of error as the earlier "add a
    // distinguishing term" line: a note that is right in one case and
    // actively wrong in another.
    //
    // An unresolved token is carried by no symbol, so nothing was cut. For a
    // compound token `symbol_freq` counts the symbols carrying its rarest
    // word (`CoreNode` -> `core`, 615), only a bound on those carrying it.
    if !(t.is_anchor_emit && t.resolved && t.symbol_freq > t.anchors_emitted) {
        return None;
    }
    let (names, cut) = if travsr_core::ident::segments(&t.token).len() > 1 {
        ("names at most", "up to ")
    } else {
        ("names", "")
    };
    Some(format!(
        "{names} {} symbols, {} became anchors, {cut}{} did not reach the anchor set",
        t.symbol_freq,
        t.anchors_emitted,
        t.symbol_freq - t.anchors_emitted
    ))
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

/// #1147: the `exact` leg looks up the whole query as one name, while a seed's
/// `source exact` means a single query token named it. For a sentence query the
/// two disagree, so say which one the row measures when they do.
fn exact_leg_note(report: &ExplainReport) -> Option<&'static str> {
    let exact_seed = [&report.live, &report.fts_only]
        .iter()
        .any(|d| d.source == Some("exact"));
    let leg_missed = report.legs.as_ref().is_some_and(|l| l.exact.is_none());
    (exact_seed && leg_missed).then_some(
        "the exact leg looks up the whole query as one name; \
         `source exact` below means a query token named it (see query tokens)",
    )
}

fn print_disposition(label: &str, d: &ExplainDisposition) {
    println!("  {label}:");
    println!("    confidence:  {}", d.confidence);
    // #822: the confidence verdict is computed from these, not from the per-token
    // idf shown above. A token just under `idf_coverage_min` still reads
    // `resolved=true` but contributes 0 to `n_resolved`, which turns `coverage_ok`
    // off; without these lines that is invisible and the output reads as though
    // the token counted. Same `value / threshold` shape as the thresholds block.
    println!(
        "    coverage:    {:.3}  ({} token(s) at/above idf_coverage_min)",
        d.coverage, d.n_resolved
    );
    println!(
        "    gates:       coverage_ok={}  exact_anchor_present={}",
        d.coverage_ok, d.exact_anchor_present
    );
    match d.max_rerank_score {
        Some(r) => println!("    max_rerank:  {r:.4}"),
        None => println!("    max_rerank:  (none, reranker absent or skipped)"),
    }
    println!(
        "    anchor rescue fired: {}  (None -> weak when all gates pass)",
        d.anchor_rescued
    );
    if d.in_seed_set {
        println!(
            "    in seed set: yes (rank {}, weight {:.4}, source {})",
            d.seed_rank.unwrap_or(0),
            d.weight.unwrap_or(0.0),
            d.source.unwrap_or("?")
        );
        match d.rerank_score {
            Some(r) => println!("    rerank score: {r:.4}"),
            None => println!("    rerank score: (none, not reranked)"),
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
        anyhow::bail!("not initialized; run `travsr init`");
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
        "node:   {}, {}",
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
        if let Some(note) = anchor_note(t) {
            println!("      note: {note}");
        }
    }

    if let Some(legs) = &report.legs {
        println!("\nper-leg raw match (pre-fusion):");
        print_leg("exact", legs.exact.as_ref());
        print_leg("word", legs.word.as_ref());
        print_leg("trigram", legs.trigram.as_ref());
        print_leg("l2a", legs.l2a.as_ref());
        print_leg("embed", legs.embed.as_ref());
        if let Some(note) = exact_leg_note(&report) {
            println!("      note: {note}");
        }
    }

    println!("\ndisposition:");
    print_disposition("live", &report.live);
    print_disposition("fts-only (embeddings disabled)", &report.fts_only);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_mcp::query::{ExplainLegMatches, ExplainThresholds};

    fn token(token: &str, freq: usize, resolved: bool, anchors: usize) -> ExplainToken {
        ExplainToken {
            token: token.to_string(),
            symbol_freq: freq,
            idf_w: 0.3,
            resolved,
            is_anchor_emit: true,
            top_node_signature: None,
            anchors_emitted: anchors,
        }
    }

    /// One word: its frequency is the count of symbols carrying it (#540).
    #[test]
    fn a_one_word_token_reports_its_exact_count() {
        assert_eq!(
            anchor_note(&token("sqlite", 168, true, 3)).as_deref(),
            Some("names 168 symbols, 3 became anchors, 165 did not reach the anchor set")
        );
    }

    /// `CoreNode` read "names 615 symbols" while none carried it: 615 is how
    /// many carry its rarest word, `core`, only a bound on the compound.
    #[test]
    fn a_compound_token_reports_its_count_as_a_bound() {
        let note = anchor_note(&token("CoreNode", 615, true, 0)).unwrap_or_default();
        assert!(note.contains("at most 615"), "{note}");
    }

    /// A token nothing carries names no symbols, so nothing was cut.
    #[test]
    fn an_unresolved_token_has_no_note() {
        assert_eq!(anchor_note(&token("FooNode", 1, false, 0)), None);
    }

    fn report(source: Option<&'static str>, exact: Option<ExplainLeg>) -> ExplainReport {
        let d = ExplainDisposition {
            in_seed_set: source.is_some(),
            seed_rank: source.map(|_| 0),
            weight: None,
            source,
            rerank_score: None,
            confidence: "none",
            n_resolved: 0,
            coverage: 0.0,
            coverage_ok: false,
            exact_anchor_present: false,
            max_rerank_score: None,
            anchor_rescued: false,
        };
        ExplainReport {
            query: "how is a RunId created".to_string(),
            symbol: "struct:RunId".to_string(),
            node_found: true,
            target_signature: None,
            target_path: None,
            tokens: Vec::new(),
            thresholds: ExplainThresholds {
                idf_coverage_min: 0.0,
                anchor_emit_cut: 0.0,
                bm25_strong_floor: 0.0,
                scope_strong_floor: 0.0,
            },
            legs: Some(ExplainLegMatches {
                exact,
                ..Default::default()
            }),
            is_noise: false,
            oracle_cosine: None,
            live: d.clone(),
            fts_only: d,
        }
    }

    /// #1147: an exact-sourced seed the whole-query exact leg missed.
    #[test]
    fn an_exact_seed_the_exact_leg_missed_has_a_note() {
        assert!(exact_leg_note(&report(Some("exact"), None)).is_some());
    }

    #[test]
    fn no_note_when_the_exact_leg_matched_or_the_seed_is_not_exact() {
        let hit = ExplainLeg {
            rank: 0,
            raw_score: 1.0,
        };
        assert_eq!(exact_leg_note(&report(Some("exact"), Some(hit))), None);
        assert_eq!(exact_leg_note(&report(Some("lexical"), None)), None);
        assert_eq!(exact_leg_note(&report(None, None)), None);
    }
}
