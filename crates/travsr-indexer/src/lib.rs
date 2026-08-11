//! travsr-indexer — Tree-sitter + LSIF parsing pipeline.
//!
//! Turns source files on disk into `travsr-core::Node` / `Edge` records.
//! Sprint 1 supports TypeScript and TSX via tree-sitter.
//! Sprint 2 adds SHA-256 file hashing and repo-relative VName paths.

#![forbid(unsafe_code)]

// Parser modules now live in travsr-analysis (RFC-017).
// Re-export the bridge modules so callers (travsr-daemon) still compile.
pub use travsr_analysis::phase_b_dart;
pub use travsr_analysis::phase_b_python;
pub use travsr_analysis::phase_b_rust;
pub use travsr_analysis::phase_b_typescript;

pub mod callsite;
pub mod ffi; // thin re-export wrapper → travsr_analysis::ffi
mod ffi_resolver;
mod hash;
pub mod lsif;
pub mod python_lsif;
pub mod ra_runner;
pub mod runner;
pub mod sandbox;
pub mod scip_unifier;

use std::path::Path;

use travsr_core::{EdgeKind, Language};

// ParseOutput and FfiMarker are now owned by travsr-analysis.
pub use ffi::{FfiMarker, FfiMarkerKind};
pub use ffi_resolver::FfiConfig;
pub use hash::hash_file;
pub use lsif::ingest as ingest_lsif;
pub use lsif::{
    ingest_g2 as ingest_lsif_g2, ingest_rust, ingest_rust_positional, ingest_rust_raw, ingest_scip,
    LsifG2Output,
};
pub use ra_runner::run_ra_lsif;
pub use runner::{run_lsif_emitter, run_lsif_py_emitter, run_scip_python};
pub use travsr_analysis::ParseOutput;
pub use travsr_core::{Edge, Node};
pub use travsr_error::IndexError;

/// Public bridge for travsr-plugin-host wrappers (RFC-011 P5-S2).
pub fn typescript_parse(
    corpus: &str,
    path: &std::path::Path,
    vname_path: &str,
) -> anyhow::Result<ParseOutput> {
    travsr_analysis::typescript::parse(corpus, path, vname_path)
}
pub fn rust_parse(
    corpus: &str,
    path: &std::path::Path,
    vname_path: &str,
) -> anyhow::Result<ParseOutput> {
    travsr_analysis::rust::parse(corpus, path, vname_path)
}
pub fn python_parse(
    corpus: &str,
    path: &std::path::Path,
    vname_path: &str,
) -> anyhow::Result<ParseOutput> {
    travsr_analysis::python::parse(corpus, path, vname_path)
}
pub fn go_parse(
    corpus: &str,
    path: &std::path::Path,
    vname_path: &str,
) -> anyhow::Result<ParseOutput> {
    travsr_analysis::go::parse(corpus, path, vname_path)
}

/// Native Phase B for Dart: calls travsr-dart-index-emitter directly.
/// Bypasses the travsr-lang-dart sidecar to avoid a Dart AOT SIGABRT that
/// occurs when the emitter is a nested subprocess of the sandboxed sidecar.
pub fn phase_b_native_dart(
    corpus: &str,
    root: &std::path::Path,
) -> anyhow::Result<(
    Vec<travsr_core::Node>,
    Vec<travsr_core::Edge>,
    Vec<travsr_core::ScipRef>,
)> {
    phase_b_dart::extract_native_phase_b(corpus, root)
}

/// Native Phase B for Rust: Cargo.toml dep graph + tree-sitter call edges.
/// Zero external-tool downloads. LSIF enrichment is merged by the caller.
///
/// Returns `(nodes, edges, unresolved_calls)`. Cross-crate bare calls are in
/// `unresolved_calls`; the daemon resolves them using Phase A store nodes.
///
/// `files`: pre-walked `(abs_path, vname_path)` pairs (P6 — #329).
/// Pass `None` to fall back to a local directory walk.
pub fn phase_b_native_rust(
    corpus: &str,
    root: &std::path::Path,
    files: Option<&[(std::path::PathBuf, String)]>,
) -> anyhow::Result<phase_b_rust::NativePhaseB> {
    phase_b_rust::extract_native_phase_b(corpus, root, files)
}

