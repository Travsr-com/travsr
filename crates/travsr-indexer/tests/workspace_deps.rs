//! A2: cross-file Cargo workspace dependency resolution.
//!
//! A member crate's `serde = { workspace = true }` (a `Consumer` marker) must
//! resolve to the workspace root's `[workspace.dependencies]` version (a
//! `Provider` marker) — producing an edge to the SAME versioned package node the
//! root emits. An unresolved consumer (root absent from the batch) falls back to
//! the `@workspace` sentinel so the edge is never dangling.

use travsr_analysis::data_format::{cargo_package_node, WorkspaceDepMarker};
use travsr_core::{EdgeKind, Node, VName};
use travsr_indexer::Indexer;

fn member_file_id() -> travsr_core::NodeId {
    Node::new(
        VName::new("c", "", "crates/member/Cargo.toml", "toml", "file"),
        "file",
    )
    .id
}

#[test]
fn member_inherited_dep_resolves_to_root_version() {
    let member = member_file_id();
    let markers = vec![
        WorkspaceDepMarker::Provider {
            name: "serde".into(),
            version: "1.0.200".into(),
        },
        WorkspaceDepMarker::Consumer {
            member_file: member,
            name: "serde".into(),
        },
    ];

    let (nodes, edges) = Indexer::with_corpus("c").resolve_workspace_deps(&markers);

    // The resolved node is byte-identical to what the root emits — same NodeId,
    // so the two never diverge into parallel graph nodes.
    let expected = cargo_package_node("serde", "1.0.200");
    assert!(nodes.iter().any(|n| n.id == expected.id));
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].src, member);
    assert_eq!(edges[0].dst, expected.id);
    assert_eq!(edges[0].kind, EdgeKind::ExternalDependency);
}

#[test]
fn unresolved_consumer_falls_back_to_workspace_sentinel() {
    let member = member_file_id();
    // No Provider for `mystery` — e.g. a single-file incremental reindex where the
    // workspace root was not part of this batch.
    let markers = vec![WorkspaceDepMarker::Consumer {
        member_file: member,
        name: "mystery".into(),
    }];

    let (nodes, edges) = Indexer::with_corpus("c").resolve_workspace_deps(&markers);

    let sentinel = cargo_package_node("mystery", "workspace");
    assert!(nodes.iter().any(|n| n.id == sentinel.id));
    assert_eq!(edges.len(), 1);
    assert_eq!(
        edges[0].dst, sentinel.id,
        "no dangling edge; @workspace fallback"
    );
}

#[test]
fn no_markers_yields_nothing() {
    let (nodes, edges) = Indexer::with_corpus("c").resolve_workspace_deps(&[]);
    assert!(nodes.is_empty());
    assert!(edges.is_empty());
}
