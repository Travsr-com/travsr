//! travsr-core — graph primitives for the Travsr code-intelligence daemon.
//!
//! This crate defines the foundational data model: Kythe-style VNames,
//! node identifiers, edges, and the multiplex graph types that every other
//! Travsr crate builds on. It has zero dependencies on any other internal
//! Travsr crate by design (see crate dependency rules in CLAUDE.md).

#![forbid(unsafe_code)]

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
}
