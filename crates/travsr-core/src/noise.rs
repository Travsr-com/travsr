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

use crate::{Node, TestRole};

/// True for a node that should never be used as retrieval seed/anchor content,
/// independent of any per-query RBAC or corpus filter.
///
/// Superset of [`crate::is_noise_node`]: adds doc-chunk/crate-kind/scip-signature
/// checks and the build-artifact/dependency-cache directory patterns that used
/// to live only in `travsr_mcp::tools::is_noise_seed`.
///
/// **#479 Phase 2:** the *test*-path patterns (`/tests/`, `_test.go`,
/// `/src/test/…`, `/spec/`, `/fixtures/`, `/fuzz/`, `/testdata/`, …) were
/// **removed** from here and moved to [`test_role_from_path`]. Test declarations
/// are no longer *excluded* from the seed/lexical set — they are *categorized*
/// (`TestRole::Support`) and rendered in the capped `tests` section instead.
/// Only genuinely-uninteresting build/vendor/cache artefacts remain hard noise.
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
    // Rust/Cargo build output — seeding into build artefacts walks into macro-expanded code.
    if p.contains("/target/debug/") || p.contains("/target/release/") {
        return true;
    }
    // JavaScript / TypeScript build artefacts.
    if p.contains("/node_modules/") || (p.contains("/dist/") && p.ends_with(".js")) {
        return true;
    }
    // __pycache__ artefacts (compiled Python — not test source).
    if p.contains("/__pycache__/") || p.ends_with(".pyc") {
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
    // Ruby — Bundler vendor/bundle/ (RSpec `spec/` moved to test_role_from_path, #479).
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

/// #479 Phase 2: classify a node as test **support** code purely from its path.
///
/// This is the path fallback of the AST-derived [`crate::TestRole`] — it fires
/// only for declarations the tree-sitter `@test.entry`/`@test.scope` captures
/// did not already classify (an entry/scope role always wins). It returns at
/// most [`TestRole::Support`]: a path is never decisive enough to name a genuine
/// *entry point*, so it never yields [`TestRole::EntryPoint`] (asymmetric-cost
/// rule, §2).
///
/// These are the test-directory / test-file conventions that used to *exclude*
/// nodes in [`is_structural_noise`] (#478). Post-#479 they are categorized
/// instead of dropped, so the declarations still reach retrieval but land in the
/// capped `tests` section rather than the top of the `exact`/`semantic` groups.
///
/// **Separator-agnostic:** store paths can arrive `\`-separated on Windows
/// (cf. #650/#659), so the path is normalized to `/` before matching.
pub fn test_role_from_path(path: &str) -> TestRole {
    let p = path.replace('\\', "/");
    let base = p.rsplit('/').next().unwrap_or(p.as_str());
    let is_test =
        // Rust/Go integration tests + benchmarks (root or nested).
        p.contains("/tests/") || p.starts_with("tests/")
        || p.contains("/benches/") || p.starts_with("benches/")
        // Fixtures + fuzz corpora (arbitrary repo layouts).
        || p.contains("/fixtures/") || p.starts_with("fixtures/")
        || p.contains("/fuzz/") || p.starts_with("fuzz/")
        // Go testdata/ convention + generic `_test/` subdirs.
        || p.contains("/testdata/") || p.starts_with("testdata/")
        || p.contains("/_test/")
        // Ruby/RSpec spec/.
        || p.contains("/spec/") || p.starts_with("spec/")
        // Maven/Gradle/sbt src/test/ (java, kotlin, scala).
        || p.contains("/src/test/") || p.starts_with("src/test/")
        // Swift Package Manager Tests/ target.
        || p.contains("/Tests/") || p.starts_with("Tests/")
        // Language test-file suffixes: Go, C/C++ (GoogleTest), Dart.
        || p.ends_with("_test.go")
        || p.ends_with("_test.c")
        || p.ends_with("_test.cc")
        || p.ends_with("_test.cpp")
        || p.ends_with("_test.cxx")
        || p.ends_with("_test.dart")
        // `*.test.*` / `*.spec.*` basenames (JS/TS/Dart/…).
        || base.contains(".test.")
        || base.contains(".spec.");
    if is_test {
        TestRole::Support
    } else {
        TestRole::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VName;

    fn node(path: &str, sig: &str, kind: &str) -> Node {
        Node::new(VName::new("c", "", path, "rust", sig), kind)
    }

    #[test]
    fn test_paths_are_categorized_not_hard_noise() {
        // #479 Phase 2: the test-path patterns moved out of is_structural_noise.
        for p in [
            "crates/foo/tests/it.rs",
            "pkg/foo_test.go",
            "src/test/java/com/Foo.java",
            "app/spec/models/user_spec.rb",
            "crates/foo/benches/bench.rs",
            "testdata/sample.json",
            "fuzz/fuzz_targets/t.rs",
        ] {
            assert!(
                !is_structural_noise(&node(p, "fn:x", "function")),
                "{p} must no longer be hard noise"
            );
            assert_eq!(
                test_role_from_path(p),
                TestRole::Support,
                "{p} must classify as Support"
            );
        }
    }

    #[test]
    fn build_and_vendor_artefacts_stay_hard_noise() {
        for p in [
            "crate/target/debug/build/x.rs",
            "node_modules/left-pad/index.js",
            "app/vendor/bundle/ruby/gem.rb",
            "obj/Debug/net8/App.cs",
        ] {
            assert!(is_structural_noise(&node(p, "fn:x", "function")), "{p}");
            // Hard noise is not test code.
            assert_eq!(test_role_from_path(p), TestRole::None, "{p}");
        }
    }

    #[test]
    fn production_paths_are_neither() {
        let p = "crates/travsr-core/src/lib.rs";
        assert!(!is_structural_noise(&node(p, "fn:x", "function")));
        assert_eq!(test_role_from_path(p), TestRole::None);
    }

    #[test]
    fn windows_separators_are_normalized() {
        assert_eq!(
            test_role_from_path(r"pkg\sub\foo_test.go"),
            TestRole::Support
        );
        assert_eq!(
            test_role_from_path(r"src\test\java\com\Foo.java"),
            TestRole::Support
        );
        assert_eq!(test_role_from_path(r"a.test.ts"), TestRole::Support);
    }

    #[test]
    fn path_fallback_never_names_an_entry_point() {
        // A path is never decisive enough for EntryPoint (asymmetric-cost).
        for p in ["tests/it.rs", "foo_test.go", "a.spec.ts"] {
            assert_ne!(test_role_from_path(p), TestRole::EntryPoint, "{p}");
        }
    }
}
