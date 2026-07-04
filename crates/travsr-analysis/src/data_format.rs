//! Phase-A indexing for data/config formats (JSON, YAML, TOML, XML).
//!
//! Level 1: emit a single file node per file (no content parsing).
//! Level 2: filename-dispatched schema parsers emit typed edges
//!   (`ExternalDependency`, `Configures`) for well-known files.
//!
//! Malformed-input policy: parse errors degrade to Level 1 (file node only)
//! with a debug-level warning — never panic on user config files.
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;
use travsr_core::{Edge, EdgeKind, Language, Node, NodeId, VName};
use yaml_rust2::{Yaml, YamlLoader};

use crate::ParseOutput;

pub fn parse(corpus: &str, abs_path: &Path, vname_path: &str) -> anyhow::Result<ParseOutput> {
    let lang_str = abs_path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(Language::from_extension)
        .map(Language::as_str)
        .unwrap_or("json");

    let file_vname = VName::new(corpus, "", vname_path, lang_str, "file");
    let file_node = Node::new(file_vname, "file");
    let file_id = file_node.id;

    // Use vname_path (repo-relative) for dispatch — abs_path may be a temp file
    // in tests or a canonical path that doesn't reflect the repo structure.
    let file_name = Path::new(vname_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    let mut out = ParseOutput {
        nodes: vec![file_node],
        ..Default::default()
    };

    match (lang_str, file_name) {
        ("json", "package.json") => parse_package_json(corpus, abs_path, file_id, &mut out),
        ("json", "tsconfig.json") => {
            parse_tsconfig_json(corpus, abs_path, vname_path, file_id, &mut out)
        }
        ("toml", "Cargo.toml") => parse_cargo_toml(corpus, abs_path, file_id, &mut out),
        ("yaml", n) if n.ends_with(".yml") || n.ends_with(".yaml") => {
            parse_yaml(corpus, abs_path, vname_path, file_id, &mut out);
        }
        ("xml", "pom.xml") => parse_pom_xml(abs_path, file_id, &mut out),
        _ => {}
    }

    Ok(out)
}

// ── JSON ──────────────────────────────────────────────────────────────────────

fn parse_package_json(_corpus: &str, path: &Path, file_id: NodeId, out: &mut ParseOutput) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        tracing::debug!("data_format: malformed package.json at {:?}", path);
        return;
    };

    for section in &["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(deps) = v.get(section).and_then(|d| d.as_object()) {
            for (name, version) in deps {
                let ver = version.as_str().unwrap_or("*");
                let sig = format!("pkg:{name}@{ver}");
                let pkg_node = Node::new(VName::new("npmjs.com", "", "", "json", &sig), "package");
                let edge = Edge::new(file_id, pkg_node.id, EdgeKind::ExternalDependency);
                out.nodes.push(pkg_node);
                out.edges.push(edge);
            }
        }
    }
}

fn parse_tsconfig_json(
    corpus: &str,
    path: &Path,
    vname_path: &str,
    file_id: NodeId,
    out: &mut ParseOutput,
) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        tracing::debug!("data_format: malformed tsconfig.json at {:?}", path);
        return;
    };

    if let Some(refs) = v.get("references").and_then(|r| r.as_array()) {
        // Use the vname_path (repo-relative) parent dir so the resolved
        // target path stays repo-relative, not absolute.
        let vname_dir = Path::new(vname_path).parent().unwrap_or(Path::new(""));
        for r in refs {
            let Some(ref_path) = r.get("path").and_then(|p| p.as_str()) else {
                continue;
            };
            let raw = vname_dir.join(ref_path);
            // Normalise to a repo-relative, forward-slash path: drop every "."
            // component (leading *and* interior, e.g. "sub/./pkg") so the target
            // NodeId matches the real file node. Keep ".." for out-of-dir refs.
            let target_path = raw
                .components()
                .filter_map(|c| match c {
                    std::path::Component::CurDir => None,
                    std::path::Component::ParentDir => Some(".."),
                    std::path::Component::Normal(s) => s.to_str(),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/");
            let target_node =
                Node::new(VName::new(corpus, "", &target_path, "json", "file"), "file");
            let edge = Edge::new(file_id, target_node.id, EdgeKind::Configures);
            out.nodes.push(target_node);
            out.edges.push(edge);
        }
    }
}

// ── TOML ─────────────────────────────────────────────────────────────────────

fn parse_cargo_toml(_corpus: &str, path: &Path, file_id: NodeId, out: &mut ParseOutput) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(doc) = text.parse::<toml::Value>() else {
        tracing::debug!("data_format: malformed Cargo.toml at {:?}", path);
        return;
    };

    for section in &["dependencies", "dev-dependencies", "build-dependencies"] {
        let Some(deps) = doc.get(section).and_then(|d| d.as_table()) else {
            continue;
        };
        for (name, spec) in deps {
            let ver = match spec {
                toml::Value::String(s) => s.as_str(),
                toml::Value::Table(t) => t.get("version").and_then(|v| v.as_str()).unwrap_or("*"),
                _ => "*",
            };
            let sig = format!("pkg:{name}@{ver}");
            let pkg_node = Node::new(VName::new("crates.io", "", "", "toml", &sig), "package");
            let edge = Edge::new(file_id, pkg_node.id, EdgeKind::ExternalDependency);
            out.nodes.push(pkg_node);
            out.edges.push(edge);
        }
    }
}

