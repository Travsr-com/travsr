use std::collections::HashMap;
use std::path::PathBuf;

use travsr_core::{Node as CoreNode, NodeId};
use travsr_retrieval::{
    context_candidates, knapsack, token_cost, EdgeFilter, OpenFilter, MAX_CONTEXT_BUDGET,
    TOKEN_CHARS_PER_TOKEN,
};
use travsr_store::{SqliteStore, Store};

use crate::sanitize::{
    sanitize_for_mcp, sanitize_mcp_body_with_limit, validate_mcp_arg, wrap_envelope,
};

/// Return the import targets (Depends edges) of the given file path.
/// Empty string when nothing is found — callers must NOT return an RPC error
/// for the no-results case (Tech Lead requirement).
pub fn get_dependencies(store: &SqliteStore, file: &str, transitive: bool, depth: u32) -> String {
    // SEC-002: reject path traversal / absolute paths / oversized args.
    if let Err(reason) = validate_mcp_arg(file) {
        tracing::warn!("get_dependencies rejected invalid arg: {reason}");
        return String::new();
    }
    let raw = if transitive {
        get_dependencies_transitive_raw(store, file, depth)
    } else {
        get_dependencies_raw(store, file)
    };
    // SEC-001: sanitize raw result before returning to MCP client / LLM.
    sanitize_for_mcp(&raw)
}

/// Transitive variant of `get_dependencies`: BFS over `depends` edges from the
/// seed file node, up to `depth` hops. Results are in BFS order — direct
/// dependencies first (no prefix), then each deeper hop indented with `  ↳ `
/// per level so the UI can render the tree depth. Deduplicated by node id, so a
/// diamond import graph lists each module once.
///
/// `depth` is caller-clamped (server dispatch clamps to 1..=10); a runaway graph
/// still terminates because every node is visited at most once.
fn get_dependencies_transitive_raw(store: &SqliteStore, file: &str, depth: u32) -> String {
    use std::collections::{HashSet, VecDeque};

    let nodes = match store.search_nodes_by_name(file) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_dependencies search error: {e}");
            return String::new();
        }
    };
    let seed = match nodes
        .iter()
        .find(|n| n.kind == "file")
        .or_else(|| nodes.first())
    {
        Some(n) => n,
        None => return String::new(),
    };

    let mut visited: HashSet<NodeId> = HashSet::new();
    visited.insert(seed.id);
    let mut queue: VecDeque<(NodeId, u32)> = VecDeque::new();
    queue.push_back((seed.id, 0));
    let mut lines: Vec<String> = Vec::new();

    while let Some((node_id, hop)) = queue.pop_front() {
        if hop >= depth {
            continue;
        }
        let edges = match store.iter_edges_from(node_id) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("get_dependencies edge query error: {e}");
                continue;
            }
        };
        // Batch: collect new dep dsts for this hop (marking visited), fetch once.
        let new_dsts: Vec<NodeId> = edges
            .iter()
            .filter(|e| e.kind.as_str() == "depends")
            .filter(|e| visited.insert(e.dst))
            .map(|e| e.dst)
            .collect();
        let node_map: HashMap<NodeId, CoreNode> = store
            .get_nodes(&new_dsts)
            .unwrap_or_default()
            .into_iter()
            .map(|n| (n.id, n))
            .collect();
        let prefix = "  ↳ ".repeat(hop as usize); // hop 0 = direct → no prefix
        for dst_id in new_dsts {
            if let Some(dst_node) = node_map.get(&dst_id) {
                lines.push(format!("{prefix}{}", dst_node.vname.signature));
                queue.push_back((dst_id, hop + 1));
            }
        }
    }
    lines.join("\n")
}

/// Raw (unsanitized) variant used by global aggregation — sanitization happens
/// once at the aggregation point, not per-store.
fn get_dependencies_raw(store: &SqliteStore, file: &str) -> String {
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

    let ids: Vec<NodeId> = edges
        .iter()
        .filter(|e| e.kind.as_str() == "depends")
        .map(|e| e.dst)
        .collect();
    let node_map: HashMap<NodeId, CoreNode> = store
        .get_nodes(&ids)
        .unwrap_or_default()
        .into_iter()
        .map(|n| (n.id, n))
        .collect();
    let lines: Vec<String> = ids
        .iter()
        .filter_map(|id| node_map.get(id).map(|n| n.vname.signature.clone()))
        .collect();
    lines.join("\n")
}

/// Returns `true` when Phase B was deferred to the daemon background scheduler
/// and has not yet completed for this repository.
///
/// The signal is: `last_commit` is set (a real init ran) but `phase_b_commit`
/// is absent (Phase B hasn't finished). This avoids false positives on no-commit
/// repos where both keys are absent and Phase B ran inline during init.
fn phase_b_pending(store: &SqliteStore) -> bool {
    let phase_b = store.get_meta("phase_b_commit").ok().flatten();
    let last = store.get_meta("last_commit").ok().flatten();
    phase_b.is_none() && last.is_some()
}

/// Return all nodes that have an incoming edge to the given symbol, tagged
/// by provenance so both semantic and structural callers are visible.
///
/// Output format:
///   `[call] fn:bar (function) — src/bar.ts`   ← LSIF RefCall (true call site)
///   `[structural] class:Foo (class) — src/foo.ts` ← Tree-sitter DefinesBinding
///
/// Both sets are always returned when present. Tagging allows the LLM to
/// distinguish a true caller from a structural parent (e.g. class→method),
/// and avoids the all-or-nothing footgun where one RefCall would hide all
/// DefinesBinding callers. The precedence policy in #47 will refine this further.
///
/// Empty string when nothing is found.
pub fn get_callers(store: &SqliteStore, symbol: &str) -> String {
    // SEC-002: validate before forwarding to store queries.
    if let Err(reason) = validate_mcp_arg(symbol) {
        tracing::warn!("get_callers rejected invalid arg: {reason}");
        return String::new();
    }
    // Phase B deferred: a HEAD commit exists but phase_b_commit hasn't been
    // stamped yet, meaning Phase B was deferred to the daemon background
    // scheduler. Return a structured message so the LLM/agent retries rather
    // than treating an absent result as "no callers exist".
    // No-commit repos (both keys absent) and fully-indexed repos (both keys
    // present) fall through to the normal path.
    if phase_b_pending(store) {
        return r#"{"status":"pending","message":"Semantic call-edge index is building in the background. Call edges will be available in ~2 minutes. Run `travsr status` to check progress."}"#.to_string();
    }
    // SEC-001: sanitize raw result before returning to MCP client / LLM.
    sanitize_for_mcp(&get_callers_raw(store, symbol))
}

/// Raw (unsanitized) variant used by global aggregation.
fn get_callers_raw(store: &SqliteStore, symbol: &str) -> String {
    use travsr_core::EdgeKind;

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

    let relevant: Vec<_> = edges
        .iter()
        .filter_map(|e| match e.kind {
            EdgeKind::RefCall => Some((e, "[call]")),
            EdgeKind::DefinesBinding => Some((e, "[structural]")),
            _ => None,
        })
        .collect();
    let ids: Vec<NodeId> = relevant.iter().map(|(e, _)| e.src).collect();
    let node_map: HashMap<NodeId, CoreNode> = store
        .get_nodes(&ids)
        .unwrap_or_default()
        .into_iter()
        .map(|n| (n.id, n))
        .collect();
    let lines: Vec<String> = relevant
        .iter()
        .filter_map(|(edge, tag)| {
            node_map.get(&edge.src).map(|src_node| {
                format!(
                    "{tag} {} ({}) — {}",
                    src_node.vname.signature, src_node.kind, src_node.vname.path
                )
            })
        })
        .collect();
    lines.join("\n")
}

/// Global variant of `get_dependencies` — searches one named repo or all registered repos.
///
/// When `repo` is `Some`, only that repo's db is queried. When `None`, all
/// registered repos are searched and results are prefixed with `[repo-name]`.
/// Stale registry entries (db file deleted) are skipped silently.
pub fn get_dependencies_global(
    repos: &HashMap<String, PathBuf>,
    file: &str,
    repo: Option<&str>,
) -> String {
    // SEC-002: validate before registry + store queries.
    if let Err(reason) = validate_mcp_arg(file) {
        tracing::warn!("get_dependencies_global rejected invalid arg: {reason}");
        return String::new();
    }
    // Aggregate raw results, prefix per-repo, then sanitize once.
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = get_dependencies_raw(store, file);
        if result.is_empty() || single {
            result
        } else {
            result
                .lines()
                .map(|l| format!("[{repo_name}] {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    // SEC-001: sanitize the fully-aggregated string once.
    sanitize_for_mcp(&raw)
}

/// Global variant of `get_callers` — searches one named repo or all registered repos.
pub fn get_callers_global(
    repos: &HashMap<String, PathBuf>,
    symbol: &str,
    repo: Option<&str>,
) -> String {
    // SEC-002: validate before registry + store queries.
    if let Err(reason) = validate_mcp_arg(symbol) {
        tracing::warn!("get_callers_global rejected invalid arg: {reason}");
        return String::new();
    }
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = get_callers_raw(store, symbol);
        if result.is_empty() || single {
            result
        } else {
            result
                .lines()
                .map(|l| format!("[{repo_name}] {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    // SEC-001: sanitize the fully-aggregated string once.
    sanitize_for_mcp(&raw)
}

fn collect_global(
    repos: &HashMap<String, PathBuf>,
    target_repo: Option<&str>,
    mut f: impl FnMut(&SqliteStore, &str, bool) -> String,
) -> String {
    // SEC-002: validate repo arg before registry lookup.
    if let Some(name) = target_repo {
        if let Err(reason) = validate_mcp_arg(name) {
            tracing::warn!("collect_global rejected invalid repo arg: {reason}");
            return String::new();
        }
    }

    let candidates: Vec<(&str, &PathBuf)> = match target_repo {
        Some(name) => match repos.get_key_value(name) {
            Some((k, v)) => vec![(k.as_str(), v)],
            None => {
                tracing::warn!("repo '{name}' not found in registry");
                return String::new();
            }
        },
        None => repos.iter().map(|(k, v)| (k.as_str(), v)).collect(),
    };

    // Filter stale entries before computing `single` so that a registry with
    // one live repo and one deleted-db repo is treated as single-repo (no
    // per-repo prefix on the output).
    let candidates: Vec<(&str, &PathBuf)> = candidates
        .into_iter()
        .filter(|(_, db_path)| {
            let exists = db_path.exists();
            if !exists {
                tracing::debug!("skipping stale registry entry: {}", db_path.display());
            }
            exists
        })
        .collect();

    let single = candidates.len() == 1;
    let mut parts: Vec<String> = Vec::new();

    for (repo_name, db_path) in candidates {
        match SqliteStore::open_read_only(db_path) {
            Ok(store) => {
                let result = f(&store, repo_name, single);
                if !result.is_empty() {
                    parts.push(result);
                }
            }
            Err(e) => tracing::warn!("failed to open {}: {e}", db_path.display()),
        }
    }

    parts.join("\n")
}

// ── get_blast_radius ──────────────────────────────────────────────────────────

/// Controls which graph edges `get_blast_radius` follows during BFS.
/// `TreeSitter` is the default and reproduces the pre-toggle behaviour exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnalysisMode {
    /// Follow `DefinesBinding`, `RefCall`, and `Depends` edges (Phase A).
    /// Always available; zero regressions from existing behaviour.
    #[default]
    TreeSitter,
    /// Follow only `RefCall` edges (Phase B — SCIP/LSIF output).
    /// Returns an empty result when no Phase B data exists for the file's language.
    Semantic,
}

/// Minimal per-language metadata used by `get_lang_status`.
/// Kept here to avoid a DAG-violating dependency on `travsr-plugin-host`.
/// Must stay in sync with `travsr-plugin-host/src/phase_b/catalog.rs`.
struct LangMeta {
    language: &'static str,
    builtin: bool,
    extensions: &'static [&'static str],
    install_hint: &'static str,
    underlying_tool_hint: &'static str,
}

static LANG_CATALOG: &[LangMeta] = &[
    LangMeta {
        language: "typescript",
        builtin: true,
        extensions: &[".ts", ".tsx"],
        install_hint: "travsr lang install typescript",
        underlying_tool_hint: "",
    },
    LangMeta {
        language: "javascript",
        builtin: true,
        extensions: &[".js", ".jsx", ".mjs", ".cjs"],
        install_hint: "travsr lang install javascript",
        underlying_tool_hint: "",
    },
    LangMeta {
        language: "rust",
        builtin: true,
        extensions: &[".rs"],
        install_hint: "travsr lang install rust",
        underlying_tool_hint: "rustup component add rust-analyzer",
    },
    LangMeta {
        language: "python",
        builtin: true,
        extensions: &[".py"],
        install_hint: "travsr lang install python",
        underlying_tool_hint: "npm install -g @sourcegraph/scip-python",
    },
    LangMeta {
        language: "go",
        builtin: false,
        extensions: &[".go"],
        install_hint: "travsr lang install go",
        underlying_tool_hint: "go install github.com/scip-code/scip-go/cmd/scip-go@latest",
    },
    LangMeta {
        language: "java",
        builtin: false,
        extensions: &[".java"],
        install_hint: "travsr lang install java",
        underlying_tool_hint: "https://github.com/sourcegraph/scip-java/releases",
    },
    LangMeta {
        language: "kotlin",
        builtin: false,
        extensions: &[".kt", ".kts"],
        install_hint: "travsr lang install kotlin",
        underlying_tool_hint: "https://github.com/sourcegraph/scip-java/releases",
    },
    LangMeta {
        language: "scala",
        builtin: false,
        extensions: &[".scala", ".sbt"],
        install_hint: "travsr lang install scala",
        underlying_tool_hint: "https://github.com/sourcegraph/scip-scala",
    },
    LangMeta {
        language: "ruby",
        builtin: false,
        extensions: &[".rb"],
        install_hint: "travsr lang install ruby",
        underlying_tool_hint: "https://github.com/sourcegraph/scip-ruby/releases",
    },
    LangMeta {
        language: "php",
        builtin: false,
        extensions: &[".php"],
        install_hint: "travsr lang install php",
        underlying_tool_hint: "https://github.com/davidrjenni/scip-php",
    },
    LangMeta {
        language: "csharp",
        builtin: false,
        extensions: &[".cs", ".csx"],
        install_hint: "travsr lang install csharp",
        underlying_tool_hint: "dotnet tool install --global scip-dotnet",
    },
    LangMeta {
        language: "cpp",
        builtin: false,
        extensions: &[".cpp", ".cc", ".cxx", ".hpp"],
        install_hint: "travsr lang install cpp",
        underlying_tool_hint: "https://github.com/sourcegraph/scip-clang/releases",
    },
    LangMeta {
        language: "c",
        builtin: false,
        extensions: &[".c", ".h"],
        install_hint: "travsr lang install c",
        underlying_tool_hint: "https://github.com/sourcegraph/scip-clang/releases",
    },
    LangMeta {
        language: "swift",
        builtin: false,
        extensions: &[".swift"],
        install_hint: "travsr lang install swift",
        underlying_tool_hint: "swift build -c release in travsr-lang/packages/swift-index-emitter",
    },
    LangMeta {
        language: "dart",
        builtin: false,
        extensions: &[".dart"],
        install_hint: "travsr lang install dart",
        underlying_tool_hint: "https://dart.dev/get-dart",
    },
];

/// Return the set of files transitively affected if the given file changes.
///
/// Uses reverse BFS over `DefinesBinding` and `RefCall` edges: starting from
/// every node defined in the file, follows incoming edges to find everything
/// that references or calls those definitions.
///
/// Output format (one line per affected file, sorted):
///   `src/service.ts`
///   `src/controller.ts`
pub fn get_blast_radius(store: &SqliteStore, file: &str, mode: AnalysisMode) -> String {
    if let Err(reason) = validate_mcp_arg(file) {
        tracing::warn!("get_blast_radius rejected invalid arg: {reason}");
        return String::new();
    }
    sanitize_for_mcp(&get_blast_radius_raw(store, file, mode))
}

fn get_blast_radius_raw(store: &SqliteStore, file: &str, mode: AnalysisMode) -> String {
    use std::collections::{HashSet, VecDeque};
    use travsr_core::EdgeKind;

    // Hard ceiling: prevents OOM on utility files imported by thousands of callers.
    // Same guard policy as PPR (MAX_SUBGRAPH_NODES = 250_000); tighter here because
    // blast-radius is a UI tool whose output must fit in an MCP response.
    const MAX_BLAST_RADIUS_NODES: usize = 50_000;

    // Find all nodes whose VName path matches the given file.
    let seed_nodes = match store.search_nodes_by_name(file) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_blast_radius search error: {e}");
            return String::new();
        }
    };

    if seed_nodes.is_empty() {
        return String::new();
    }

    let mut visited: HashSet<travsr_core::NodeId> = HashSet::new();
    let mut queue: VecDeque<travsr_core::NodeId> = VecDeque::new();
    let mut affected_files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for node in &seed_nodes {
        if visited.insert(node.id) {
            queue.push_back(node.id);
        }
        // The file itself counts as affected.
        if !node.vname.path.is_empty() {
            affected_files.insert(node.vname.path.clone());
        }
    }

    while let Some(current_id) = queue.pop_front() {
        let incoming = match store.iter_edges_to(current_id) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("get_blast_radius edge error: {e}");
                continue;
            }
        };

        if visited.len() >= MAX_BLAST_RADIUS_NODES {
            tracing::warn!(
                "get_blast_radius hit ceiling ({MAX_BLAST_RADIUS_NODES} nodes) \
                 for file '{file}'; result may be incomplete"
            );
            break;
        }

        // Batch: collect followable new-src ids, enqueue, then fetch paths once.
        let new_srcs: Vec<NodeId> = incoming
            .into_iter()
            .filter(|edge| match mode {
                AnalysisMode::TreeSitter => matches!(
                    edge.kind,
                    EdgeKind::DefinesBinding | EdgeKind::RefCall | EdgeKind::Depends
                ),
                AnalysisMode::Semantic => matches!(edge.kind, EdgeKind::RefCall),
            })
            .filter(|edge| visited.insert(edge.src))
            .map(|edge| edge.src)
            .collect();
        for &id in &new_srcs {
            queue.push_back(id);
        }
        let node_map: HashMap<NodeId, CoreNode> = store
            .get_nodes(&new_srcs)
            .unwrap_or_default()
            .into_iter()
            .map(|n| (n.id, n))
            .collect();
        for id in &new_srcs {
            if let Some(src_node) = node_map.get(id) {
                if !src_node.vname.path.is_empty() {
                    affected_files.insert(src_node.vname.path.clone());
                }
            }
        }
    }

    // Phase 2: file-level blast radius via language-aware import resolution.
    // Many languages store deps as file --[Depends]--> import:pkg with no
    // ResolvesTo chain back. ImportResolver bridges this for all 14 languages.
    // Skipped in Semantic mode — RefCall edges are already precise.
    //
    // Two search hints: the package directory (Go, Java packages, PHP, C#) and
    // the file stem (Java/Kotlin/Scala class-level imports like import:com.Foo).
    if mode == AnalysisMode::TreeSitter {
        let fp_normalized = file.replace('\\', "/");
        let p = std::path::Path::new(&fp_normalized);
        let dir_hint = p
            .parent()
            .and_then(|d| d.file_name()) // last component of directory
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty());
        let stem_hint = p.file_stem().and_then(|s| s.to_str());

        let mut hints: Vec<&str> = Vec::new();
        if let Some(d) = dir_hint {
            hints.push(d);
        }
        if let Some(s) = stem_hint {
            if Some(s) != dir_hint {
                hints.push(s);
            }
        }

        let mut checked_import_ids: std::collections::HashSet<travsr_core::NodeId> =
            std::collections::HashSet::new();

        for hint in hints {
            let Ok(import_nodes) = store.search_nodes_by_name(hint) else {
                continue;
            };
            for imp in import_nodes {
                if imp.kind != "import" {
                    continue;
                }
                if !checked_import_ids.insert(imp.id) {
                    continue;
                } // dedup across hints
                let resolver = travsr_core::resolver_for_language(&imp.vname.language);
                if !resolver.resolves_to(&imp.vname.signature, file) {
                    continue;
                }
                let Ok(edges) = store.iter_edges_to(imp.id) else {
                    continue;
                };
                // Batch: all Depends src ids (visited+queue separately; path fetch once).
                let dep_srcs: Vec<NodeId> = edges
                    .iter()
                    .filter(|e| matches!(e.kind, EdgeKind::Depends))
                    .map(|e| e.src)
                    .collect();
                for &id in &dep_srcs {
                    if visited.insert(id) {
                        queue.push_back(id);
                    }
                }
                let node_map: HashMap<NodeId, CoreNode> = store
                    .get_nodes(&dep_srcs)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|n| (n.id, n))
                    .collect();
                for id in &dep_srcs {
                    if let Some(src_node) = node_map.get(id) {
                        if !src_node.vname.path.is_empty() {
                            affected_files.insert(src_node.vname.path.clone());
                        }
                    }
                }
            }
        }
    }

    affected_files.into_iter().collect::<Vec<_>>().join("\n")
}

