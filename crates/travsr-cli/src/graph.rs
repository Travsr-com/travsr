use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Context as _;
use travsr_core::{Node, NodeId};
use travsr_store::{SqliteStore, Store};

use crate::repo::find_git_root;

type GraphData = (
    HashMap<NodeId, (Node, u8)>,
    Vec<(NodeId, NodeId, String, String)>,
);

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Direction {
    /// Follow outgoing edges (what does this symbol import / define?)
    Deps,
    /// Follow incoming edges (who calls / depends on this symbol?)
    Callers,
    /// Follow both directions
    Both,
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

pub fn run(query: &str, depth: u8, direction: Direction, format: Format) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;
    let db_path = repo_root.join(".travsr/graph.db");

    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    let store = SqliteStore::open(&db_path)?;
    let matches = store.search_nodes_by_name(query)?;

    if matches.is_empty() {
        println!("no symbols matching '{query}'");
        return Ok(());
    }

    let seed = matches
        .iter()
        .find(|n| n.kind == "file")
        .unwrap_or(&matches[0]);

    match format {
        Format::Tree => {
            println!("{} ({})", seed.vname.signature, seed.kind);
            let mut visited = HashSet::new();
            visited.insert(seed.id);
            print_tree(&store, seed.id, depth, 0, direction, &mut visited, "")?;
        }
        Format::Dot => {
            print_dot(&store, seed.id, depth, direction)?;
        }
        Format::Json => {
            let (nodes_map, edges) = collect_graph(&store, seed.id, depth, direction)?;
            print_json(&store, Some(seed), &nodes_map, &edges)?;
        }
    }

    Ok(())
}

fn print_tree(
    store: &SqliteStore,
    node_id: NodeId,
    max_depth: u8,
    depth: u8,
    direction: Direction,
    visited: &mut HashSet<NodeId>,
    prefix: &str,
) -> anyhow::Result<()> {
    if depth >= max_depth {
        return Ok(());
    }

    let edges = next_edges(store, node_id, direction)?;

    let children: Vec<(String, NodeId)> = edges
        .into_iter()
        .filter_map(|(edge_kind, next_id)| {
            if visited.contains(&next_id) {
                return None;
            }
            Some((edge_kind, next_id))
        })
        .collect();

    for (i, (edge_kind, child_id)) in children.iter().enumerate() {
        let is_last = i == children.len() - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let extension = if is_last { "    " } else { "│   " };

        if let Some(child) = store.get_node(*child_id)? {
            println!(
                "{prefix}{connector}{edge_kind} → {} ({})",
                child.vname.signature, child.kind
            );
            visited.insert(*child_id);
            print_tree(
                store,
                *child_id,
                max_depth,
                depth + 1,
                direction,
                visited,
                &format!("{prefix}{extension}"),
            )?;
        }
    }

    Ok(())
}

fn print_dot(
    store: &SqliteStore,
    seed_id: NodeId,
    max_depth: u8,
    direction: Direction,
) -> anyhow::Result<()> {
    let (nodes_map, raw_edges) = collect_graph(store, seed_id, max_depth, direction)?;
    print_dot_from_map(&nodes_map, &raw_edges)
}

