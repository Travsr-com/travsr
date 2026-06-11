//! travsr-core — graph primitives for the Travsr code-intelligence daemon.
//!
//! This crate defines the foundational data model: Kythe-style VNames,
//! node identifiers, edges, and the multiplex graph types that every other
//! Travsr crate builds on. It has zero dependencies on any other internal
//! Travsr crate by design (see crate dependency rules in CLAUDE.md).

#![forbid(unsafe_code)]

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Version of the VName signature format baked into every `NodeId` hash.
///
/// This byte is the **first input** to the BLAKE3 hasher in `VName::id()`.
/// Changing it produces a disjoint `NodeId` space — any `.travsr/graph.db`
/// built with a different version must be fully re-indexed before it can be
/// queried. See `docs/rfcs/RFC-002-vname-signature-versioning.md`.
///
/// Version history:
///   0 — legacy (no version byte; all pre-RFC-002 databases)
///   1 — current: Tree-sitter vocabulary (`class:X`, `fn:X`, `method:X.Y`, `var:X`)
pub const SIGNATURE_FORMAT_VERSION: u8 = 1;

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
            _ => None,
        }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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
    /// | `RefCall`         | 1.00   | Direct call — strongest semantic link   |
    /// | `DefinesBinding`  | 0.70   | Parent→child definition — strong structural link |
    /// | `Exports`         | 0.60   | Exported API surface — important for callers |
    /// | `Depends`         | 0.50   | File import — broad but less targeted   |
    /// | `ResolvesTo`      | 0.50   | Import→file resolution — same as Depends |
    /// | `RefImports`      | 0.40   | Named import specifier — narrower than file import |
    /// | `IsImplementation`| 0.40   | Class implements interface — type-system link |
    /// | `Overrides`       | 0.30   | Method override — weakest semantic tie  |
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
}

impl Node {
    /// Build a `Node` from a `VName` and a free-form kind string.
    ///
    /// The `id` is derived deterministically from the VName. `package`
    /// defaults to an empty string; use [`Node::with_package`] to set it.
    /// `line` defaults to `None`; use [`Node::with_line`] to set it.
    pub fn new(vname: VName, kind: impl Into<String>) -> Self {
        let id = vname.id();
        Self {
            id,
            vname,
            kind: kind.into(),
            package: String::new(),
            line: None,
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
/// vendored paths and SCIP anonymous locals.
pub fn is_noise_node(node: &Node) -> bool {
    let p = &node.vname.path;
    p.starts_with("third_party/")
        || p.starts_with("vendor/")
        || p.starts_with("node_modules/")
        || p.contains("/node_modules/")
        || is_scip_anonymous_local(&node.vname.signature)
}

fn is_scip_anonymous_local(sig: &str) -> bool {
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
        ] {
            assert_eq!(EdgeKind::from_str(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn ppr_weights_are_ordered_by_semantic_strength() {
        // RefCall > DefinesBinding > Exports > Depends == ResolvesTo > RefImports == IsImplementation > Overrides
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
        assert!(EdgeKind::IsImplementation.ppr_weight() > EdgeKind::Overrides.ppr_weight());
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
        // prepended and that length-prefix encoding is used. The v1 format starts
        // with [0x01][len][corpus...]; the v0 format starts with raw corpus bytes.
        // These byte streams can never be equal regardless of field contents.
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
        assert_eq!(Language::from_extension(""), None);
    }

    #[test]
    fn language_as_str_and_from_str_round_trip() {
        for lang in [
            Language::TypeScript,
            Language::Rust,
            Language::Python,
            Language::Go,
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
    }

    #[test]
    fn language_from_str_returns_none_for_unknown() {
        assert_eq!(Language::from_str("go"), Some(Language::Go));
        assert_eq!(Language::from_str("TypeScript"), None);
        assert_eq!(Language::from_str(""), None);
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
}
