//! travsr-indexer — Tree-sitter + LSIF parsing pipeline.
//!
//! Turns source files on disk into `travsr-core::Node` / `Edge` records.
//! Sprint 1 supports TypeScript and TSX via tree-sitter.
//! Sprint 2 adds SHA-256 file hashing and repo-relative VName paths.

#![forbid(unsafe_code)]

mod emit;
mod hash;
pub mod lsif;
mod python;
pub mod ra_runner;
pub mod runner;
mod rust;
pub mod sandbox;
mod typescript;

use std::path::Path;

use travsr_core::{EdgeKind, Language};

pub use hash::hash_file;
pub use lsif::ingest as ingest_lsif;
pub use lsif::{ingest_rust, ingest_rust_raw};
pub use ra_runner::run_ra_lsif;
pub use runner::run_lsif_emitter;
pub use travsr_core::{Edge, Node};
pub use travsr_error::IndexError;

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
/// `corpus` must match the corpus used when the file nodes were created so
/// that the target `NodeId`s resolve correctly (ARCH-102).
///
/// Call this after [`Indexer::parse_file_with_vname`] and persist the
/// returned edges alongside the parse output.
pub fn link_imports(nodes: &[Node], vname_path: &str, corpus: &str) -> Vec<Edge> {
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
            let target = emit::file_node(corpus, &candidate_str);
            edges.push(emit::resolves_to_edge(node.id, target.id));
        }
    }

    edges
}

/// Resolve within-crate Rust `use` paths and file-module declarations to
/// `resolves-to` edges.
///
/// For `use self::foo::bar` / `use super::baz`, emits:
/// ```text
/// use:self::foo::bar  --[resolves-to]-->  file:dir/foo/bar.rs
/// use:self::foo::bar  --[resolves-to]-->  file:dir/foo/bar/mod.rs
/// ```
///
/// For `filemod:foo` (from `mod foo;` declarations), emits:
/// ```text
/// filemod:foo  --[resolves-to]-->  file:dir/foo.rs
/// filemod:foo  --[resolves-to]-->  file:dir/foo/mod.rs
/// ```
///
/// Both candidate paths are always emitted; only the one whose `file:` node was
/// actually indexed will be reachable during graph traversal.
///
/// External crate paths (e.g. `use std::fmt`, `use tokio::Runtime`) and
/// `crate::*` paths are silently skipped — cross-crate resolution is provided
/// by LSIF in Sprint 9. Wildcard imports (`use foo::*`) are also skipped.
///
/// `corpus` must match the corpus used when file nodes were indexed (ARCH-102).
///
/// Call this after [`Indexer::parse_file_with_vname`] for `.rs` files and
/// persist the returned edges alongside the parse output.
pub fn link_imports_rust(nodes: &[Node], vname_path: &str, corpus: &str) -> Vec<Edge> {
    let dir = match Path::new(vname_path).parent() {
        Some(p) => p,
        None => return Vec::new(),
    };

    let mut edges = Vec::new();

    for node in nodes {
        match node.kind.as_str() {
            "import" => {
                let Some(path) = node.vname.signature.strip_prefix("use:") else {
                    continue;
                };
                // Wildcards (`use foo::*`) cannot resolve to a specific file.
                if path.ends_with("::*") || path == "*" {
                    continue;
                }
                if let Some(base) = resolve_rust_module_path(path, dir) {
                    for candidate in rust_candidate_paths(&base) {
                        let target_id = rust_file_node_id(corpus, &candidate);
                        edges.push(Edge::new(node.id, target_id, EdgeKind::ResolvesTo));
                    }
                }
            }
            "file-module" => {
                // `mod foo;` — resolve to foo.rs / foo/mod.rs relative to the
                // declaring file's directory.
                let Some(name) = node.vname.signature.strip_prefix("filemod:") else {
                    continue;
                };
                let base = normalize_vname_path(&dir.join(name));
                for candidate in rust_candidate_paths(&base) {
                    let target_id = rust_file_node_id(corpus, &candidate);
                    edges.push(Edge::new(node.id, target_id, EdgeKind::ResolvesTo));
                }
            }
            _ => {}
        }
    }

    edges
}

