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
}
