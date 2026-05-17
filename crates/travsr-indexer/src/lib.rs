//! travsr-indexer — Tree-sitter + LSIF parsing pipeline.
//!
//! Turns source files on disk into `travsr-core::Node` / `Edge` records.
//! Sprint 1 supports TypeScript and TSX via tree-sitter.
//! Sprint 2 adds SHA-256 file hashing and repo-relative VName paths.

#![forbid(unsafe_code)]

mod emit;
mod hash;
pub mod lsif;
pub mod runner;
mod typescript;

use std::path::Path;

pub use hash::hash_file;
pub use lsif::ingest as ingest_lsif;
pub use runner::run_lsif_emitter;
pub use travsr_core::{Edge, Node};

/// Resolve relative imports in `nodes` to `resolves-to` edges.
///
/// For each import node whose module specifier starts with `./` or `../`,
/// computes the target file's repo-relative vname path and emits an edge:
///   `import:./foo  --[resolves-to]-->  file:foo.ts`
///
/// Both `.ts` and `.tsx` candidates are emitted; only the one whose file node
/// was actually indexed will be reachable during graph traversal. Package
/// imports (e.g. `"vscode"`) are silently skipped.
///
/// Call this after [`Indexer::parse_file_with_vname`] and persist the
/// returned edges alongside the parse output.
pub fn link_imports(nodes: &[Node], vname_path: &str) -> Vec<Edge> {
    let parent = match std::path::Path::new(vname_path).parent() {
        Some(p) => p,
        None => return Vec::new(),
    };

    let mut edges = Vec::new();

    for node in nodes {
        if node.kind != "import" {
            continue;
        }
        let Some(module) = node.vname.signature.strip_prefix("import:") else {
            continue;
        };
        if !module.starts_with("./") && !module.starts_with("../") {
            continue;
        }

        let raw = parent.join(module);
        let normalized = normalize_vname_path(&raw);

        for ext in ["ts", "tsx"] {
            let candidate = normalized.with_extension(ext);
            let candidate_str = candidate.to_string_lossy().replace('\\', "/");
            let target = emit::file_node(&candidate_str);
            edges.push(emit::resolves_to_edge(node.id, target.id));
        }
    }

    edges
}

/// Normalize a logical path by resolving `.` and `..` components without
/// touching the filesystem. Returns a relative path with no redundant segments.
fn normalize_vname_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(s) => parts.push(s.to_owned()),
            Component::ParentDir => {
                parts.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    parts.iter().collect()
}

/// All graph records produced by parsing a single source file.
#[derive(Debug, Default)]
pub struct ParseOutput {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// Streaming indexer that walks a repository and emits graph records.
#[derive(Debug, Default)]
pub struct Indexer;

impl Indexer {
    pub fn new() -> Self {
        Self
    }

    /// Parse a single source file into nodes and edges.
    ///
    /// Uses the file's own path string as the VName path. Callers that need
    /// repo-relative VName paths (e.g. the daemon) should use
    /// [`parse_file_with_vname`] instead.
    pub fn parse_file(&self, path: &Path) -> anyhow::Result<ParseOutput> {
        let vname_path = path.to_string_lossy().replace('\\', "/");
        self.parse_file_with_vname(path, &vname_path)
    }

    /// Parse `abs_path` using `vname_path` as the stable, repo-relative path
    /// stored in every emitted VName (closes DEBT-012).
    pub fn parse_file_with_vname(
        &self,
        abs_path: &Path,
        vname_path: &str,
    ) -> anyhow::Result<ParseOutput> {
        match abs_path.extension().and_then(|e| e.to_str()) {
            Some("ts" | "tsx") => typescript::parse(abs_path, vname_path),
            _ => Ok(ParseOutput::default()),
        }
    }
}