/// Global variant of `get_blast_radius`.
pub fn get_blast_radius_global(
    repos: &HashMap<String, PathBuf>,
    file: &str,
    repo: Option<&str>,
    mode: AnalysisMode,
) -> String {
    if let Err(reason) = validate_mcp_arg(file) {
        tracing::warn!("get_blast_radius_global rejected invalid arg: {reason}");
        return String::new();
    }
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = get_blast_radius_raw(store, file, mode);
        if result.is_empty() || single {
            result
        } else {
            result
                .lines()
                .map(|l| format!("[{repo_name}] {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    sanitize_for_mcp(&raw)
}

// ── get_lang_status ───────────────────────────────────────────────────────────

/// Detect the language of `file` from its extension, then check whether Phase B
/// (SCIP/LSIF) data exists in the store for that language.
///
/// Returns JSON: `{"language":"go","builtin":false,"semantic_available":false,
/// "install_hint":"travsr lang install go"}`
///
/// `install_hint` is empty when semantic is already available; for builtins
/// (ts/js/rust/python) with missing Phase B it shows `underlying_tool_hint`.
/// JSON is returned unsanitised — it is parsed by first-party TypeScript code,
/// not fed to an LLM.
pub fn get_lang_status(store: &SqliteStore, file: &str) -> String {
    if let Err(reason) = validate_mcp_arg(file) {
        tracing::warn!("get_lang_status rejected invalid arg: {reason}");
        return r#"{"language":"unknown","builtin":false,"semantic_available":false,"install_hint":""}"#
            .to_string();
    }
    get_lang_status_raw(store, file)
}

fn get_lang_status_raw(store: &SqliteStore, file: &str) -> String {
    let ext = std::path::Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();

    let entry = LANG_CATALOG
        .iter()
        .find(|e| e.extensions.contains(&ext.as_str()));

    let Some(meta) = entry else {
        return r#"{"language":"unknown","builtin":false,"semantic_available":false,"install_hint":"unknown language"}"#
            .to_string();
    };

    let semantic_available = store.has_refcall_edges_for_language(meta.language);

    let install_hint = if semantic_available {
        ""
    } else if meta.builtin {
        meta.underlying_tool_hint
    } else {
        meta.install_hint
    };

    // #318 O5: staleness marker — the commit Phase B data was last built at.
    // A hex SHA needs no JSON escaping; anything else is rejected here.
    let phase_b_commit = store
        .get_meta("phase_b_commit")
        .ok()
        .flatten()
        .filter(|s| s.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|s| format!("\"{s}\""))
        .unwrap_or_else(|| "null".to_string());

    // Manual JSON to avoid a serde_json dependency on a hot path.
    // Fields are all static strings — no escaping needed.
    format!(
        r#"{{"language":"{lang}","builtin":{builtin},"semantic_available":{sem},"install_hint":"{hint}","phase_b_commit":{pbc}}}"#,
        lang = meta.language,
        builtin = meta.builtin,
        sem = semantic_available,
        hint = install_hint,
        pbc = phase_b_commit,
    )
}

/// Global variant of `get_lang_status` — opens the first matched repo store.
pub fn get_lang_status_global(
    repos: &HashMap<String, PathBuf>,
    file: &str,
    repo: Option<&str>,
) -> String {
    if let Err(reason) = validate_mcp_arg(file) {
        tracing::warn!("get_lang_status_global rejected invalid arg: {reason}");
        return r#"{"language":"unknown","builtin":false,"semantic_available":false,"install_hint":""}"#
            .to_string();
    }
    // collect_global returns newline-joined results; we only need the first repo's answer.
    let raw = collect_global(repos, repo, |store, _repo_name, _single| {
        get_lang_status_raw(store, file)
    });
    if raw.is_empty() {
        r#"{"language":"unknown","builtin":false,"semantic_available":false,"install_hint":""}"#
            .to_string()
    } else {
        // collect_global joins results with "\n"; take only the first JSON line.
        raw.lines().next().unwrap_or("").to_string()
    }
}

// ── search_symbol ─────────────────────────────────────────────────────────────

/// Find symbol definitions matching a name across the indexed graph.
///
/// Returns matching symbols formatted as:
///   `fn:charge (function) — src/payment.ts`
pub fn search_symbol(store: &SqliteStore, name: &str) -> String {
    if let Err(reason) = validate_mcp_arg(name) {
        tracing::warn!("search_symbol rejected invalid arg: {reason}");
        return String::new();
    }
    sanitize_for_mcp(&search_symbol_raw(store, name))
}

fn search_symbol_raw(store: &SqliteStore, name: &str) -> String {
    // Cap results: prevents self-DoS from wildcard queries (e.g. "a") and limits
    // accidental bulk exfiltration. The store LIKE query has no SQL LIMIT yet —
    // this Rust-side cap is the guard until that is added at the store layer.
    const MAX_SEARCH_RESULTS: usize = 50;

    let nodes = match store.search_nodes_fuzzy(name) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("search_symbol error: {e}");
            return String::new();
        }
    };

    let lines: Vec<String> = nodes
        .iter()
        .take(MAX_SEARCH_RESULTS)
        .map(|n| format!("{} ({}) — {}", n.vname.signature, n.kind, n.vname.path))
        .collect();
    lines.join("\n")
}

/// Global variant of `search_symbol`.
pub fn search_symbol_global(
    repos: &HashMap<String, PathBuf>,
    name: &str,
    repo: Option<&str>,
) -> String {
    if let Err(reason) = validate_mcp_arg(name) {
        tracing::warn!("search_symbol_global rejected invalid arg: {reason}");
        return String::new();
    }
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = search_symbol_raw(store, name);
        if result.is_empty() || single {
            result
        } else {
            result
                .lines()
                .map(|l| format!("[{repo_name}] {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    sanitize_for_mcp(&raw)
}

// ── get_repo_map ──────────────────────────────────────────────────────────────

/// Return a structural overview of the indexed repository.
///
/// Groups all indexed nodes by file path and renders a tree annotated with
/// symbol counts and the top symbols per file, suitable for LLM consumption:
///
/// ```text
/// src/
///   service.ts  [3 symbols]  fn:charge, class:PaymentService, fn:refund
///   index.ts    [1 symbol]   fn:activate
/// ```
pub fn get_repo_map(store: &SqliteStore) -> String {
    sanitize_for_mcp(&get_repo_map_raw(store))
}

fn get_repo_map_raw(store: &SqliteStore) -> String {
    use std::collections::BTreeMap;

    let nodes = match store.all_nodes() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_repo_map error: {e}");
            return String::new();
        }
    };

    if nodes.is_empty() {
        return String::new();
    }

    // Group nodes by file path.
    let mut by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for node in &nodes {
        let path = if node.vname.path.is_empty() {
            "<unknown>".to_string()
        } else {
            node.vname.path.clone()
        };
        // Only include named symbols (skip file nodes and internal go-pkg nodes).
        if !node.vname.signature.is_empty() && node.kind != "file" && node.kind != "go-pkg" {
            by_file
                .entry(path)
                .or_default()
                .push(node.vname.signature.clone());
        }
    }

    const MAX_SYMBOLS_PER_FILE: usize = 5;
    let mut lines: Vec<String> = Vec::with_capacity(by_file.len() + 1);

    for (path, mut symbols) in by_file {
        symbols.sort();
        symbols.dedup();
        let count = symbols.len();
        let preview: Vec<&str> = symbols
            .iter()
            .take(MAX_SYMBOLS_PER_FILE)
            .map(|s| s.as_str())
            .collect();
        let suffix = if count > MAX_SYMBOLS_PER_FILE {
            format!(", … (+{})", count - MAX_SYMBOLS_PER_FILE)
        } else {
            String::new()
        };
        lines.push(format!(
            "{}  [{} symbol{}]  {}{}",
            path,
            count,
            if count == 1 { "" } else { "s" },
            preview.join(", "),
            suffix,
        ));
    }

    lines.join("\n")
}

// ── get_graph_stats ───────────────────────────────────────────────────────────

/// Return accurate node and edge counts directly from the SQLite store.
///
/// Output format (newline-separated key-value pairs):
///   `nodes: 2121`
///   `edges: 8432`
///
/// Always returns a non-empty string — callers can check `nodes: 0` for an
/// empty graph. No sanitization needed: the output contains no user data.
pub fn get_graph_stats(store: &SqliteStore) -> String {
    let nodes = match store.node_count() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_graph_stats node_count error: {e}");
            0
        }
    };
    let edges = match store.edge_count() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_graph_stats edge_count error: {e}");
            0
        }
    };
    // schema_version lets the extension surface migration state in its stats
    // popup. Status-bar parsing reads only the `nodes:` line, so appending here
    // is backward compatible. 0 on read error — never fails the whole call.
    let schema_version = store.current_schema_version().unwrap_or(0);
    format!("nodes: {nodes}\nedges: {edges}\nschema_version: {schema_version}")
}

/// Global variant of `get_graph_stats` — sums across all registered repos.
///
/// Input validation for `repo` is performed by `collect_global`; do not bypass that helper.
pub fn get_graph_stats_global(repos: &HashMap<String, PathBuf>, repo: Option<&str>) -> String {
    let mut total_nodes: u64 = 0;
    let mut total_edges: u64 = 0;
    // DEBT(cloud-launch): counts must be filtered to caller's EdgeFilter scope before SSE ships
    collect_global(repos, repo, |store, _repo_name, _single| {
        total_nodes += match store.node_count() {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("get_graph_stats_global node_count error: {e}");
                0
            }
        };
        total_edges += match store.edge_count() {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("get_graph_stats_global edge_count error: {e}");
                0
            }
        };
        String::new() // accumulation done via captured mutables; return value unused
    });
    format!("nodes: {total_nodes}\nedges: {total_edges}")
}

/// Return per-language node counts for the current repo graph.
///
/// Output format: TSV `language\tcount`, one line per language, sorted by
/// count descending. Empty string when no nodes have language metadata.
/// No sanitization needed — language names are enum values from the indexer.
pub fn repo_languages(store: &SqliteStore) -> String {
    match store.language_distribution() {
        Ok(pairs) if pairs.is_empty() => String::new(),
        Ok(pairs) => pairs
            .iter()
            .map(|(lang, cnt)| format!("{lang}\t{cnt}"))
            .collect::<Vec<_>>()
            .join("\n"),
        Err(e) => {
            tracing::warn!("repo_languages error: {e}");
            String::new()
        }
    }
}

// ── synonyms (RFC-012 A2 F1) ────────────────────────────────────────────────────
//
// These tools manage the per-repo dynamic synonym table backing query expansion.
// Unlike the retrieval tools, their return strings are NOT passed through
// `sanitize_for_mcp`: they are control responses to the first-party VS Code
// extension UI (success / cap-error), not repo-derived data fed to an LLM. The
// extension matches on `"ok"` vs an error message. Arguments are still validated
// (SEC-002) before any store write.

/// Add one (term, alias) synonym pair. Returns `"ok"` on success or the store's
/// error text (e.g. the 200-row cap message) so the UI can surface it.
pub fn synonym_add(store: &mut SqliteStore, term: &str, alias: &str) -> String {
    if validate_mcp_arg(term).is_err() || validate_mcp_arg(alias).is_err() {
        tracing::warn!("synonym_add rejected invalid arg");
        return "invalid input".to_string();
    }
    match store.synonym_add(term, alias) {
        Ok(()) => "ok".to_string(),
        Err(e) => e.to_string(),
    }
}

/// Replace ALL aliases for `term` with the comma-separated `aliases_csv` list.
/// Atomic in the store layer. Rejects the whole call if any alias is invalid.
pub fn synonym_set(store: &mut SqliteStore, term: &str, aliases_csv: &str) -> String {
    if validate_mcp_arg(term).is_err() {
        tracing::warn!("synonym_set rejected invalid term");
        return "invalid input".to_string();
    }
    let aliases: Vec<String> = aliases_csv
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if aliases.iter().any(|a| validate_mcp_arg(a).is_err()) {
        tracing::warn!("synonym_set rejected invalid alias");
        return "invalid input".to_string();
    }
    match store.synonym_set(term, &aliases) {
        Ok(()) => "ok".to_string(),
        Err(e) => e.to_string(),
    }
}

/// Remove a single (term, alias) pair. No-op if it does not exist.
pub fn synonym_remove(store: &mut SqliteStore, term: &str, alias: &str) -> String {
    if validate_mcp_arg(term).is_err() || validate_mcp_arg(alias).is_err() {
        tracing::warn!("synonym_remove rejected invalid arg");
        return "invalid input".to_string();
    }
    match store.synonym_remove(term, alias) {
        Ok(()) => "ok".to_string(),
        Err(e) => e.to_string(),
    }
}

