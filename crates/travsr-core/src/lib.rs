//! travsr-core — graph primitives for the Travsr code-intelligence daemon.
//!
//! This crate defines the foundational data model: Kythe-style VNames,
//! node identifiers, edges, and the multiplex graph types that every other
//! Travsr crate builds on. It has zero dependencies on any other internal
//! Travsr crate by design (see crate dependency rules in CLAUDE.md).

#![forbid(unsafe_code)]

use std::path::Path;

use serde::{Deserialize, Serialize};

pub mod exec;
pub mod ident;
pub mod noise;

/// Version of the VName signature format baked into every `NodeId` hash.
///
/// This byte is the **first input** to the BLAKE3 hasher in `VName::id()`.
/// Changing it produces a disjoint `NodeId` space — any `.travsr/graph.db`
/// built with a different version must be fully re-indexed before it can be
/// queried. See `docs/rfcs/RFC-002-vname-signature-versioning.md`.
///
/// Version history:
///   0 — legacy (no version byte; all pre-RFC-002 databases)
///   1 — Tree-sitter vocabulary (`class:X`, `fn:X`, `method:X.Y`, `var:X`)
///   2 — current: RFC-014 Phase B graph unification. Phase A now captures
///       type-definition nodes and `end_line` spans that the G1/G2 unification
///       passes depend on, so v1 databases lack the tree-sitter nodes that
///       SCIP symbols unify onto. Bumping intentionally invalidates every
///       existing `.travsr/graph.db` so the daemon skew check and the
///       `travsr status` warning force a full re-index (RFC-014 "Re-index
///       Policy").
pub const SIGNATURE_FORMAT_VERSION: u8 = 2;

// ── Corpus derivation (ARCH-102) ─────────────────────────────────────────────

/// Derive the canonical Travsr corpus identifier from a git remote URL.
///
/// **Canonical form:** `host/org/repo` — all lowercase, no scheme prefix,
/// no `.git` suffix, no trailing slash. See `docs/rfcs/ARCH-102`.
///
/// Handles all standard git remote URL formats:
///
/// | Input | Output |
/// |---|---|
/// | `https://github.com/acme/foo.git` | `github.com/acme/foo` |
/// | `git@github.com:acme/foo.git`     | `github.com/acme/foo` |
/// | `ssh://git@github.com/acme/foo`   | `github.com/acme/foo` |
/// | `git://github.com/acme/foo.git`   | `github.com/acme/foo` |
pub fn canonical_corpus(remote_url: &str) -> String {
    let s = remote_url.trim();

    // SCP-style SSH: git@host:org/repo[.git]
    if let Some(rest) = s.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            return format!("{}/{}", host.to_lowercase(), normalize_path(path));
        }
    }

    // URL schemes: https://, http://, ssh://, git://
    let after_scheme = s.split_once("://").map_or(s, |(_, r)| r);
    // Strip userinfo (ssh://git@host/path → host/path)
    let after_at = after_scheme
        .split_once('@')
        .map_or(after_scheme, |(_, r)| r);

    if let Some((host_port, path)) = after_at.split_once('/') {
        // Strip port from host (github.com:443 → github.com)
        let host = host_port.split(':').next().unwrap_or(host_port);
        return format!("{}/{}", host.to_lowercase(), normalize_path(path));
    }

    // No path component — fall back to local name
    format!("local/{}", sanitize_local(s))
}

/// Derive corpus for a local-only repo (no git remote): `local/<basename>`.
///
/// Non-alphanumeric characters (except `-` and `_`) are replaced by `-`.
/// Cross-repo `Exports` edges are impossible for local corpora by definition.
pub fn canonical_corpus_local(repo_root: &Path) -> String {
    let basename = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    format!("local/{}", sanitize_local(basename))
}

fn normalize_path(path: &str) -> String {
    // Lowercase first so .trim_end_matches(".git") also catches ".GIT".
    let lower = path.to_lowercase();
    lower
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string()
}

fn sanitize_local(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .to_lowercase()
}

// ── Language ──────────────────────────────────────────────────────────────────

/// Source language of a graph node.
///
/// Used by the indexer dispatcher ([`Language::from_extension`]) and stored on
/// the `nodes.language` column. The `#[non_exhaustive]` attribute prevents
/// external crates from writing exhaustive matches — Phase 4 will add `Go`
/// without a breaking change. **Within the travsr workspace** the compiler
/// still enforces exhaustive matches, so adding a variant is a compile-time
/// forcing function that updates every dispatch site.
///
/// See RFC-003 and ADR-005 for the design rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Language {
    TypeScript,
    Rust,
    Python,
    Go,
    Java,
    Kotlin,
    Ruby,
    CSharp,
    Php,
    Scala,
    Cpp,
    C,
    Swift,
    Dart,
    ObjectiveC,
    // Data/config formats — Phase A only, never Phase B.
    Json,
    Yaml,
    Toml,
    Xml,
    // Prose — Phase A only, never Phase B (#376). No Tree-sitter grammar and
    // no call-site semantics, so a Phase B sidecar has nothing to contribute.
    Markdown,
}

impl Language {
    /// Map a file extension to a `Language`.
    ///
    /// Returns `None` for unrecognised extensions — callers skip those files.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => Some(Self::TypeScript),
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "go" => Some(Self::Go),
            "java" => Some(Self::Java),
            "kt" | "kts" => Some(Self::Kotlin),
            "rb" | "rake" | "gemspec" => Some(Self::Ruby),
            "cs" => Some(Self::CSharp),
            "php" | "phtml" | "php8" => Some(Self::Php),
            "scala" | "sc" => Some(Self::Scala),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(Self::Cpp),
            "c" | "h" => Some(Self::C),
            "swift" => Some(Self::Swift),
            "dart" => Some(Self::Dart),
            "m" | "mm" => Some(Self::ObjectiveC),
            "json" | "jsonc" => Some(Self::Json),
            "yml" | "yaml" => Some(Self::Yaml),
            "toml" => Some(Self::Toml),
            "xml" | "xsd" | "xsl" => Some(Self::Xml),
            "md" | "markdown" | "mdx" => Some(Self::Markdown),
            _ => None,
        }
    }

    /// Human-readable string stored in the `nodes.language` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TypeScript => "typescript",
            Self::Rust => "rust",
            Self::Python => "python",
            Self::Go => "go",
            Self::Java => "java",
            Self::Kotlin => "kotlin",
            Self::Ruby => "ruby",
            Self::CSharp => "csharp",
            Self::Php => "php",
            Self::Scala => "scala",
            Self::Cpp => "cpp",
            Self::C => "c",
            Self::Swift => "swift",
            Self::Dart => "dart",
            Self::ObjectiveC => "objectivec",
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Xml => "xml",
            Self::Markdown => "markdown",
        }
    }

    /// Parse from the storage string produced by [`Language::as_str`].
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "typescript" => Some(Self::TypeScript),
            "rust" => Some(Self::Rust),
            "python" => Some(Self::Python),
            "go" => Some(Self::Go),
            "java" => Some(Self::Java),
            "kotlin" => Some(Self::Kotlin),
            "ruby" => Some(Self::Ruby),
            "csharp" => Some(Self::CSharp),
            "php" => Some(Self::Php),
            "scala" => Some(Self::Scala),
            "cpp" => Some(Self::Cpp),
            "c" => Some(Self::C),
            "swift" => Some(Self::Swift),
            "dart" => Some(Self::Dart),
            // "objc" alias is accepted at the protocol layer (language_map.rs in
            // travsr-plugin-protocol) for external API callers; core uses the
            // canonical form only.
            "objectivec" => Some(Self::ObjectiveC),
            "json" => Some(Self::Json),
            "yaml" => Some(Self::Yaml),
            "toml" => Some(Self::Toml),
            "xml" => Some(Self::Xml),
            "markdown" => Some(Self::Markdown),
            _ => None,
        }
    }

    /// Returns `true` for data/config formats that index in Phase A only.
    ///
    /// These variants have no Phase B tool (`travsr lang install` / `CATALOG`),
    /// so they must be excluded from the Phase B `present_languages` set and
    /// from the `language_as_str_covered_by_catalog` coverage gate.
    pub fn is_data_format(self) -> bool {
        matches!(self, Self::Json | Self::Yaml | Self::Toml | Self::Xml)
    }

    /// Returns `true` for every variant that has no Phase B tool at all —
    /// data/config formats plus prose (#376's [`Self::Markdown`]).
    ///
    /// Superset of [`Self::is_data_format`]. Phase B dispatch, the
    /// `present_languages` coverage gate, and the embed-catalog coverage test
    /// all key off this rather than `is_data_format` so that adding a future
    /// Phase-A-only variant only requires touching this one match arm.
    pub fn is_phase_a_only(self) -> bool {
        self.is_data_format() || matches!(self, Self::Markdown)
    }
}

