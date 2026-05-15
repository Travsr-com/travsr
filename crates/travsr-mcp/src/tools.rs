use travsr_store::{SqliteStore, Store};

/// Return the import targets (Depends edges) of the given file path.
/// Empty string when nothing is found — callers must NOT return an RPC error
/// for the no-results case (Tech Lead requirement).
pub fn get_dependencies(store: &SqliteStore, file: &str) -> String {
    let nodes = match store.search_nodes_by_name(file) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_dependencies search error: {e}");
            return String::new();
        }
    };

    // Prefer a node whose kind is "file"; fall back to first match.
    let seed = nodes
        .iter()
        .find(|n| n.kind == "file")
        .or_else(|| nodes.first());

    let seed = match seed {
        Some(n) => n,
        None => return String::new(),
    };

    let edges = match store.iter_edges_from(seed.id) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("get_dependencies edge query error: {e}");
            return String::new();
        }
    };

    let mut lines: Vec<String> = Vec::new();
    for edge in edges.iter().filter(|e| e.kind.as_str() == "depends") {
        if let Ok(Some(dst_node)) = store.get_node(edge.dst) {
            lines.push(dst_node.vname.signature.clone());
        }
    }
    lines.join("\n")
}

/// Return all nodes that have an incoming edge to the given symbol.
/// Empty string when nothing is found.
pub fn get_callers(store: &SqliteStore, symbol: &str) -> String {
    let nodes = match store.search_nodes_by_name(symbol) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_callers search error: {e}");
            return String::new();
        }
    };

    let seed = match nodes.first() {
        Some(n) => n,
        None => return String::new(),
    };

    let edges = match store.iter_edges_to(seed.id) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("get_callers edge query error: {e}");
            return String::new();
        }
    };

    let mut lines: Vec<String> = Vec::new();
    for edge in &edges {
        if let Ok(Some(src_node)) = store.get_node(edge.src) {
            lines.push(format!(
                "{} ({}) — {}",
                src_node.vname.signature, src_node.kind, src_node.vname.path
            ));
        }
    }
    lines.join("\n")
}
