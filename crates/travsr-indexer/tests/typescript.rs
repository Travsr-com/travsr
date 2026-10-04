use std::path::Path;

use travsr_core::EdgeKind;
use travsr_indexer::{
    hash_file, link_imports, link_imports_aliased, parse_tsconfig_path_aliases, Indexer,
};

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/ts-small")
        .join(name)
}

fn indexer() -> Indexer {
    Indexer::new()
}

#[test]
fn interface_type_alias_enum_emitted() {
    // RFC-014 #317: SCIP marks interfaces, type aliases, enums and abstract
    // classes as `#` type symbols — Phase A must emit G1-matchable nodes.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("types.ts");
    std::fs::write(
        &path,
        b"export interface Shape { area(): number }\nexport type Velocity = number;\nexport enum Color { Red, Green }\nexport abstract class Base { }\n",
    )
    .unwrap();
    let out = indexer().parse_file(&path).unwrap();
    let sigs: Vec<&str> = out
        .nodes
        .iter()
        .map(|n| n.vname.signature.as_str())
        .collect();
    for expected in [
        "interface:Shape",
        "type:Velocity",
        "enum:Color",
        "class:Base",
    ] {
        assert!(sigs.contains(&expected), "missing {expected}, got {sigs:?}");
    }
}

#[test]
fn parse_empty_file_emits_only_file_node() {
    let out = indexer().parse_file(&fixture("empty.ts")).unwrap();
    assert_eq!(out.nodes.len(), 1, "expected exactly one file node");
    assert_eq!(out.nodes[0].kind, "file");
    assert_eq!(out.edges.len(), 0);
}

#[test]
fn parse_class_emits_nodes_and_edges() {
    // a.ts: export class Greeter { hello() { return "hi"; } }
    let out = indexer().parse_file(&fixture("a.ts")).unwrap();

    let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
    assert!(kinds.contains(&"file"), "missing file node");
    assert!(kinds.contains(&"class"), "missing class node");
    assert!(kinds.contains(&"method"), "missing method node");

    // file → class (DefinesBinding)
    let file_node = out.nodes.iter().find(|n| n.kind == "file").unwrap();
    let class_node = out.nodes.iter().find(|n| n.kind == "class").unwrap();
    let method_node = out.nodes.iter().find(|n| n.kind == "method").unwrap();

    assert!(
        out.edges.iter().any(|e| e.src == file_node.id
            && e.dst == class_node.id
            && e.kind == EdgeKind::DefinesBinding),
        "expected DefinesBinding edge from file to class"
    );
    // class → method (DefinesBinding) — Tech Lead locked hierarchy
    assert!(
        out.edges.iter().any(|e| e.src == class_node.id
            && e.dst == method_node.id
            && e.kind == EdgeKind::DefinesBinding),
        "expected DefinesBinding edge from class to method"
    );
    // no file → method direct edge
    assert!(
        !out.edges
            .iter()
            .any(|e| e.src == file_node.id && e.dst == method_node.id),
        "unexpected direct file→method edge (should be class→method)"
    );
}

#[test]
fn parse_import_emits_depends_edge() {
    // b.ts: import { Greeter } from "./a"; function go() { ... }
    let out = indexer().parse_file(&fixture("b.ts")).unwrap();

    let kinds: Vec<&str> = out.nodes.iter().map(|n| n.kind.as_str()).collect();
    assert!(kinds.contains(&"file"), "missing file node");
    assert!(kinds.contains(&"import"), "missing import node");
    assert!(kinds.contains(&"function"), "missing function node");

    let file_node = out.nodes.iter().find(|n| n.kind == "file").unwrap();
    let import_node = out.nodes.iter().find(|n| n.kind == "import").unwrap();

    assert!(
        out.edges.iter().any(|e| e.src == file_node.id
            && e.dst == import_node.id
            && e.kind == EdgeKind::Depends),
        "expected Depends edge from file to import"
    );
    assert!(
        import_node.vname.signature.contains("./a"),
        "import node signature should contain the module path"
    );
}

#[test]
fn parse_malformed_file_still_emits_file_node() {
    let tmp = tempfile::NamedTempFile::with_suffix(".ts").unwrap();
    std::fs::write(tmp.path(), b"};; class { )").unwrap();

    let out = indexer().parse_file(tmp.path()).unwrap();
    assert!(
        !out.nodes.is_empty(),
        "expected at least the file node for malformed input"
    );
    assert_eq!(out.nodes[0].kind, "file");
}