/// Kythe-style globally unique identifier for a code entity.
///
/// VNames are stable across repos, languages, and time — they form the
/// universal address space of the Travsr graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VName {
    /// Logical corpus (e.g. repo URL or org/project).
    pub corpus: String,
    /// Root within the corpus (e.g. branch or build root).
    pub root: String,
    /// Path within the root (e.g. `src/foo.ts`).
    pub path: String,
    /// Source language identifier (e.g. `typescript`, `rust`).
    pub language: String,
    /// Symbol signature within the file (e.g. `class:PaymentService#charge`).
    pub signature: String,
}

impl VName {
    /// Construct a `VName` from its five components.
    pub fn new(
        corpus: impl Into<String>,
        root: impl Into<String>,
        path: impl Into<String>,
        language: impl Into<String>,
        signature: impl Into<String>,
    ) -> Self {
        Self {
            corpus: corpus.into(),
            root: root.into(),
            path: path.into(),
            language: language.into(),
            signature: signature.into(),
        }
    }

    /// Stable 64-bit identifier derived from the five-field VName.
    ///
    /// The hash is the first 8 bytes of the BLAKE3 digest of:
    ///   `[SIGNATURE_FORMAT_VERSION] || [len_u32_le][corpus] || [len_u32_le][root] || ...`
    ///
    /// Length-prefix encoding (4-byte little-endian field length before each
    /// field) replaces the NUL-separator scheme. This guarantees that no two
    /// distinct VNames share the same byte stream, and that a v0 byte stream
    /// (which starts with raw corpus bytes) can never equal a v1 stream (which
    /// starts with `[version_byte][len]`). See RFC-002.
    pub fn id(&self) -> NodeId {
        let mut hasher = blake3::Hasher::new();
        // Version domain separator — must be first. Changing SIGNATURE_FORMAT_VERSION
        // produces disjoint NodeId spaces; see RFC-002.
        hasher.update(&[SIGNATURE_FORMAT_VERSION]);
        // Length-prefix each field so no two distinct VNames share the same byte
        // stream regardless of field contents (no NUL-injection ambiguity).
        for field in [
            self.corpus.as_str(),
            self.root.as_str(),
            self.path.as_str(),
            self.language.as_str(),
            self.signature.as_str(),
        ] {
            let bytes = field.as_bytes();
            hasher.update(&(bytes.len() as u32).to_le_bytes());
            hasher.update(bytes);
        }
        let digest = hasher.finalize();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&digest.as_bytes()[..8]);
        NodeId(u64::from_le_bytes(buf))
    }
}

/// Opaque, content-addressed identifier for a node in the graph.
///
/// `NodeId` is a stable BLAKE3-derived hash of a `VName` (see
/// [`VName::id`]). It is the SQLite primary key for the `nodes` table.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
pub struct NodeId(pub u64);

/// The kinds of edges supported in the Travsr multiplex graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeKind {
    /// File / module import. Corresponds to Kythe `%kythe/edge/depends`.
    #[serde(rename = "depends")]
    Depends,
    /// Call-site reference. Corresponds to Kythe `%kythe/edge/ref/call`.
    #[serde(rename = "ref/call")]
    RefCall,
    /// Definition-binding edge (parent → child in the AST).
    #[serde(rename = "defines/binding")]
    DefinesBinding,
    /// A symbol exported from a module.
    #[serde(rename = "exports")]
    Exports,
    /// An import node resolved to the file node it targets.
    /// Connects `import:./foo` → `file:foo.ts`, enabling transitive
    /// caller traversal across file boundaries.
    #[serde(rename = "resolves-to")]
    ResolvesTo,
    /// Named import specifier reference emitted by the LSIF pipeline.
    /// Distinguishes semantic import references from file-level `Depends` edges.
    #[serde(rename = "ref/imports")]
    RefImports,
    /// Class-to-interface implementation edge emitted by the LSIF pipeline.
    #[serde(rename = "is-implementation")]
    IsImplementation,
    /// Method override edge emitted by the LSIF pipeline when a subclass
    /// method shadows a same-named method in the base class.
    #[serde(rename = "overrides")]
    Overrides,
    /// Cross-language FFI call edge (RFC-005). Confidence lives on `Edge.confidence`
    /// so `EdgeKind` stays `Copy`. PPR weight: 0.85 (ADR-003 amendment, 2026-05-24).
    #[serde(rename = "ffi/call")]
    FFICall,
    /// Config file → source file or target it configures.
    /// E.g. `tsconfig.json` references a sub-project, `docker-compose.yml` depends_on a service.
    #[serde(rename = "configures")]
    Configures,
    /// Config file → external package node (registry-hosted dependency).
    /// E.g. `package.json` dependency, `Cargo.toml` crate, `pom.xml` artifact.
    #[serde(rename = "external-dependency")]
    ExternalDependency,
}