/// Resolve a within-crate Rust module path to a base filesystem path (no
/// extension). Returns `None` for external crate references and `crate::*`
/// (cross-crate resolution deferred to Sprint 9 LSIF).
///
/// Handles:
/// - `self::foo::bar`           → `{dir}/foo/bar`
/// - `super::foo`               → `{parent_dir}/foo`
/// - `super::super::foo`        → `{grandparent_dir}/foo`
fn resolve_rust_module_path(use_path: &str, dir: &Path) -> Option<std::path::PathBuf> {
    if let Some(rest) = use_path.strip_prefix("self::") {
        let segments: std::path::PathBuf = rest.split("::").collect();
        return Some(normalize_vname_path(&dir.join(segments)));
    }
    if use_path.starts_with("super::") {
        let mut base = dir.to_path_buf();
        let mut rest = use_path;
        while let Some(stripped) = rest.strip_prefix("super::") {
            base = base.parent().map(Path::to_path_buf)?;
            rest = stripped;
        }
        if rest.is_empty() {
            return None;
        }
        let segments: std::path::PathBuf = rest.split("::").collect();
        return Some(normalize_vname_path(&base.join(segments)));
    }
    // `crate::*` and bare external identifiers: skip.
    // DEBT(travsr-indexer): crate:: resolution requires the crate root path,
    // which is not available from vname_path alone. Deferred to Sprint 9 LSIF.
    None
}

/// Returns the two candidate repo-relative file paths for a Rust module:
/// `{base}.rs` (flat file) and `{base}/mod.rs` (directory module).
fn rust_candidate_paths(base: &std::path::Path) -> [String; 2] {
    let flat = base
        .with_extension("rs")
        .to_string_lossy()
        .replace('\\', "/");
    let dir_mod = base.join("mod.rs").to_string_lossy().replace('\\', "/");
    [flat, dir_mod]
}

/// NodeId for a Rust `file:` node — must match the VName emitted by
/// `rust_file_node()` in rust.rs (language="rust", root="", signature="file").
/// If either side changes, the ResolvesTo edges produced here will point to
/// the wrong NodeId and traversal will silently miss cross-file links.
fn rust_file_node_id(corpus: &str, path: &str) -> travsr_core::NodeId {
    travsr_core::VName::new(corpus, "", path, "rust", "file").id()
}

/// Resolve Python import statements in `nodes` to `ResolvesTo` edges.
///
/// For each `import` node, converts the signature specifier to one or two
/// candidate repo-relative file paths and emits `ResolvesTo` edges:
/// ```text
/// import:os         --[resolves-to]-->  os.py
/// import:os         --[resolves-to]-->  os/__init__.py
/// import:.utils     --[resolves-to]-->  {dir}/utils.py          (relative, level 1)
/// import:..pkg      --[resolves-to]-->  {parent_dir}/pkg.py     (relative, level 2)
/// import:.          --[resolves-to]-->  {dir}/__init__.py        (bare dot)
/// ```
///
/// Both `.py` and `/__init__.py` candidates are always emitted; only the one
/// whose `file:` node was indexed will be reachable in the graph. Third-party
/// imports (stdlib, site-packages) produce dangling edges that are harmless —
/// use [`link_imports_python_fs`] to filter them out when `repo_root` is available.
///
/// `corpus` must match the corpus used when file nodes were indexed (ARCH-102).
pub fn link_imports_python(nodes: &[Node], vname_path: &str, corpus: &str) -> Vec<Edge> {
    let dir = match Path::new(vname_path).parent() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let mut edges = Vec::new();
    for node in nodes {
        if node.kind != "import" {
            continue;
        }
        let Some(spec) = node.vname.signature.strip_prefix("import:") else {
            continue;
        };
        for candidate in py_resolve_import_candidates(spec, dir) {
            edges.push(Edge::new(
                node.id,
                py_file_node_id(corpus, &candidate),
                EdgeKind::ResolvesTo,
            ));
        }
    }
    edges
}

/// Filesystem-aware variant of [`link_imports_python`] that filters out edges
/// whose target file does not exist under `repo_root`.
///
/// This eliminates dangling edges for stdlib and third-party imports without
/// inspecting site-packages. The heuristic: if neither `{repo_root}/{mod}.py`
/// nor `{repo_root}/{mod}/__init__.py` exists on disk, the import is external.
///
/// `repo_root` is always available in `travsr-daemon`; use this variant there.
/// Use the pure [`link_imports_python`] in tests or when FS access is undesirable.
pub fn link_imports_python_fs(
    nodes: &[Node],
    vname_path: &str,
    corpus: &str,
    repo_root: &std::path::Path,
) -> Vec<Edge> {
    let dir = match Path::new(vname_path).parent() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let mut edges = Vec::new();
    for node in nodes {
        if node.kind != "import" {
            continue;
        }
        let Some(spec) = node.vname.signature.strip_prefix("import:") else {
            continue;
        };
        for candidate in py_resolve_import_candidates(spec, dir) {
            if repo_root.join(&candidate).exists() {
                edges.push(Edge::new(
                    node.id,
                    py_file_node_id(corpus, &candidate),
                    EdgeKind::ResolvesTo,
                ));
            }
        }
    }
    edges
}

