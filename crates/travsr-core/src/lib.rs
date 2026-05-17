//! travsr-core — graph primitives for the Travsr code-intelligence daemon.
//!
//! This crate defines the foundational data model: Kythe-style VNames,
//! node identifiers, edges, and the multiplex graph types that every other
//! Travsr crate builds on. It has zero dependencies on any other internal
//! Travsr crate by design (see crate dependency rules in CLAUDE.md).

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

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
}
