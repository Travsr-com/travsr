//! `travsr graph` — subgraph rendering around a symbol or file.
//!
//! Data acquisition is shared with the daemon via `travsr_mcp::query`
//! (#318 O1): a running daemon answers from its warm store; otherwise the
//! store is opened directly (read-only fast path). Rendering happens here,
//! from the payload, so both routes produce identical output.

use std::collections::{HashMap, HashSet};

use anyhow::Context as _;
use travsr_mcp::query::{
    self, EdgeEntry, GraphPayload, GraphQueryArgs, NodeEntry, QueryDirection, QueryEdgeMode,
};

use crate::daemon_client;
use crate::repo::find_git_root;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Direction {
    /// Follow outgoing edges (what does this symbol import / define?)
    Deps,
    /// Follow incoming edges (who calls / depends on this symbol?)
    Callers,
    /// Follow both directions
    Both,
}

impl From<Direction> for QueryDirection {
    fn from(d: Direction) -> Self {
        match d {
            Direction::Deps => QueryDirection::Deps,
            Direction::Callers => QueryDirection::Callers,
            Direction::Both => QueryDirection::Both,
        }
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Format {
    /// ASCII tree printed to stdout
    Tree,
    /// Graphviz DOT with clusters and shapes — pipe to `dot -Tsvg` to render
    Dot,
    /// Structured JSON for AI / tooling consumption
    Json,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum EdgeMode {
    /// Prefer semantic call edges; fall back to structural if none exist
    Semantic,
    /// Follow all edge kinds (original behaviour)
    All,
}

impl From<EdgeMode> for QueryEdgeMode {
    fn from(m: EdgeMode) -> Self {
        match m {
            EdgeMode::Semantic => QueryEdgeMode::Semantic,
            EdgeMode::All => QueryEdgeMode::All,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    query_str: &str,
    depth: u8,
    direction: Direction,
    format: Format,
    edge_mode: EdgeMode,
    include_noise: bool,
    budget: usize,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;
    let db_path = repo_root.join(".travsr/graph.db");

    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    let args = GraphQueryArgs {
        query: query_str.to_string(),
        depth,
        direction: direction.into(),
        edge_mode: edge_mode.into(),
        include_noise,
    };

    // Daemon route first (#318 O1), direct read-only open as fallback.
    let payload: GraphPayload =
        match daemon_client::try_query(&repo_root, "graph", serde_json::to_value(&args)?) {
            Some(p) => p,
            None => {
                let store = daemon_client::open_read_store(&db_path)?;
                query::graph_query(&store, &args)?
            }
        };

    if payload.seed.is_none() {
        println!("no symbols matching '{query_str}'");
        return Ok(());
    }

    render(payload, format, budget, Some(&repo_root))
}

pub fn run_all(format: Format, budget: usize) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;
    let db_path = repo_root.join(".travsr/graph.db");

    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    // --all dumps are large by construction — always computed locally rather
    // than shipped through the daemon socket.
    let store = daemon_client::open_read_store(&db_path)?;
    let payload = query::graph_all_payload(&store)?;
    render(payload, format, budget, None)
}

fn render(
    mut payload: GraphPayload,
    format: Format,
    budget: usize,
    repo_root: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    if payload.no_semantic_fallback {
        print_semantic_fallback_note(&payload, repo_root);
    }

    // #318 O6: token budget — prefix of BFS order, seed always kept.
    let truncated = query::apply_token_budget(&mut payload, budget);

    match format {
        Format::Tree => {
            if let Some(seed) = &payload.seed {
                println!("{} ({})", seed.label, seed.kind);
                print_tree(&payload);
            } else {
                // --all tree mode: file listing (historic behaviour).
                let mut files: Vec<&NodeEntry> =
                    payload.nodes.iter().filter(|n| n.kind == "file").collect();
                files.sort_by(|a, b| a.path.cmp(&b.path));
                for node in files {
                    println!("{}", node.path);
                }
            }
            if truncated > 0 {
                println!("… {truncated} more nodes beyond budget (raise with --budget)");
            }
        }
        Format::Dot => {
            print_dot(&payload)?;
            if truncated > 0 {
                println!("// … {truncated} more nodes beyond budget (raise with --budget)");
            }
        }
        Format::Json => print_json(&payload, budget, truncated)?,
    }

    Ok(())
}

/// O5 staleness note: the structural-fallback hint, extended with how far the
/// semantic (Phase B / LSIF) data lags behind the indexed commit.
fn print_semantic_fallback_note(payload: &GraphPayload, repo_root: Option<&std::path::Path>) {
    eprintln!(
        "note: no semantic caller edges found — showing file-level imports. \
         Run `travsr lang` to enable call-site analysis."
    );
    let (Some(cov), Some(last)) = (&payload.coverage, &payload.last_commit) else {
        return;
    };
    let Some(pbc) = &cov.phase_b_commit else {
        return;
    };
    if pbc == last {
        return;
    }
    let short: String = pbc.chars().take(8).collect();
    match repo_root.and_then(|root| commits_between(root, pbc, last)) {
        Some(n) if n > 0 => eprintln!(
            "note: semantic data is {n} commit{} old (indexed at {short}) — run `travsr init` to refresh.",
            if n == 1 { "" } else { "s" }
        ),
        _ => eprintln!(
            "note: semantic data was indexed at {short} — run `travsr init` to refresh."
        ),
    }
}

/// Count commits between two SHAs via `git rev-list --count`. Best-effort:
/// any git failure (shallow clone, gc'd commit) yields `None`.
fn commits_between(repo_root: &std::path::Path, from: &str, to: &str) -> Option<u64> {
    // Both arguments are commit SHAs read from our own meta table; reject
    // anything that is not plain hex before handing them to git.
    if ![from, to]
        .iter()
        .all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit()))
    {
        return None;
    }
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-list", "--count", &format!("{from}..{to}")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn print_tree(payload: &GraphPayload) {
    let nodes_by_id: HashMap<u64, &NodeEntry> = payload.nodes.iter().map(|n| (n.id, n)).collect();
    // Children per parent, in BFS discovery order.
    let mut children: HashMap<u64, Vec<(&str, u64)>> = HashMap::new();
    for step in &payload.tree {
        children
            .entry(step.parent)
            .or_default()
            .push((step.edge_kind.as_str(), step.child));
    }
    if let Some(seed) = &payload.seed {
        print_tree_level(seed.id, &nodes_by_id, &children, "");
    }
}

fn print_tree_level(
    node_id: u64,
    nodes_by_id: &HashMap<u64, &NodeEntry>,
    children: &HashMap<u64, Vec<(&str, u64)>>,
    prefix: &str,
) {
    let Some(kids) = children.get(&node_id) else {
        return;
    };
    for (i, (edge_kind, child_id)) in kids.iter().enumerate() {
        let is_last = i == kids.len() - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let extension = if is_last { "    " } else { "│   " };

        if let Some(child) = nodes_by_id.get(child_id) {
            println!(
                "{prefix}{connector}{edge_kind} → {} ({})",
                child.label, child.kind
            );
            print_tree_level(
                *child_id,
                nodes_by_id,
                children,
                &format!("{prefix}{extension}"),
            );
        }
    }
}

fn print_dot(payload: &GraphPayload) -> anyhow::Result<()> {
    let nodes_map: HashMap<u64, &NodeEntry> = payload.nodes.iter().map(|n| (n.id, n)).collect();

    // Resolve import nodes to the file node they reference.
    let mut import_redirect: HashMap<u64, u64> = HashMap::new();
    for node in &payload.nodes {
        if node.kind == "import" {
            // Best-effort local resolution without store lookup in --all mode
            for cand in &payload.nodes {
                if cand.kind != "file" {
                    continue;
                }
                let specifier = node
                    .signature
                    .strip_prefix("import:")
                    .unwrap_or(&node.signature);
                if specifier.starts_with('.') {
                    let basename = std::path::Path::new(specifier)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(specifier);
                    let stem = std::path::Path::new(&cand.path)
                        .file_stem()
                        .and_then(|s| s.to_str());
                    if stem == Some(basename) {
                        import_redirect.insert(node.id, cand.id);
                        break;
                    }
                }
            }
        }
    }

    // Rewrite edges through the redirect table; drop self-loops and duplicates.
    let mut seen: HashSet<(u64, u64, String)> = HashSet::new();
    let mut edges: Vec<(u64, u64, String)> = Vec::new();
    for EdgeEntry { src, dst, kind, .. } in &payload.edges {
        let s = import_redirect.get(src).copied().unwrap_or(*src);
        let d = import_redirect.get(dst).copied().unwrap_or(*dst);
        if s == d {
            continue;
        }
        let key = (s, d, kind.clone());
        if seen.insert(key) {
            edges.push((s, d, kind.clone()));
        }
    }

    // Group visible nodes by kind (skip resolved import stubs).
    let mut by_kind: HashMap<&str, Vec<u64>> = HashMap::new();
    for node in &payload.nodes {
        if node.kind == "import" && import_redirect.contains_key(&node.id) {
            continue;
        }
        by_kind.entry(node.kind.as_str()).or_default().push(node.id);
    }

    // Cluster definitions: (kind, label, shape, fill, border)
    let clusters: &[(&str, &str, &str, &str, &str)] = &[
        ("file", "Files", "folder", "#dbeafe", "#3b82f6"),
        ("class", "Classes", "box3d", "#dcfce7", "#22c55e"),
        ("function", "Functions", "ellipse", "#fef9c3", "#eab308"),
        ("method", "Methods", "ellipse", "#fce7f3", "#ec4899"),
        ("variable", "Variables", "plaintext", "#f3e8ff", "#a855f7"),
        ("import", "Imports", "note", "#f1f5f9", "#94a3b8"),
    ];

    println!("digraph travsr {{");
    println!("  rankdir=LR;");
    println!("  compound=true;");
    println!("  graph [fontname=\"monospace\" fontsize=11];");
    println!("  node  [fontname=\"monospace\" fontsize=10];");
    println!("  edge  [fontname=\"monospace\" fontsize=9];");
    println!();

    for (idx, (kind, label, shape, fill, border)) in clusters.iter().enumerate() {
        let Some(ids) = by_kind.get(kind) else {
            continue;
        };
        if ids.is_empty() {
            continue;
        }
        println!("  subgraph cluster_{idx} {{");
        println!("    label=\"{label}\";");
        println!("    style=filled;");
        println!("    color=\"{border}\";");
        println!("    fillcolor=\"{fill}\";");
        println!();
        for &nid in ids {
            if let Some(node) = nodes_map.get(&nid) {
                let label = escape_dot(&format!("{}\n{}", node.signature, node.path));
                println!(
                    "    n{nid} [label=\"{label}\" shape={shape} style=filled \
                     fillcolor=\"{fill}\" color=\"{border}\"];",
                );
            }
        }
        println!("  }}");
        println!();
    }

    // Emit edges; suppress defines/binding labels from containers to members.
    for (src_id, dst_id, kind) in &edges {
        let src_kind = nodes_map.get(src_id).map(|n| n.kind.as_str()).unwrap_or("");
        let dst_kind = nodes_map.get(dst_id).map(|n| n.kind.as_str()).unwrap_or("");

        let suppress = kind == "defines/binding"
            && matches!(src_kind, "file" | "class")
            && matches!(dst_kind, "function" | "method" | "variable" | "class");

        if suppress {
            println!("  n{src_id} -> n{dst_id};");
        } else {
            println!("  n{src_id} -> n{dst_id} [label=\"{kind}\"];");
        }
    }

    println!("}}");
    Ok(())
}

fn print_json(payload: &GraphPayload, budget: usize, truncated: usize) -> anyhow::Result<()> {
    let mut kinds: HashMap<String, usize> = HashMap::new();
    for node in &payload.nodes {
        *kinds.entry(node.kind.clone()).or_default() += 1;
    }

    let mut node_entries: Vec<serde_json::Value> = payload
        .nodes
        .iter()
        .map(|node| {
            serde_json::json!({
                "id": node.id.to_string(),
                "signature": node.signature,
                "kind": node.kind,
                "path": node.path,
                "language": node.language,
                "depth_from_seed": node.depth,
            })
        })
        .collect();
    node_entries.sort_by(|a, b| {
        let da = a["depth_from_seed"].as_u64().unwrap_or(0);
        let db = b["depth_from_seed"].as_u64().unwrap_or(0);
        da.cmp(&db).then(
            a["signature"]
                .as_str()
                .unwrap_or("")
                .cmp(b["signature"].as_str().unwrap_or("")),
        )
    });

    let edge_entries: Vec<serde_json::Value> = payload
        .edges
        .iter()
        .filter(|e| !e.src_sig.is_empty() && !e.dst_sig.is_empty())
        .map(|e| {
            serde_json::json!({
                "from": e.src_sig,
                "to": e.dst_sig,
                "kind": e.kind,
                "provenance": e.provenance,
            })
        })
        .collect();

    let mut summary = if let Some(s) = &payload.seed {
        serde_json::json!({
            "mode": "query",
            "root": s.signature,
            "root_path": s.path,
            "total_nodes": payload.nodes.len(),
            "total_edges": edge_entries.len(),
            "kinds": kinds,
        })
    } else {
        serde_json::json!({
            "mode": "all",
            "total_nodes": payload.nodes.len(),
            "total_edges": edge_entries.len(),
            "kinds": kinds,
        })
    };
    if truncated > 0 {
        summary["token_budget"] = serde_json::json!(budget);
        summary["truncated_nodes"] = serde_json::json!(truncated);
    }
    let mut out = serde_json::json!({
        "schema_version": 1,
        "summary": summary,
        "nodes": node_entries,
        "edges": edge_entries,
    });
    if let Some(cov) = &payload.coverage {
        out["coverage"] = serde_json::to_value(cov)?;
    }

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn escape_dot(s: &str) -> String {
    s.replace('"', "\\\"")
}
