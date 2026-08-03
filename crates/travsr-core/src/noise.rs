//! Structural noise classification — the single source of truth for "is this
//! node ever useful as retrieval seed/anchor content", independent of any
//! per-query RBAC or corpus filter.
//!
//! Before this module existed, the answer lived in two places: [`crate::is_noise_node`]
//! (vendored/build-cache/repo-escaping paths, SCIP anonymous locals — checked at
//! ingest time) and `travsr_mcp::tools::is_noise_seed` (the same, plus doc-chunk/
//! crate-kind/scip-signature and a long per-language test/build-artifact
//! directory list, checked at query time). #478 RFC-023 §6 needs a
//! `nodes.is_noise` column populated at index time in `travsr-store`, which
//! cannot depend on `travsr-mcp` (crate dependency rule) — so the full
//! predicate now lives here, and `is_noise_seed` becomes a thin wrapper.

use crate::Node;

/// True for a node that should never be used as retrieval seed/anchor content,
/// independent of any per-query RBAC or corpus filter.
///
/// Superset of [`crate::is_noise_node`]: adds doc-chunk/crate-kind/scip-signature
/// checks and the per-language test/build-artifact/dependency-cache directory
/// patterns that used to live only in `travsr_mcp::tools::is_noise_seed`.
pub fn is_structural_noise(node: &Node) -> bool {
    if crate::is_noise_node(node) {
        return true;
    }
    // #376: doc-chunk nodes are legitimate content, not noise in the path-based
    // sense every other check here is — but this is the one gate every generic
    // seed/anchor candidate is proven to pass through, so it is the correct
    // permanent place to keep doc-chunk nodes out of the unfloored lexical/PPR
    // path (they have their own floored doc-space lane). Do not remove.
    if node.kind == "doc-chunk" {
        return true;
    }
    if node.kind == "crate" {
        return true;
    }
    // Signatures prefixed "scip:" are synthetic SCIP reference nodes with no real body.
    if node.vname.signature.starts_with("scip:") {
        return true;
    }
    let p = &node.vname.path;
    // Rust/Go integration test and benchmark directories.
    if p.contains("/tests/") || p.contains("/benches/") {
        return true;
    }
    // Common fixture and fuzz directories (arbitrary repo layouts).
    if p.contains("/fixtures/") || p.starts_with("fixtures/") {
        return true;
    }
    if p.contains("/fuzz/") || p.starts_with("fuzz/") {
        return true;
    }
    // Go test files (_test.go suffix) and generic test subdirectories.
    if p.ends_with("_test.go") || p.contains("/_test/") {
        return true;
    }
    // Rust/Go integration tests at repo root (tests/) and benches at root.
    if p.starts_with("tests/") || p.starts_with("benches/") {
        return true;
    }
    // Rust/Cargo build output — seeding into build artefacts walks into macro-expanded code.
    if p.contains("/target/debug/") || p.contains("/target/release/") {
        return true;
    }
    // JavaScript / TypeScript build artefacts.
    if p.contains("/node_modules/") || (p.contains("/dist/") && p.ends_with(".js")) {
        return true;
    }
    // testdata/ directories (Go convention) and __pycache__ artefacts.
    if p.contains("/testdata/") || p.starts_with("testdata/") {
        return true;
    }
    if p.contains("/__pycache__/") || p.ends_with(".pyc") {
        return true;
    }
    // Java / Kotlin / Scala — Maven src/test, Gradle build/, sbt target/
    if p.contains("/src/test/java/")
        || p.starts_with("src/test/java/")
        || p.contains("/src/test/kotlin/")
        || p.starts_with("src/test/kotlin/")
        || p.contains("/src/test/scala/")
        || p.starts_with("src/test/scala/")
    {
        return true;
    }
    if p.contains("/build/classes/")
        || p.starts_with("build/classes/")
        || p.contains("/build/generated/")
        || p.starts_with("build/generated/")
    {
        return true;
    }
    // Maven/sbt general build output under target/ (not just Rust debug/release).
    if p.contains("/target/classes/")
        || p.contains("/target/generated-sources/")
        || p.contains("/target/scala-")
        || p.ends_with(".class")
    {
        return true;
    }
    // Ruby — RSpec spec/, Bundler vendor/bundle/
    if p.contains("/spec/") || p.starts_with("spec/") {
        return true;
    }
    if p.contains("/vendor/bundle/") {
        return true;
    }
    // PHP — Composer vendor/ (parallel to JS node_modules).
    if p.contains("/vendor/") && p.ends_with(".php") {
        return true;
    }
    // C/C++ — object files, CMake out-of-source build directories.
    if p.ends_with(".o") || p.ends_with(".obj") {
        return true;
    }
    if p.contains("/CMakeFiles/") || p.contains("/_deps/") {
        return true;
    }
    // Dart/Flutter — package cache and build output.
    if p.contains("/.dart_tool/") || p.contains("/.pub-cache/") {
        return true;
    }
    // C# / .NET — compiler-generated build output in obj/ and bin/ subdirectories.
    if p.contains("/obj/Debug/")
        || p.contains("/obj/Release/")
        || p.starts_with("obj/Debug/")
        || p.starts_with("obj/Release/")
        || p.contains("/bin/Debug/")
        || p.contains("/bin/Release/")
        || p.starts_with("bin/Debug/")
        || p.starts_with("bin/Release/")
        || p.ends_with(".AssemblyInfo.cs")
        || p.ends_with(".GlobalUsings.g.cs")
    {
        return true;
    }
    false
}