#[test]
fn vname_signature_disambiguates_function_and_class() {
    let tmp = tempfile::NamedTempFile::with_suffix(".ts").unwrap();
    std::fs::write(tmp.path(), b"function x() {}\nclass X {}\n").unwrap();

    let out = indexer().parse_file(tmp.path()).unwrap();

    let fn_node = out
        .nodes
        .iter()
        .find(|n| n.kind == "function")
        .expect("expected function node");
    let class_node = out
        .nodes
        .iter()
        .find(|n| n.kind == "class")
        .expect("expected class node");

    assert_ne!(
        fn_node.id, class_node.id,
        "function and class must have distinct NodeIds"
    );
    assert!(
        fn_node.vname.signature.starts_with("fn:"),
        "function signature must start with fn:"
    );
    assert!(
        class_node.vname.signature.starts_with("class:"),
        "class signature must start with class:"
    );
}

#[test]
fn link_imports_emits_resolves_to_for_relative_import() {
    // b.ts imports from "./a" — should produce edges to a.ts and a.tsx candidates
    let vname_path = "fixtures/ts-small/b.ts";
    let out = indexer()
        .parse_file_with_vname(&fixture("b.ts"), vname_path)
        .unwrap();

    let edges = link_imports(&out.nodes, vname_path, "");

    // #610: a TypeScript importer tries .ts, .tsx and .js — the last for
    // `allowJs` interop, where a .ts file legitimately imports a .js module.
    assert_eq!(edges.len(), 3, "expected one edge per extension candidate");
    assert!(
        edges.iter().all(|e| e.kind == EdgeKind::ResolvesTo),
        "all emitted edges must be ResolvesTo"
    );

    // The import node must be the source of every edge
    let import_node = out.nodes.iter().find(|n| n.kind == "import").unwrap();
    assert!(
        edges.iter().all(|e| e.src == import_node.id),
        "resolves-to edge src must be the import node"
    );

    // One of the candidates must resolve to a.ts
    let expected_target = Indexer::new()
        .parse_file_with_vname(&fixture("a.ts"), "fixtures/ts-small/a.ts")
        .unwrap()
        .nodes
        .into_iter()
        .find(|n| n.kind == "file")
        .unwrap();
    assert!(
        edges.iter().any(|e| e.dst == expected_target.id),
        "one edge must resolve to fixtures/ts-small/a.ts file node"
    );
}

#[test]
fn link_imports_skips_package_imports() {
    // extension.ts imports "vscode" — not relative, must be skipped
    let abs =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/travsr-vscode/src/extension.ts");
    let vname_path = "packages/travsr-vscode/src/extension.ts";
    let out = indexer().parse_file_with_vname(&abs, vname_path).unwrap();

    let edges = link_imports(&out.nodes, vname_path, "");

    // "vscode" is a package import — skipped.  "./status" is relative — 2 candidates.
    let package_edges: Vec<_> = edges
        .iter()
        .filter(|e| {
            // A resolves-to edge targeting a node whose path contains "vscode" package
            // would be wrong; we detect by checking that all targets are under the src tree
            let dst_sig = out
                .nodes
                .iter()
                .find(|n| n.id == e.dst)
                .map(|n| n.vname.path.as_str())
                .unwrap_or("");
            dst_sig.contains("node_modules") || dst_sig == "vscode"
        })
        .collect();
    assert!(
        package_edges.is_empty(),
        "package imports must not produce resolves-to edges"
    );

    // 16 relative imports (./mcp, ./clientProxy, ./status, ./codelens, ./hover,
    // ./liveResolution, ./tree, ./repoFileTree, ./welcome, ./graph, ./installer,
    // ./telemetry, ./commands, ./contextExplorer, ./mcpRegister,
    // ./contextCodeAction) × 3 candidates each
    // (#610: .ts + .tsx + .js probe) = 48 edges.
    assert_eq!(
        edges.len(),
        48,
        "16 relative imports × 3 extension candidates = 48 resolves-to edges"
    );
}

/// #610: a JavaScript importer must probe JavaScript extensions.
///
/// Only `.ts`/`.tsx` used to be tried, so `./animal` from a `.js` file produced
/// candidates for `animal.ts` and `animal.tsx` — neither of which exists in a
/// JS project. No JavaScript import resolved at all, whichever module syntax
/// it used, which is why `get_blast_radius` on a JS module returned only the
/// queried file.
#[test]
fn link_imports_probes_js_extensions_for_a_js_importer() {
    use travsr_core::{NodeId, VName};

    fn import_node(path: &str, spec: &str) -> travsr_core::Node {
        travsr_analysis::emit::import_node("", path, spec)
    }
    fn file_id(path: &str) -> NodeId {
        travsr_analysis::emit::file_node("", path).id
    }
    let _ = VName::new("", "", "", "", "");

    let importer = "app/main.js";
    let edges = link_imports(&[import_node(importer, "./animal")], importer, "");

    assert_eq!(
        edges.len(),
        4,
        "a .js importer probes js, jsx, mjs and cjs: {edges:?}"
    );
    assert!(
        edges.iter().any(|e| e.dst == file_id("app/animal.js")),
        "the .js candidate is the one that matters and was previously missing"
    );
    assert!(
        !edges.iter().any(|e| e.dst == file_id("app/animal.ts")),
        "a JS importer must not speculate about TypeScript targets"
    );
}