/// Compute candidate repo-relative file paths for a Python import specifier.
///
/// Absolute (`"os.path"`) → `["os/path.py", "os/path/__init__.py"]`
/// Relative (`".utils"`)  → `["{dir}/utils.py", "{dir}/utils/__init__.py"]`
/// Bare dot (`"."`)       → `["{dir}/__init__.py"]`
fn py_resolve_import_candidates(spec: &str, dir: &std::path::Path) -> Vec<String> {
    if spec.starts_with('.') {
        py_resolve_relative_candidates(spec, dir)
    } else {
        py_resolve_absolute_candidates(spec)
    }
}

fn py_resolve_absolute_candidates(spec: &str) -> Vec<String> {
    let base: std::path::PathBuf = spec.split('.').collect();
    let flat = base
        .with_extension("py")
        .to_string_lossy()
        .replace('\\', "/");
    let pkg = base
        .join("__init__.py")
        .to_string_lossy()
        .replace('\\', "/");
    vec![flat, pkg]
}

fn py_resolve_relative_candidates(spec: &str, dir: &std::path::Path) -> Vec<String> {
    let level = spec.chars().take_while(|&c| c == '.').count();
    let module_part = &spec[level..];

    // Level 1 = same package (dir), level 2 = parent, etc.
    let mut anchor = dir.to_path_buf();
    for _ in 0..level.saturating_sub(1) {
        anchor = match anchor.parent() {
            Some(p) => p.to_path_buf(),
            None => return Vec::new(),
        };
    }

    if module_part.is_empty() {
        // `from . import X` → the package __init__.py
        let pkg = normalize_vname_path(&anchor.join("__init__.py"))
            .to_string_lossy()
            .replace('\\', "/");
        vec![pkg]
    } else {
        let segments: std::path::PathBuf = module_part.split('.').collect();
        let base = normalize_vname_path(&anchor.join(segments));
        let flat = base
            .with_extension("py")
            .to_string_lossy()
            .replace('\\', "/");
        let pkg = base
            .join("__init__.py")
            .to_string_lossy()
            .replace('\\', "/");
        vec![flat, pkg]
    }
}

/// NodeId for a Python `file:` node — must match the VName emitted by
/// `python::py_file_node` (language="python", root="", signature="file").
/// If either side changes, ResolvesTo edges produced here will point to the
/// wrong NodeId and traversal will silently miss cross-file links.
fn py_file_node_id(corpus: &str, path: &str) -> travsr_core::NodeId {
    travsr_core::VName::new(corpus, "", path, "python", "file").id()
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
///
/// The `corpus` field is the canonical repo identifier (ARCH-102) baked into
/// every `VName` this indexer produces. Use [`Indexer::with_corpus`] to set it;
/// [`Indexer::new`] defaults to an empty string for backward compatibility.
#[derive(Debug, Default)]
pub struct Indexer {
    corpus: String,
}

impl Indexer {
    /// Create an indexer with an empty corpus (backward-compatible default).
    pub fn new() -> Self {
        Self {
            corpus: String::new(),
        }
    }

    /// Create an indexer stamped with the given canonical corpus identifier.
    /// See `travsr_core::canonical_corpus` and `docs/rfcs/ARCH-102`.
    pub fn with_corpus(corpus: impl Into<String>) -> Self {
        Self {
            corpus: corpus.into(),
        }
    }

    /// Parse a single source file into nodes and edges.
    ///
    /// Uses the file's own path string as the VName path. Callers that need
    /// repo-relative VName paths (e.g. the daemon) should use
    /// [`parse_file_with_vname`] instead.
    pub fn parse_file(&self, path: &Path) -> Result<ParseOutput, IndexError> {
        let vname_path = path.to_string_lossy().replace('\\', "/");
        self.parse_file_with_vname(path, &vname_path)
    }

    /// Parse `abs_path` using `vname_path` as the stable, repo-relative path
    /// stored in every emitted VName (closes DEBT-012).
    pub fn parse_file_with_vname(
        &self,
        abs_path: &Path,
        vname_path: &str,
    ) -> Result<ParseOutput, IndexError> {
        let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let map_err = |e: anyhow::Error| IndexError::Parse {
            file: abs_path.to_string_lossy().into_owned(),
            message: e.to_string(),
        };
        match Language::from_extension(ext) {
            Some(Language::TypeScript) => {
                typescript::parse(&self.corpus, abs_path, vname_path).map_err(map_err)
            }
            Some(Language::Rust) => {
                rust::parse(&self.corpus, abs_path, vname_path).map_err(map_err)
            }
            Some(Language::Python) => {
                python::parse(&self.corpus, abs_path, vname_path).map_err(map_err)
            }
            // Other future languages (#[non_exhaustive]) are silently skipped
            // until their parsers ship.
            _ => Ok(ParseOutput::default()),
        }
    }
}