impl EdgeKind {
    /// Stable string representation used as the storage key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Depends => "depends",
            Self::RefCall => "ref/call",
            Self::DefinesBinding => "defines/binding",
            Self::Exports => "exports",
            Self::ResolvesTo => "resolves-to",
            Self::RefImports => "ref/imports",
            Self::IsImplementation => "is-implementation",
            Self::Overrides => "overrides",
            Self::FFICall => "ffi/call",
            Self::Configures => "configures",
            Self::ExternalDependency => "external-dependency",
        }
    }

    /// PPR transition weight for this edge kind.
    ///
    /// Weights encode the semantic importance of each edge type for
    /// Personalized PageRank: a higher weight means PPR mass flows more
    /// readily across edges of this kind, producing higher scores for
    /// reachable nodes.
    ///
    /// # Rationale (DEBT-016 / ADR-003)
    ///
    /// | Kind              | Weight | Reasoning                               |
    /// |---|---|---|
    /// | Kind                | Weight | Reasoning                               |
    /// |---|---|---|
    /// | `RefCall`           | 1.00   | Direct call — strongest semantic link   |
    /// | `FFICall`           | 0.85   | Cross-language call — near-call strength|
    /// | `DefinesBinding`    | 0.70   | Parent→child definition — strong structural link |
    /// | `Exports`           | 0.60   | Exported API surface — important for callers |
    /// | `Depends`           | 0.50   | File import — broad but less targeted   |
    /// | `ResolvesTo`        | 0.50   | Import→file resolution — same as Depends |
    /// | `RefImports`        | 0.40   | Named import specifier — narrower than file import |
    /// | `IsImplementation`  | 0.40   | Class implements interface — type-system link |
    /// | `Overrides`         | 0.30   | Method override — weakest semantic tie  |
    /// | `Configures`        | 0.35   | Config→target — below structural, above override |
    /// | `ExternalDependency`| 0.30   | Config→registry package — declaration, not usage |
    ///
    /// Weights are normalised per-node at PPR iteration time so their
    /// absolute scale does not matter — only the ratios between kinds.
    pub fn ppr_weight(self) -> f32 {
        match self {
            Self::RefCall => 1.00,
            Self::DefinesBinding => 0.70,
            Self::Exports => 0.60,
            Self::Depends => 0.50,
            Self::ResolvesTo => 0.50,
            Self::RefImports => 0.40,
            Self::IsImplementation => 0.40,
            Self::Overrides => 0.30,
            Self::FFICall => 0.85,
            Self::Configures => 0.35,
            Self::ExternalDependency => 0.30,
        }
    }

    /// Parse from the stable string representation.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "depends" => Some(Self::Depends),
            "ref/call" => Some(Self::RefCall),
            "defines/binding" => Some(Self::DefinesBinding),
            "exports" => Some(Self::Exports),
            "resolves-to" => Some(Self::ResolvesTo),
            "ref/imports" => Some(Self::RefImports),
            "is-implementation" => Some(Self::IsImplementation),
            "overrides" => Some(Self::Overrides),
            "ffi/call" => Some(Self::FFICall),
            "configures" => Some(Self::Configures),
            "external-dependency" => Some(Self::ExternalDependency),
            _ => None,
        }
    }
}

/// A node in the code graph.
///
/// `PartialEq` compares all fields including `package`. Use `node.id == other.id`
/// for identity-only comparisons (two nodes are the same symbol regardless of
/// their package annotation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub vname: VName,
    pub kind: String,
    /// Sub-unit identity within the corpus (ADR-005 Rule 2).
    ///
    /// Stored in `nodes.package`; **not** part of the BLAKE3 hash input.
    /// Empty string for nodes where package identity is unknown or irrelevant.
    ///
    /// | Language   | Value                                       |
    /// |------------|---------------------------------------------|
    /// | TypeScript | npm package name from `package.json`        |
    /// | Rust       | Cargo package name from `Cargo.toml`        |
    /// | Python     | top-level package dir (highest `__init__.py`)|
    pub package: String,
    /// 1-based source line of the symbol's definition site.
    /// `None` for file-kind nodes and synthetic import nodes.
    pub line: Option<u32>,
    /// 1-based last source line of the symbol's body (inclusive).
    /// Used by G2 span attribution to find the enclosing function for a SCIP
    /// reference occurrence. `None` until migration v13 backfills the column.
    pub end_line: Option<u32>,
}

impl Node {
    /// Build a `Node` from a `VName` and a free-form kind string.
    ///
    /// The `id` is derived deterministically from the VName. `package`
    /// defaults to an empty string; use [`Node::with_package`] to set it.
    /// `line` / `end_line` default to `None`; use the builder methods to set them.
    pub fn new(vname: VName, kind: impl Into<String>) -> Self {
        let id = vname.id();
        Self {
            id,
            vname,
            kind: kind.into(),
            package: String::new(),
            line: None,
            end_line: None,
        }
    }

    /// Set the `package` field and return `self` (builder pattern).
    ///
    /// ```
    /// use travsr_core::{Node, VName};
    /// let n = Node::new(VName::new("github.com/a/b", "", "src/lib.rs", "rust", "fn:main"), "function")
    ///     .with_package("my-crate");
    /// assert_eq!(n.package, "my-crate");
    /// ```
    pub fn with_package(mut self, package: impl Into<String>) -> Self {
        self.package = package.into();
        self
    }

    /// Set the `line` field (1-based) and return `self` (builder pattern).
    pub fn with_line(mut self, line: u32) -> Self {
        self.line = Some(line);
        self
    }

    /// Set the `end_line` field (1-based, inclusive) and return `self` (builder pattern).
    pub fn with_end_line(mut self, end_line: u32) -> Self {
        self.end_line = Some(end_line);
        self
    }
}

/// A raw SCIP reference occurrence used for G2 call-site attribution.
///
/// The store's `write_scip_attributed_batch` takes a slice of these and emits
/// `ref/call` edges from the enclosing function node (or the file node as fallback)
/// to `callee_id`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScipRef {
    /// Repo-relative path of the file containing the reference (e.g. `pkg/foo/bar.go`).
    pub caller_path: String,
    /// 1-based source line of the reference occurrence.
    pub caller_line: u32,
    /// `NodeId` of the called symbol (from `symbol_map` or `symbol_aliases`).
    pub callee_id: NodeId,
}

/// A positional reference occurrence from a rust-analyzer LSIF dump (E3 W3b).
///
/// Unlike [`ScipRef`], the callee is identified by its **definition location**
/// (`callee_def_path`, `callee_def_line`) rather than a pre-resolved `NodeId`,
/// because rust-analyzer LSIF carries no `travsr_vname` — only monikers and
/// definition ranges. The store resolves the callee positionally (narrowest
/// node span containing the def line) and fails closed when nothing resolves,
/// turning these into `ScipRef`s. This is what replaces the old moniker-synth
/// path whose callee VName (at `path = project_root`) matched no Phase A node.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LsifPositionalRef {
    /// Repo-relative path of the file containing the reference occurrence.
    pub caller_path: String,
    /// 1-based source line of the reference occurrence.
    pub caller_line: u32,
    /// Repo-relative path of the file containing the callee's definition.
    pub callee_def_path: String,
    /// 1-based source line of the callee's definition occurrence.
    pub callee_def_line: u32,
}

/// A single reference occurrence returned by `find_references` (issue #299):
/// the file path and 1-based line of one use site of a symbol.
///
/// Produced by `SqliteStore::reference_sites` from the `edge_sites` occurrence
/// store. Language-agnostic — every ingestion path (SCIP, LSIF, native
/// tree-sitter) feeds the same table, so one struct describes them all.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RefSite {
    /// Repo-relative path of the file containing the reference occurrence.
    pub path: String,
    /// 1-based source line of the occurrence.
    pub line: u32,
}

/// Human-readable label: path for file nodes (whose `signature` is the
/// literal `"file"`), signature for everything else.
pub fn display_label(node: &Node) -> &str {
    if node.kind == "file" && !node.vname.path.is_empty() {
        &node.vname.path
    } else {
        &node.vname.signature
    }
}

/// Returns `true` for nodes that carry no developer-facing signal:
/// vendored paths, OS/build caches, repo-escaping paths, and SCIP anonymous locals.
///
/// This is the single source of truth for structural noise. `is_noise_seed`
/// (travsr-mcp) calls this first and then adds MCP-layer policy on top.
pub fn is_noise_node(node: &Node) -> bool {
    let p = &node.vname.path;
    // Vendored / package-manager directories.
    if p.starts_with("third_party/")
        || p.starts_with("vendor/")
        || p.starts_with("node_modules/")
        || p.contains("/node_modules/")
    {
        return true;
    }
    // Paths that escape the repo root are never real source nodes
    // (e.g. Go build-cache: `../../../Library/Caches/go-build/…`).
    if p.starts_with("../") {
        return true;
    }
    // OS-level build and package caches.
    if p.contains("/Library/Caches/")
        || p.contains("/.cache/")
        || p.contains("/go-build/")
        || p.contains("/go/pkg/mod/")
    {
        return true;
    }
    is_scip_anonymous_local(&node.vname.signature)
}