/// A TypeScript importer keeps its existing candidates and gains `.js` for
/// `allowJs` interop, where a `.ts` file legitimately imports a `.js` module.
#[test]
fn link_imports_keeps_ts_candidates_and_adds_js_interop() {
    let importer = "app/main.ts";
    let edges = link_imports(
        &[travsr_analysis::emit::import_node("", importer, "./animal")],
        importer,
        "",
    );
    let dsts: Vec<travsr_core::NodeId> = edges.iter().map(|e| e.dst).collect();
    for ext in ["ts", "tsx", "js"] {
        let want = travsr_analysis::emit::file_node("", &format!("app/animal.{ext}")).id;
        assert!(dsts.contains(&want), "missing .{ext} candidate");
    }
    assert_eq!(edges.len(), 3, "and nothing beyond those three");
}

#[test]
fn link_imports_empty_for_file_with_no_imports() {
    let out = indexer().parse_file(&fixture("empty.ts")).unwrap();
    let edges = link_imports(&out.nodes, "fixtures/ts-small/empty.ts", "");
    assert!(edges.is_empty());
}

/// A tsconfig `paths` alias such as `"@/*": ["./src/*"]` resolves against the
/// repo root, not the importer's directory. Before this, every `@/...` import
/// was skipped, so get_dependencies dead-ended at the alias boundary.
#[test]
fn link_imports_aliased_resolves_tsconfig_path_alias() {
    let importer = "src/app/page.tsx";
    let aliases = [("@/".to_string(), "src/".to_string())];
    let edges = link_imports_aliased(
        &[travsr_analysis::emit::import_node(
            "",
            importer,
            "@/lib/auth",
        )],
        importer,
        "",
        &aliases,
    );
    let want = travsr_analysis::emit::file_node("", "src/lib/auth.ts").id;
    assert!(
        edges.iter().any(|e| e.dst == want),
        "@/lib/auth should resolve to src/lib/auth.ts: {edges:?}"
    );
    // Resolves against the repo root, never relative to the importer's dir.
    let wrong = travsr_analysis::emit::file_node("", "src/app/lib/auth.ts").id;
    assert!(
        !edges.iter().any(|e| e.dst == wrong),
        "an alias target must not be importer-relative"
    );
}

/// When two alias prefixes both match a specifier (`@/` and `@/components/`),
/// the longer, more specific one wins and is not shadowed by the shorter. The
/// result is independent of the order the aliases are listed in.
#[test]
fn link_imports_aliased_longest_prefix_wins() {
    let importer = "src/app/page.tsx";
    // Listed shortest-first so a naive first-match would pick the wrong target.
    let aliases = [
        ("@/".to_string(), "src/".to_string()),
        ("@/components/".to_string(), "design/ui/".to_string()),
    ];
    let edges = link_imports_aliased(
        &[travsr_analysis::emit::import_node(
            "",
            importer,
            "@/components/Button",
        )],
        importer,
        "",
        &aliases,
    );
    let want = travsr_analysis::emit::file_node("", "design/ui/Button.ts").id;
    assert!(
        edges.iter().any(|e| e.dst == want),
        "@/components/Button should resolve via the longer alias to design/ui/Button.ts: {edges:?}"
    );
    let shadowed = travsr_analysis::emit::file_node("", "src/components/Button.ts").id;
    assert!(
        !edges.iter().any(|e| e.dst == shadowed),
        "the shorter @/ alias must not shadow the more specific @/components/"
    );
}

/// With no alias table (the `link_imports` wrapper), a non-relative specifier
/// is still skipped: only `./` and `../` resolve.
#[test]
fn link_imports_skips_bare_specifiers_without_aliases() {
    let importer = "src/app/page.tsx";
    let edges = link_imports(
        &[travsr_analysis::emit::import_node(
            "",
            importer,
            "@/lib/auth",
        )],
        importer,
        "",
    );
    assert!(
        edges.is_empty(),
        "no alias table: @/... must not resolve: {edges:?}"
    );
}