fn print_dot_from_map(
    nodes_map: &HashMap<NodeId, (Node, u8)>,
    raw_edges: &[(NodeId, NodeId, String, String)],
) -> anyhow::Result<()> {
    // Resolve import nodes to the file node they reference.
    let mut import_redirect: HashMap<NodeId, NodeId> = HashMap::new();
    for (node_id, (node, _)) in nodes_map {
        if node.kind == "import" {
            // Best-effort local resolution without store lookup in --all mode
            for (cand_id, (cand, _)) in nodes_map {
                if cand.kind != "file" {
                    continue;
                }
                let specifier = node
                    .vname
                    .signature
                    .strip_prefix("import:")
                    .unwrap_or(&node.vname.signature);
                if specifier.starts_with('.') {
                    let basename = std::path::Path::new(specifier)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(specifier);
                    let stem = std::path::Path::new(&cand.vname.path)
                        .file_stem()
                        .and_then(|s| s.to_str());
                    if stem == Some(basename) {
                        import_redirect.insert(*node_id, *cand_id);
                        break;
                    }
                }
            }
        }
    }

    // Rewrite edges through the redirect table; drop self-loops and duplicates.
    let mut seen: HashSet<(NodeId, NodeId, String)> = HashSet::new();
    let mut edges: Vec<(NodeId, NodeId, String)> = Vec::new();
    for (src, dst, kind, _provenance) in raw_edges {
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
    let mut by_kind: HashMap<&str, Vec<NodeId>> = HashMap::new();
    for (node_id, (node, _)) in nodes_map {
        if node.kind == "import" && import_redirect.contains_key(node_id) {
            continue;
        }
        by_kind
            .entry(node.kind.as_str())
            .or_default()
            .push(*node_id);
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
            if let Some((node, _)) = nodes_map.get(&nid) {
                let label = escape_dot(&format!("{}\n{}", node.vname.signature, node.vname.path));
                println!(
                    "    n{} [label=\"{label}\" shape={shape} style=filled \
                     fillcolor=\"{fill}\" color=\"{border}\"];",
                    nid.0,
                );
            }
        }
        println!("  }}");
        println!();
    }

    // Emit edges; suppress defines/binding labels from containers to members.
    for (src_id, dst_id, kind) in &edges {
        let src_kind = nodes_map
            .get(src_id)
            .map(|(n, _)| n.kind.as_str())
            .unwrap_or("");
        let dst_kind = nodes_map
            .get(dst_id)
            .map(|(n, _)| n.kind.as_str())
            .unwrap_or("");

        let suppress = kind == "defines/binding"
            && matches!(src_kind, "file" | "class")
            && matches!(dst_kind, "function" | "method" | "variable" | "class");

        if suppress {
            println!("  n{} -> n{};", src_id.0, dst_id.0);
        } else {
            println!("  n{} -> n{} [label=\"{kind}\"];", src_id.0, dst_id.0);
        }
    }

    println!("}}");
    Ok(())
}