/// Returns `true` if `sig` is a SCIP anonymous-local symbol (`local N` suffix).
/// Used by G3 ingest filter in `travsr-indexer` and by `is_noise_node`.
pub fn is_scip_anonymous_local(sig: &str) -> bool {
    // SCIP local symbols end with "local <digits>", e.g. "local 27".
    if let Some(pos) = sig.rfind("local ") {
        let suffix = &sig[pos + 6..];
        !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit())
    } else {
        false
    }
}

/// A directed, typed edge between two nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub src: NodeId,
    pub dst: NodeId,
    pub kind: EdgeKind,
    /// Confidence score 0..=100 for cross-language FFI edges (RFC-005).
    /// `None` for all non-FFI edges. Stored in `edges.confidence` (migration v6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<u8>,
}

impl Edge {
    pub fn new(src: NodeId, dst: NodeId, kind: EdgeKind) -> Self {
        Self {
            src,
            dst,
            kind,
            confidence: None,
        }
    }

    /// Build a cross-language FFI edge with a confidence score (RFC-005).
    ///
    /// `confidence` must be in `0..=100`. Panics in debug builds if violated.
    pub fn ffi_call(src: NodeId, dst: NodeId, confidence: u8) -> Self {
        debug_assert!(
            confidence <= 100,
            "confidence must be 0..=100, got {confidence}"
        );
        Self {
            src,
            dst,
            kind: EdgeKind::FFICall,
            confidence: Some(confidence),
        }
    }
}

/// A cross-crate call that Phase B could not resolve to a concrete NodeId.
///
/// Phase B tree-sitter extraction knows the callee name but not its file path
/// (the file path is part of the VName hash). The daemon resolves these after
/// Phase B completes by querying Phase A nodes already in the store.
///
/// Propagated through `InvokeResponse.unresolved_calls`; resolved in the daemon
/// before writing to the graph (see `resolve_unresolved_calls` in travsr-daemon).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UnresolvedCall {
    /// The caller node ID (already resolved — it's in the same file as the call site).
    pub src: NodeId,
    /// Signature of the callee to resolve, e.g. `"fn:ppr_weighted"`.
    pub callee_sig: String,
    /// Last path segment of the qualifying crate/module for `call.scoped`, e.g.
    /// `"travsr_retrieval"` from `travsr_retrieval::ppr_weighted(...)`.
    /// `None` for bare `call.fn` identifiers — resolver uses name-only lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint_crate: Option<String>,
    /// 1-based source line of the call-site occurrence (issue #299). Recorded so
    /// the daemon can emit an `edge_sites` row when it resolves `callee_sig` to a
    /// concrete node, giving `find_references` occurrence-level `path:line` for
    /// cross-crate bare calls. `0` means "unknown" (older extractors); the daemon
    /// skips `edge_sites` emission for zero lines.
    #[serde(default)]
    pub caller_line: u32,
    /// True when this call came from a method-call receiver (`recv.method()`)
    /// whose type is not known syntactically. A method call can never resolve
    /// to a bare free function — the daemon resolver requires a qualified
    /// `fn:Type.method` / `method:Type.method` match when this is set (#521 F3).
    /// `false` for `call.fn` / `call.scoped` and for non-Rust emitters that
    /// predate this field (serde default keeps old sidecar payloads valid).
    #[serde(default)]
    pub is_method_call: bool,
    /// Receiver type for a method call (`recv.method()`), when the extractor
    /// could recover it from the enclosing function's own text (#529). `Some(T)`
    /// lets the daemon resolve `fn:T.method` exactly instead of guessing by
    /// unique leaf name; `None` keeps the pre-#529 leaf-fallback behavior. Only
    /// ever set alongside `is_method_call`. `#[serde(default)]` keeps older
    /// sidecar payloads valid, matching how `caller_line` and `is_method_call`
    /// were introduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recv_type: Option<String>,
}

// ── Import Resolution ────────────────────────────────────────────────────────
//
// Query-time bridge over missing `ResolvesTo` edges.
// When the indexer has not emitted `ResolvesTo` edges (Java, Go cross-module,
// PHP, C#, C/C++, Swift, Dart), these resolvers answer at query time:
// "does import node `import_sig` point to this `file_path`?"
//
// Used by `travsr-mcp::tools::get_blast_radius_raw` (Phase 2).
// Lives in `travsr-core` so all downstream crates can use it without
// violating the crate dependency DAG.

pub trait ImportResolver: Send + Sync {
    fn resolves_to(&self, import_sig: &str, file_path: &str) -> bool;
}

/// Return the correct resolver for a `VName::language` string.
/// Falls back to `NoopResolver` for TypeScript, JS, and Rust — `link_imports*`
/// already emits `ResolvesTo` edges for those at index time.
pub fn resolver_for_language(language: &str) -> &'static dyn ImportResolver {
    match language {
        "go" => &GoResolver,
        "java" => &JavaResolver,
        "kotlin" => &KotlinResolver,
        "scala" => &ScalaResolver,
        "python" => &PythonResolver,
        "php" => &PhpResolver,
        "csharp" | "c#" | "cs" => &CsharpResolver,
        "cpp" | "c++" | "c" => &CppResolver,
        "swift" => &SwiftResolver,
        "dart" => &DartResolver,
        // TypeScript/JS/Rust: link_imports* already emits ResolvesTo — noop here.
        // Ruby: no import nodes emitted by the indexer — noop.
        _ => &NoopResolver,
    }
}

struct NoopResolver;
impl ImportResolver for NoopResolver {
    fn resolves_to(&self, _: &str, _: &str) -> bool {
        false
    }
}

// ── shared helpers ──────────────────────────────────────────────────────────

fn file_parent_dir(fp: &str) -> String {
    std::path::Path::new(fp)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|d| !d.is_empty() && *d != ".")
        .unwrap_or("")
        .replace('\\', "/")
}

/// True when `haystack` equals `needle` OR `haystack` ends with `/<needle>`.
fn path_suffix_match(haystack: &str, needle: &str) -> bool {
    haystack == needle || haystack.ends_with(&format!("/{needle}"))
}

/// Shared logic for dot-separated languages (Java, Kotlin, Scala, C#).
/// Handles class-level imports ("import:com.example.Foo") and package-level
/// imports ("import:com.example"), with optional wildcard stripping (".*").
fn dot_lang_resolves_to(import_sig: &str, file_path: &str, exts: &[&str]) -> bool {
    let raw = match import_sig.strip_prefix("import:") {
        Some(p) => p,
        None => return false,
    };
    let import_path = raw.trim_end_matches(".*"); // strip wildcard suffix
    let as_slash = import_path.replace('.', "/");
    let fp = file_path.replace('\\', "/");

    // Class-level: "com/example/Foo.java"
    for ext in exts {
        let class_file = format!("{as_slash}{ext}");
        if path_suffix_match(&fp, &class_file) {
            return true;
        }
    }
    // Package-level: any file whose parent dir ends with the package path
    let dir = file_parent_dir(&fp);
    path_suffix_match(&dir, &as_slash)
}

// ── Go ─────────────────────────────────────────────────────────────────────
// Signature: `import:github.com/Foo/repo/pkg`
// `link_imports_go` emits ResolvesTo for same-module paths; this resolves
// cross-module references where the import path ends with the file's dir.