/// Remove ALL aliases for `term`.
pub fn synonym_remove_term(store: &mut SqliteStore, term: &str) -> String {
    if validate_mcp_arg(term).is_err() {
        tracing::warn!("synonym_remove_term rejected invalid term");
        return "invalid input".to_string();
    }
    match store.synonym_remove_term(term) {
        Ok(()) => "ok".to_string(),
        Err(e) => e.to_string(),
    }
}

/// Reset the synonym table to the built-in static defaults.
pub fn synonym_reset(store: &mut SqliteStore) -> String {
    match store.synonym_reset() {
        Ok(()) => "ok".to_string(),
        Err(e) => e.to_string(),
    }
}

/// List all active synonym pairs as `term => alias`, one per line, sorted by the
/// store. Empty string when the table is empty.
pub fn synonym_list(store: &SqliteStore) -> String {
    match store.synonym_list() {
        Ok(pairs) => pairs
            .iter()
            .map(|(term, alias)| format!("{term} => {alias}"))
            .collect::<Vec<_>>()
            .join("\n"),
        Err(e) => {
            tracing::warn!("synonym_list error: {e}");
            String::new()
        }
    }
}

// ── repos (registry management, VSCODE-247) ──────────────────────────────────
//
// Global-registry operations exposed for the VS Code "Registered repos" webview.
// Like the synonym tools, return strings are plain control responses (not
// `sanitize_for_mcp`'d) — they are consumed by the first-party extension UI, not
// fed to an LLM. The registry is global (independent of the open store), so these
// are valid on the stdio server regardless of which repo it was started for.

/// List registry entries as TSV: `name\tdb_path\t{0|1}` (1 = graph.db exists).
/// Empty string when the registry is empty.
pub fn repos_list() -> String {
    let repos = match travsr_store::registry::all_repos() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("repos_list error: {e}");
            return String::new();
        }
    };
    let mut rows: Vec<(String, std::path::PathBuf)> = repos.into_iter().collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.iter()
        .map(|(name, db_path)| {
            let exists = if db_path.exists() { "1" } else { "0" };
            format!("{name}\t{}\t{exists}", db_path.display())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Prune stale registry entries. Returns `pruned: N` followed by the removed
/// names, or `pruned: 0` when nothing was stale.
pub fn repos_prune() -> String {
    match travsr_store::registry::prune() {
        Ok(removed) => {
            let mut out = format!("pruned: {}", removed.len());
            for name in &removed {
                out.push('\n');
                out.push_str(name);
            }
            out
        }
        Err(e) => {
            tracing::warn!("repos_prune error: {e}");
            "error".to_string()
        }
    }
}

/// Remove a single repo by registry-key name. Returns `ok` / `not found`.
pub fn repos_remove(name: &str) -> String {
    match travsr_store::registry::unregister(name) {
        Ok(true) => "ok".to_string(),
        Ok(false) => "not found".to_string(),
        Err(e) => {
            tracing::warn!("repos_remove error: {e}");
            "error".to_string()
        }
    }
}

// ── get_execution_path ────────────────────────────────────────────────────────

/// Find a traversal path from `source` symbol to `sink` symbol through the graph.
///
/// Uses PCST (Prize-Collecting Steiner Tree) approximation when a path exists,
/// falls back to BFS depth-3 on timeout (> 80ms) or disconnected graphs.
///
/// Output format (one line per node on path):
///   `fn:charge (function) — src/payment.ts`
///   `fn:processPayment (function) — src/processor.ts`
///
/// # SEC P0
/// Returns empty string for both "symbol not found" and "symbol access denied".
/// These cases are indistinguishable to the caller (prevents existence oracle).
pub fn get_execution_path(store: &SqliteStore, source: &str, sink: &str) -> String {
    // Phase B deferred: execution paths require call edges which are not yet indexed.
    if phase_b_pending(store) {
        return r#"{"status":"pending","message":"Semantic call-edge index is building in the background. Execution paths will be available in ~2 minutes. Run `travsr status` to check progress."}"#.to_string();
    }
    get_execution_path_with_filter(store, source, sink, &OpenFilter)
}

/// Authenticated variant — applies RBAC filter at traversal time.
/// Wired to session context in S16 when `SessionStore` is integrated into the server loop.
// DEBT(travsr-199): wire into server.rs dispatch when SessionStore lands in S16.
#[allow(dead_code)]
pub(crate) fn get_execution_path_authed(
    store: &SqliteStore,
    source: &str,
    sink: &str,
    filter: &dyn EdgeFilter,
) -> String {
    get_execution_path_with_filter(store, source, sink, filter)
}

fn get_execution_path_with_filter(
    store: &SqliteStore,
    source: &str,
    sink: &str,
    filter: &dyn EdgeFilter,
) -> String {
    if let Err(reason) = validate_mcp_arg(source) {
        tracing::warn!("get_execution_path rejected invalid source arg: {reason}");
        return String::new();
    }
    if let Err(reason) = validate_mcp_arg(sink) {
        tracing::warn!("get_execution_path rejected invalid sink arg: {reason}");
        return String::new();
    }
    sanitize_for_mcp(&get_execution_path_raw(store, source, sink, filter))
}

fn get_execution_path_raw(
    store: &SqliteStore,
    source: &str,
    sink: &str,
    filter: &dyn EdgeFilter,
) -> String {
    // SEC P0: resolve source and sink; treat "not found" == "access denied" identically.
    let src_node = match store.search_nodes_by_name(source) {
        Ok(n) => n
            .into_iter()
            .find(|n| filter.allow(n.id, n.id, Some(n.vname.corpus.as_str()))),
        Err(e) => {
            tracing::warn!("get_execution_path source search error: {e}");
            return String::new();
        }
    };

    let sink_node = match store.search_nodes_by_name(sink) {
        Ok(n) => n
            .into_iter()
            .find(|n| filter.allow(n.id, n.id, Some(n.vname.corpus.as_str()))),
        Err(e) => {
            tracing::warn!("get_execution_path sink search error: {e}");
            return String::new();
        }
    };

    // SEC P0: both outcomes (not found and access denied) produce the same empty result.
    let (src, snk) = match (src_node, sink_node) {
        (Some(a), Some(b)) => (a, b),
        _ => return String::new(),
    };

    let path = match travsr_retrieval::pcst_path(store, src.id, snk.id, filter, 4096) {
        Ok(nodes) => nodes,
        Err(e) => {
            tracing::warn!("get_execution_path pcst error: {e}");
            return String::new();
        }
    };

    if path.is_empty() {
        return String::new();
    }

    let lines: Vec<String> = path
        .iter()
        .map(|n| format!("{} ({}) — {}", n.vname.signature, n.kind, n.vname.path))
        .collect();
    lines.join("\n")
}

/// Global variant of `get_execution_path` — searches one named repo or all registered repos.
///
/// `filter` is applied at traversal time. Pass `&OpenFilter` for unauthenticated local mode;
/// pass `&session.filter()` in authenticated mode (S16). Do NOT hardcode `&OpenFilter` at
/// call sites — the caller owns the auth context.
pub fn get_execution_path_global(
    repos: &HashMap<String, PathBuf>,
    source: &str,
    sink: &str,
    repo: Option<&str>,
    filter: &dyn EdgeFilter,
) -> String {
    if let Err(reason) = validate_mcp_arg(source) {
        tracing::warn!("get_execution_path_global rejected invalid source: {reason}");
        return String::new();
    }
    if let Err(reason) = validate_mcp_arg(sink) {
        tracing::warn!("get_execution_path_global rejected invalid sink: {reason}");
        return String::new();
    }
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = get_execution_path_raw(store, source, sink, filter);
        if result.is_empty() || single {
            result
        } else {
            result
                .lines()
                .map(|l| format!("[{repo_name}] {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    sanitize_for_mcp(&raw)
}

/// Global variant of `get_repo_map`.
pub fn get_repo_map_global(repos: &HashMap<String, PathBuf>, repo: Option<&str>) -> String {
    // repo arg validated inside collect_global.
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = get_repo_map_raw(store);
        if result.is_empty() || single {
            result
        } else {
            result
                .lines()
                .map(|l| format!("[{repo_name}] {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    sanitize_for_mcp(&raw)
}

// ── get_context ───────────────────────────────────────────────────────────────

/// Retrieve the most relevant context for `query` within `token_budget` tokens.
///
/// Pipeline: validate → seed lookup (RBAC-filtered) → PPR → get_nodes →
/// knapsack → format → sanitize → append footer → wrap envelope.
///
/// # SEC P0
/// Returns identical output for "not found" and "access denied" to prevent
/// existence oracle attacks. Seeds are filtered through the `OpenFilter` so
/// callers cannot distinguish missing vs denied symbols.
pub fn get_context(
    store: &SqliteStore,
    query: &str,
    token_budget: usize,
    include_snippets: bool,
    snippet_budget: Option<usize>,
) -> String {
    get_context_with_filter(
        store,
        query,
        token_budget,
        &OpenFilter,
        include_snippets,
        snippet_budget,
    )
}

/// Authenticated variant — applies RBAC filter at seed lookup and node fetch.
#[allow(dead_code)]
pub(crate) fn get_context_authed(
    store: &SqliteStore,
    query: &str,
    token_budget: usize,
    filter: &dyn EdgeFilter,
    include_snippets: bool,
    snippet_budget: Option<usize>,
) -> String {
    get_context_with_filter(
        store,
        query,
        token_budget,
        filter,
        include_snippets,
        snippet_budget,
    )
}

/// Raw variant — returns body without envelope. Used by global aggregation to
/// prevent double-sanitization when multiple stores are aggregated before wrapping.
pub(crate) fn get_context_raw(
    store: &SqliteStore,
    query: &str,
    token_budget: usize,
    include_snippets: bool,
    snippet_budget: Option<usize>,
) -> String {
    get_context_body(
        store,
        query,
        token_budget,
        &OpenFilter,
        include_snippets,
        snippet_budget,
    )
}

fn get_context_with_filter(
    store: &SqliteStore,
    query: &str,
    token_budget: usize,
    filter: &dyn EdgeFilter,
    include_snippets: bool,
    snippet_budget: Option<usize>,
) -> String {
    let body = get_context_body(
        store,
        query,
        token_budget,
        filter,
        include_snippets,
        snippet_budget,
    );
    wrap_envelope(&body)
}

/// Why a node appeared in `get_context` output.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NodeRole {
    /// Direct text match — one of the up-to-5 seeds.
    Seed,
    /// This node calls one of the seed symbols (reverse RefCall/RefImports/Depends edge).
    Caller,
    /// A seed symbol calls or imports this node (forward RefCall/Depends/RefImports edge).
    Dependency,
    /// PPR structural relevance; no direct 1-hop edge to or from any seed.
    Context,
}

impl NodeRole {
    fn label(self) -> &'static str {
        match self {
            NodeRole::Seed => "seed",
            NodeRole::Caller => "caller",
            NodeRole::Dependency => "dependency",
            NodeRole::Context => "context",
        }
    }
}

fn get_context_body(
    store: &SqliteStore,
    query: &str,
    token_budget: usize,
    filter: &dyn EdgeFilter,
    include_snippets: bool,
    snippet_budget: Option<usize>,
) -> String {
    // SEC-002: validate before any store access.
    if let Err(reason) = validate_mcp_arg(query) {
        tracing::warn!("get_context rejected invalid query arg: {reason}");
        return String::new();
    }
    // Defense-in-depth budget guard (RFC-010 §3.3).
    if token_budget > MAX_CONTEXT_BUDGET {
        return "token_budget exceeds maximum allowed value".to_string();
    }
    if token_budget == 0 {
        return String::new();
    }

    // Seed lookup: up to 5 seeds matching query, RBAC-filtered (SEC P0).
    let seeds: Vec<NodeId> = match store.search_nodes_fuzzy(query) {
        Ok(nodes) => nodes
            .into_iter()
            .filter(|n| filter.allow(n.id, n.id, Some(n.vname.corpus.as_str())))
            .take(5)
            .map(|n| n.id)
            .collect(),
        Err(e) => {
            tracing::warn!("get_context seed search error: {e}");
            return String::new();
        }
    };

    // SEC P0: identical response for "not found" and "access denied".
    if seeds.is_empty() {
        return format!("No symbols matching '{query}' found in the graph.");
    }

    // PPR over the seed set.
    let ppr_scores = match travsr_retrieval::ppr(store, &seeds, context_candidates()) {
        Ok(scores) => scores,
        Err(e) => {
            tracing::warn!("get_context ppr error: {e}");
            return String::new();
        }
    };

    if ppr_scores.is_empty() {
        return format!("No symbols matching '{query}' found in the graph.");
    }

    // Build score map for keyed join (prevents node/score misalignment).
    let score_map: HashMap<NodeId, f32> = ppr_scores.iter().cloned().collect();
    let node_ids: Vec<NodeId> = ppr_scores.into_iter().map(|(id, _)| id).collect();

    // Batch-fetch nodes.
    let fetched = match store.get_nodes(&node_ids) {
        Ok(nodes) => nodes,
        Err(e) => {
            tracing::warn!("get_context get_nodes error: {e}");
            return String::new();
        }
    };

    // Post-filter fetched nodes through RBAC and join with scores.
    // SEC P0: `selected` is the authoritative RBAC-filtered list; snippets are
    // only ever read for nodes already in this set (never re-queried separately).
    let items: Vec<(CoreNode, f32)> = fetched
        .into_iter()
        .filter(|n| filter.allow(n.id, n.id, Some(n.vname.corpus.as_str())))
        .filter_map(|n| score_map.get(&n.id).map(|&s| (n, s)))
        .collect();

    if items.is_empty() {
        return format!("No symbols matching '{query}' found in the graph.");
    }

    // Boost PPR scores by k-core shell number (global structural importance).
    // KCORE_ALPHA is small so PPR local relevance still dominates; shell number
    // acts as a tiebreaker that favours structurally central nodes.
    const KCORE_ALPHA: f32 = 0.05;
    let item_ids: Vec<NodeId> = items.iter().map(|(n, _)| n.id).collect();
    let shell_map = store.get_shell_numbers_batch(&item_ids).unwrap_or_default();
    let items: Vec<(CoreNode, f32)> = items
        .into_iter()
        .map(|(n, s)| {
            let shell = shell_map.get(&n.id).copied().unwrap_or(0);
            (n, s * (1.0 + KCORE_ALPHA * shell as f32))
        })
        .collect();

    // Knapsack selection.
    let selected = knapsack(items, token_budget);
    let n_nodes = selected.len();
    let total_tokens: usize = selected.iter().map(token_cost).sum();

    // Build 1-hop role map: classify each selected node's relationship to the seeds.
    // O(S × avg_degree); seeds are capped at 5 so this is negligible.
    // Errors are silently ignored — roles degrade to Context, never block output.
    let roles: HashMap<NodeId, NodeRole> = {
        use travsr_core::EdgeKind;
        let seed_set: std::collections::HashSet<NodeId> = seeds.iter().copied().collect();
        let selected_set: std::collections::HashSet<NodeId> =
            selected.iter().map(|n| n.id).collect();
        let mut map: HashMap<NodeId, NodeRole> =
            selected.iter().map(|n| (n.id, NodeRole::Context)).collect();
        for &seed in &seeds {
            // Forward edges: seed → node  →  node is a Dependency of seed.
            if let Ok(fwd) = store.iter_edges_from(seed) {
                for e in fwd {
                    if matches!(
                        e.kind,
                        EdgeKind::RefCall
                            | EdgeKind::Depends
                            | EdgeKind::RefImports
                            | EdgeKind::Exports
                    ) && selected_set.contains(&e.dst)
                    {
                        map.entry(e.dst).and_modify(|r| {
                            if *r == NodeRole::Context {
                                *r = NodeRole::Dependency;
                            }
                        });
                    }
                }
            }
            // Reverse edges: node → seed  →  node is a Caller of seed.
            if let Ok(rev) = store.iter_edges_to(seed) {
                for e in rev {
                    if matches!(
                        e.kind,
                        EdgeKind::RefCall | EdgeKind::RefImports | EdgeKind::Depends
                    ) && selected_set.contains(&e.src)
                    {
                        map.entry(e.src).and_modify(|r| {
                            if *r == NodeRole::Context {
                                *r = NodeRole::Caller;
                            }
                        });
                    }
                }
            }
            // Seed overrides both.
            if seed_set.contains(&seed) {
                map.insert(seed, NodeRole::Seed);
            }
        }
        map
    };

    let char_cap = (token_budget * TOKEN_CHARS_PER_TOKEN * 2).min(1_024_000);

    if include_snippets {
        // Read repo_root. Pre-snippet indexes lack this key → degrade to metadata-only.
        let repo_root = match store.get_meta("repo_root") {
            Ok(Some(r)) if !r.is_empty() => PathBuf::from(r),
            _ => {
                tracing::warn!(
                    "get_context: repo_root not in meta — falling back to metadata-only"
                );
                let lines: Vec<String> = selected
                    .iter()
                    .map(|n| {
                        let role = roles
                            .get(&n.id)
                            .copied()
                            .unwrap_or(NodeRole::Context)
                            .label();
                        format!(
                            "{} ({}) — {} [package: {}] [via: {}]",
                            n.vname.signature, n.kind, n.vname.path, n.package, role
                        )
                    })
                    .collect();
                let sanitized = sanitize_mcp_body_with_limit(&lines.join("\n"), char_cap);
                return format!(
                    "{sanitized}\n\n[{n_nodes} nodes, ~{total_tokens} tokens — run `travsr init` to enable inline snippets]"
                );
            }
        };

        // Snippet ceiling: explicit separate budget or whatever remains after metadata.
        let snippet_ceiling =
            snippet_budget.unwrap_or_else(|| token_budget.saturating_sub(total_tokens));
        let mode_label = if snippet_budget.is_some() {
            "separate"
        } else {
            "shared"
        };

        let mut snip_tokens: usize = 0;
        let mut n_with_snippet: usize = 0;
        let mut blocks: Vec<String> = Vec::with_capacity(selected.len());

        for n in &selected {
            let role = roles
                .get(&n.id)
                .copied()
                .unwrap_or(NodeRole::Context)
                .label();
            let header = format!(
                "{} ({}) — {} [package: {}] [via: {}]",
                n.vname.signature, n.kind, n.vname.path, n.package, role
            );
            // Use skeleton when the body exceeds the kind-aware line cap.
            let n_height = n
                .end_line
                .unwrap_or_else(|| n.line.unwrap_or(0))
                .saturating_sub(n.line.unwrap_or(0)) as usize;
            let block = if snip_tokens < snippet_ceiling {
                let body = if n_height > snippet_line_cap(&n.kind) {
                    skeleton_for_node_inner(n, &repo_root)
                        .map(|s| s.render())
                        .or_else(|| snippet_for_node(n, &repo_root))
                } else {
                    snippet_for_node(n, &repo_root)
                        .or_else(|| skeleton_for_node_inner(n, &repo_root).map(|s| s.render()))
                };
                if let Some(snip) = body {
                    let cost = snip.len() / TOKEN_CHARS_PER_TOKEN + 1;
                    if snip_tokens + cost <= snippet_ceiling {
                        snip_tokens += cost;
                        n_with_snippet += 1;
                        format!("{header}\n{SNIPPET_SEP}\n{snip}")
                    } else {
                        header
                    }
                } else {
                    header
                }
            } else {
                header
            };
            blocks.push(block);
        }

        let sanitized = sanitize_mcp_body_with_limit(&blocks.join("\n\n"), char_cap);
        format!(
            "{sanitized}\n\n[{n_nodes} nodes, {n_with_snippet} with snippets, ~{total_tokens} metadata-tokens + ~{snip_tokens} snippet-tokens ({mode_label} budget)]"
        )
    } else {
        // Legacy path — metadata-only with role annotations.
        let lines: Vec<String> = selected
            .iter()
            .map(|n| {
                let role = roles
                    .get(&n.id)
                    .copied()
                    .unwrap_or(NodeRole::Context)
                    .label();
                format!(
                    "{} ({}) — {} [package: {}] [via: {}]",
                    n.vname.signature, n.kind, n.vname.path, n.package, role
                )
            })
            .collect();
        let sanitized = sanitize_mcp_body_with_limit(&lines.join("\n"), char_cap);
        format!("{sanitized}\n\n[{n_nodes} nodes, ~{total_tokens} tokens]")
    }
}

/// Global variant of `get_context` — searches one named repo or all registered repos.
pub fn get_context_global(
    repos: &HashMap<String, PathBuf>,
    query: &str,
    token_budget: usize,
    repo: Option<&str>,
    include_snippets: bool,
    snippet_budget: Option<usize>,
) -> String {
    if let Err(reason) = validate_mcp_arg(query) {
        tracing::warn!("get_context_global rejected invalid query: {reason}");
        return wrap_envelope("");
    }
    if token_budget > MAX_CONTEXT_BUDGET {
        return wrap_envelope("token_budget exceeds maximum allowed value");
    }
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = get_context_raw(store, query, token_budget, include_snippets, snippet_budget);
        if result.is_empty() || single {
            result
        } else {
            result
                .lines()
                .map(|l| format!("[{repo_name}] {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    wrap_envelope(&raw)
}

// ── get_graph_json ────────────────────────────────────────────────────────────

/// Max nodes returned by `get_graph_json` to keep MCP payloads manageable.
const MAX_GRAPH_JSON_NODES: usize = 200;

/// Return a subgraph around `query` as structured JSON for graph renderers.
///
/// Query parameters shared by `get_graph_json` and `get_graph_json_global`.
/// Keeps both function signatures under clippy's 7-argument limit.
pub struct GraphJsonParams<'a> {
    pub query: &'a str,
    pub direction: &'a str,
    pub depth: u8,
    pub kind_filter: &'a str,
    pub token_budget: usize,
    pub mode: &'a str,
    pub path_prefix: &'a str,
}

/// BFS from seed node(s) matching `query`, respecting `direction` and `depth`.
/// Returns `{"nodes":[...],"edges":[...]}`.
/// Unlike prose tools, output is NOT sanitized — it is structured JSON consumed
/// by the VS Code graph panel, not forwarded to an LLM as freetext.
pub fn get_graph_json(store: &SqliteStore, params: &GraphJsonParams<'_>) -> String {
    let GraphJsonParams {
        query,
        direction,
        depth,
        kind_filter,
        token_budget,
        mode,
        path_prefix,
    } = params;
    if !matches!(*mode, "" | "overview") {
        tracing::warn!("get_graph_json rejected unknown mode: {mode}");
        return "{}".to_string();
    }
    if *mode == "overview" {
        if !path_prefix.is_empty() {
            if let Err(reason) = validate_mcp_arg(path_prefix) {
                tracing::warn!("get_graph_json rejected invalid path_prefix: {reason}");
                return "{}".to_string();
            }
        }
        return overview_graph(store, path_prefix);
    }
    // Only "" (all kinds) and "file" are valid kind_filter values.
    if !matches!(*kind_filter, "" | "file") {
        tracing::warn!("get_graph_json rejected unknown kind_filter: {kind_filter}");
        return "{}".to_string();
    }
    // Empty query is valid when kind_filter=="file" (returns full import graph).
    if !(query.is_empty() && *kind_filter == "file") {
        if let Err(reason) = validate_mcp_arg(query) {
            tracing::warn!("get_graph_json rejected invalid arg: {reason}");
            return "{}".to_string();
        }
    }
    let depth = (*depth).clamp(1, 4);
    get_graph_json_raw(store, query, direction, depth, kind_filter, *token_budget)
}

// ── Repo-map LOD overview (P3 #319) ──────────────────────────────────────────

/// Maps a file path to its top-level directory segment (the first `/`-delimited part).
/// Returns an empty string for external/build-cache paths that should be excluded.
///
/// "pkg/api/types.go"             → "pkg"
/// "cmd/kubeadm/main.go"          → "cmd"
/// "src/index.ts"                 → "src"
/// "main.go"                      → "(root)"
/// "../../../Library/Caches/..."  → ""  (excluded)
/// "/abs/path/file.go"            → ""  (excluded)
/// "scip://corpus/file"           → ""  (excluded)
fn pkg_key_from_path(path: &str) -> String {
    // Skip build-cache, absolute, or protocol-prefixed paths (SCIP, etc.)
    if path.is_empty() || path.starts_with("..") || path.starts_with('/') || path.contains("://") {
        return String::new();
    }
    let segs: Vec<&str> = path.split('/').collect();
    match segs.len().saturating_sub(1) {
        0 => "(root)".to_string(),
        _ => segs[0].to_string(),
    }
}

fn file_label(path: &str) -> &str {
    path.rfind('/').map_or(path, |i| &path[i + 1..])
}

/// Entry point for `mode="overview"`. Routes by whether path_prefix is set.
fn overview_graph(store: &SqliteStore, path_prefix: &str) -> String {
    let file_nodes = match store.nodes_by_kind("file") {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("overview_graph: nodes_by_kind error: {e}");
            return r#"{"nodes":[],"edges":[]}"#.to_string();
        }
    };
    let pairs = match store.file_import_pairs() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("overview_graph: file_import_pairs error: {e}");
            vec![]
        }
    };
    if path_prefix.is_empty() {
        repo_overview_graph(&file_nodes, &pairs)
    } else {
        package_drill_graph(&file_nodes, &pairs, path_prefix)
    }
}

/// Aggregate all file nodes into synthetic package tiles + cross-package import edges.
// O(F + P) where F = file nodes, P = import pairs
fn repo_overview_graph(file_nodes: &[travsr_core::Node], pairs: &[(String, String)]) -> String {
    use std::collections::{BTreeMap, HashMap};

    let mut pkg_files: BTreeMap<String, u32> = BTreeMap::new();

    for node in file_nodes {
        let key = pkg_key_from_path(&node.vname.path);
        if key.is_empty() {
            continue; // skip build-cache, absolute, or protocol paths
        }
        *pkg_files.entry(key).or_default() += 1;
    }

    if pkg_files.is_empty() {
        return r#"{"nodes":[],"edges":[],"mode":"overview"}"#.to_string();
    }

    let nodes_out: Vec<serde_json::Value> = pkg_files
        .iter()
        .map(|(pkg, file_count)| {
            serde_json::json!({
                "id":         format!("pkg:{pkg}"),
                "label":      pkg,
                "kind":       "pkg",
                "file_count": file_count,
                "ghost":      false,
            })
        })
        .collect();

    let mut cross_pkg: HashMap<(String, String), u32> = HashMap::new();
    for (src_path, dst_path) in pairs {
        let src_pkg = pkg_key_from_path(src_path);
        let dst_pkg = pkg_key_from_path(dst_path);
        if src_pkg.is_empty() || dst_pkg.is_empty() || src_pkg == dst_pkg {
            continue;
        }
        // Only emit edges where both packages are known (present in file_nodes)
        if !pkg_files.contains_key(&src_pkg) || !pkg_files.contains_key(&dst_pkg) {
            continue;
        }
        *cross_pkg.entry((src_pkg, dst_pkg)).or_default() += 1;
    }

    let edges_out: Vec<serde_json::Value> = cross_pkg
        .iter()
        .map(|((src, dst), count)| {
            serde_json::json!({
                "source": format!("pkg:{src}"),
                "target": format!("pkg:{dst}"),
                "kind":   "imports",
                "count":  count,
            })
        })
        .collect();

    match serde_json::to_string(&serde_json::json!({
        "nodes": nodes_out,
        "edges": edges_out,
        "mode":  "overview",
    })) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("repo_overview_graph serialization error: {e}");
            "{}".to_string()
        }
    }
}

/// File-level drill into a package: emit file nodes inside `path_prefix` plus ghost-port
/// package nodes for any cross-boundary import targets.
// O(F + P) where F = file nodes, P = import pairs
fn package_drill_graph(
    file_nodes: &[travsr_core::Node],
    pairs: &[(String, String)],
    path_prefix: &str,
) -> String {
    use std::collections::{HashMap, HashSet};

    let prefix_paths: HashSet<&str> = file_nodes
        .iter()
        .filter(|n| n.vname.path.starts_with(path_prefix))
        .map(|n| n.vname.path.as_str())
        .collect();

    if prefix_paths.is_empty() {
        return r#"{"nodes":[],"edges":[],"mode":"prefix"}"#.to_string();
    }

    let mut nodes_out: Vec<serde_json::Value> = Vec::new();
    let mut node_ids_out: HashSet<String> = HashSet::new();

    for path in &prefix_paths {
        let json_id = format!("file:{path}");
        node_ids_out.insert(json_id.clone());
        nodes_out.push(serde_json::json!({
            "id":    json_id,
            "label": file_label(path),
            "kind":  "file",
            "path":  path,
            "ghost": false,
        }));
    }

    let mut ghost_pkg_counts: HashMap<String, u32> = HashMap::new();
    let mut edges_out: Vec<serde_json::Value> = Vec::new();
    let mut edge_seen: HashSet<(String, String)> = HashSet::new();

    for (src_path, dst_path) in pairs {
        // src must be inside the prefix (src_path may be a function path — check prefix match)
        if !src_path.starts_with(path_prefix) {
            continue;
        }
        // Resolve src to the nearest file in prefix_paths (take the path directly if present,
        // otherwise derive the file path from the src path's directory)
        let src_file = if prefix_paths.contains(src_path.as_str()) {
            src_path.as_str()
        } else {
            // src is a symbol node whose path happens to start with prefix — use it as-is
            // (it won't match any file node, so skip intra edges but allow cross-boundary)
            src_path.as_str()
        };

        if prefix_paths.contains(dst_path.as_str()) {
            // Intra-prefix: file → file edge
            let key = (src_file.to_string(), dst_path.clone());
            if edge_seen.insert(key) {
                edges_out.push(serde_json::json!({
                    "source": format!("file:{src_file}"),
                    "target": format!("file:{dst_path}"),
                    "kind":   "imports",
                }));
            }
        } else {
            // Cross-boundary: collapse dst into a ghost package node
            let ghost_pkg = pkg_key_from_path(dst_path);
            if ghost_pkg.is_empty() {
                continue;
            }
            let ghost_id = format!("pkg:{ghost_pkg}");

            let count = ghost_pkg_counts.entry(ghost_pkg.clone()).or_insert(0);
            *count += 1;
            if *count == 1 {
                node_ids_out.insert(ghost_id.clone());
                nodes_out.push(serde_json::json!({
                    "id":    ghost_id.clone(),
                    "label": ghost_pkg,
                    "kind":  "ghost",
                    "ghost": true,
                }));
            }

            let key = (src_file.to_string(), ghost_id.clone());
            if edge_seen.insert(key) {
                edges_out.push(serde_json::json!({
                    "source": format!("file:{src_file}"),
                    "target": ghost_id,
                    "kind":   "imports",
                }));
            }
        }
    }

    match serde_json::to_string(&serde_json::json!({
        "nodes":       nodes_out,
        "edges":       edges_out,
        "mode":        "prefix",
        "path_prefix": path_prefix,
    })) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("package_drill_graph serialization error: {e}");
            "{}".to_string()
        }
    }
}

fn edge_kind_str(kind: &travsr_core::EdgeKind) -> &'static str {
    use travsr_core::EdgeKind;
    match kind {
        EdgeKind::DefinesBinding => "defines",
        EdgeKind::RefCall => "calls",
        EdgeKind::Depends => "imports",
        EdgeKind::ResolvesTo => "resolves-to",
        EdgeKind::Exports => "exports",
        EdgeKind::RefImports => "ref/imports",
        EdgeKind::IsImplementation => "is-implementation",
        EdgeKind::Overrides => "overrides",
        EdgeKind::FFICall => "ffi/call",
    }
}