// ── YAML ─────────────────────────────────────────────────────────────────────

fn parse_yaml(
    _corpus: &str,
    path: &Path,
    vname_path: &str,
    file_id: NodeId,
    out: &mut ParseOutput,
) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(docs) = YamlLoader::load_from_str(&text) else {
        tracing::debug!("data_format: malformed YAML at {:?}", path);
        return;
    };
    let Some(doc) = docs.into_iter().next() else {
        return;
    };

    // Dispatch by filename within the repo-relative path.
    if vname_path.contains(".github/workflows/") {
        parse_github_actions(&doc, file_id, out);
    } else if Path::new(vname_path)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            n == "docker-compose.yml" || n == "docker-compose.yaml" || n.starts_with("compose.")
        })
        .unwrap_or(false)
    {
        parse_docker_compose(&doc, file_id, out);
    }
}

/// Extract `uses:` action references from GitHub Actions workflow steps.
fn parse_github_actions(doc: &Yaml, file_id: NodeId, out: &mut ParseOutput) {
    let Some(jobs) = doc["jobs"].as_hash() else {
        return;
    };
    for (_job_name, job) in jobs {
        let Some(steps) = job["steps"].as_vec() else {
            continue;
        };
        for step in steps {
            let Some(uses) = step["uses"].as_str() else {
                continue;
            };
            // `uses` format: "owner/repo@ref" or "owner/repo/path@ref"
            let sig = format!("pkg:{uses}");
            let node = Node::new(
                VName::new("github.com/actions", "", "", "yaml", &sig),
                "package",
            );
            let edge = Edge::new(file_id, node.id, EdgeKind::ExternalDependency);
            out.nodes.push(node);
            out.edges.push(edge);
        }
    }
}

/// Extract `image:` and `depends_on:` edges from docker-compose files.
fn parse_docker_compose(doc: &Yaml, file_id: NodeId, out: &mut ParseOutput) {
    let Some(services) = doc["services"].as_hash() else {
        return;
    };
    for (svc_name, svc) in services {
        // ExternalDependency edge for each image pulled from a registry.
        if let Some(image) = svc["image"].as_str() {
            let sig = format!("pkg:{image}");
            let node = Node::new(
                VName::new("hub.docker.com", "", "", "yaml", &sig),
                "package",
            );
            let edge = Edge::new(file_id, node.id, EdgeKind::ExternalDependency);
            out.nodes.push(node);
            out.edges.push(edge);
        }

        // Configures edge for each depends_on entry (service-to-service dependency).
        let deps = match &svc["depends_on"] {
            Yaml::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect::<Vec<_>>(),
            Yaml::Hash(h) => h
                .keys()
                .filter_map(|k| k.as_str())
                .map(String::from)
                .collect::<Vec<_>>(),
            _ => vec![],
        };
        for dep in deps {
            let sig = format!("service:{dep}");
            let node = Node::new(VName::new("", "", "", "yaml", &sig), "service");
            let edge = Edge::new(file_id, node.id, EdgeKind::Configures);
            // Avoid duplicate service nodes when multiple services depend on the same one.
            if !out.nodes.iter().any(|n| n.id == node.id) {
                out.nodes.push(node);
            }
            out.edges.push(edge);
        }
        let _ = svc_name;
    }
}

// ── XML ──────────────────────────────────────────────────────────────────────