struct GoResolver;
impl ImportResolver for GoResolver {
    fn resolves_to(&self, import_sig: &str, file_path: &str) -> bool {
        let import_path = match import_sig.strip_prefix("import:") {
            Some(p) => p,
            None => return false,
        };
        let fp = file_path.replace('\\', "/");
        let file_dir = file_parent_dir(&fp);
        if file_dir.is_empty() {
            return false; // root-level Go files are unexportable main packages
        }
        path_suffix_match(import_path, &file_dir)
    }
}

// ── Java ───────────────────────────────────────────────────────────────────
// Signature: `import:org.springframework.web.Controller`
// Dot-to-slash: "org/springframework/web/Controller.java"

struct JavaResolver;
impl ImportResolver for JavaResolver {
    fn resolves_to(&self, import_sig: &str, file_path: &str) -> bool {
        dot_lang_resolves_to(import_sig, file_path, &[".java"])
    }
}

// ── Kotlin ─────────────────────────────────────────────────────────────────
// Signature: `import:kotlin.collections.List` — same convention as Java.

struct KotlinResolver;
impl ImportResolver for KotlinResolver {
    fn resolves_to(&self, import_sig: &str, file_path: &str) -> bool {
        dot_lang_resolves_to(import_sig, file_path, &[".kt", ".kts"])
    }
}

// ── Scala ──────────────────────────────────────────────────────────────────
// Signature: `import:scala.collection.mutable` — same convention as Java.

struct ScalaResolver;
impl ImportResolver for ScalaResolver {
    fn resolves_to(&self, import_sig: &str, file_path: &str) -> bool {
        dot_lang_resolves_to(import_sig, file_path, &[".scala", ".sc"])
    }
}

// ── Python ─────────────────────────────────────────────────────────────────
// Signature: `import:os.path` (absolute), `import:.utils` (relative).
// `link_imports_python` handles both at index time; relative imports require
// the caller's path which is not available here — return false for those.

struct PythonResolver;
impl ImportResolver for PythonResolver {
    fn resolves_to(&self, import_sig: &str, file_path: &str) -> bool {
        let spec = match import_sig.strip_prefix("import:") {
            Some(s) => s,
            None => return false,
        };
        if spec.starts_with('.') {
            return false; // relative: link_imports_python is authoritative
        }
        let as_slash = spec.replace('.', "/");
        let fp = file_path.replace('\\', "/");

        // Module file: "os/path.py"
        if path_suffix_match(&fp, &format!("{as_slash}.py")) {
            return true;
        }
        // Package init: "os/path/__init__.py"
        if path_suffix_match(&fp, &format!("{as_slash}/__init__.py")) {
            return true;
        }
        // Directory match: any .py file in the package dir
        let dir = file_parent_dir(&fp);
        path_suffix_match(&dir, &as_slash)
    }
}

// ── PHP ────────────────────────────────────────────────────────────────────
// Signature: `import:Symfony\Component\HttpKernel\Controller`
// PSR-4: backslash namespace separator → filesystem slash.

struct PhpResolver;
impl ImportResolver for PhpResolver {
    fn resolves_to(&self, import_sig: &str, file_path: &str) -> bool {
        let import_path = match import_sig.strip_prefix("import:") {
            Some(p) => p,
            None => return false,
        };
        let as_slash = import_path.replace('\\', "/");
        let fp = file_path.replace('\\', "/");

        if path_suffix_match(&fp, &format!("{as_slash}.php")) {
            return true;
        }
        let dir = file_parent_dir(&fp);
        path_suffix_match(&dir, &as_slash)
    }
}

// ── C# ─────────────────────────────────────────────────────────────────────
// Signature: `import:System.Collections.Generic`
// By convention (not enforced) C# namespaces map to directory structure.

struct CsharpResolver;
impl ImportResolver for CsharpResolver {
    fn resolves_to(&self, import_sig: &str, file_path: &str) -> bool {
        dot_lang_resolves_to(import_sig, file_path, &[".cs"])
    }
}

// ── C / C++ ────────────────────────────────────────────────────────────────
// Signature: `import:<stdio.h>` (system) or `import:"local/header.h"` (local).
// Angle-bracket includes are external — cannot map to a project file.
// Quote includes carry a relative path that directly matches the file.

struct CppResolver;
impl ImportResolver for CppResolver {
    fn resolves_to(&self, import_sig: &str, file_path: &str) -> bool {
        let include = match import_sig.strip_prefix("import:") {
            Some(p) => p,
            None => return false,
        };
        if include.starts_with('<') {
            return false; // system header
        }
        let bare = include.trim_matches('"');
        let fp = file_path.replace('\\', "/");
        path_suffix_match(&fp, bare)
    }
}

// ── Swift ──────────────────────────────────────────────────────────────────
// Signature: `import:Foundation`, `import:UIKit`, `import:MyModule`
// SPM convention: module name == the directory under Sources/ that holds
// the .swift files. A file belongs to a module when its parent dir name
// matches the module name (handles Sources/MyModule/*.swift).

struct SwiftResolver;
impl ImportResolver for SwiftResolver {
    fn resolves_to(&self, import_sig: &str, file_path: &str) -> bool {
        let module = match import_sig.strip_prefix("import:") {
            Some(m) => m,
            None => return false,
        };
        let fp = file_path.replace('\\', "/");
        if !fp.ends_with(".swift") {
            return false;
        }
        let dir = file_parent_dir(&fp);
        // Last component of parent dir must equal the module name
        let last_dir = dir.rsplit('/').next().unwrap_or(&dir);
        last_dir == module
    }
}

// ── Dart ───────────────────────────────────────────────────────────────────
// Signature: `import:package:myapp/src/util.dart`, `import:dart:core`
// `dart:` → stdlib, no local file.
// Relative (`./`, `../`) → ResolvesTo already emitted by link_imports.
// `package:` → strip scheme + package name, match against remaining path.

struct DartResolver;
impl ImportResolver for DartResolver {
    fn resolves_to(&self, import_sig: &str, file_path: &str) -> bool {
        let uri = match import_sig.strip_prefix("import:") {
            Some(u) => u,
            None => return false,
        };
        if uri.starts_with("dart:") {
            return false; // stdlib
        }
        if uri.starts_with("./") || uri.starts_with("../") {
            return false; // relative: link_imports is authoritative
        }
        let path_part = if let Some(rest) = uri.strip_prefix("package:") {
            // "package:myapp/src/util.dart" → "src/util.dart"
            match rest.find('/') {
                Some(pos) => &rest[pos + 1..],
                None => return false, // "package:myapp" with no path
            }
        } else {
            uri
        };
        let fp = file_path.replace('\\', "/");
        path_suffix_match(&fp, path_part)
    }
}

// ── Graph GC types ────────────────────────────────────────────────────────────

/// Paths of files that held inbound edges to deleted/changed symbols.
/// The daemon enqueues these for Tier-0 dirty re-resolution.
pub type DirtySet = std::collections::HashSet<String>;

/// Returned by the store's `reindex_replace` operation.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ReplaceReport {
    /// Number of symbols that vanished in this edit (old id not present in new parse).
    pub removed_count: usize,
    /// Paths of files that had inbound edges to the removed symbols.
    pub callers: DirtySet,
}