/// Unique JSON id for a node.
/// File nodes all share `signature == "file"`, so we use `path` (prefixed with
/// corpus when non-empty) to keep Cytoscape ids distinct across repos.
fn node_json_id(node: &CoreNode) -> String {
    if node.kind == "file" {
        if node.vname.corpus.is_empty() {
            node.vname.path.clone()
        } else {
            format!("{}:{}", node.vname.corpus, node.vname.path)
        }
    } else {
        node.vname.signature.clone()
    }
}

/// Short display label — basename for files, full signature for everything else.
fn node_json_label(node: &CoreNode) -> String {
    if node.kind == "file" {
        node.vname
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&node.vname.path)
            .to_string()
    } else if node.vname.signature.starts_with("scip:") {
        // Extract the short name from a scip qualified signature.
        // "scip:...HomeController#home()." → "home()"
        // "scip:...HomeController#"        → "HomeController"
        let sig = &node.vname.signature;
        if let Some(hash_pos) = sig.rfind('#') {
            let after = sig[hash_pos + 1..].trim_end_matches('.');
            if !after.is_empty() {
                return after.to_string();
            }
            let before = &sig[..hash_pos];
            return before
                .rsplit(['/', ' '])
                .next()
                .unwrap_or(before)
                .to_string();
        }
        sig.to_string()
    } else {
        // Strip structural prefixes (fn:, class:, method:, interface:, import:)
        // so "fn:home" displays as "home", "class:HomeController" as "HomeController".
        let sig = &node.vname.signature;
        if let Some(rest) = sig
            .strip_prefix("fn:")
            .or_else(|| sig.strip_prefix("class:"))
            .or_else(|| sig.strip_prefix("method:"))
            .or_else(|| sig.strip_prefix("interface:"))
            .or_else(|| sig.strip_prefix("import:"))
        {
            rest.to_string()
        } else {
            sig.clone()
        }
    }
}