fn print_json(
    store: &SqliteStore,
    seed: Option<&Node>,
    nodes_map: &HashMap<NodeId, (Node, u8)>,
    edges: &[(NodeId, NodeId, String, String)],
) -> anyhow::Result<()> {
    let mut sig_lookup: HashMap<NodeId, String> = nodes_map
        .iter()
        .map(|(id, (node, _))| (*id, node.vname.signature.clone()))
        .collect();
    for (src_id, dst_id, _, _) in edges {
        for &nid in &[*src_id, *dst_id] {
            if let std::collections::hash_map::Entry::Vacant(e) = sig_lookup.entry(nid) {
                if let Some(node) = store.get_node(nid)? {
                    e.insert(node.vname.signature.clone());
                }
            }
        }
    }

    let mut kinds: HashMap<String, usize> = HashMap::new();
    for (node, _) in nodes_map.values() {
        *kinds.entry(node.kind.clone()).or_default() += 1;
    }

    let mut node_entries: Vec<serde_json::Value> = nodes_map
        .values()
        .map(|(node, depth)| {
            serde_json::json!({
                "id": node.id.0.to_string(),
                "signature": node.vname.signature,
                "kind": node.kind,
                "path": node.vname.path,
                "language": node.vname.language,
                "depth_from_seed": depth,
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

    let edge_entries: Vec<serde_json::Value> = edges
        .iter()
        .filter_map(|(src_id, dst_id, kind, provenance)| {
            let from = sig_lookup.get(src_id)?;
            let to = sig_lookup.get(dst_id)?;
            Some(serde_json::json!({ "from": from, "to": to, "kind": kind, "provenance": provenance }))
        })
        .collect();

    let summary = if let Some(s) = seed {
        serde_json::json!({
            "mode": "query",
            "root": s.vname.signature,
            "root_path": s.vname.path,
            "total_nodes": nodes_map.len(),
            "total_edges": edges.len(),
            "kinds": kinds,
        })
    } else {
        serde_json::json!({
            "mode": "all",
            "total_nodes": nodes_map.len(),
            "total_edges": edges.len(),
            "kinds": kinds,
        })
    };
    let out = serde_json::json!({
        "schema_version": 1,
        "summary": summary,
        "nodes": node_entries,
        "edges": edge_entries,
    });

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn collect_graph(
    store: &SqliteStore,
    seed_id: NodeId,
    max_depth: u8,
    direction: Direction,
) -> anyhow::Result<GraphData> {
    let mut nodes_map: HashMap<NodeId, (Node, u8)> = HashMap::new();
    let mut edge_list: Vec<(NodeId, NodeId, String, String)> = Vec::new();
    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut queue: VecDeque<(NodeId, u8)> = VecDeque::new();

    visited.insert(seed_id);
    queue.push_back((seed_id, 0));

    while let Some((current_id, depth)) = queue.pop_front() {
        if let Some(node) = store.get_node(current_id)? {
            nodes_map.entry(current_id).or_insert((node, depth));
        }

        if depth >= max_depth {
            continue;
        }

        for (edge_kind, next_id) in next_edges(store, current_id, direction)? {
            let (src, dst) = match direction {
                Direction::Callers => (next_id, current_id),
                _ => (current_id, next_id),
            };
            // DEBT(travsr-75): iter_edges_from/to do not return provenance, so
            // BFS-traversed edges always show "tree-sitter" in JSON output even
            // when the DB row is "lsif". Only --all mode (all_edges) is correct.
            edge_list.push((src, dst, edge_kind, "tree-sitter".to_string()));

            if !visited.contains(&next_id) {
                visited.insert(next_id);
                queue.push_back((next_id, depth + 1));
            }
        }
    }

    Ok((nodes_map, edge_list))
}

fn collect_all_graph(store: &SqliteStore) -> anyhow::Result<GraphData> {
    let nodes_map: HashMap<NodeId, (Node, u8)> = store
        .all_nodes()?
        .into_iter()
        .map(|n| (n.id, (n, 0u8)))
        .collect();
    let edges = store.all_edges()?;
    Ok((nodes_map, edges))
}

fn next_edges(
    store: &SqliteStore,
    node_id: NodeId,
    direction: Direction,
) -> anyhow::Result<Vec<(String, NodeId)>> {
    let mut out = Vec::new();
    if matches!(direction, Direction::Deps | Direction::Both) {
        for e in store.iter_edges_from(node_id)? {
            out.push((e.kind.as_str().to_string(), e.dst));
        }
    }
    if matches!(direction, Direction::Callers | Direction::Both) {
        for e in store.iter_edges_to(node_id)? {
            out.push((e.kind.as_str().to_string(), e.src));
        }
    }
    Ok(out)
}

pub fn run_all(format: Format) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;
    let db_path = repo_root.join(".travsr/graph.db");

    if !db_path.exists() {
        anyhow::bail!("not initialized — run `travsr init`");
    }

    let store = SqliteStore::open(&db_path)?;
    let (nodes_map, edges) = collect_all_graph(&store)?;

    match format {
        Format::Tree => {
            let mut files: Vec<&Node> = nodes_map
                .values()
                .filter(|(n, _)| n.kind == "file")
                .map(|(n, _)| n)
                .collect();
            files.sort_by(|a, b| a.vname.path.cmp(&b.vname.path));
            for node in files {
                println!("{}", node.vname.path);
            }
        }
        Format::Dot => print_dot_from_map(&nodes_map, &edges)?,
        Format::Json => print_json(&store, None, &nodes_map, &edges)?,
    }

    Ok(())
}

fn escape_dot(s: &str) -> String {
    s.replace('"', "\\\"")
}