/// `parse_tsconfig_path_aliases` reads `compilerOptions.paths` wildcard entries
/// and resolves each target against `baseUrl` (default `"."`). Only the
/// single-target wildcard form is handled; non-wildcard and missing-config
/// cases yield no aliases. Locks the contract the daemon threads into
/// `link_imports_aliased`.
#[test]
fn parse_tsconfig_path_aliases_reads_wildcard_paths() {
    fn aliases_for(tsconfig: &str) -> Vec<(String, String)> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tsconfig.json"), tsconfig).unwrap();
        parse_tsconfig_path_aliases(dir.path())
    }

    // The create-next-app default: "@/*" -> ["./src/*"] with baseUrl ".".
    assert_eq!(
        aliases_for(r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["./src/*"]}}}"#),
        vec![("@/".to_string(), "src/".to_string())],
    );

    // baseUrl defaults to "." when absent.
    assert_eq!(
        aliases_for(r#"{"compilerOptions":{"paths":{"@/*":["./src/*"]}}}"#),
        vec![("@/".to_string(), "src/".to_string())],
    );

    // A non-"." baseUrl prefixes the target: "@/*" -> ["*"], baseUrl "src".
    assert_eq!(
        aliases_for(r#"{"compilerOptions":{"baseUrl":"src","paths":{"@/*":["*"]}}}"#),
        vec![("@/".to_string(), "src/".to_string())],
    );

    // Non-wildcard entries are skipped; nothing to resolve by prefix.
    assert!(aliases_for(r#"{"compilerOptions":{"paths":{"@/foo":["./src/foo.ts"]}}}"#).is_empty());

    // A config with no `paths` yields nothing.
    assert!(aliases_for(r#"{"compilerOptions":{"strict":true}}"#).is_empty());

    // A hand-written tsconfig is JSONC: line/block comments and trailing commas
    // are tolerated (TypeScript's own loader accepts them), so an alias behind
    // them still resolves rather than silently dropping out.
    assert_eq!(
        aliases_for(
            "{\n  // leading\n  \"compilerOptions\": {\n    /* block */\n    \"paths\": {\n      \"@/*\": [\"./src/*\"],\n    },\n  },\n}"
        ),
        vec![("@/".to_string(), "src/".to_string())],
    );

    // A `//` sequence inside a string value is not a comment and is preserved.
    assert_eq!(
        aliases_for(r#"{"compilerOptions":{"baseUrl":"./a//b","paths":{"@/*":["*"]}}}"#),
        vec![("@/".to_string(), "a/b/".to_string())],
    );
}

/// A missing tsconfig.json yields no aliases rather than erroring.
#[test]
fn parse_tsconfig_path_aliases_empty_without_tsconfig() {
    let dir = tempfile::tempdir().unwrap();
    assert!(parse_tsconfig_path_aliases(dir.path()).is_empty());
}

/// A JS-only repo (jsconfig.json, no tsconfig) gets the same alias resolution:
/// jsconfig.json is the JavaScript convention for the identical schema, and the
/// create-next-app JS template ships one. tsconfig wins when both are present.
#[test]
fn parse_tsconfig_path_aliases_falls_back_to_jsconfig() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("jsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["./src/*"]}}}"#,
    )
    .unwrap();
    assert_eq!(
        parse_tsconfig_path_aliases(dir.path()),
        vec![("@/".to_string(), "src/".to_string())],
    );

    // tsconfig.json takes precedence over jsconfig.json when both exist.
    std::fs::write(
        dir.path().join("tsconfig.json"),
        r#"{"compilerOptions":{"paths":{"~/*":["./app/*"]}}}"#,
    )
    .unwrap();
    assert_eq!(
        parse_tsconfig_path_aliases(dir.path()),
        vec![("~/".to_string(), "app/".to_string())],
    );
}

#[test]
fn hash_file_is_deterministic() {
    let h1 = hash_file(&fixture("a.ts")).unwrap();
    let h2 = hash_file(&fixture("a.ts")).unwrap();
    assert_eq!(h1, h2, "same file must produce the same hash");
}

#[test]
fn hash_file_differs_on_change() {
    let h1 = hash_file(&fixture("a.ts")).unwrap();
    let tmp = tempfile::NamedTempFile::with_suffix(".ts").unwrap();
    std::fs::write(tmp.path(), b"export class Different {}").unwrap();
    let h2 = hash_file(tmp.path()).unwrap();
    assert_ne!(h1, h2, "different content must produce a different hash");
}

#[test]
fn parse_file_with_vname_uses_vname_path() {
    let out = indexer()
        .parse_file_with_vname(&fixture("a.ts"), "custom/path.ts")
        .unwrap();
    for node in &out.nodes {
        assert_eq!(
            node.vname.path, "custom/path.ts",
            "all nodes must carry the supplied vname_path"
        );
    }
}