fn get_graph_json_raw(
    store: &SqliteStore,
    query: &str,
    direction: &str,
    depth: u8,
    kind_filter: &str,
    token_budget: usize,
) -> String {
    use std::collections::{HashSet, VecDeque};

    // File mode with empty query: search broadly by path separator to seed all file nodes.
    // DEBT(travsr): replace "." sentinel with an explicit all-files store query
    let search_term = if kind_filter == "file" && query.is_empty() {
        "."
    } else {
        query
    };
    let seed_nodes_raw = match store.search_nodes_by_name(search_term) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("get_graph_json search error: {e}");
            return r#"{"nodes":[],"edges":[]}"#.to_string();
        }
    };

    // Filter seeds to the requested kind when kind_filter is set.
    // In symbol mode: also exclude scip: nodes from seeds — Phase B semantic nodes
    // have no edges in most projects and appear as isolated floating dots.
    // They are still reachable via BFS traversal if edges exist.
    let seed_nodes: Vec<_> = if !kind_filter.is_empty() {
        seed_nodes_raw
            .into_iter()
            .filter(|n| n.kind == kind_filter)
            .collect()
    } else {
        seed_nodes_raw
            .into_iter()
            .filter(|n| !n.vname.signature.starts_with("scip:"))
            .collect()
    };

    if seed_nodes.is_empty() {
        return r#"{"nodes":[],"edges":[]}"#.to_string();
    }

    // Coverage metadata (#318 O5): sourced from the first seed's language so
    // "no callers" is distinguishable from "cannot see callers".
    let seed_language = seed_nodes[0].vname.language.clone();
    let coverage = serde_json::json!({
        "language": seed_language,
        "semantic": store.has_refcall_edges_for_language(&seed_language),
        "phase_b_commit": store.get_meta("phase_b_commit").ok().flatten(),
    });

    // (NodeId, hop_distance)
    let mut visited: HashSet<NodeId> = HashSet::new();
    let mut queue: VecDeque<(NodeId, u8)> = VecDeque::new();
    let mut nodes_out: Vec<serde_json::Value> = Vec::new();
    let mut edges_out: Vec<serde_json::Value> = Vec::new();
    let mut edge_seen: HashSet<(NodeId, NodeId, &'static str)> = HashSet::new();
    // Token budget (#318 O6): cumulative cost of emitted nodes; once the next
    // node would exceed the budget, stop adding nodes (mirrors the node cap).
    let mut tokens_used = 0usize;
    let mut truncated_by_budget = false;
    // Tracks JSON ids already in nodes_out. Two different DB rows can share the
    // same signature (Phase B stores the same symbol once per referencing file).
    // Deduplicate here so Cytoscape never sees two nodes with the same id.
    let mut node_ids_out: HashSet<String> = HashSet::new();

    for node in &seed_nodes {
        if visited.insert(node.id) {
            queue.push_back((node.id, 0));
        }
    }

    while let Some((current_id, hop)) = queue.pop_front() {
        let node = match store.get_node(current_id) {
            Ok(Some(n)) => n,
            _ => continue,
        };

        // Skip nodes that don't match the kind filter.
        if !kind_filter.is_empty() && node.kind != kind_filter {
            continue;
        }

        // In symbol mode: skip import nodes (long qualified names, no navigation
        // value in a symbol graph) and scip:local N internal tokens.
        // Skip empty-path external stubs at hop>0.
        let is_scip_local =
            node.vname.signature.starts_with("scip:") && node.vname.signature.contains(":local ");
        let is_import_in_symbol_mode = node.kind == "import" && kind_filter != "file";
        if is_scip_local || is_import_in_symbol_mode || (node.vname.path.is_empty() && hop > 0) {
            continue;
        }

        // Budget check before emission: the seed (first node) always survives
        // so a tiny budget still yields a non-empty, honest payload.
        let node_tokens = token_cost(&node);
        if token_budget > 0 && !nodes_out.is_empty() && tokens_used + node_tokens > token_budget {
            truncated_by_budget = true;
            continue;
        }

        let json_id = node_json_id(&node);
        if !node_ids_out.insert(json_id.clone()) {
            continue; // duplicate signature across DB rows — skip to avoid Cytoscape crash
        }
        tokens_used += node_tokens;

        let score = {
            let raw = 0.7_f64.powi(i32::from(hop));
            (raw * 1000.0).round() / 1000.0
        };
        let mut node_obj = serde_json::json!({
            "id":      json_id,
            "label":   node_json_label(&node),
            "kind":    node.kind,
            "path":    node.vname.path,
            "package": node.package,
            "score":   score,
            "line":    node.line,
        });
        if hop == 0 {
            node_obj["root"] = serde_json::Value::Bool(true);
        }
        nodes_out.push(node_obj);

        if nodes_out.len() >= MAX_GRAPH_JSON_NODES {
            tracing::warn!(
                "get_graph_json hit node cap ({MAX_GRAPH_JSON_NODES}) for query '{query}'"
            );
            continue;
        }

        if hop >= depth {
            continue;
        }

        // File mode: the import schema uses a two-hop chain
        //   file --[Depends]--> import_node --[ResolvesTo]--> file
        // Look through the intermediary to emit direct file→file edges.
        if kind_filter == "file" {
            use travsr_core::EdgeKind;
            if direction == "deps" || direction == "both" {
                if let Ok(dep_edges) = store.iter_edges_from(current_id) {
                    // Batch: collect all ResolvesTo target ids across all Depends hops,
                    // then fetch once instead of one get_node per res.dst.
                    let mut res_targets: Vec<NodeId> = Vec::new();
                    for dep in &dep_edges {
                        if !matches!(dep.kind, EdgeKind::Depends) {
                            continue;
                        }
                        if let Ok(res_edges) = store.iter_edges_from(dep.dst) {
                            for res in &res_edges {
                                if matches!(res.kind, EdgeKind::ResolvesTo) {
                                    res_targets.push(res.dst);
                                }
                            }
                        }
                    }
                    let node_map: HashMap<NodeId, CoreNode> = store
                        .get_nodes(&res_targets)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|n| (n.id, n))
                        .collect();
                    for target_id in res_targets {
                        if let Some(target) = node_map.get(&target_id) {
                            if target.kind != "file" {
                                continue;
                            }
                            if edge_seen.insert((current_id, target.id, "imports")) {
                                edges_out.push(serde_json::json!({
                                    "source": node_json_id(&node),
                                    "target": node_json_id(target),
                                    "kind":   "imports",
                                }));
                            }
                            if visited.insert(target.id) {
                                queue.push_back((target.id, hop + 1));
                            }
                        }
                    }
                }
            }
            if direction == "callers" || direction == "both" {
                // Reverse: who imports this file?
                //   importer_file --[Depends]--> import_node --[ResolvesTo]--> current_file
                if let Ok(rev_res_edges) = store.iter_edges_to(current_id) {
                    // Batch: collect all Depends src ids across all ResolvesTo hops,
                    // then fetch once.
                    let mut source_ids: Vec<NodeId> = Vec::new();
                    for rev_res in &rev_res_edges {
                        if !matches!(rev_res.kind, EdgeKind::ResolvesTo) {
                            continue;
                        }
                        if let Ok(rev_dep_edges) = store.iter_edges_to(rev_res.src) {
                            for rev_dep in &rev_dep_edges {
                                if matches!(rev_dep.kind, EdgeKind::Depends) {
                                    source_ids.push(rev_dep.src);
                                }
                            }
                        }
                    }
                    let node_map: HashMap<NodeId, CoreNode> = store
                        .get_nodes(&source_ids)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|n| (n.id, n))
                        .collect();
                    for source_id in source_ids {
                        if let Some(source) = node_map.get(&source_id) {
                            if source.kind != "file" {
                                continue;
                            }
                            if edge_seen.insert((source.id, current_id, "imports")) {
                                edges_out.push(serde_json::json!({
                                    "source": node_json_id(source),
                                    "target": node_json_id(&node),
                                    "kind":   "imports",
                                }));
                            }
                            if visited.insert(source.id) {
                                queue.push_back((source.id, hop + 1));
                            }
                        }
                    }
                }
            }
            continue; // skip normal edge traversal
        }

        // Normal traversal (symbol mode)
        if direction == "deps" || direction == "both" {
            if let Ok(edges) = store.iter_edges_from(current_id) {
                // First pass: dedup via edge_seen, enqueue new visits.
                // Collect (dst_id, kind_s) only for edges that produce JSON output.
                let mut new_edges: Vec<(NodeId, &str)> = Vec::new();
                for edge in &edges {
                    let kind_s = edge_kind_str(&edge.kind);
                    if edge_seen.insert((edge.src, edge.dst, kind_s)) {
                        new_edges.push((edge.dst, kind_s));
                    }
                    if visited.insert(edge.dst) {
                        queue.push_back((edge.dst, hop + 1));
                    }
                }
                // Batch-fetch dst nodes, then emit JSON edges in original order.
                let dst_ids: Vec<NodeId> = new_edges.iter().map(|(id, _)| *id).collect();
                let node_map: HashMap<NodeId, CoreNode> = store
                    .get_nodes(&dst_ids)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|n| (n.id, n))
                    .collect();
                for (dst_id, kind_s) in &new_edges {
                    if let Some(dst) = node_map.get(dst_id) {
                        edges_out.push(serde_json::json!({
                            "source": node_json_id(&node),
                            "target": node_json_id(dst),
                            "kind":   kind_s,
                        }));
                    }
                }
            }
        }

        if direction == "callers" || direction == "both" {
            if let Ok(edges) = store.iter_edges_to(current_id) {
                // First pass: dedup via edge_seen, enqueue new visits.
                let mut new_edges: Vec<(NodeId, &str)> = Vec::new();
                for edge in &edges {
                    let kind_s = edge_kind_str(&edge.kind);
                    if edge_seen.insert((edge.src, edge.dst, kind_s)) {
                        new_edges.push((edge.src, kind_s));
                    }
                    if visited.insert(edge.src) {
                        queue.push_back((edge.src, hop + 1));
                    }
                }
                // Batch-fetch src nodes, then emit JSON edges in original order.
                let src_ids: Vec<NodeId> = new_edges.iter().map(|(id, _)| *id).collect();
                let node_map: HashMap<NodeId, CoreNode> = store
                    .get_nodes(&src_ids)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|n| (n.id, n))
                    .collect();
                for (src_id, kind_s) in &new_edges {
                    if let Some(src) = node_map.get(src_id) {
                        edges_out.push(serde_json::json!({
                            "source": node_json_id(src),
                            "target": node_json_id(&node),
                            "kind":   kind_s,
                        }));
                    }
                }
            }
        }
    }

    // Remove edges whose source or target was filtered/deduplicated from nodes_out.
    // Orphan edges cause Cytoscape to silently crash.
    edges_out.retain(|e| {
        let src = e["source"].as_str().unwrap_or("");
        let tgt = e["target"].as_str().unwrap_or("");
        node_ids_out.contains(src) && node_ids_out.contains(tgt)
    });

    // Additive envelope fields (#318 O5/O6) — first-party consumers read only
    // `nodes`/`edges`; the global merge likewise ignores extra keys.
    let mut out = serde_json::json!({
        "nodes": nodes_out,
        "edges": edges_out,
        "coverage": coverage,
    });
    if token_budget > 0 {
        out["token_budget"] = serde_json::json!(token_budget);
        out["truncated_by_budget"] = serde_json::json!(truncated_by_budget);
    }
    match serde_json::to_string(&out) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("get_graph_json serialization error: {e}");
            "{}".to_string()
        }
    }
}

/// Global variant of `get_graph_json` — merges subgraphs across repos, deduping by node id.
pub fn get_graph_json_global(
    repos: &HashMap<String, PathBuf>,
    repo: Option<&str>,
    params: &GraphJsonParams<'_>,
) -> String {
    let GraphJsonParams {
        query,
        direction,
        depth,
        kind_filter,
        token_budget: _,
        mode,
        path_prefix,
    } = params;
    if *mode == "overview" {
        if !path_prefix.is_empty() {
            if let Err(reason) = validate_mcp_arg(path_prefix) {
                tracing::warn!("get_graph_json_global rejected invalid path_prefix: {reason}");
                return "{}".to_string();
            }
        }
        // Overview mode: run per-repo and merge package tiles
        return get_graph_json_global_overview(repos, repo, path_prefix);
    }
    if !(query.is_empty() && *kind_filter == "file") {
        if let Err(reason) = validate_mcp_arg(query) {
            tracing::warn!("get_graph_json_global rejected invalid arg: {reason}");
            return "{}".to_string();
        }
    }
    let depth = (*depth).clamp(1, 4);

    let candidates: Vec<(&str, &PathBuf)> = match repo {
        Some(name) => {
            if let Err(reason) = validate_mcp_arg(name) {
                tracing::warn!("get_graph_json_global rejected invalid repo arg: {reason}");
                return "{}".to_string();
            }
            match repos.get_key_value(name) {
                Some((k, v)) => vec![(k.as_str(), v)],
                None => {
                    tracing::warn!("repo '{name}' not found in registry");
                    return r#"{"nodes":[],"edges":[]}"#.to_string();
                }
            }
        }
        None => repos.iter().map(|(k, v)| (k.as_str(), v)).collect(),
    };

    let mut all_nodes: Vec<serde_json::Value> = Vec::new();
    let mut all_edges: Vec<serde_json::Value> = Vec::new();
    let mut seen_node_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_edge_keys: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    for (_repo_name, db_path) in candidates {
        if !db_path.exists() {
            continue;
        }
        match SqliteStore::open_read_only(db_path) {
            Ok(store) => {
                let raw = get_graph_json_raw(&store, query, direction, depth, kind_filter, 0);
                let parsed: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                for node in parsed["nodes"].as_array().into_iter().flatten() {
                    let id = node["id"].as_str().unwrap_or("").to_string();
                    if !id.is_empty() && seen_node_ids.insert(id) {
                        all_nodes.push(node.clone());
                    }
                }
                for edge in parsed["edges"].as_array().into_iter().flatten() {
                    let src = edge["source"].as_str().unwrap_or("").to_string();
                    let tgt = edge["target"].as_str().unwrap_or("").to_string();
                    if !src.is_empty() && seen_edge_keys.insert((src, tgt)) {
                        all_edges.push(edge.clone());
                    }
                }
            }
            Err(e) => tracing::warn!("failed to open {}: {e}", db_path.display()),
        }
    }

    match serde_json::to_string(&serde_json::json!({
        "nodes": all_nodes,
        "edges": all_edges,
    })) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("get_graph_json_global serialization error: {e}");
            "{}".to_string()
        }
    }
}