/// Summary returned by `reconcile` / `travsr fsck`.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct GcReport {
    /// VName paths ghost-deleted from the graph.
    pub ghost_paths: Vec<String>,
    /// Number of orphan edges swept (should be 0 in normal operation).
    pub orphan_edges_swept: u64,
    /// Total node count at reconcile start.
    pub node_count: u64,
    /// Total edge count at reconcile start.
    pub edge_count: u64,
    /// `true` if the mass-delete circuit breaker aborted the reconcile.
    pub aborted: bool,
    /// Human-readable reason for abort (set when `aborted = true`).
    pub abort_reason: Option<String>,
    /// #478 RFC-023 §8: set when `nodes` / `nodes_fts_map` / `nodes_fts_words_map`
    /// row counts disagree — indicates a partial write on one of the lexical
    /// index write paths. Report-only; `fsck --fix` does not auto-repair this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lexical_index_parity_issue: Option<String>,
}

/// Safety guards for reconciliation (§6.5 of the GC architecture doc).
#[derive(Debug, Clone)]
pub struct SafetyPolicy {
    /// Maximum fraction of tracked paths deletable in one pass (0.0–1.0).
    /// Circuit breaker aborts if ghost count exceeds
    /// `max(mass_delete_ceiling_min, db_paths.len() * pct)`. Default `0.50`.
    pub mass_delete_ceiling_pct: f64,
    /// Minimum number of deletions always allowed regardless of `pct`. Default `100`.
    pub mass_delete_ceiling_min: usize,
    /// Re-check `exists_on_disk` immediately before each `delete_file` to guard
    /// against TOCTOU races (§6.5 S3). Default `true`.
    pub toctou_recheck: bool,
}

