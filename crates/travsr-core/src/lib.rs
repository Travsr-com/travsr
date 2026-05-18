//! travsr-core — graph primitives for the Travsr code-intelligence daemon.
//!
//! This crate defines the foundational data model: Kythe-style VNames,
//! node identifiers, edges, and the multiplex graph types that every other
//! Travsr crate builds on. It has zero dependencies on any other internal
//! Travsr crate by design (see crate dependency rules in CLAUDE.md).

#![forbid(unsafe_code)]

use std::path::Path;

use serde::{Deserialize, Serialize};

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
    /// The hash is the first 8 bytes of the BLAKE3 digest of the
    /// NUL-separated fields, interpreted as little-endian `u64`. This is
    /// deterministic across machines, processes, and Travsr versions and
    /// is suitable for use as the SQLite primary key.
    pub fn id(&self) -> NodeId {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.corpus.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.root.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.language.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.signature.as_bytes());
        let digest = hasher.finalize();
        let bytes = digest.as_bytes();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes[..8]);
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
    Depends,
    /// Call-site reference. Corresponds to Kythe `%kythe/edge/ref/call`.
    RefCall,
    /// Definition-binding edge (parent → child in the AST).
    DefinesBinding,
    /// A symbol exported from a module.
    Exports,
    /// An import node resolved to the file node it targets.
    /// Connects `import:./foo` → `file:foo.ts`, enabling transitive
    /// caller traversal across file boundaries.
    ResolvesTo,
    /// Named import specifier reference emitted by the LSIF pipeline.
    /// Distinguishes semantic import references from file-level `Depends` edges.
    RefImports,
    /// Class-to-interface implementation edge emitted by the LSIF pipeline.
    IsImplementation,
    /// Method override edge emitted by the LSIF pipeline when a subclass
    /// method shadows a same-named method in the base class.
    Overrides,
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
            _ => None,
        }
    }
}

/// A node in the code graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub vname: VName,
    pub kind: String,
}

impl Node {
    /// Build a `Node` from a `VName` and a free-form kind string. The id is
    /// derived deterministically from the VName.
    pub fn new(vname: VName, kind: impl Into<String>) -> Self {
        let id = vname.id();
        Self {
            id,
            vname,
            kind: kind.into(),
        }
    }
}

/// A directed, typed edge between two nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub src: NodeId,
    pub dst: NodeId,
    pub kind: EdgeKind,
}

impl Edge {
    pub fn new(src: NodeId, dst: NodeId, kind: EdgeKind) -> Self {
        Self { src, dst, kind }
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
        ] {
            assert_eq!(EdgeKind::from_str(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn node_id_matches_vname_id() {
        let v = sample_vname();
        let node = Node::new(v.clone(), "function");
        assert_eq!(node.id, v.id());
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
}