/// Merge overview graphs across repos for the global (SSE) session.
fn get_graph_json_global_overview(
    repos: &HashMap<String, PathBuf>,
    repo: Option<&str>,
    path_prefix: &str,
) -> String {
    use std::collections::HashMap as HMap;

    let candidates: Vec<(&str, &PathBuf)> = match repo {
        Some(name) => match repos.get_key_value(name) {
            Some((k, v)) => vec![(k.as_str(), v)],
            None => {
                tracing::warn!("get_graph_json_global_overview: repo '{name}' not found");
                return r#"{"nodes":[],"edges":[],"mode":"overview"}"#.to_string();
            }
        },
        None => repos.iter().map(|(k, v)| (k.as_str(), v)).collect(),
    };

    let mut merged_nodes: HMap<String, serde_json::Value> = HMap::new();
    let mut merged_edges: HMap<(String, String), u32> = HMap::new();

    for (_repo_name, db_path) in candidates {
        if !db_path.exists() {
            continue;
        }
        match SqliteStore::open_read_only(db_path) {
            Ok(store) => {
                let raw = overview_graph(&store, path_prefix);
                let parsed: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                for node in parsed["nodes"].as_array().into_iter().flatten() {
                    let id = node["id"].as_str().unwrap_or("").to_string();
                    merged_nodes.entry(id).or_insert_with(|| node.clone());
                }
                for edge in parsed["edges"].as_array().into_iter().flatten() {
                    let src = edge["source"].as_str().unwrap_or("").to_string();
                    let tgt = edge["target"].as_str().unwrap_or("").to_string();
                    let count = edge["count"].as_u64().unwrap_or(1) as u32;
                    *merged_edges.entry((src, tgt)).or_default() += count;
                }
            }
            Err(e) => tracing::warn!("get_graph_json_global_overview: open error: {e}"),
        }
    }

    let nodes_out: Vec<&serde_json::Value> = merged_nodes.values().collect();
    let edges_out: Vec<serde_json::Value> = merged_edges
        .iter()
        .map(|((src, tgt), count)| {
            serde_json::json!({
                "source": src,
                "target": tgt,
                "kind":   "imports",
                "count":  count,
            })
        })
        .collect();

    match serde_json::to_string(&serde_json::json!({
        "nodes": nodes_out,
        "edges": edges_out,
        "mode":  "overview",
    })) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("get_graph_json_global_overview serialization error: {e}");
            "{}".to_string()
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// SEC-002 end-to-end: a path-traversal repo arg must be rejected through
    /// the full get_callers_global → collect_global → validate_mcp_arg pipeline.
    /// This exercises the wiring that the validate_mcp_arg unit tests in
    /// sanitize.rs do not cover — a regression here would be invisible to those
    /// unit tests.
    #[test]
    fn get_callers_global_rejects_path_traversal_repo_arg() {
        let repos: HashMap<String, PathBuf> = HashMap::new();
        let result = get_callers_global(&repos, "charge", Some("../evil"));
        // Invalid repo arg must return an empty envelope, not a panic or error.
        assert_eq!(
            result, "<travsr-data></travsr-data>",
            "path traversal in repo arg must be rejected and return empty envelope"
        );
    }

    /// SEC-002 end-to-end: an absolute-path repo arg must also be rejected.
    #[test]
    fn get_dependencies_global_rejects_absolute_repo_arg() {
        let repos: HashMap<String, PathBuf> = HashMap::new();
        let result = get_dependencies_global(&repos, "src/main.ts", Some("/etc/passwd"));
        assert_eq!(
            result, "<travsr-data></travsr-data>",
            "absolute path in repo arg must be rejected and return empty envelope"
        );
    }

    // ── blast radius unit tests ───────────────────────────────────────────────

    fn make_store(
        nodes: &[travsr_core::Node],
        edges: &[(
            travsr_core::NodeId,
            travsr_core::NodeId,
            travsr_core::EdgeKind,
        )],
    ) -> travsr_store::SqliteStore {
        use travsr_core::Edge;
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        for n in nodes {
            store.put_node(n).unwrap();
        }
        for &(src, dst, kind) in edges {
            store.put_edge(&Edge::new(src, dst, kind)).unwrap();
        }
        store
    }

    fn make_node(path: &str, sig: &str) -> travsr_core::Node {
        use travsr_core::VName;
        travsr_core::Node::new(VName::new("", "", path, "typescript", sig), "function")
    }

    /// A file with no callers — blast radius is just itself.
    #[test]
    fn blast_radius_includes_source_file() {
        let a = make_node("a.ts", "fn:a");
        let store = make_store(std::slice::from_ref(&a), &[]);
        let result = get_blast_radius(&store, "a.ts", AnalysisMode::TreeSitter);
        assert!(
            result.contains("a.ts"),
            "source file must appear in its own blast radius"
        );
    }

    /// B → A (incoming call): blast_radius("a.ts") must include b.ts.
    #[test]
    fn blast_radius_follows_incoming_edges() {
        use travsr_core::EdgeKind;
        let a = make_node("a.ts", "fn:a");
        let b = make_node("b.ts", "fn:b");
        // B calls A — reverse BFS from A should reach B.
        let store = make_store(&[a.clone(), b.clone()], &[(b.id, a.id, EdgeKind::RefCall)]);
        let result = get_blast_radius(&store, "a.ts", AnalysisMode::TreeSitter);
        assert!(result.contains("a.ts"), "source file must be included");
        assert!(
            result.contains("b.ts"),
            "caller file must be included in blast radius"
        );
    }

    /// B depends on A (import): blast_radius("a.ts") must include b.ts.
    #[test]
    fn blast_radius_follows_depends_edges() {
        use travsr_core::EdgeKind;
        let a = make_node("a.ts", "fn:a");
        let b = make_node("b.ts", "fn:b");
        // B imports A — Depends edge B→A; reverse BFS from A must reach B.
        let store = make_store(&[a.clone(), b.clone()], &[(b.id, a.id, EdgeKind::Depends)]);
        let result = get_blast_radius(&store, "a.ts", AnalysisMode::TreeSitter);
        assert!(result.contains("a.ts"), "source file must be included");
        assert!(
            result.contains("b.ts"),
            "file that imports the changed file must appear in blast radius"
        );
    }

    /// Cycle A ↔ B — must terminate without infinite loop.
    #[test]
    fn blast_radius_handles_cycle() {
        use travsr_core::EdgeKind;
        let a = make_node("a.ts", "fn:a");
        let b = make_node("b.ts", "fn:b");
        let store = make_store(
            &[a.clone(), b.clone()],
            &[
                (b.id, a.id, EdgeKind::RefCall),
                (a.id, b.id, EdgeKind::RefCall), // cycle
            ],
        );
        // Must not hang; both files reachable.
        let result = get_blast_radius(&store, "a.ts", AnalysisMode::TreeSitter);
        assert!(result.contains("a.ts"));
        assert!(result.contains("b.ts"));
    }

    /// Go co-package: blast_radius("a.go") must include all sibling files in
    /// the same package, via the Depends edges written by init_repo's co-package pass.
    #[test]
    fn blast_radius_includes_go_copackage_siblings() {
        use travsr_core::{EdgeKind, Node, VName};

        fn go_file_node(path: &str) -> Node {
            Node::new(VName::new("", "", path, "go", "file"), "file")
        }

        let a = go_file_node("strategies/serverConfig.go");
        let b = go_file_node("strategies/roundRobinStrategy.go");
        let c = go_file_node("strategies/loadBalancer.go");

        // Mirrors what init_repo's co-package pass writes: all ordered pairs.
        let store = make_store(
            &[a.clone(), b.clone(), c.clone()],
            &[
                (b.id, a.id, EdgeKind::Depends),
                (c.id, a.id, EdgeKind::Depends),
                (a.id, b.id, EdgeKind::Depends),
                (c.id, b.id, EdgeKind::Depends),
                (a.id, c.id, EdgeKind::Depends),
                (b.id, c.id, EdgeKind::Depends),
            ],
        );

        let result = get_blast_radius(
            &store,
            "strategies/serverConfig.go",
            AnalysisMode::TreeSitter,
        );
        assert!(
            result.contains("strategies/serverConfig.go"),
            "the file itself must appear in its blast radius"
        );
        assert!(
            result.contains("strategies/roundRobinStrategy.go"),
            "co-package sibling roundRobinStrategy.go must appear in blast radius"
        );
        assert!(
            result.contains("strategies/loadBalancer.go"),
            "co-package sibling loadBalancer.go must appear in blast radius"
        );
    }

    /// Semantic mode follows only RefCall; Depends-only files must be excluded.
    #[test]
    fn blast_radius_semantic_excludes_depends_only_files() {
        use travsr_core::EdgeKind;
        let a = make_node("a.ts", "fn:a");
        let b = make_node("b.ts", "fn:b"); // connected via RefCall
        let c = make_node("c.ts", "fn:c"); // connected via Depends only
        let store = make_store(
            &[a.clone(), b.clone(), c.clone()],
            &[
                (b.id, a.id, EdgeKind::RefCall),
                (c.id, a.id, EdgeKind::Depends),
            ],
        );
        let result = get_blast_radius(&store, "a.ts", AnalysisMode::Semantic);
        assert!(
            result.contains("b.ts"),
            "RefCall caller must appear in semantic blast radius"
        );
        assert!(
            !result.contains("c.ts"),
            "Depends-only file must NOT appear in semantic mode"
        );
    }

    /// TreeSitter mode follows both RefCall and Depends — unchanged from before.
    #[test]
    fn blast_radius_tree_sitter_follows_both_edge_kinds() {
        use travsr_core::EdgeKind;
        let a = make_node("a.ts", "fn:a");
        let b = make_node("b.ts", "fn:b");
        let c = make_node("c.ts", "fn:c");
        let store = make_store(
            &[a.clone(), b.clone(), c.clone()],
            &[
                (b.id, a.id, EdgeKind::RefCall),
                (c.id, a.id, EdgeKind::Depends),
            ],
        );
        let result = get_blast_radius(&store, "a.ts", AnalysisMode::TreeSitter);
        assert!(
            result.contains("b.ts") && result.contains("c.ts"),
            "TreeSitter mode must follow both RefCall and Depends"
        );
    }

    /// get_lang_status returns valid JSON for a known extension with no RefCall data.
    #[test]
    fn get_lang_status_known_extension_semantic_unavailable() {
        let store = make_store(&[], &[]);
        let json = get_lang_status(&store, "src/main.ts");
        assert!(json.contains(r#""language":"typescript""#));
        assert!(json.contains(r#""builtin":true"#));
        assert!(json.contains(r#""semantic_available":false"#));
    }

    /// get_lang_status returns semantic_available:true after a RefCall edge is inserted.
    #[test]
    fn get_lang_status_returns_semantic_available_true_after_refcall_insert() {
        use travsr_core::{Edge, EdgeKind, Node, VName};
        let n1 = Node::new(VName::new("", "", "a.ts", "typescript", "fn:a"), "function");
        let n2 = Node::new(VName::new("", "", "b.ts", "typescript", "fn:b"), "function");
        let mut store = make_store(&[n1.clone(), n2.clone()], &[]);
        store
            .put_edge(&Edge::new(n1.id, n2.id, EdgeKind::RefCall))
            .unwrap();
        let json = get_lang_status(&store, "a.ts");
        assert!(json.contains(r#""semantic_available":true"#));
        assert!(json.contains(r#""install_hint":"""#));
    }

    /// get_lang_status returns a safe unknown envelope for an unrecognised extension.
    #[test]
    fn get_lang_status_unknown_extension_returns_unknown() {
        let store = make_store(&[], &[]);
        let json = get_lang_status(&store, "Makefile");
        assert!(json.contains(r#""language":"unknown""#));
        assert!(json.contains(r#""semantic_available":false"#));
    }

    /// Non-builtin language (Go) shows install_hint when semantic unavailable.
    #[test]
    fn get_lang_status_non_builtin_shows_install_hint() {
        let store = make_store(&[], &[]);
        let json = get_lang_status(&store, "main.go");
        assert!(json.contains(r#""language":"go""#));
        assert!(json.contains(r#""builtin":false"#));
        assert!(json.contains(r#""semantic_available":false"#));
        assert!(json.contains("travsr lang install go"));
    }

    /// Builtin language (Rust) shows underlying_tool_hint when semantic unavailable.
    #[test]
    fn get_lang_status_builtin_shows_underlying_tool_hint() {
        let store = make_store(&[], &[]);
        let json = get_lang_status(&store, "src/main.rs");
        assert!(json.contains(r#""language":"rust""#));
        assert!(json.contains(r#""builtin":true"#));
        assert!(json.contains("rust-analyzer"));
    }

    // ── get_graph_stats unit tests ───────────────────────────────────────────

    #[test]
    fn get_graph_stats_empty_graph() {
        let store = make_store(&[], &[]);
        let out = get_graph_stats(&store);
        assert!(out.starts_with("nodes: 0\nedges: 0"), "got: {out}");
        assert!(out.contains("schema_version: "), "got: {out}");
    }

    #[test]
    fn get_graph_stats_counts_match_store() {
        use travsr_core::EdgeKind;
        let a = make_node("a.ts", "fn:a");
        let b = make_node("b.ts", "fn:b");
        let store = make_store(&[a.clone(), b.clone()], &[(a.id, b.id, EdgeKind::Depends)]);
        let out = get_graph_stats(&store);
        assert!(out.starts_with("nodes: 2\nedges: 1"), "got: {out}");
        assert!(out.contains("schema_version: "), "got: {out}");
    }

    // ── synonym tool unit tests (RFC-012 A2 F1 / VSCODE-247) ──────────────────

    // Note: `open_in_memory` seeds the synonym table with static defaults, so
    // these tests use a `zz`-prefixed namespace unlikely to collide with seeds
    // and never assert the table is globally empty.

    #[test]
    fn synonym_add_then_list_roundtrip() {
        let mut store = make_store(&[], &[]);
        assert_eq!(synonym_add(&mut store, "zzpayment", "zzcharge"), "ok");
        let list = synonym_list(&store);
        assert!(list.contains("zzpayment => zzcharge"), "got: {list}");
    }

    #[test]
    fn synonym_set_replaces_all_aliases() {
        let mut store = make_store(&[], &[]);
        synonym_add(&mut store, "zzauth", "zzlogin");
        synonym_add(&mut store, "zzauth", "zzsignin");
        assert_eq!(synonym_set(&mut store, "zzauth", "zzsession"), "ok");
        let list = synonym_list(&store);
        assert!(list.contains("zzauth => zzsession"), "got: {list}");
        assert!(
            !list.contains("zzauth => zzlogin"),
            "old alias must be gone: {list}"
        );
    }

    #[test]
    fn synonym_remove_term_clears_all() {
        let mut store = make_store(&[], &[]);
        synonym_add(&mut store, "zzdb", "zzdatabase");
        synonym_add(&mut store, "zzdb", "zzstore");
        assert_eq!(synonym_remove_term(&mut store, "zzdb"), "ok");
        let list = synonym_list(&store);
        assert!(
            !list.contains("zzdb => "),
            "all zzdb aliases must be gone: {list}"
        );
    }

    #[test]
    fn synonym_set_csv_splits_and_trims() {
        let mut store = make_store(&[], &[]);
        assert_eq!(
            synonym_set(&mut store, "zzk8s", " zzkube , zzkubernetes ,"),
            "ok"
        );
        let list = synonym_list(&store);
        assert!(list.contains("zzk8s => zzkube"), "got: {list}");
        assert!(list.contains("zzk8s => zzkubernetes"), "got: {list}");
    }

    #[test]
    fn synonym_add_rejects_path_traversal() {
        let mut store = make_store(&[], &[]);
        // SEC-002: term/alias go through validate_mcp_arg.
        let result = synonym_add(&mut store, "../etc/passwd", "x");
        assert_eq!(result, "invalid input");
        assert!(!synonym_list(&store).contains("../etc/passwd"));
    }

    #[test]
    fn synonym_set_rejects_invalid_alias_wholesale() {
        let mut store = make_store(&[], &[]);
        let result = synonym_set(&mut store, "zzterm", "zzgood,../bad");
        assert_eq!(result, "invalid input");
        // Rejected before the store call — neither alias written.
        let list = synonym_list(&store);
        assert!(!list.contains("zzterm => zzgood"), "got: {list}");
    }

    #[test]
    fn synonym_add_over_cap_returns_error_message() {
        let mut store = make_store(&[], &[]);
        // Add distinct aliases until the 200-row cap rejects one. The table is
        // pre-seeded with defaults, so the failure arrives before 200 zz-adds.
        let mut last = String::new();
        for i in 0..300 {
            last = synonym_add(&mut store, "zzfill", &format!("zza{i}"));
            if last != "ok" {
                break;
            }
        }
        assert_ne!(last, "ok", "the cap must eventually reject an add");
        assert!(last.contains("200"), "cap message expected, got: {last}");
    }

    #[test]
    fn synonym_reset_restores_defaults() {
        let mut store = make_store(&[], &[]);
        synonym_add(&mut store, "zzcustom", "zzalias");
        assert_eq!(synonym_reset(&mut store), "ok");
        let list = synonym_list(&store);
        assert!(
            !list.contains("zzcustom => zzalias"),
            "custom pair must be cleared: {list}"
        );
        assert!(!list.is_empty(), "defaults must be re-seeded after reset");
    }

    // Note: repos_list/repos_prune/repos_remove operate on the *real* global
    // registry (~/.travsr), so they are intentionally NOT unit-tested here — a
    // test calling repos_prune() would mutate the developer's HOME. The
    // underlying registry::{prune,unregister,all_repos} are covered by
    // travsr-store/src/registry.rs tests under a temp-HOME lock.

    // ── transitive dependencies unit test ────────────────────────────────────

    #[test]
    fn get_dependencies_transitive_reaches_depth_two() {
        use travsr_core::{EdgeKind, Node, VName};
        // file_a depends file_b depends file_c
        let mk = |path: &str| Node::new(VName::new("", "", path, "typescript", path), "file");
        let a = mk("a.ts");
        let b = mk("b.ts");
        let c = mk("c.ts");
        let store = make_store(
            &[a.clone(), b.clone(), c.clone()],
            &[
                (a.id, b.id, EdgeKind::Depends),
                (b.id, c.id, EdgeKind::Depends),
            ],
        );

        // Direct only: just b.ts.
        let direct = get_dependencies(&store, "a.ts", false, 3);
        assert!(direct.contains("b.ts"), "direct dep b.ts missing: {direct}");
        assert!(
            !direct.contains("c.ts"),
            "c.ts is transitive, not direct: {direct}"
        );

        // Transitive: both b.ts and c.ts.
        let trans = get_dependencies(&store, "a.ts", true, 3);
        assert!(trans.contains("b.ts"), "b.ts missing: {trans}");
        assert!(trans.contains("c.ts"), "transitive c.ts missing: {trans}");
    }

    // ── get_repo_map unit tests ───────────────────────────────────────────────

    /// Two nodes in different files must produce two entries.
    #[test]
    fn get_repo_map_groups_by_file() {
        let a = make_node("src/a.ts", "fn:a");
        let b = make_node("src/b.ts", "fn:b");
        let store = make_store(&[a, b], &[]);
        let result = get_repo_map(&store);
        assert!(result.contains("src/a.ts"), "a.ts must appear in repo map");
        assert!(result.contains("src/b.ts"), "b.ts must appear in repo map");
    }

    /// File-kind nodes must not appear as symbols in the map.
    #[test]
    fn get_graph_json_includes_line_for_symbol_nodes() {
        use travsr_core::{Node, VName};
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        let sym = Node::new(
            VName::new("test", "", "src/foo.ts", "typescript", "fn:bar"),
            "function",
        )
        .with_line(42);
        let file = Node::new(
            VName::new("test", "", "src/foo.ts", "typescript", "file"),
            "file",
        );
        store.put_node(&sym).unwrap();
        store.put_node(&file).unwrap();

        let json = get_graph_json(
            &store,
            &GraphJsonParams {
                query: "fn:bar",
                direction: "both",
                depth: 1,
                kind_filter: "",
                token_budget: 0,
                mode: "",
                path_prefix: "",
            },
        );
        assert!(
            json.contains("\"line\":42"),
            "symbol node must carry line in JSON: {json}"
        );
        // #318 O5: coverage envelope must always be present on the JSON tool.
        assert!(
            json.contains("\"coverage\""),
            "coverage envelope missing: {json}"
        );

        let file_json = get_graph_json(
            &store,
            &GraphJsonParams {
                query: "src/foo.ts",
                direction: "both",
                depth: 1,
                kind_filter: "file",
                token_budget: 0,
                mode: "",
                path_prefix: "",
            },
        );
        assert!(
            file_json.contains("\"line\":null"),
            "file node must have null line in JSON: {file_json}"
        );
    }

    /// #318 O6: a token budget caps the payload but always keeps the seed,
    /// and the truncation is reported in-band.
    #[test]
    fn get_graph_json_token_budget_truncates() {
        use travsr_core::{Edge, EdgeKind, Node, VName};
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        let seed = Node::new(
            VName::new("test", "", "src/seed.ts", "typescript", "fn:seed"),
            "function",
        );
        store.put_node(&seed).unwrap();
        for i in 0..20 {
            let n = Node::new(
                VName::new(
                    "test",
                    "",
                    format!("src/dep{i}.ts"),
                    "typescript",
                    format!("fn:dep{i}"),
                ),
                "function",
            );
            store.put_node(&n).unwrap();
            store
                .put_edge(&Edge::new(seed.id, n.id, EdgeKind::RefCall))
                .unwrap();
        }

        let unbounded = get_graph_json(
            &store,
            &GraphJsonParams {
                query: "fn:seed",
                direction: "deps",
                depth: 2,
                kind_filter: "",
                token_budget: 0,
                mode: "",
                path_prefix: "",
            },
        );
        assert!(!unbounded.contains("truncated_by_budget"));

        let capped = get_graph_json(
            &store,
            &GraphJsonParams {
                query: "fn:seed",
                direction: "deps",
                depth: 2,
                kind_filter: "",
                token_budget: 30,
                mode: "",
                path_prefix: "",
            },
        );
        assert!(
            capped.contains("\"truncated_by_budget\":true"),
            "tiny budget must truncate: {capped}"
        );
        assert!(
            capped.contains("fn:seed"),
            "seed must survive any budget: {capped}"
        );
        assert!(capped.len() < unbounded.len());
    }

    // ── P3 overview / LOD tests ────────────────────────────────────────────────

    fn make_file_graph_store() -> travsr_store::SqliteStore {
        use travsr_core::{Edge, EdgeKind, Node, VName};
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();

        // Two top-level packages: "pkg" (2 files) and "test" (1 file)
        let fa = Node::new(
            VName::new("", "", "pkg/a/mod.ts", "typescript", "pkg/a/mod.ts"),
            "file",
        );
        let fb = Node::new(
            VName::new("", "", "test/b/util.ts", "typescript", "test/b/util.ts"),
            "file",
        );
        let fc = Node::new(
            VName::new("", "", "pkg/a/helper.ts", "typescript", "pkg/a/helper.ts"),
            "file",
        );
        // Import node: fa depends on fb via an import node
        let imp = Node::new(
            VName::new("", "", "test/b/util.ts", "typescript", "import:b"),
            "import",
        );
        store.put_node(&fa).unwrap();
        store.put_node(&fb).unwrap();
        store.put_node(&fc).unwrap();
        store.put_node(&imp).unwrap();
        // fa --[Depends]--> imp --[ResolvesTo]--> fb  (cross-package: pkg → test)
        store
            .put_edge(&Edge::new(fa.id, imp.id, EdgeKind::Depends))
            .unwrap();
        store
            .put_edge(&Edge::new(imp.id, fb.id, EdgeKind::ResolvesTo))
            .unwrap();
        store
    }

    #[test]
    fn overview_graph_repo_level_groups_by_pkg() {
        let store = make_file_graph_store();
        let raw = get_graph_json(
            &store,
            &GraphJsonParams {
                query: "",
                direction: "both",
                depth: 2,
                kind_filter: "",
                token_budget: 0,
                mode: "overview",
                path_prefix: "",
            },
        );
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();

        assert_eq!(v["mode"], "overview", "mode field must be 'overview'");

        let nodes = v["nodes"].as_array().unwrap();
        let ids: Vec<&str> = nodes.iter().map(|n| n["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"pkg:pkg"), "must have 'pkg' package: {ids:?}");
        assert!(
            ids.contains(&"pkg:test"),
            "must have 'test' package: {ids:?}"
        );

        // file_count for "pkg" should be 2 (mod.ts + helper.ts)
        let pkg_pkg = nodes.iter().find(|n| n["id"] == "pkg:pkg").unwrap();
        assert_eq!(pkg_pkg["file_count"], 2, "'pkg' should have 2 files");

        // Cross-package edge pkg → test
        let edges = v["edges"].as_array().unwrap();
        let cross = edges
            .iter()
            .find(|e| e["source"] == "pkg:pkg" && e["target"] == "pkg:test");
        assert!(
            cross.is_some(),
            "must have cross-package edge pkg → test: {edges:?}"
        );
        assert!(cross.unwrap()["count"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn overview_graph_package_drill_emits_files_and_ghost() {
        let store = make_file_graph_store();
        let raw = get_graph_json(
            &store,
            &GraphJsonParams {
                query: "",
                direction: "both",
                depth: 2,
                kind_filter: "",
                token_budget: 0,
                mode: "overview",
                path_prefix: "pkg/a/",
            },
        );
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();

        assert_eq!(v["mode"], "prefix");

        let nodes = v["nodes"].as_array().unwrap();
        let ids: Vec<&str> = nodes.iter().map(|n| n["id"].as_str().unwrap()).collect();

        // File nodes inside prefix
        assert!(
            ids.contains(&"file:pkg/a/mod.ts"),
            "missing mod.ts: {ids:?}"
        );
        assert!(
            ids.contains(&"file:pkg/a/helper.ts"),
            "missing helper.ts: {ids:?}"
        );

        // Ghost port for external dep (1-segment key: "test")
        assert!(
            ids.contains(&"pkg:test"),
            "missing ghost port pkg:test: {ids:?}"
        );
        let ghost = nodes.iter().find(|n| n["id"] == "pkg:test").unwrap();
        assert_eq!(ghost["ghost"], true, "external package must be ghost");

        // Cross-boundary edge: file:pkg/a/mod.ts → pkg:test
        let edges = v["edges"].as_array().unwrap();
        let cross = edges
            .iter()
            .find(|e| e["source"] == "file:pkg/a/mod.ts" && e["target"] == "pkg:test");
        assert!(cross.is_some(), "must have cross-boundary edge: {edges:?}");
    }

    #[test]
    fn package_drill_graph_intra_prefix_edge_emitted() {
        use travsr_core::{Edge, EdgeKind, Node, VName};
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        let fa = Node::new(
            VName::new("", "", "pkg/a/mod.ts", "typescript", "pkg/a/mod.ts"),
            "file",
        );
        let fb = Node::new(
            VName::new("", "", "pkg/a/util.ts", "typescript", "pkg/a/util.ts"),
            "file",
        );
        let imp = Node::new(
            VName::new("", "", "pkg/a/util.ts", "typescript", "import:u"),
            "import",
        );
        store.put_node(&fa).unwrap();
        store.put_node(&fb).unwrap();
        store.put_node(&imp).unwrap();
        store
            .put_edge(&Edge::new(fa.id, imp.id, EdgeKind::Depends))
            .unwrap();
        store
            .put_edge(&Edge::new(imp.id, fb.id, EdgeKind::ResolvesTo))
            .unwrap();

        let raw = get_graph_json(
            &store,
            &GraphJsonParams {
                query: "",
                direction: "both",
                depth: 2,
                kind_filter: "",
                token_budget: 0,
                mode: "overview",
                path_prefix: "pkg/a/",
            },
        );
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let edges = v["edges"].as_array().unwrap();
        let intra = edges
            .iter()
            .find(|e| e["source"] == "file:pkg/a/mod.ts" && e["target"] == "file:pkg/a/util.ts");
        assert!(
            intra.is_some(),
            "intra-prefix edge must be emitted: {edges:?}"
        );
    }

    #[test]
    fn overview_graph_rejects_unknown_mode() {
        let store = travsr_store::SqliteStore::open_in_memory().unwrap();
        let raw = get_graph_json(
            &store,
            &GraphJsonParams {
                query: "",
                direction: "both",
                depth: 2,
                kind_filter: "",
                token_budget: 0,
                mode: "badmode",
                path_prefix: "",
            },
        );
        assert_eq!(raw, "{}", "unknown mode must return empty object");
    }

    #[test]
    fn overview_graph_rejects_path_traversal_prefix() {
        let store = travsr_store::SqliteStore::open_in_memory().unwrap();
        let raw = get_graph_json(
            &store,
            &GraphJsonParams {
                query: "",
                direction: "both",
                depth: 2,
                kind_filter: "",
                token_budget: 0,
                mode: "overview",
                path_prefix: "../etc/passwd",
            },
        );
        assert_eq!(raw, "{}", "path traversal prefix must be rejected");
    }

    #[test]
    fn pkg_key_from_path_one_segment() {
        // Any path with subdirs → first segment only
        assert_eq!(
            pkg_key_from_path("crates/travsr-mcp/src/tools.rs"),
            "crates"
        );
        assert_eq!(pkg_key_from_path("src/components/Button.tsx"), "src");
        assert_eq!(pkg_key_from_path("src/index.ts"), "src");
        assert_eq!(pkg_key_from_path("index.ts"), "(root)");
        // External / build-cache paths → empty (excluded)
        assert_eq!(pkg_key_from_path("../../../Library/Caches/go-build/xx"), "");
        assert_eq!(pkg_key_from_path("/abs/path/file.go"), "");
        assert_eq!(pkg_key_from_path("scip://corpus/path/file.go"), "");
        assert_eq!(pkg_key_from_path(""), "");
    }

    #[test]
    fn get_repo_map_excludes_file_kind_nodes() {
        use travsr_core::VName;
        let file_node = travsr_core::Node::new(
            VName::new("", "", "src/a.ts", "typescript", "src/a.ts"),
            "file",
        );
        let fn_node = make_node("src/a.ts", "fn:a");
        let store = make_store(&[file_node, fn_node], &[]);
        let result = get_repo_map(&store);
        // Should list "src/a.ts" as a file entry.
        assert!(result.contains("src/a.ts"));
        // The "file" kind node's signature must not appear as a symbol entry.
        // It should show [1 symbol] (only fn:a), not [2 symbols].
        assert!(
            result.contains("[1 symbol]"),
            "file-kind node must be excluded from symbol count"
        );
    }
}

// ── get_snippets ──────────────────────────────────────────────────────────────
// Snippet helpers are now owned by travsr-analysis (RFC-017 Phase 4).

use travsr_analysis::skeleton::skeleton_for_node as skeleton_for_node_inner;
pub use travsr_analysis::snippet::SNIPPET_DEFAULT_BUDGET;
use travsr_analysis::snippet::{snippet_for_node, snippet_line_cap, SNIPPET_SEP};

/// Core snippet assembly: resolves symbols, fetches snippets, enforces budget.
///
/// `symbols_arg` is a newline- or comma-separated list of symbol names.
/// Symbols are processed in order; truncation happens at symbol granularity
/// (no partial symbol output) once the token budget is exhausted.
fn get_snippets_body(store: &SqliteStore, symbols_arg: &str, token_budget: usize) -> String {
    // Read repo_root from meta — written by init_repo_with_progress.
    // Absent on indexes created before this feature; degrade gracefully.
    let repo_root = match store.get_meta("repo_root") {
        Ok(Some(r)) if !r.is_empty() => PathBuf::from(r),
        _ => {
            tracing::warn!("get_snippets: repo_root not in meta — index predates snippet support");
            return "Snippet data unavailable: run `travsr init` to refresh the index.".to_string();
        }
    };

    // Parse symbol list — support both newline and comma as separators so the
    // tool is easy to call after copy-pasting output from get_context / get_callers.
    let symbol_names: Vec<&str> = symbols_arg
        .split(['\n', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if symbol_names.is_empty() {
        return String::new();
    }

    // Resolve each name → Node.  search_nodes_by_name accepts partial matches;
    // skip file-kind nodes (they have no meaningful snippet body).
    let mut resolved: Vec<CoreNode> = Vec::new();
    for name in &symbol_names {
        match store.search_nodes_by_name(name) {
            Ok(nodes) => {
                if let Some(n) = nodes.into_iter().find(|n| n.kind != "file") {
                    resolved.push(n);
                }
            }
            Err(e) => tracing::warn!("get_snippets lookup for '{name}': {e}"),
        }
    }

    if resolved.is_empty() {
        return "No symbols matching the provided names found in the graph.".to_string();
    }

    // Accumulate blocks within budget.  All symbols are equally requested so
    // there is no PPR score to rank by — preserve the caller's order and stop
    // when the budget is exhausted (no knapsack needed here).
    let mut parts: Vec<String> = Vec::new();
    let mut tokens_used: usize = 0;
    let mut n_with_snippet: usize = 0;

    for node in &resolved {
        let header = format!(
            "{} ({}) — {} [package: {}]",
            node.vname.signature, node.kind, node.vname.path, node.package
        );
        // Use skeleton when the body exceeds the kind-aware line cap (truncated
        // snippets are half-implementations — skeleton gives a complete summary).
        let node_height = node
            .end_line
            .unwrap_or_else(|| node.line.unwrap_or(0))
            .saturating_sub(node.line.unwrap_or(0)) as usize;
        let body_text: Option<String> = if node_height > snippet_line_cap(&node.kind) {
            skeleton_for_node_inner(node, &repo_root)
                .map(|s| s.render())
                .or_else(|| snippet_for_node(node, &repo_root))
        } else {
            snippet_for_node(node, &repo_root)
                .or_else(|| skeleton_for_node_inner(node, &repo_root).map(|s| s.render()))
        };

        let snippet_chars = body_text.as_deref().map(str::len).unwrap_or(0);
        let block_cost = (header.len() + snippet_chars) / TOKEN_CHARS_PER_TOKEN + 1;

        if tokens_used + block_cost > token_budget {
            break;
        }
        tokens_used += block_cost;

        let block = if let Some(code) = &body_text {
            n_with_snippet += 1;
            format!("{header}\n{SNIPPET_SEP}\n{code}")
        } else {
            header
        };
        parts.push(block);
    }

    if parts.is_empty() {
        return "Token budget too small to include any symbols.".to_string();
    }

    let body = parts.join("\n\n");
    let sanitized = sanitize_mcp_body_with_limit(
        &body,
        (token_budget * TOKEN_CHARS_PER_TOKEN * 2).min(1_024_000),
    );
    format!(
        "{sanitized}\n\n[{} symbols, {n_with_snippet} with snippets, ~{tokens_used} tokens]",
        parts.len()
    )
}

/// Return tailored code snippets for one or more named symbols.
///
/// Accepts the symbol names returned by `get_context`, `get_callers`, and
/// `search_symbol`. Kind-aware extraction: functions → ≤40 lines, classes →
/// ≤15 lines (header + fields), interfaces/traits/enums → ≤60 lines.
/// Leading docblocks are stripped so the AI sees real code immediately.
pub fn get_snippets(store: &SqliteStore, symbols: &str, token_budget: usize) -> String {
    if let Err(reason) = validate_mcp_arg(symbols) {
        tracing::warn!("get_snippets rejected invalid arg: {reason}");
        return String::new();
    }
    let budget = token_budget.clamp(1, MAX_CONTEXT_BUDGET);
    sanitize_for_mcp(&get_snippets_raw(store, symbols, budget))
}

pub(crate) fn get_snippets_raw(store: &SqliteStore, symbols: &str, token_budget: usize) -> String {
    get_snippets_body(store, symbols, token_budget)
}

/// Global variant: searches across all registered repos (or a named one).
pub fn get_snippets_global(
    repos: &HashMap<String, PathBuf>,
    symbols: &str,
    token_budget: usize,
    repo: Option<&str>,
) -> String {
    if let Err(reason) = validate_mcp_arg(symbols) {
        tracing::warn!("get_snippets_global rejected invalid arg: {reason}");
        return String::new();
    }
    let budget = token_budget.clamp(1, MAX_CONTEXT_BUDGET);
    let raw = collect_global(repos, repo, |store, repo_name, single| {
        let result = get_snippets_raw(store, symbols, budget);
        if result.is_empty() || single {
            result
        } else {
            format!("[{repo_name}]\n{result}")
        }
    });
    sanitize_for_mcp(&raw)
}

#[cfg(test)]
mod snippet_tests {
    use super::*;
    use std::path::Path;
    use travsr_core::VName;

    // ── helper: build a Node with explicit line/end_line ─────────────────────

    fn make_fn_node(path: &str, sig: &str, line: u32, end_line: u32) -> CoreNode {
        CoreNode::new(
            VName::new("corpus", "", path, "typescript", sig),
            "function",
        )
        .with_line(line)
        .with_end_line(end_line)
    }

    // Unit tests for is_comment_line / skip_leading_comments / snippet_line_cap /
    // snippet_for_node have moved to travsr-analysis/src/snippet.rs (RFC-017).

    // ── get_snippets_body (integration) ──────────────────────────────────────

    fn make_store_with_meta(nodes: &[CoreNode], root: &Path) -> SqliteStore {
        let db_path = root.join(".travsr").join("graph.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let mut store = SqliteStore::open(&db_path).unwrap();
        for n in nodes {
            store
                .write_file_graphs_batch(
                    &[travsr_store::FileGraph {
                        nodes: vec![n.clone()],
                        edges: vec![],
                        vname_path: n.vname.path.clone(),
                        new_hash: "deadbeef".to_string(),
                    }],
                    false,
                )
                .unwrap();
        }
        store.set_meta("repo_root", root.to_str().unwrap()).unwrap();
        store
    }

    #[test]
    fn get_snippets_body_returns_snippet_for_known_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("lib.ts");
        std::fs::write(&src, "function hello() {\n  return 'hi';\n}\n").unwrap();

        let node = make_fn_node("lib.ts", "fn:hello", 1, 3);
        let store = make_store_with_meta(&[node], dir.path());

        let result = get_snippets_body(&store, "fn:hello", 2000);
        assert!(result.contains("function hello()"), "snippet body missing");
        assert!(result.contains("return 'hi'"), "snippet content missing");
        assert!(
            result.contains("1 symbols, 1 with snippets"),
            "footer missing"
        );
    }

    #[test]
    fn get_snippets_body_no_repo_root_returns_hint() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph.db");
        let store = SqliteStore::open(&db_path).unwrap();
        // No set_meta("repo_root") — simulates an old index.
        let result = get_snippets_body(&store, "anything", 2000);
        assert!(
            result.contains("travsr init"),
            "must prompt user to re-init: {result}"
        );
    }

    #[test]
    fn get_snippets_body_budget_truncates_in_order() {
        let dir = tempfile::tempdir().unwrap();
        // Two source files
        for (name, body) in [
            ("a.ts", "function aaa() {\n  return 1;\n}\n"),
            ("b.ts", "function bbb() {\n  return 2;\n}\n"),
        ] {
            std::fs::write(dir.path().join(name), body).unwrap();
        }

        let nodes = vec![
            make_fn_node("a.ts", "fn:aaa", 1, 3),
            make_fn_node("b.ts", "fn:bbb", 1, 3),
        ];
        let store = make_store_with_meta(&nodes, dir.path());

        // Tight budget (5 tokens) — neither symbol fits; expect the hint.
        let tight = get_snippets_body(&store, "fn:aaa\nfn:bbb", 5);
        assert!(
            tight.contains("Token budget too small"),
            "5-token budget must return the hint: {tight}"
        );

        // Ordering budget (20 tokens) — enough for one symbol (header ~9 tokens +
        // 3-line snippet ~8 tokens = ~17 tokens) but not both. The first symbol
        // in request order ("aaa") must appear; the second ("bbb") must not.
        let ordered = get_snippets_body(&store, "fn:aaa\nfn:bbb", 20);
        assert!(
            ordered.contains("aaa"),
            "first symbol must be included within budget: {ordered}"
        );
        assert!(
            !ordered.contains("bbb"),
            "second symbol must be excluded when budget exhausted: {ordered}"
        );
    }

    #[test]
    fn get_snippets_body_missing_file_returns_metadata_only() {
        let dir = tempfile::tempdir().unwrap();
        // Node points to a file that doesn't exist on disk.
        let node = make_fn_node("missing.ts", "fn:ghost", 1, 5);
        let store = make_store_with_meta(&[node], dir.path());

        let result = get_snippets_body(&store, "fn:ghost", 2000);
        // Should return metadata line without panic.
        assert!(
            result.contains("fn:ghost"),
            "metadata must appear: {result}"
        );
        // No snippet separator — degraded gracefully.
        assert!(
            !result.contains(SNIPPET_SEP),
            "no snippet separator when file missing: {result}"
        );
        assert!(
            result.contains("0 with snippets"),
            "snippet count must be 0: {result}"
        );
    }

    #[test]
    fn snippet_for_node_inverted_line_range_returns_none_not_panic() {
        // Regression test for the reversed-slice panic: end_line < line must
        // degrade to None instead of panicking with "range start > end".
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("inv.ts");
        std::fs::write(&src, "line1\nline2\nline3\nline4\nline5\n").unwrap();

        // node.line=5, node.end_line=2 → to=2 < from=4 → would panic without guard
        let node = make_fn_node("inv.ts", "fn:inv", 5, 2);
        assert!(
            snippet_for_node(&node, dir.path()).is_none(),
            "inverted line range must return None, not panic"
        );
    }

    #[test]
    fn snippet_for_node_reads_via_canonical_path() {
        // Regression test for the TOCTOU fix: the read must use canon_abs
        // (the resolved path) rather than the pre-canonicalization abs path.
        // On systems where tempdir() returns a symlinked path (e.g. macOS
        // /tmp → /private/tmp), this test would silently pass either way
        // because the symlink is valid. The key invariant we verify: a file
        // that exists and is inside the repo is readable via snippet_for_node.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("real.ts");
        std::fs::write(&src, "function real() {\n  return 42;\n}\n").unwrap();

        let node = make_fn_node("real.ts", "fn:real", 1, 3);
        let snippet = snippet_for_node(&node, dir.path());
        assert!(
            snippet.is_some(),
            "readable repo-internal file must produce a snippet"
        );
        assert!(snippet.unwrap().contains("return 42"));
    }

    // ── get_context include_snippets tests ───────────────────────────────────

    fn make_store_with_root(
        dir: &tempfile::TempDir,
        nodes: &[CoreNode],
    ) -> travsr_store::SqliteStore {
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        store
            .set_meta("repo_root", dir.path().to_str().unwrap())
            .unwrap();
        for n in nodes {
            store.put_node(n).unwrap();
        }
        store
    }

    fn make_fn_node_with_pkg(path: &str, sig: &str, line: u32, end_line: u32) -> CoreNode {
        CoreNode::new(
            VName::new("corpus", "", path, "typescript", sig),
            "function",
        )
        .with_line(line)
        .with_end_line(end_line)
    }

    #[test]
    fn get_context_include_snippets_false_has_no_separator() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("charge.ts");
        std::fs::write(&src, "function charge() {\n  return 1;\n}\n").unwrap();

        let node = make_fn_node_with_pkg("charge.ts", "fn:charge", 1, 3);
        let store = make_store_with_root(&dir, &[node]);

        let without = get_context_body(&store, "charge", 4096, &OpenFilter, false, None);
        let with_snip = get_context_body(&store, "charge", 4096, &OpenFilter, true, None);

        // false path: metadata-only — no separator, footer present
        assert!(without.contains("["), "footer must be present");
        assert!(
            !without.contains(SNIPPET_SEP),
            "no separator in metadata-only output"
        );
        // true path must include the separator — same node, opposite outcome
        assert!(
            with_snip.contains(SNIPPET_SEP),
            "SNIPPET_SEP must appear with include_snippets=true"
        );
    }

    #[test]
    fn get_context_include_snippets_appends_code() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("pay.ts");
        std::fs::write(&src, "function pay() {\n  return 42;\n}\n").unwrap();

        let node = make_fn_node_with_pkg("pay.ts", "fn:pay", 1, 3);
        let store = make_store_with_root(&dir, &[node]);

        let result = get_context_body(&store, "pay", 4096, &OpenFilter, true, None);
        assert!(
            result.contains(SNIPPET_SEP),
            "SNIPPET_SEP must appear when include_snippets=true"
        );
        assert!(result.contains("return 42"), "snippet body must be inlined");
        assert!(
            result.contains("with snippets"),
            "footer must report snippet count"
        );
    }

    #[test]
    fn get_context_include_snippets_shared_budget_truncates() {
        let dir = tempfile::tempdir().unwrap();
        // Write a large file so the snippet alone would bust any small budget.
        let body: String = (0..200).map(|i| format!("  let x{i} = {i};\n")).collect();
        let src = dir.path().join("big.ts");
        std::fs::write(&src, format!("function big() {{\n{body}}}\n")).unwrap();

        let node = make_fn_node_with_pkg("big.ts", "fn:big", 1, 202);
        let store = make_store_with_root(&dir, &[node]);

        // Tiny budget: metadata fits but snippet cannot.
        let result = get_context_body(&store, "big", 10, &OpenFilter, true, None);
        // Must not panic; footer must be present regardless of truncation.
        assert!(result.contains("nodes"), "footer must always be present");
    }

    #[test]
    fn get_context_include_snippets_separate_budget() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("sep.ts");
        std::fs::write(&src, "function sep() {\n  return 7;\n}\n").unwrap();

        let node = make_fn_node_with_pkg("sep.ts", "fn:sep", 1, 3);
        let store = make_store_with_root(&dir, &[node]);

        // Separate snippet_budget of 512; main budget governs node selection only.
        let result = get_context_body(&store, "sep", 4096, &OpenFilter, true, Some(512));
        assert!(
            result.contains("separate"),
            "footer must label separate-budget mode"
        );
        assert!(
            result.contains("return 7"),
            "snippet must be present within separate budget"
        );
    }

    #[test]
    fn get_context_include_snippets_no_repo_root_degrades() {
        // Store with no repo_root meta → must return metadata-only, no panic.
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        let node = make_fn_node_with_pkg("x.ts", "fn:x", 1, 2);
        store.put_node(&node).unwrap();

        let result = get_context_body(&store, "x", 4096, &OpenFilter, true, None);
        // Either "not found" (no FTS match on empty index) or the init hint — never a panic.
        let has_meta_hint =
            result.contains("travsr init") || result.is_empty() || result.contains("No symbols");
        assert!(
            has_meta_hint,
            "absent repo_root must degrade gracefully: {result}"
        );
        assert!(!result.contains("panic"), "must not contain panic text");
    }

    #[test]
    fn get_context_include_snippets_rbac_excluded_node_not_read() {
        // A filter that rejects everything. No disk read must occur.
        struct DenyAll;
        impl travsr_retrieval::EdgeFilter for DenyAll {
            fn allow(
                &self,
                _src: travsr_core::NodeId,
                _dst: travsr_core::NodeId,
                _corpus: Option<&str>,
            ) -> bool {
                false
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("secret.ts");
        std::fs::write(&src, "function secret() { return 99; }\n").unwrap();

        let node = make_fn_node_with_pkg("secret.ts", "fn:secret", 1, 1);
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        store
            .set_meta("repo_root", dir.path().to_str().unwrap())
            .unwrap();
        store.put_node(&node).unwrap();

        let result = get_context_body(&store, "secret", 4096, &DenyAll, true, None);
        // RBAC filter rejects all nodes → "not found" (SEC P0) with no snippet leak.
        assert!(
            !result.contains("return 99"),
            "RBAC-filtered node must never appear in snippet output"
        );
    }

    // ── Edge-relationship annotation tests ──────────────────────────────────

    fn make_store_with_edges(
        nodes: &[CoreNode],
        edges: &[(
            travsr_core::NodeId,
            travsr_core::NodeId,
            travsr_core::EdgeKind,
        )],
    ) -> travsr_store::SqliteStore {
        use travsr_store::Store;
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        for n in nodes {
            store.put_node(n).unwrap();
        }
        for &(src, dst, kind) in edges {
            store
                .put_edge(&travsr_core::Edge::new(src, dst, kind))
                .unwrap();
        }
        store
    }

    #[test]
    fn get_context_annotation_labels_seed() {
        // The node that directly matches the query must be labelled [via: seed].
        let node = make_fn_node_with_pkg("seed.ts", "fn:seed_target", 1, 3);
        let store = make_store_with_edges(&[node], &[]);

        let result = get_context_body(&store, "seed_target", 4096, &OpenFilter, false, None);
        assert!(
            result.contains("[via: seed]"),
            "direct match must be labelled [via: seed]: {result}"
        );
    }

    #[test]
    fn get_context_annotation_labels_caller() {
        // caller_fn has a RefCall edge to seed_fn.
        // get_context(query="seed_fn") → seed_fn=[via:seed], caller_fn=[via:caller].
        // PPR BFS only follows forward edges, so caller_fn must be reachable
        // from seed_fn via a forward path to appear in the output at all.
        // Graph: seed_fn → Depends → bridge_fn → Depends → caller_fn
        //        caller_fn → RefCall → seed_fn
        // PPR surfaces caller_fn (forward path depth 2).
        // Role check finds the reverse RefCall → labels caller_fn [via: caller].
        use travsr_core::EdgeKind;
        use travsr_store::Store;
        let seed_node = make_fn_node_with_pkg("s.ts", "fn:seed_fn", 1, 3);
        let bridge_node = make_fn_node_with_pkg("b.ts", "fn:bridge_fn", 1, 3);
        let caller_node = make_fn_node_with_pkg("c.ts", "fn:caller_fn", 1, 3);
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        let seed_id = store.put_node(&seed_node).unwrap();
        let bridge_id = store.put_node(&bridge_node).unwrap();
        let caller_id = store.put_node(&caller_node).unwrap();
        // Forward path: seed_fn → bridge_fn → caller_fn (PPR discovery)
        store
            .put_edge(&travsr_core::Edge::new(seed_id, bridge_id, EdgeKind::Depends))
            .unwrap();
        store
            .put_edge(&travsr_core::Edge::new(
                bridge_id,
                caller_id,
                EdgeKind::Depends,
            ))
            .unwrap();
        // Reverse semantic edge: caller_fn calls seed_fn → earns [via: caller]
        store
            .put_edge(&travsr_core::Edge::new(
                caller_id,
                seed_id,
                EdgeKind::RefCall,
            ))
            .unwrap();

        let result = get_context_body(&store, "seed_fn", 4096, &OpenFilter, false, None);
        assert!(
            result.contains("[via: seed]"),
            "seed node must be labelled [via: seed]: {result}"
        );
        assert!(
            result.contains("[via: caller]"),
            "caller node must be labelled [via: caller]: {result}"
        );
    }

    #[test]
    fn get_context_annotation_labels_dependency() {
        // seed_fn has a Depends edge to dep_fn.
        // get_context(query="seed_fn") → seed_fn=[via:seed], dep_fn=[via:dependency].
        use travsr_core::EdgeKind;
        use travsr_store::Store;
        let seed_node = make_fn_node_with_pkg("s.ts", "fn:seed_fn2", 1, 3);
        let dep_node = make_fn_node_with_pkg("d.ts", "fn:dep_fn", 1, 3);
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        let seed_id = store.put_node(&seed_node).unwrap();
        let dep_id = store.put_node(&dep_node).unwrap();
        // seed_fn → Depends → dep_fn
        store
            .put_edge(&travsr_core::Edge::new(seed_id, dep_id, EdgeKind::Depends))
            .unwrap();

        let result = get_context_body(&store, "seed_fn2", 4096, &OpenFilter, false, None);
        assert!(
            result.contains("[via: seed]"),
            "seed node must be labelled [via: seed]: {result}"
        );
        assert!(
            result.contains("[via: dependency]"),
            "dep node must be labelled [via: dependency]: {result}"
        );
    }

    #[test]
    fn get_context_annotation_labels_context() {
        // A node with no edge to/from the seed is labelled [via: context].
        use travsr_store::Store;
        let seed_node = make_fn_node_with_pkg("s.ts", "fn:seed_only", 1, 3);
        let ctx_node = make_fn_node_with_pkg("u.ts", "fn:unrelated_fn", 1, 3);
        let mut store = travsr_store::SqliteStore::open_in_memory().unwrap();
        store.put_node(&seed_node).unwrap();
        store.put_node(&ctx_node).unwrap();
        // No edges between them — ctx_node is reachable only via PPR structural walk.

        // Search for "seed_only" — ctx_node should appear as [via: context] if PPR
        // surfaces it, or just not appear. Either way, if it does appear it must NOT
        // carry seed/caller/dependency labels.
        let result = get_context_body(&store, "seed_only", 4096, &OpenFilter, false, None);
        assert!(
            !result.contains("unrelated_fn") || result.contains("[via: context]"),
            "unrelated node must be [via: context] if it appears at all: {result}"
        );
    }
}