impl Default for SafetyPolicy {
    fn default() -> Self {
        Self {
            mass_delete_ceiling_pct: 0.50,
            mass_delete_ceiling_min: 100,
            toctou_recheck: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_vname() -> VName {
        VName::new(
            "github.com/raj-rkv/travsr",
            "main",
            "crates/travsr-core/src/lib.rs",
            "rust",
            "fn:sample",
        )
    }

    #[test]
    fn vname_round_trips_through_serde_json() {
        let v = sample_vname();
        let json = serde_json::to_string(&v).unwrap();
        let back: VName = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn vname_id_is_deterministic() {
        assert_eq!(sample_vname().id(), sample_vname().id());
    }

    #[test]
    fn vname_id_differs_on_any_field_change() {
        let base = sample_vname();
        let mut other = base.clone();
        other.signature = "fn:different".into();
        assert_ne!(base.id(), other.id());
    }

    #[test]
    fn edge_kind_round_trips_through_string() {
        for kind in [
            EdgeKind::Depends,
            EdgeKind::RefCall,
            EdgeKind::DefinesBinding,
            EdgeKind::Exports,
            EdgeKind::ResolvesTo,
            EdgeKind::RefImports,
            EdgeKind::IsImplementation,
            EdgeKind::Overrides,
            EdgeKind::FFICall,
            EdgeKind::Configures,
            EdgeKind::ExternalDependency,
        ] {
            assert_eq!(EdgeKind::from_str(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn ppr_weights_are_ordered_by_semantic_strength() {
        // RefCall > DefinesBinding > Exports > Depends == ResolvesTo > RefImports == IsImplementation > Configures > Overrides == ExternalDependency
        assert!(EdgeKind::RefCall.ppr_weight() > EdgeKind::DefinesBinding.ppr_weight());
        assert!(EdgeKind::DefinesBinding.ppr_weight() > EdgeKind::Exports.ppr_weight());
        assert!(EdgeKind::Exports.ppr_weight() > EdgeKind::Depends.ppr_weight());
        assert_eq!(
            EdgeKind::Depends.ppr_weight(),
            EdgeKind::ResolvesTo.ppr_weight()
        );
        assert!(EdgeKind::Depends.ppr_weight() > EdgeKind::RefImports.ppr_weight());
        assert_eq!(
            EdgeKind::RefImports.ppr_weight(),
            EdgeKind::IsImplementation.ppr_weight()
        );
        assert!(EdgeKind::IsImplementation.ppr_weight() > EdgeKind::Configures.ppr_weight());
        assert!(EdgeKind::Configures.ppr_weight() > EdgeKind::Overrides.ppr_weight());
        assert_eq!(
            EdgeKind::Overrides.ppr_weight(),
            EdgeKind::ExternalDependency.ppr_weight()
        );
    }

    #[test]
    fn ppr_weights_are_positive_and_at_most_one() {
        for kind in [
            EdgeKind::Depends,
            EdgeKind::RefCall,
            EdgeKind::DefinesBinding,
            EdgeKind::Exports,
            EdgeKind::ResolvesTo,
            EdgeKind::RefImports,
            EdgeKind::IsImplementation,
            EdgeKind::Overrides,
            EdgeKind::FFICall,
            EdgeKind::Configures,
            EdgeKind::ExternalDependency,
        ] {
            let w = kind.ppr_weight();
            assert!(
                w > 0.0 && w <= 1.0,
                "weight {w} for {kind:?} must be in (0, 1]"
            );
        }
    }

    #[test]
    fn node_id_matches_vname_id() {
        let v = sample_vname();
        let node = Node::new(v.clone(), "function");
        assert_eq!(node.id, v.id());
    }

    #[test]
    fn version_byte_produces_different_id_than_unversioned() {
        // Regression guard: confirms the RFC-002 domain separator is actually
        // prepended and that length-prefix encoding is used. The versioned format
        // starts with [SIGNATURE_FORMAT_VERSION][len][corpus...]; the v0 format
        // starts with raw corpus bytes. These byte streams can never be equal
        // regardless of field contents.
        let v = sample_vname();
        let versioned_id = v.id(); // uses SIGNATURE_FORMAT_VERSION byte + length-prefix

        // Compute the legacy (no version byte, NUL-separated) hash directly.
        let mut hasher = blake3::Hasher::new();
        hasher.update(v.corpus.as_bytes());
        hasher.update(b"\0");
        hasher.update(v.root.as_bytes());
        hasher.update(b"\0");
        hasher.update(v.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(v.language.as_bytes());
        hasher.update(b"\0");
        hasher.update(v.signature.as_bytes());
        let digest = hasher.finalize();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&digest.as_bytes()[..8]);
        let legacy_id = NodeId(u64::from_le_bytes(buf));

        assert_ne!(
            versioned_id, legacy_id,
            "RFC-002 version byte + length-prefix must produce a different NodeId than the legacy NUL-separated hash"
        );
    }

    // ── ARCH-102: canonical_corpus tests ─────────────────────────────────────

    #[test]
    fn canonical_corpus_handles_https_with_git_suffix() {
        assert_eq!(
            canonical_corpus("https://github.com/raj-rkv/travsr.git"),
            "github.com/raj-rkv/travsr"
        );
    }

    #[test]
    fn canonical_corpus_handles_https_without_git_suffix() {
        assert_eq!(
            canonical_corpus("https://github.com/raj-rkv/travsr"),
            "github.com/raj-rkv/travsr"
        );
    }

    #[test]
    fn canonical_corpus_handles_scp_style_ssh() {
        assert_eq!(
            canonical_corpus("git@github.com:raj-rkv/travsr.git"),
            "github.com/raj-rkv/travsr"
        );
        assert_eq!(
            canonical_corpus("git@github.com:raj-rkv/travsr"),
            "github.com/raj-rkv/travsr"
        );
    }

    #[test]
    fn canonical_corpus_handles_ssh_url() {
        assert_eq!(
            canonical_corpus("ssh://git@github.com/raj-rkv/travsr.git"),
            "github.com/raj-rkv/travsr"
        );
    }

    #[test]
    fn canonical_corpus_handles_git_protocol() {
        assert_eq!(
            canonical_corpus("git://github.com/raj-rkv/travsr.git"),
            "github.com/raj-rkv/travsr"
        );
    }

    #[test]
    fn canonical_corpus_lowercases_input() {
        assert_eq!(
            canonical_corpus("HTTPS://GITHUB.COM/Raj-Rkv/Travsr.GIT"),
            "github.com/raj-rkv/travsr"
        );
    }

    #[test]
    fn canonical_corpus_strips_port() {
        assert_eq!(
            canonical_corpus("https://github.com:443/raj-rkv/travsr.git"),
            "github.com/raj-rkv/travsr"
        );
    }

    #[test]
    fn canonical_corpus_strips_trailing_slash() {
        assert_eq!(
            canonical_corpus("https://github.com/raj-rkv/travsr/"),
            "github.com/raj-rkv/travsr"
        );
    }

    #[test]
    fn canonical_corpus_gitlab() {
        assert_eq!(
            canonical_corpus("https://gitlab.com/acme/payments-api.git"),
            "gitlab.com/acme/payments-api"
        );
    }

    #[test]
    fn canonical_corpus_local_uses_basename() {
        let path = std::path::Path::new("/home/user/my-project");
        assert_eq!(canonical_corpus_local(path), "local/my-project");
    }

    #[test]
    fn canonical_corpus_local_sanitises_special_chars() {
        let path = std::path::Path::new("/tmp/My Project (v2)");
        let result = canonical_corpus_local(path);
        assert!(result.starts_with("local/"));
        assert!(!result.contains(' '), "spaces must be replaced");
        assert!(!result.contains('('), "parens must be replaced");
    }

    #[test]
    fn different_corpus_produces_non_colliding_node_ids() {
        // Regression: same file + same signature in two different repos must
        // produce different NodeIds because corpus is part of the BLAKE3 input.
        let v_repo_a = VName::new(
            "github.com/acme/repo-a",
            "",
            "src/foo.ts",
            "typescript",
            "fn:bar",
        );
        let v_repo_b = VName::new(
            "github.com/acme/repo-b",
            "",
            "src/foo.ts",
            "typescript",
            "fn:bar",
        );
        assert_ne!(
            v_repo_a.id(),
            v_repo_b.id(),
            "different corpora must produce different NodeIds (cross-repo VName collision)"
        );
    }

    // ── Language enum (ADR-005 / RFC-003) ─────────────────────────────────────

    #[test]
    fn language_from_extension_covers_all_variants() {
        assert_eq!(Language::from_extension("ts"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("tsx"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("mts"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("cts"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("rs"), Some(Language::Rust));
        assert_eq!(Language::from_extension("py"), Some(Language::Python));
        assert_eq!(Language::from_extension("pyi"), Some(Language::Python));
        assert_eq!(Language::from_extension("go"), Some(Language::Go));
        assert_eq!(Language::from_extension("js"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("jsx"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("mjs"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("cjs"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("m"), Some(Language::ObjectiveC));
        assert_eq!(Language::from_extension("mm"), Some(Language::ObjectiveC));
        assert_eq!(Language::from_extension("json"), Some(Language::Json));
        assert_eq!(Language::from_extension("jsonc"), Some(Language::Json));
        assert_eq!(Language::from_extension("yml"), Some(Language::Yaml));
        assert_eq!(Language::from_extension("yaml"), Some(Language::Yaml));
        assert_eq!(Language::from_extension("toml"), Some(Language::Toml));
        assert_eq!(Language::from_extension("xml"), Some(Language::Xml));
        assert_eq!(Language::from_extension("xsd"), Some(Language::Xml));
        assert_eq!(Language::from_extension("xsl"), Some(Language::Xml));
        assert_eq!(Language::from_extension("md"), Some(Language::Markdown));
        assert_eq!(
            Language::from_extension("markdown"),
            Some(Language::Markdown)
        );
        assert_eq!(Language::from_extension("mdx"), Some(Language::Markdown));
        assert_eq!(Language::from_extension(""), None);
    }

    #[test]
    fn language_as_str_and_from_str_round_trip() {
        for lang in [
            Language::TypeScript,
            Language::Rust,
            Language::Python,
            Language::Go,
            Language::ObjectiveC,
            Language::Json,
            Language::Yaml,
            Language::Toml,
            Language::Xml,
            Language::Markdown,
        ] {
            let s = lang.as_str();
            assert_eq!(
                Language::from_str(s),
                Some(lang),
                "round-trip failed for {s}"
            );
        }
    }

    #[test]
    fn language_as_str_values_are_lowercase() {
        assert_eq!(Language::TypeScript.as_str(), "typescript");
        assert_eq!(Language::Rust.as_str(), "rust");
        assert_eq!(Language::Python.as_str(), "python");
        assert_eq!(Language::Go.as_str(), "go");
        assert_eq!(Language::Json.as_str(), "json");
        assert_eq!(Language::Yaml.as_str(), "yaml");
        assert_eq!(Language::Toml.as_str(), "toml");
        assert_eq!(Language::Xml.as_str(), "xml");
        assert_eq!(Language::Markdown.as_str(), "markdown");
    }

    #[test]
    fn language_from_str_returns_none_for_unknown() {
        assert_eq!(Language::from_str("go"), Some(Language::Go));
        assert_eq!(Language::from_str("TypeScript"), None);
        assert_eq!(Language::from_str(""), None);
    }

    #[test]
    fn noise_keeps_external_package_node() {
        // Synthetic registry nodes (npmjs.com, crates.io) must NOT be filtered
        // out by is_noise_node — they have an empty path and a registry corpus.
        for (corpus, sig, lang) in [
            ("npmjs.com", "pkg:express@^4.18.0", "json"),
            ("crates.io", "pkg:serde@1", "toml"),
            (
                "maven.org",
                "pkg:org.springframework:spring-core@6.0.0",
                "xml",
            ),
        ] {
            let node = Node::new(VName::new(corpus, "", "", lang, sig), "package");
            assert!(
                !is_noise_node(&node),
                "external package node for {corpus} must not be noise"
            );
        }
    }

    #[test]
    fn is_data_format_true_for_data_formats_false_for_code() {
        assert!(Language::Json.is_data_format());
        assert!(Language::Yaml.is_data_format());
        assert!(Language::Toml.is_data_format());
        assert!(Language::Xml.is_data_format());
        assert!(!Language::Rust.is_data_format());
        assert!(!Language::TypeScript.is_data_format());
        assert!(!Language::Python.is_data_format());
        // Markdown is Phase-A-only but is prose, not a data/config format —
        // it must stay out of is_data_format() (whose callers key skeleton
        // text on "language path", wrong for prose) while still tripping
        // is_phase_a_only() (whose callers gate Phase B dispatch).
        assert!(!Language::Markdown.is_data_format());
    }

    #[test]
    fn is_phase_a_only_covers_data_formats_and_markdown_not_code() {
        for lang in [
            Language::Json,
            Language::Yaml,
            Language::Toml,
            Language::Xml,
            Language::Markdown,
        ] {
            assert!(lang.is_phase_a_only(), "{lang:?} must be Phase-A-only");
        }
        for lang in [Language::Rust, Language::TypeScript, Language::Python] {
            assert!(!lang.is_phase_a_only(), "{lang:?} must not be Phase-A-only");
        }
    }

    // Regression: two symbols in different languages (same file path, same sig)
    // produce different NodeIds because language is part of the BLAKE3 input.
    #[test]
    fn language_field_prevents_cross_language_vname_collision() {
        let ts = VName::new("github.com/a/b", "", "src/main.rs", "typescript", "fn:main");
        let rs = VName::new("github.com/a/b", "", "src/main.rs", "rust", "fn:main");
        assert_ne!(
            ts.id(),
            rs.id(),
            "different language fields must produce different NodeIds"
        );
    }

    // node.with_package() sets package without changing id.
    #[test]
    fn node_with_package_does_not_change_id() {
        let vname = VName::new("github.com/a/b", "", "src/lib.rs", "rust", "fn:open");
        let plain = Node::new(vname.clone(), "function");
        let packaged = Node::new(vname, "function").with_package("my-crate");
        assert_eq!(plain.id, packaged.id, "package must not affect NodeId");
        assert_eq!(packaged.package, "my-crate");
        assert_eq!(plain.package, "");
    }

    #[test]
    fn edge_ffi_call_builder_sets_confidence() {
        let e = Edge::ffi_call(NodeId(1), NodeId(2), 90);
        assert_eq!(e.kind, EdgeKind::FFICall);
        assert_eq!(e.confidence, Some(90));
    }

    #[test]
    fn edge_new_has_no_confidence() {
        let e = Edge::new(NodeId(1), NodeId(2), EdgeKind::RefCall);
        assert_eq!(e.confidence, None);
    }

    #[test]
    fn edge_kind_ffi_call_roundtrip() {
        assert_eq!(EdgeKind::FFICall.as_str(), "ffi/call");
        assert_eq!(EdgeKind::from_str("ffi/call"), Some(EdgeKind::FFICall));
    }

    #[test]
    fn ppr_weight_ffi_call_is_between_refcall_and_defines_binding() {
        assert!(EdgeKind::FFICall.ppr_weight() < EdgeKind::RefCall.ppr_weight());
        assert!(EdgeKind::FFICall.ppr_weight() > EdgeKind::DefinesBinding.ppr_weight());
        assert!((EdgeKind::FFICall.ppr_weight() - 0.85_f32).abs() < 1e-6);
    }

    #[test]
    fn edge_serde_roundtrip_with_confidence() {
        let e = Edge::ffi_call(NodeId(42), NodeId(99), 75);
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"confidence\":75"));
        let e2: Edge = serde_json::from_str(&json).unwrap();
        assert_eq!(e2.confidence, Some(75));
    }

    #[test]
    fn edge_serde_roundtrip_without_confidence_field() {
        // JSON produced before v6 (no confidence field) must deserialize to None
        let json = r#"{"src":1,"dst":2,"kind":"ref/call"}"#;
        let e: Edge = serde_json::from_str(json).unwrap();
        assert_eq!(e.confidence, None);
    }

    #[test]
    fn unresolved_call_serde_roundtrip_without_recv_type_field() {
        // T-11 (#529): a pre-#529 sidecar payload (no recv_type key at all)
        // must still deserialize, with recv_type defaulting to None.
        let json = r#"{"src":1,"callee_sig":"fn:filter","caller_line":42,"is_method_call":true}"#;
        let u: UnresolvedCall = serde_json::from_str(json).unwrap();
        assert_eq!(u.recv_type, None);
        assert!(u.is_method_call);
    }

    #[test]
    fn unresolved_call_serde_roundtrip_with_recv_type_field() {
        let u = UnresolvedCall {
            src: NodeId(1),
            callee_sig: "fn:filter".to_string(),
            hint_crate: None,
            caller_line: 42,
            is_method_call: true,
            recv_type: Some("Session".to_string()),
        };
        let json = serde_json::to_string(&u).unwrap();
        assert!(json.contains("\"recv_type\":\"Session\""));
        let u2: UnresolvedCall = serde_json::from_str(&json).unwrap();
        assert_eq!(u2.recv_type, Some("Session".to_string()));
    }

    #[test]
    fn unresolved_call_recv_type_omitted_from_json_when_none() {
        // skip_serializing_if keeps wire payloads small when the extractor
        // could not recover a receiver type (the overwhelming common case).
        let u = UnresolvedCall {
            src: NodeId(1),
            callee_sig: "fn:filter".to_string(),
            hint_crate: None,
            caller_line: 42,
            is_method_call: true,
            recv_type: None,
        };
        let json = serde_json::to_string(&u).unwrap();
        assert!(!json.contains("recv_type"));
    }

    #[test]
    fn display_label_uses_path_for_file_nodes() {
        let n = Node::new(VName::new("", "", "src/lib.rs", "rust", "file"), "file");
        assert_eq!(display_label(&n), "src/lib.rs");
    }

    #[test]
    fn display_label_uses_signature_otherwise() {
        let n = Node::new(
            VName::new("", "", "src/lib.rs", "rust", "fn:main"),
            "function",
        );
        assert_eq!(display_label(&n), "fn:main");
    }

    #[test]
    fn noise_detects_third_party() {
        let n = Node::new(
            VName::new("", "", "third_party/foo/bar.go", "go", "fn:bar"),
            "function",
        );
        assert!(is_noise_node(&n));
    }

    #[test]
    fn noise_detects_scip_local() {
        let n = Node::new(
            VName::new("", "", "pkg/eviction/handler.go", "go", "local 27"),
            "variable",
        );
        assert!(is_noise_node(&n));
    }

    #[test]
    fn noise_keeps_production_node() {
        let n = Node::new(
            VName::new("", "", "pkg/eviction/handler.go", "go", "fn:Handle"),
            "function",
        );
        assert!(!is_noise_node(&n));
    }

    #[test]
    fn noise_detects_root_node_modules() {
        let n = Node::new(
            VName::new(
                "",
                "",
                "node_modules/react/index.js",
                "typescript",
                "fn:createElement",
            ),
            "function",
        );
        assert!(is_noise_node(&n));
    }

    #[test]
    fn noise_detects_repo_escaping_path() {
        let n = Node::new(
            VName::new(
                "",
                "",
                "../../../Library/Caches/go-build/80/abc-d",
                "go",
                "file",
            ),
            "file",
        );
        assert!(is_noise_node(&n));
    }

    #[test]
    fn noise_detects_library_caches() {
        let n = Node::new(
            VName::new("", "", "a/b/Library/Caches/go-build/x", "go", "fn:foo"),
            "function",
        );
        assert!(is_noise_node(&n));
    }

    #[test]
    fn noise_detects_dot_cache() {
        let n = Node::new(
            VName::new("", "", "x/.cache/yarn/v6/blah", "typescript", "fn:bar"),
            "function",
        );
        assert!(is_noise_node(&n));
    }

    #[test]
    fn noise_detects_go_build_cache() {
        let n = Node::new(
            VName::new("", "", "home/user/.cache/go-build/ab/abcdef", "go", "fn:f"),
            "function",
        );
        assert!(is_noise_node(&n));
    }

    #[test]
    fn noise_detects_go_pkg_mod() {
        let n = Node::new(
            VName::new(
                "",
                "",
                "a/go/pkg/mod/github.com/foo/bar@v1.0.0/baz.go",
                "go",
                "fn:Baz",
            ),
            "function",
        );
        assert!(is_noise_node(&n));
    }

    #[test]
    fn noise_keeps_real_source_node() {
        let n = Node::new(
            VName::new(
                "",
                "",
                "pkg/api/servicecidr/servicecidr.go",
                "go",
                "fn:OverlapsPrefix",
            ),
            "function",
        );
        assert!(!is_noise_node(&n));
    }
}