/// Extract Maven `<dependency>` entries from a `pom.xml`.
///
/// Emits one `ExternalDependency` edge per `<dependency>` block found inside
/// the top-level `<dependencies>` element. The synthetic node signature is
/// `pkg:<groupId>:<artifactId>@<version>` on corpus `maven.org`.
fn parse_pom_xml(path: &Path, file_id: NodeId, out: &mut ParseOutput) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let mut reader = Reader::from_str(&text);
    reader.config_mut().trim_text(true);

    // Collect the three sub-elements of each <dependency> block.
    let mut in_dependencies = 0u32; // depth counter — handles nested <dependencies>
    let mut in_dependency = false;
    let mut group_id = String::new();
    let mut artifact_id = String::new();
    let mut version = String::new();
    let mut current_tag = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name_bytes = e.name();
                let name = std::str::from_utf8(name_bytes.as_ref())
                    .unwrap_or("")
                    .to_string();
                match name.as_str() {
                    "dependencies" => in_dependencies += 1,
                    "dependency" if in_dependencies > 0 => {
                        in_dependency = true;
                        group_id.clear();
                        artifact_id.clear();
                        version.clear();
                    }
                    _ => current_tag = name,
                }
            }
            Ok(Event::Text(e)) if in_dependency => {
                let text = std::str::from_utf8(e.as_ref())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                match current_tag.as_str() {
                    "groupId" => group_id = text,
                    "artifactId" => artifact_id = text,
                    "version" => version = text,
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name_bytes = e.name();
                let name = std::str::from_utf8(name_bytes.as_ref()).unwrap_or("");
                match name {
                    "dependencies" => {
                        in_dependencies = in_dependencies.saturating_sub(1);
                    }
                    "dependency" if in_dependency => {
                        in_dependency = false;
                        current_tag.clear();
                        if !group_id.is_empty() && !artifact_id.is_empty() {
                            let ver = if version.is_empty() { "*" } else { &version };
                            let sig = format!("pkg:{group_id}:{artifact_id}@{ver}");
                            let node =
                                Node::new(VName::new("maven.org", "", "", "xml", &sig), "package");
                            let edge = Edge::new(file_id, node.id, EdgeKind::ExternalDependency);
                            out.nodes.push(node);
                            out.edges.push(edge);
                        }
                    }
                    _ => current_tag.clear(),
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_tmp(content: &str, suffix: &str) -> NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(suffix).tempfile().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn data_format_parse_emits_single_file_node() {
        let path = Path::new("config.json");
        let out = parse("github.com/a/b", path, "config.json").unwrap();
        assert_eq!(out.nodes.len(), 1, "exactly one file node");
        assert_eq!(out.edges.len(), 0);
        let node = &out.nodes[0];
        assert_eq!(node.vname.language, "json");
        assert_eq!(node.vname.signature, "file");
        assert_eq!(node.vname.path, "config.json");
        assert_eq!(node.kind, "file");
    }

    #[test]
    fn data_format_parse_yaml_uses_yaml_language() {
        let path = Path::new("ci.yml");
        let out = parse("github.com/a/b", path, ".github/workflows/ci.yml").unwrap();
        assert_eq!(out.nodes[0].vname.language, "yaml");
    }

    #[test]
    fn data_format_parse_toml_uses_toml_language() {
        let path = Path::new("Cargo.toml");
        let out = parse("github.com/a/b", path, "Cargo.toml").unwrap();
        assert_eq!(out.nodes[0].vname.language, "toml");
    }

    #[test]
    fn data_format_parse_xml_uses_xml_language() {
        let path = Path::new("pom.xml");
        let out = parse("github.com/a/b", path, "pom.xml").unwrap();
        assert_eq!(out.nodes[0].vname.language, "xml");
    }

    #[test]
    fn package_json_emits_external_dependency_edges() {
        let content = r#"{
            "name": "myapp",
            "dependencies": { "express": "^4.18.0", "lodash": "4.17.21" },
            "devDependencies": { "typescript": "^5.0.0" }
        }"#;
        let f = write_tmp(content, "package.json");
        let out = parse("github.com/a/b", f.path(), "package.json").unwrap();

        // file node + 3 package nodes
        assert_eq!(out.nodes.len(), 4);
        assert_eq!(out.edges.len(), 3);
        assert!(out
            .edges
            .iter()
            .all(|e| e.kind == EdgeKind::ExternalDependency));

        let corpora: Vec<_> = out.nodes[1..]
            .iter()
            .map(|n| n.vname.corpus.as_str())
            .collect();
        assert!(corpora.iter().all(|c| *c == "npmjs.com"));

        let sigs: Vec<_> = out.nodes[1..]
            .iter()
            .map(|n| n.vname.signature.as_str())
            .collect();
        assert!(sigs.iter().any(|s| s.starts_with("pkg:express@")));
        assert!(sigs.iter().any(|s| s.starts_with("pkg:lodash@")));
        assert!(sigs.iter().any(|s| s.starts_with("pkg:typescript@")));
    }

    #[test]
    fn package_json_malformed_degrades_to_file_node_only() {
        let f = write_tmp("{ not valid json", "package.json");
        let out = parse("github.com/a/b", f.path(), "package.json").unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.edges.len(), 0);
    }

    #[test]
    fn tsconfig_json_emits_configures_edges() {
        let content = r#"{
            "compilerOptions": { "strict": true },
            "references": [{ "path": "./packages/core" }, { "path": "./packages/ui" }]
        }"#;
        let f = write_tmp(content, "tsconfig.json");
        let out = parse("github.com/a/b", f.path(), "tsconfig.json").unwrap();

        // file node + 2 target nodes
        assert_eq!(out.nodes.len(), 3);
        assert_eq!(out.edges.len(), 2);
        assert!(out.edges.iter().all(|e| e.kind == EdgeKind::Configures));
    }

    #[test]
    fn tsconfig_nested_dir_normalises_interior_dot_in_target_path() {
        // A tsconfig in a subdirectory with a "./"-prefixed ref must resolve to a
        // clean repo-relative path (no interior "/./"), so the Configures edge's
        // target NodeId matches the real file node.
        let content = r#"{ "references": [{ "path": "./pkg/core" }] }"#;
        let f = write_tmp(content, "tsconfig.json");
        let out = parse("github.com/a/b", f.path(), "sub/tsconfig.json").unwrap();
        assert_eq!(out.nodes.len(), 2);
        assert_eq!(out.nodes[1].vname.path, "sub/pkg/core");
    }

    #[test]
    fn cargo_toml_emits_external_dependency_edges() {
        let content = r#"
[package]
name = "mylib"
version = "0.1.0"

[dependencies]
serde = "1"
anyhow = { version = "1.0", features = ["std"] }

[dev-dependencies]
tempfile = "3"
"#;
        let f = write_tmp(content, "Cargo.toml");
        let out = parse("github.com/a/b", f.path(), "Cargo.toml").unwrap();

        // file node + 3 crate nodes
        assert_eq!(out.nodes.len(), 4);
        assert_eq!(out.edges.len(), 3);
        assert!(out
            .edges
            .iter()
            .all(|e| e.kind == EdgeKind::ExternalDependency));

        let corpora: Vec<_> = out.nodes[1..]
            .iter()
            .map(|n| n.vname.corpus.as_str())
            .collect();
        assert!(corpora.iter().all(|c| *c == "crates.io"));

        let sigs: Vec<_> = out.nodes[1..]
            .iter()
            .map(|n| n.vname.signature.as_str())
            .collect();
        assert!(sigs.iter().any(|s| s.starts_with("pkg:serde@")));
        assert!(sigs.iter().any(|s| s.starts_with("pkg:anyhow@")));
        assert!(sigs.iter().any(|s| s.starts_with("pkg:tempfile@")));
    }

    #[test]
    fn cargo_toml_malformed_degrades_to_file_node_only() {
        let f = write_tmp("[not valid\ntoml = {", "Cargo.toml");
        let out = parse("github.com/a/b", f.path(), "Cargo.toml").unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.edges.len(), 0);
    }

    #[test]
    fn github_actions_workflow_emits_external_dependency_for_uses() {
        let content = r#"
name: CI
on: [push]
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: Just a run step
        run: cargo test
"#;
        let f = write_tmp(content, "ci.yml");
        let out = parse("github.com/a/b", f.path(), ".github/workflows/ci.yml").unwrap();

        // file node + 2 action nodes
        assert_eq!(out.nodes.len(), 3);
        assert_eq!(out.edges.len(), 2);
        assert!(out
            .edges
            .iter()
            .all(|e| e.kind == EdgeKind::ExternalDependency));

        let sigs: Vec<_> = out.nodes[1..]
            .iter()
            .map(|n| n.vname.signature.as_str())
            .collect();
        assert!(sigs.contains(&"pkg:actions/checkout@v4"));
        assert!(sigs.contains(&"pkg:dtolnay/rust-toolchain@stable"));
    }

    #[test]
    fn github_actions_malformed_degrades_to_file_node_only() {
        let f = write_tmp(": : invalid: yaml: {{{{", "ci.yml");
        let out = parse("github.com/a/b", f.path(), ".github/workflows/ci.yml").unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.edges.len(), 0);
    }

    #[test]
    fn docker_compose_emits_image_and_depends_on_edges() {
        let content = r#"
services:
  web:
    image: nginx:1.25
    depends_on:
      - db
  db:
    image: postgres:16
"#;
        let f = write_tmp(content, "docker-compose.yml");
        let out = parse("github.com/a/b", f.path(), "docker-compose.yml").unwrap();

        // file node + 2 image nodes + 1 service node (db)
        assert_eq!(out.nodes.len(), 4);
        // 2 ExternalDependency (images) + 1 Configures (depends_on)
        assert_eq!(out.edges.len(), 3);

        let ext_dep_count = out
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::ExternalDependency)
            .count();
        let configures_count = out
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Configures)
            .count();
        assert_eq!(ext_dep_count, 2);
        assert_eq!(configures_count, 1);
    }

    #[test]
    fn plain_yaml_emits_file_node_only() {
        let content = "foo: bar\nbaz: 42\n";
        let f = write_tmp(content, "config.yml");
        let out = parse("github.com/a/b", f.path(), "config/app.yml").unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.edges.len(), 0);
    }

    #[test]
    fn pom_xml_emits_external_dependency_edges() {
        let content = r#"<?xml version="1.0"?>
<project>
  <dependencies>
    <dependency>
      <groupId>org.springframework</groupId>
      <artifactId>spring-core</artifactId>
      <version>6.0.0</version>
    </dependency>
    <dependency>
      <groupId>junit</groupId>
      <artifactId>junit</artifactId>
      <version>4.13.2</version>
    </dependency>
  </dependencies>
</project>"#;
        let f = write_tmp(content, "pom.xml");
        let out = parse("github.com/a/b", f.path(), "pom.xml").unwrap();

        // file node + 2 maven package nodes
        assert_eq!(out.nodes.len(), 3);
        assert_eq!(out.edges.len(), 2);
        assert!(out
            .edges
            .iter()
            .all(|e| e.kind == EdgeKind::ExternalDependency));

        let corpora: Vec<_> = out.nodes[1..]
            .iter()
            .map(|n| n.vname.corpus.as_str())
            .collect();
        assert!(corpora.iter().all(|c| *c == "maven.org"));

        let sigs: Vec<_> = out.nodes[1..]
            .iter()
            .map(|n| n.vname.signature.as_str())
            .collect();
        assert!(sigs
            .iter()
            .any(|s| s.starts_with("pkg:org.springframework:spring-core@")));
        assert!(sigs.iter().any(|s| s.starts_with("pkg:junit:junit@")));
    }

    #[test]
    fn pom_xml_missing_version_uses_wildcard() {
        let content = r#"<?xml version="1.0"?>
<project>
  <dependencies>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>lib</artifactId>
    </dependency>
  </dependencies>
</project>"#;
        let f = write_tmp(content, "pom.xml");
        let out = parse("github.com/a/b", f.path(), "pom.xml").unwrap();
        assert_eq!(out.nodes.len(), 2);
        let sig = &out.nodes[1].vname.signature;
        assert_eq!(sig, "pkg:com.example:lib@*");
    }

    #[test]
    fn pom_xml_malformed_degrades_to_file_node_only() {
        let f = write_tmp("<not valid xml ><<", "pom.xml");
        let out = parse("github.com/a/b", f.path(), "pom.xml").unwrap();
        // Malformed XML hits Eof/Err branch — still produces the file node
        assert!(!out.nodes.is_empty());
        assert_eq!(out.nodes[0].kind, "file");
    }

    #[test]
    fn plain_xml_emits_file_node_only() {
        let content = "<config><key>value</key></config>";
        let f = write_tmp(content, "config.xml");
        let out = parse("github.com/a/b", f.path(), "config/app.xml").unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.edges.len(), 0);
    }
}