/// Native Phase B for TypeScript: tree-sitter call + inheritance edges.
/// Zero external-tool downloads. LSIF enrichment is merged by the caller.
///
/// Returns `(nodes, edges, unresolved_calls)`. E4: call sites are `UnresolvedCall`s
/// (receiver type recovered where possible) resolved fail-closed by the daemon
/// against Phase A store nodes — no same-file leaf guesses that dangle cross-file.
///
/// `files`: pre-walked `(abs_path, vname_path)` pairs (P6 — #329).
/// Pass `None` to fall back to a local directory walk.
pub fn phase_b_native_typescript(
    corpus: &str,
    root: &std::path::Path,
    files: Option<&[(std::path::PathBuf, String)]>,
) -> anyhow::Result<(
    Vec<travsr_core::Node>,
    Vec<travsr_core::Edge>,
    Vec<travsr_core::UnresolvedCall>,
)> {
    phase_b_typescript::extract_native_phase_b(corpus, root, files)
}

/// Native Phase B for Python: tree-sitter call + inheritance edges.
/// Zero external-tool downloads. LSIF enrichment is merged by the caller.
///
/// Returns `(nodes, edges, unresolved_calls)`. E4: call sites are `UnresolvedCall`s
/// (receiver type recovered where possible) resolved fail-closed by the daemon
/// against Phase A store nodes — no same-file leaf guesses that dangle cross-file.
///
/// `files`: pre-walked `(abs_path, vname_path)` pairs (P6 — #329).
/// Pass `None` to fall back to a local directory walk.
pub fn phase_b_native_python(
    corpus: &str,
    root: &std::path::Path,
    files: Option<&[(std::path::PathBuf, String)]>,
) -> anyhow::Result<(
    Vec<travsr_core::Node>,
    Vec<travsr_core::Edge>,
    Vec<travsr_core::UnresolvedCall>,
)> {
    phase_b_python::extract_native_phase_b(corpus, root, files)
}

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

        // #610: only `ts`/`tsx` used to be tried, so no JavaScript import ever
        // resolved — `./animal` from a `.js` file produced candidates for
        // `animal.ts` and `animal.tsx`, neither of which exists in a JS
        // project. That broke JS dependency traversal for *both* module styles,
        // not only CommonJS.
        //
        // Candidates are scoped by the importer's own extension rather than
        // emitting the whole family for everyone: each one is a speculative
        // edge whose target node may never be created, and `fsck` counts those
        // as orphans. A TypeScript file still gets `js` too, since `allowJs`
        // interop is common in real projects.
        let importer_ext = std::path::Path::new(vname_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let candidates: &[&str] = match importer_ext {
            "js" | "jsx" | "mjs" | "cjs" => &["js", "jsx", "mjs", "cjs"],
            _ => &["ts", "tsx", "js"],
        };

        for ext in candidates.iter().copied() {
            let candidate = normalized.with_extension(ext);
            let candidate_str = candidate.to_string_lossy().replace('\\', "/");
            let target = travsr_analysis::emit::file_node(corpus, &candidate_str);
            edges.push(travsr_analysis::emit::resolves_to_edge(node.id, target.id));
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

/// Resolve Go import paths in `nodes` to `ResolvesTo` edges.
///
/// Go import paths are module-qualified (e.g. `"github.com/foo/bar"`). Without
/// a full module graph, we cannot reliably map them to local file paths — that
/// is deferred to Phase 4 (Go LSIF). This function handles only the two
/// resolvable cases:
///
/// 1. **Same-module imports** — when `module_root` is `Some((module_path, repo_root))`,
///    an import whose path starts with `module_path` is mapped to a local
///    directory:
///    ```text
///    import:github.com/foo/bar/pkg/util
///      --[resolves-to]-->  pkg/util/{any}.go   (represented as pkg/util/ dir node)
///    ```
///    Actually we emit two candidate `file:` edges using `{rel_dir}` ↔ no single
///    file is canonical, so we emit a ResolvesTo edge to every `*.go` file node
///    we have indexed under that directory — this is approximated as an edge to
///    a synthetic `file:` node whose path is `{rel_dir}` (a dir placeholder that
///    never matches a real indexed node but is harmless).
///
/// 2. **Relative imports** — Go does not support relative imports (`./foo`) in
///    module mode; this case does not arise in practice.
///
/// `cgo` pseudo-package (`import:C`) is always skipped — it was already filtered
/// by the parser.
///
/// **DEBT-018:** Full cross-module resolution requires go.mod parsing and is
/// deferred to Phase 4 Go LSIF sprint.
///
/// `corpus` must match the corpus used when file nodes were indexed (ARCH-102).
pub fn link_imports_go(
    nodes: &[Node],
    _vname_path: &str,
    corpus: &str,
    module_root: Option<(&str, &str)>,
) -> Vec<Edge> {
    let mut edges = Vec::new();

    for node in nodes {
        if node.kind != "import" {
            continue;
        }
        let Some(import_path) = node.vname.signature.strip_prefix("import:") else {
            continue;
        };
        // Skip cgo (already filtered by parser, but defensive).
        if import_path == "C" {
            continue;
        }
        // If we have a module root, attempt same-module resolution.
        if let Some((module_path, _repo_root)) = module_root {
            if let Some(rel) = import_path.strip_prefix(module_path) {
                // Strip leading slash separator.
                let rel = rel.trim_start_matches('/');
                if !rel.is_empty() {
                    // Emit a ResolvesTo edge to a synthetic directory placeholder.
                    // The daemon can use this to scope BFS traversal to all .go
                    // files under the directory once LSIF is wired (Phase 4).
                    let target_id = travsr_core::VName::new(corpus, "", rel, "go", "file").id();
                    edges.push(Edge::new(node.id, target_id, EdgeKind::ResolvesTo));
                }
            }
        }
        // stdlib and third-party imports produce no edges — dangling refs would
        // be harmless but add noise. Phase 4 LSIF provides semantic resolution.
    }

    edges
}

/// Emit intra-package co-file `Depends` edges for Go.
///
/// In Go, files within the same package share the namespace without import
/// statements. Each file parsed by `go::parse` emits a `go-pkg` node whose
/// VName path is the parent directory — so every file in `strategies/` produces
/// the same `go-pkg` NodeId regardless of which file is being parsed.
///
/// This function groups `file` nodes that share a `go-pkg` node (via
/// `DefinesBinding` edges) and emits `file_B --Depends--> file_A` for every
/// ordered pair (B ≠ A). Blast-radius Phase 1 follows these `Depends` edges
/// during reverse BFS, so all co-package siblings appear in the affected set.
///
/// Call this once, after ALL Go files in the repo have been parsed, so that
/// the complete package group is visible.
///
/// # DEBT-024
/// Incremental reindex (`reindex_files`) does not call this function — the same
/// limitation as FFI resolution. A full `travsr init` is required after adding
/// or removing a `.go` file from a package to refresh co-package edges.
///
/// # Complexity
/// O(N + E) where N = nodes, E = edges in the slice.
pub fn link_go_copackage_edges(nodes: &[Node], edges: &[Edge]) -> Vec<Edge> {
    use std::collections::HashMap;

    // Index node kind by NodeId — O(N).
    let kind_of: HashMap<travsr_core::NodeId, &str> =
        nodes.iter().map(|n| (n.id, n.kind.as_str())).collect();

    // Build pkg_node_id → Vec<file_node_id> from DefinesBinding edges — O(E).
    let mut pkg_to_files: HashMap<travsr_core::NodeId, Vec<travsr_core::NodeId>> = HashMap::new();
    for edge in edges {
        if edge.kind != EdgeKind::DefinesBinding {
            continue;
        }
        if kind_of.get(&edge.src).copied() != Some("file") {
            continue;
        }
        if kind_of.get(&edge.dst).copied() != Some("go-pkg") {
            continue;
        }
        pkg_to_files.entry(edge.dst).or_default().push(edge.src);
    }

    // Emit all ordered pairs for groups with ≥2 files — O(F²) where F = files per pkg.
    // F is typically small (< 30 for realistic packages), so the quadratic factor is fine.
    let mut result: Vec<Edge> = Vec::new();
    for file_ids in pkg_to_files.values() {
        if file_ids.len() < 2 {
            continue;
        }
        for &b in file_ids {
            for &a in file_ids {
                if a != b {
                    result.push(Edge::new(b, a, EdgeKind::Depends));
                }
            }
        }
    }
    result
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

/// Streaming indexer that walks a repository and emits graph records.
///
/// The `corpus` field is the canonical repo identifier (ARCH-102) baked into
/// every `VName` this indexer produces. Use [`Indexer::with_corpus`] to set it;
/// [`Indexer::new`] defaults to an empty string for backward compatibility.
#[derive(Debug, Default)]
pub struct Indexer {
    corpus: String,
    ffi_config: ffi_resolver::FfiConfig,
    /// Extra `docs.exclude` path-substring patterns (#376 §3.3), additive to
    /// `markdown`'s built-in default exclusion list. Empty unless a caller
    /// opts in via [`Indexer::with_doc_excludes`] — travsr-indexer has no
    /// dependency on travsr-config (see CLAUDE.md's crate dependency rules),
    /// so resolving the config layer is the caller's job; this is just where
    /// the resolved value is threaded through to the markdown parser.
    doc_excludes: Vec<String>,
}

impl Indexer {
    /// Create an indexer with an empty corpus (backward-compatible default).
    pub fn new() -> Self {
        Self {
            corpus: String::new(),
            ffi_config: ffi_resolver::FfiConfig::default(),
            doc_excludes: Vec::new(),
        }
    }

    /// Create an indexer stamped with the given canonical corpus identifier.
    /// See `travsr_core::canonical_corpus` and `docs/rfcs/ARCH-102`.
    pub fn with_corpus(corpus: impl Into<String>) -> Self {
        Self {
            corpus: corpus.into(),
            ffi_config: ffi_resolver::FfiConfig::default(),
            doc_excludes: Vec::new(),
        }
    }

    /// Override the FFI resolver configuration (RFC-005).
    ///
    /// Call this after [`Indexer::with_corpus`] (or on a value from [`Indexer::new`])
    /// to control the emit threshold, enable/disable FFI resolution, or adjust
    /// the pyright timeout.
    pub fn with_ffi_config(mut self, cfg: ffi_resolver::FfiConfig) -> Self {
        self.ffi_config = cfg;
        self
    }

    /// Add extra path-substring patterns (`docs.exclude`, #376 §3.3) to the
    /// markdown chunker's built-in exclusion list. Case-insensitive substring
    /// match against the repo-relative path, same semantics as the built-in
    /// patterns in `travsr_analysis::markdown`.
    pub fn with_doc_excludes(mut self, patterns: Vec<String>) -> Self {
        self.doc_excludes = patterns;
        self
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
    ///
    /// FFI markers are collected into `ParseOutput.ffi_markers` but NOT resolved
    /// here — resolution requires markers from multiple files. Call
    /// [`Indexer::resolve_ffi_edges`] after parsing all files in the batch.
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
        let mut output = match Language::from_extension(ext) {
            Some(Language::TypeScript) => {
                travsr_analysis::typescript::parse(&self.corpus, abs_path, vname_path)
                    .map_err(map_err)?
            }
            Some(Language::Rust) => {
                travsr_analysis::rust::parse(&self.corpus, abs_path, vname_path).map_err(map_err)?
            }
            Some(Language::Python) => {
                let mut ts_out = travsr_analysis::python::parse(&self.corpus, abs_path, vname_path)
                    .map_err(map_err)?;
                // Best-effort semantic enrichment via pyright (RFC-005 §3).
                // Runs after tree-sitter; failures are logged and silently ignored.
                let pyright_out = python_lsif::parse_python_with_pyright(
                    abs_path,
                    std::time::Duration::from_secs(self.ffi_config.pyright_timeout_secs),
                )
                .unwrap_or_default();
                ts_out.merge_deduped(pyright_out);
                ts_out
            }
            Some(Language::Go) => {
                travsr_analysis::go::parse(&self.corpus, abs_path, vname_path).map_err(map_err)?
            }
            Some(Language::Json | Language::Yaml | Language::Toml | Language::Xml) => {
                travsr_analysis::data_format::parse(&self.corpus, abs_path, vname_path)
                    .map_err(map_err)?
            }
            Some(Language::Markdown) => travsr_analysis::markdown::parse(
                &self.corpus,
                abs_path,
                vname_path,
                &self.doc_excludes,
            )
            .map_err(map_err)?,
            // Other future languages (#[non_exhaustive]) are silently skipped
            // until their parsers ship.
            _ => ParseOutput::default(),
        };

        // Collect per-language FFI call-site markers (RFC-005 §3).
        match Language::from_extension(ext) {
            Some(Language::TypeScript) => {
                let napi_markers = travsr_analysis::typescript::collect_napi_dts_markers(
                    &self.corpus,
                    abs_path,
                    vname_path,
                    &output.nodes,
                );
                output.ffi_markers.extend(napi_markers);
            }
            Some(Language::Python) => {
                let pyo3_markers = travsr_analysis::python::collect_pyo3_pyi_markers(
                    &self.corpus,
                    abs_path,
                    vname_path,
                    &output.nodes,
                );
                output.ffi_markers.extend(pyo3_markers);
            }
            _ => {}
        }

        Ok(output)
    }

    /// Resolve cross-language FFI edges from markers accumulated across multiple files.
    ///
    /// This is the correct entry point for RFC-005 resolution: call this after
    /// parsing all files in a batch so markers from both sides of each FFI boundary
    /// are present (e.g. Rust `NapiExport` and TypeScript `NapiCall`).
    pub fn resolve_ffi_edges(&self, markers: &[crate::ffi::FfiMarker]) -> Vec<Edge> {
        if markers.is_empty() {
            return Vec::new();
        }
        let resolver = ffi_resolver::Resolver::build_from_markers(markers);
        let edges = resolver.resolve(markers, &self.ffi_config);
        if !edges.is_empty() {
            tracing::info!(
                count = edges.len(),
                "ffi_resolver: emitted cross-language edges (repo-level pass)"
            );
        }
        edges
    }
}
