//! Phase-A indexing for data/config formats (JSON, YAML, TOML, XML) and the
//! dependency manifests expressed in them.
//!
//! Level 1: emit a single file node per file (no content parsing). This is all a
//!   generic `.json`/`.yaml`/`.toml`/`.xml` gets — config *values* are
//!   intentionally never embedded (local-first resource limits).
//! Level 2: filename-dispatched schema parsers emit typed edges
//!   (`ExternalDependency`, `Configures`) for recognized manifests:
//!     - npm `package.json`, `tsconfig.json`
//!     - Cargo `Cargo.toml` (incl. `[workspace.dependencies]`; member
//!       `{ workspace = true }` versions are resolved cross-file by the indexer)
//!     - Python `pyproject.toml` (PEP 621 + Poetry)
//!     - PHP `composer.json`, Dart `pubspec.yaml`, Go `go.mod`, .NET `*.csproj`
//!     - Maven `pom.xml`, GitHub Actions workflows, docker-compose
//!
//! Dispatch is filename-first, so a manifest is recognized by its canonical name
//! even when its extension is unmapped (`go.mod`) or not a data format
//! (`*.csproj` is XML). The enumeration gates admit those names via
//! [`travsr_core::is_manifest_file`]; keep the two lists in agreement.
//!
//! Prose (`.md`) is *not* handled here — it has its own chunking parser
//! (`crate::markdown`) that produces embeddable `doc-chunk` nodes.
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
    // Use vname_path (repo-relative) for dispatch — abs_path may be a temp file
    // in tests or a canonical path that doesn't reflect the repo structure.
    let file_name = Path::new(vname_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    // Language label for the file node. Extension-mapped formats use their
    // canonical label; name-recognized manifests whose extension is unmapped
    // (`go.mod`) or non-data-format (`*.csproj`) fall back to an honest label.
    let lang_str = abs_path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(Language::from_extension)
        .map(Language::as_str)
        .unwrap_or_else(|| manifest_lang_label(file_name));

    let file_vname = VName::new(corpus, "", vname_path, lang_str, "file");
    let file_node = Node::new(file_vname, "file");
    let file_id = file_node.id;

    let mut out = ParseOutput {
        nodes: vec![file_node],
        ..Default::default()
    };

    // Filename-first dispatch: a manifest is recognized by its canonical name
    // regardless of extension, so extensionless/odd-extension manifests
    // (`go.mod`, `*.csproj`) are handled the same as extension-mapped ones.
    match file_name {
        "package.json" => parse_package_json(corpus, abs_path, file_id, &mut out),
        "tsconfig.json" => parse_tsconfig_json(corpus, abs_path, vname_path, file_id, &mut out),
        "Cargo.toml" => parse_cargo_toml(corpus, abs_path, file_id, &mut out),
        "pyproject.toml" => parse_pyproject_toml(corpus, abs_path, file_id, &mut out),
        "composer.json" => parse_composer_json(corpus, abs_path, file_id, &mut out),
        "pubspec.yaml" | "pubspec.yml" => parse_pubspec_yaml(corpus, abs_path, file_id, &mut out),
        "go.mod" => parse_go_mod(abs_path, file_id, &mut out),
        "pom.xml" => parse_pom_xml(abs_path, file_id, &mut out),
        n if n.ends_with(".csproj") => parse_csproj(abs_path, file_id, &mut out),
        n if n.ends_with(".yml") || n.ends_with(".yaml") => {
            parse_yaml(corpus, abs_path, vname_path, file_id, &mut out);
        }
        _ => {}
    }

    Ok(out)
}

/// File-node language label for a name-recognized manifest whose extension is
/// unmapped (`go.mod`) or not itself a data format (`*.csproj` is XML). Falls
/// back to `"json"` for anything else that reaches `data_format::parse` without
/// a mapped extension (defensive; such files are otherwise inert).
fn manifest_lang_label(file_name: &str) -> &'static str {
    if file_name == "go.mod" {
        "go"
    } else if file_name.ends_with(".csproj") {
        "xml"
    } else {
        "json"
    }
}

// ── JSON ──────────────────────────────────────────────────────────────────────

/// Normalise a joined path to a repo-relative, forward-slash string so the
/// resulting `NodeId` matches the real file node. Drops every `.` component
/// (leading *and* interior, e.g. `sub/./pkg`) and collapses interior `..`
/// against the preceding segment (`packages/x/../base` → `packages/base`). A
/// leading `..` that escapes the repo root is preserved. Used for tsconfig
/// `extends`/`references` target resolution.
fn normalise_repo_path(raw: &Path) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in raw.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if matches!(parts.last(), Some(&p) if p != "..") {
                    parts.pop();
                } else {
                    parts.push("..");
                }
            }
            std::path::Component::Normal(s) => {
                if let Some(s) = s.to_str() {
                    parts.push(s);
                }
            }
            _ => {}
        }
    }
    parts.join("/")
}

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

    // Use the vname_path (repo-relative) parent dir so the resolved
    // target path stays repo-relative, not absolute.
    let vname_dir = Path::new(vname_path).parent().unwrap_or(Path::new(""));

    // A3: `"extends"` inherits from a base config. Emit a Configures edge to the
    // base when it is a relative path (the common case). TypeScript appends
    // `.json` when the reference omits an extension, so mirror that. Bare
    // package-name extends (`@tsconfig/node18/tsconfig.json`) resolve through
    // node_modules and are left alone — they are not repo-local file nodes.
    if let Some(extends) = v.get("extends").and_then(|e| e.as_str()) {
        if extends.starts_with('.') {
            let with_ext = if Path::new(extends).extension().is_some() {
                extends.to_string()
            } else {
                format!("{extends}.json")
            };
            let target_path = normalise_repo_path(&vname_dir.join(&with_ext));
            let target_node =
                Node::new(VName::new(corpus, "", &target_path, "json", "file"), "file");
            out.edges
                .push(Edge::new(file_id, target_node.id, EdgeKind::Configures));
            out.nodes.push(target_node);
        }
    }

    if let Some(refs) = v.get("references").and_then(|r| r.as_array()) {
        for r in refs {
            let Some(ref_path) = r.get("path").and_then(|p| p.as_str()) else {
                continue;
            };
            let target_path = normalise_repo_path(&vname_dir.join(ref_path));
            let target_node =
                Node::new(VName::new(corpus, "", &target_path, "json", "file"), "file");
            let edge = Edge::new(file_id, target_node.id, EdgeKind::Configures);
            out.nodes.push(target_node);
            out.edges.push(edge);
        }
    }
}

// ── TOML ─────────────────────────────────────────────────────────────────────

/// Cross-file marker for Cargo workspace dependency resolution (A2).
///
/// A workspace **root** `Cargo.toml` declares versions under
/// `[workspace.dependencies]`; **member** crates inherit them with
/// `name = { workspace = true }` and carry no version of their own. A single
/// file cannot resolve a member's version — it lives in a different file — so
/// `parse_cargo_toml` emits these markers and the indexer resolves them once
/// every file in the batch is parsed (mirrors the FFI-marker second pass).
#[derive(Debug, Clone)]
pub enum WorkspaceDepMarker {
    /// The workspace root declares `[workspace.dependencies] name = version`.
    Provider { name: String, version: String },
    /// A member declares `name = { workspace = true }`; the edge from
    /// `member_file` to the versioned package node is emitted at resolution time.
    Consumer { member_file: NodeId, name: String },
}

/// Build the canonical `crates.io` package node for a Cargo dependency.
///
/// Shared by `parse_cargo_toml` and the indexer's workspace-dep resolver so a
/// member's resolved node has the **same** `NodeId` as the versioned node the
/// workspace root emits — otherwise the two would be distinct graph nodes.
pub fn cargo_package_node(name: &str, version: &str) -> Node {
    let sig = format!("pkg:{name}@{version}");
    Node::new(VName::new("crates.io", "", "", "toml", &sig), "package")
}

/// Version string for a Cargo dependency spec (`"1.0"` or `{ version = "1.0" }`).
fn cargo_dep_version(spec: &toml::Value) -> &str {
    match spec {
        toml::Value::String(s) => s.as_str(),
        toml::Value::Table(t) => t.get("version").and_then(|v| v.as_str()).unwrap_or("*"),
        _ => "*",
    }
}

/// A member spec that inherits from the workspace: `{ workspace = true }`.
fn is_workspace_inherited(spec: &toml::Value) -> bool {
    matches!(spec, toml::Value::Table(t)
        if t.get("workspace").and_then(|v| v.as_bool()) == Some(true))
}

fn parse_cargo_toml(_corpus: &str, path: &Path, file_id: NodeId, out: &mut ParseOutput) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(doc) = text.parse::<toml::Value>() else {
        tracing::debug!("data_format: malformed Cargo.toml at {:?}", path);
        return;
    };

    // A1: a workspace root declares its dependency versions under
    // `[workspace.dependencies]`. Emit versioned edges for the root itself and
    // record Provider markers so member `{ workspace = true }` entries resolve.
    if let Some(ws_deps) = doc
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        for (name, spec) in ws_deps {
            let ver = cargo_dep_version(spec);
            let pkg_node = cargo_package_node(name, ver);
            out.edges.push(Edge::new(
                file_id,
                pkg_node.id,
                EdgeKind::ExternalDependency,
            ));
            out.nodes.push(pkg_node);
            out.workspace_dep_markers
                .push(WorkspaceDepMarker::Provider {
                    name: name.clone(),
                    version: ver.to_string(),
                });
        }
    }

    for section in &["dependencies", "dev-dependencies", "build-dependencies"] {
        let Some(deps) = doc.get(section).and_then(|d| d.as_table()) else {
            continue;
        };
        for (name, spec) in deps {
            // A2: a member's `{ workspace = true }` entry carries no version here;
            // defer to the cross-file resolver, which looks it up on the root.
            if is_workspace_inherited(spec) {
                out.workspace_dep_markers
                    .push(WorkspaceDepMarker::Consumer {
                        member_file: file_id,
                        name: name.clone(),
                    });
                continue;
            }
            let ver = cargo_dep_version(spec);
            let pkg_node = cargo_package_node(name, ver);
            out.edges.push(Edge::new(
                file_id,
                pkg_node.id,
                EdgeKind::ExternalDependency,
            ));
            out.nodes.push(pkg_node);
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

// ── Additional dependency manifests (pyproject / composer / pubspec / go.mod / csproj) ──

/// Emit one `ExternalDependency` edge from `file_id` to a synthetic package
/// node `pkg:<name>@<version>` on the given registry `corpus`.
fn emit_package(
    file_id: NodeId,
    corpus: &str,
    lang: &str,
    name: &str,
    version: &str,
    out: &mut ParseOutput,
) {
    let sig = format!("pkg:{name}@{version}");
    let node = Node::new(VName::new(corpus, "", "", lang, &sig), "package");
    out.edges
        .push(Edge::new(file_id, node.id, EdgeKind::ExternalDependency));
    out.nodes.push(node);
}

/// Split a PEP 508 requirement string (`"requests[security]>=2.0 ; python_version<'3.9'"`)
/// into `(name, version_constraint)`. Extras and environment markers are
/// stripped; a bare name yields version `"*"`.
fn pep508_split(spec: &str) -> Option<(String, String)> {
    let spec = spec.split(';').next().unwrap_or(spec).trim();
    let name: String = spec
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    if name.is_empty() {
        return None;
    }
    let mut rest = spec[name.len()..].trim_start();
    // Drop an optional extras group: `[security,socks]`.
    if let Some(after_open) = rest.strip_prefix('[') {
        if let Some(close) = after_open.find(']') {
            rest = after_open[close + 1..].trim_start();
        }
    }
    let ver = if rest.is_empty() {
        "*".to_string()
    } else {
        rest.trim().to_string()
    };
    Some((name, ver))
}

/// Version for a Poetry dependency spec (`"^2.28"` or `{ version = "^2.28" }`).
fn poetry_dep_version(spec: &toml::Value) -> String {
    match spec {
        toml::Value::String(s) => s.clone(),
        toml::Value::Table(t) => t
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("*")
            .to_string(),
        _ => "*".to_string(),
    }
}

/// Parse `pyproject.toml` dependency declarations from both PEP 621
/// (`[project.dependencies]`, `[project.optional-dependencies]`) and Poetry
/// (`[tool.poetry.dependencies]` + `[tool.poetry.group.*.dependencies]`).
fn parse_pyproject_toml(_corpus: &str, path: &Path, file_id: NodeId, out: &mut ParseOutput) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(doc) = text.parse::<toml::Value>() else {
        tracing::debug!("data_format: malformed pyproject.toml at {:?}", path);
        return;
    };

    // PEP 621: `[project].dependencies` is an array of requirement strings.
    if let Some(project) = doc.get("project") {
        if let Some(deps) = project.get("dependencies").and_then(|d| d.as_array()) {
            for d in deps.iter().filter_map(|d| d.as_str()) {
                if let Some((name, ver)) = pep508_split(d) {
                    emit_package(file_id, "pypi.org", "toml", &name, &ver, out);
                }
            }
        }
        // `[project.optional-dependencies]` is a table of requirement arrays.
        if let Some(opt) = project
            .get("optional-dependencies")
            .and_then(|t| t.as_table())
        {
            for arr in opt.values().filter_map(|v| v.as_array()) {
                for d in arr.iter().filter_map(|d| d.as_str()) {
                    if let Some((name, ver)) = pep508_split(d) {
                        emit_package(file_id, "pypi.org", "toml", &name, &ver, out);
                    }
                }
            }
        }
    }

    // Poetry: `[tool.poetry.dependencies]` + per-group dependency tables. The
    // implicit `python` constraint is a language requirement, not a package.
    if let Some(poetry) = doc.get("tool").and_then(|t| t.get("poetry")) {
        let mut poetry_tables = Vec::new();
        if let Some(t) = poetry.get("dependencies").and_then(|d| d.as_table()) {
            poetry_tables.push(t);
        }
        if let Some(groups) = poetry.get("group").and_then(|g| g.as_table()) {
            for gv in groups.values() {
                if let Some(t) = gv.get("dependencies").and_then(|d| d.as_table()) {
                    poetry_tables.push(t);
                }
            }
        }
        for table in poetry_tables {
            for (name, spec) in table {
                if name == "python" {
                    continue;
                }
                let ver = poetry_dep_version(spec);
                emit_package(file_id, "pypi.org", "toml", name, &ver, out);
            }
        }
    }
}

/// Parse `composer.json` (`require` + `require-dev`). Platform requirements
/// (`php`, `ext-*`, `lib-*`) are not Packagist packages and are skipped.
fn parse_composer_json(_corpus: &str, path: &Path, file_id: NodeId, out: &mut ParseOutput) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        tracing::debug!("data_format: malformed composer.json at {:?}", path);
        return;
    };

    for section in &["require", "require-dev"] {
        if let Some(deps) = v.get(section).and_then(|d| d.as_object()) {
            for (name, version) in deps {
                if name == "php" || name.starts_with("ext-") || name.starts_with("lib-") {
                    continue;
                }
                let ver = version.as_str().unwrap_or("*");
                emit_package(file_id, "packagist.org", "json", name, ver, out);
            }
        }
    }
}

/// Parse `pubspec.yaml` (`dependencies` + `dev_dependencies`). A dependency
/// value may be a version string or a map (`sdk:`, `git:`, `path:`, `version:`).
fn parse_pubspec_yaml(_corpus: &str, path: &Path, file_id: NodeId, out: &mut ParseOutput) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(docs) = YamlLoader::load_from_str(&text) else {
        tracing::debug!("data_format: malformed pubspec.yaml at {:?}", path);
        return;
    };
    let Some(doc) = docs.into_iter().next() else {
        return;
    };

    for section in &["dependencies", "dev_dependencies"] {
        let Some(map) = doc[*section].as_hash() else {
            continue;
        };
        for (k, val) in map {
            let Some(name) = k.as_str() else {
                continue;
            };
            let ver = match val {
                Yaml::String(s) => s.clone(),
                Yaml::Real(s) => s.clone(),
                Yaml::Integer(i) => i.to_string(),
                Yaml::Hash(_) => {
                    if let Some(v) = val["version"].as_str() {
                        v.to_string()
                    } else if let Some(sdk) = val["sdk"].as_str() {
                        format!("sdk:{sdk}")
                    } else {
                        "*".to_string()
                    }
                }
                _ => "*".to_string(),
            };
            emit_package(file_id, "pub.dev", "yaml", name, &ver, out);
        }
    }
}

/// Parse `go.mod` `require` directives (both the grouped `require ( … )` block
/// and single-line `require <module> <version>`). `// indirect` and other
/// trailing comments are stripped; `replace`/`exclude`/`module`/`go` lines are
/// ignored.
fn parse_go_mod(path: &Path, file_id: NodeId, out: &mut ParseOutput) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let mut in_require = false;
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if in_require {
            if line == ")" {
                in_require = false;
            } else {
                emit_go_require(line, file_id, out);
            }
        } else if line == "require (" || line == "require(" {
            in_require = true;
        } else if let Some(rest) = line.strip_prefix("require ") {
            emit_go_require(rest.trim(), file_id, out);
        }
    }
}

/// Emit a Go module requirement (`<module-path> <version>`).
fn emit_go_require(spec: &str, file_id: NodeId, out: &mut ParseOutput) {
    let mut it = spec.split_whitespace();
    if let (Some(name), Some(ver)) = (it.next(), it.next()) {
        emit_package(file_id, "go.dev", "go", name, ver, out);
    }
}

/// Local element name (namespace-stripped) for a quick-xml start/end tag.
fn xml_local_name(name: quick_xml::name::QName) -> String {
    std::str::from_utf8(name.local_name().as_ref())
        .unwrap_or("")
        .to_string()
}

/// Read the `Include` and `Version` attributes of a `<PackageReference>` tag.
fn packageref_attrs(e: &quick_xml::events::BytesStart) -> (Option<String>, Option<String>) {
    let mut include = None;
    let mut version = None;
    for attr in e.attributes().flatten() {
        let key = attr.key.local_name();
        let key = std::str::from_utf8(key.as_ref()).unwrap_or("");
        let val = std::str::from_utf8(attr.value.as_ref())
            .unwrap_or("")
            .to_string();
        match key {
            "Include" => include = Some(val),
            "Version" => version = Some(val),
            _ => {}
        }
    }
    (include, version)
}

/// Parse a `.csproj` (MSBuild XML) for `<PackageReference Include="X" Version="Y" />`
/// entries, including the child-element form
/// `<PackageReference Include="X"><Version>Y</Version></PackageReference>`.
fn parse_csproj(path: &Path, file_id: NodeId, out: &mut ParseOutput) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let mut reader = Reader::from_str(&text);
    reader.config_mut().trim_text(true);

    let mut include: Option<String> = None;
    let mut version: Option<String> = None;
    let mut in_pkgref = false;
    let mut current_tag = String::new();

    loop {
        match reader.read_event() {
            // Self-closing: `<PackageReference Include="X" Version="Y" />`.
            Ok(Event::Empty(e)) => {
                if xml_local_name(e.name()) == "PackageReference" {
                    let (inc, ver) = packageref_attrs(&e);
                    if let Some(name) = inc {
                        let ver = ver.unwrap_or_else(|| "*".to_string());
                        emit_package(file_id, "nuget.org", "xml", &name, &ver, out);
                    }
                }
            }
            Ok(Event::Start(e)) => {
                let name = xml_local_name(e.name());
                if name == "PackageReference" {
                    let (inc, ver) = packageref_attrs(&e);
                    include = inc;
                    version = ver;
                    in_pkgref = true;
                    current_tag.clear();
                } else if in_pkgref {
                    current_tag = name;
                }
            }
            Ok(Event::Text(e)) if in_pkgref && current_tag == "Version" => {
                version = Some(
                    std::str::from_utf8(e.as_ref())
                        .unwrap_or("")
                        .trim()
                        .to_string(),
                );
            }
            Ok(Event::End(e)) => {
                if xml_local_name(e.name()) == "PackageReference" && in_pkgref {
                    if let Some(name) = include.take() {
                        let ver = version.take().unwrap_or_else(|| "*".to_string());
                        emit_package(file_id, "nuget.org", "xml", &name, &ver, out);
                    }
                    in_pkgref = false;
                    current_tag.clear();
                } else {
                    current_tag.clear();
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

    // ── Cargo workspace (A1 + A2) ───────────────────────────────────────────

    fn sigs(out: &ParseOutput) -> Vec<&str> {
        out.nodes[1..]
            .iter()
            .map(|n| n.vname.signature.as_str())
            .collect()
    }

    #[test]
    fn cargo_workspace_root_emits_versioned_deps_and_provider_markers() {
        let content = r#"
[workspace]
members = ["a", "b"]

[workspace.dependencies]
serde = "1.0"
anyhow = { version = "1.0.75", features = ["backtrace"] }
"#;
        let f = write_tmp(content, "Cargo.toml");
        let out = parse("github.com/a/b", f.path(), "Cargo.toml").unwrap();

        let s = sigs(&out);
        assert!(s.contains(&"pkg:serde@1.0"));
        assert!(
            s.contains(&"pkg:anyhow@1.0.75"),
            "table dep keeps its version"
        );
        assert_eq!(out.edges.len(), 2);
        assert!(out
            .edges
            .iter()
            .all(|e| e.kind == EdgeKind::ExternalDependency));

        let providers: Vec<_> = out
            .workspace_dep_markers
            .iter()
            .filter_map(|m| match m {
                WorkspaceDepMarker::Provider { name, version } => {
                    Some((name.as_str(), version.as_str()))
                }
                _ => None,
            })
            .collect();
        assert!(providers.contains(&("serde", "1.0")));
        assert!(providers.contains(&("anyhow", "1.0.75")));
    }

    #[test]
    fn cargo_member_workspace_inherited_defers_via_consumer_marker_not_star() {
        let content = r#"
[package]
name = "member"
version = "0.1.0"

[dependencies]
serde = { workspace = true }
local_dep = "2.0"
"#;
        let f = write_tmp(content, "Cargo.toml");
        let out = parse("github.com/a/b", f.path(), "crates/member/Cargo.toml").unwrap();

        let s = sigs(&out);
        assert!(s.contains(&"pkg:local_dep@2.0"));
        assert!(
            !s.iter().any(|sig| sig.starts_with("pkg:serde@")),
            "an inherited dep must NOT emit a versionless @* node (A2 bug)"
        );

        let consumers: Vec<_> = out
            .workspace_dep_markers
            .iter()
            .filter_map(|m| match m {
                WorkspaceDepMarker::Consumer { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(consumers, vec!["serde"]);
    }

    // ── tsconfig extends (A3) ────────────────────────────────────────────────

    #[test]
    fn tsconfig_extends_emits_configures_edge() {
        let content = r#"{ "extends": "./tsconfig.base.json", "compilerOptions": {} }"#;
        let f = write_tmp(content, "tsconfig.json");
        let out = parse("github.com/a/b", f.path(), "packages/x/tsconfig.json").unwrap();
        assert_eq!(out.edges.len(), 1);
        assert_eq!(out.edges[0].kind, EdgeKind::Configures);
        assert_eq!(out.nodes[1].vname.path, "packages/x/tsconfig.base.json");
    }

    #[test]
    fn tsconfig_extends_appends_json_when_extension_omitted() {
        let content = r#"{ "extends": "../base" }"#;
        let f = write_tmp(content, "tsconfig.json");
        let out = parse("github.com/a/b", f.path(), "packages/x/tsconfig.json").unwrap();
        assert_eq!(out.nodes[1].vname.path, "packages/base.json");
    }

    #[test]
    fn tsconfig_extends_bare_package_name_is_ignored() {
        let content = r#"{ "extends": "@tsconfig/node18/tsconfig.json" }"#;
        let f = write_tmp(content, "tsconfig.json");
        let out = parse("github.com/a/b", f.path(), "tsconfig.json").unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.edges.len(), 0);
    }

    // ── Cross-language manifests (B1/B2) ─────────────────────────────────────

    #[test]
    fn pyproject_pep621_and_poetry_emit_deps() {
        let content = r#"
[project]
name = "app"
dependencies = ["requests>=2.0", "flask", "httpx[http2]>=0.27; python_version >= '3.8'"]

[project.optional-dependencies]
dev = ["pytest>=7"]

[tool.poetry.dependencies]
python = "^3.11"
rich = "^13.0"

[tool.poetry.group.test.dependencies]
mypy = "^1.0"
"#;
        let f = write_tmp(content, "pyproject.toml");
        let out = parse("github.com/a/b", f.path(), "pyproject.toml").unwrap();
        let s = sigs(&out);
        assert!(s.contains(&"pkg:requests@>=2.0"));
        assert!(s.contains(&"pkg:flask@*"));
        assert!(s.contains(&"pkg:httpx@>=0.27"), "extras + marker stripped");
        assert!(s.contains(&"pkg:pytest@>=7"));
        assert!(s.contains(&"pkg:rich@^13.0"));
        assert!(s.contains(&"pkg:mypy@^1.0"));
        assert!(!s.iter().any(|sig| sig.starts_with("pkg:python@")));
        assert!(out.nodes[1..].iter().all(|n| n.vname.corpus == "pypi.org"));
    }

    #[test]
    fn composer_require_and_dev_skip_platform_packages() {
        let content = r#"{
            "require": { "php": ">=8.1", "monolog/monolog": "^3.0", "ext-json": "*" },
            "require-dev": { "phpunit/phpunit": "^10.0" }
        }"#;
        let f = write_tmp(content, "composer.json");
        let out = parse("github.com/a/b", f.path(), "composer.json").unwrap();
        let s = sigs(&out);
        assert!(s.contains(&"pkg:monolog/monolog@^3.0"));
        assert!(s.contains(&"pkg:phpunit/phpunit@^10.0"));
        assert!(!s
            .iter()
            .any(|sig| sig.contains("php@") || sig.contains("ext-json")));
        assert!(out.nodes[1..]
            .iter()
            .all(|n| n.vname.corpus == "packagist.org"));
    }

    #[test]
    fn pubspec_dependencies_and_dev_emit_deps() {
        let content = r#"
name: myapp
dependencies:
  http: ^1.0.0
  path: any
  flutter:
    sdk: flutter
dev_dependencies:
  test: ^1.24.0
"#;
        let f = write_tmp(content, "pubspec.yaml");
        let out = parse("github.com/a/b", f.path(), "pubspec.yaml").unwrap();
        let s = sigs(&out);
        assert!(s.contains(&"pkg:http@^1.0.0"));
        assert!(s.contains(&"pkg:path@any"));
        assert!(s.contains(&"pkg:flutter@sdk:flutter"));
        assert!(s.contains(&"pkg:test@^1.24.0"));
        assert!(out.nodes[1..].iter().all(|n| n.vname.corpus == "pub.dev"));
    }

    #[test]
    fn go_mod_require_block_and_single_line() {
        let content = r#"
module example.com/foo

go 1.21

require (
    github.com/pkg/errors v0.9.1
    golang.org/x/net v0.17.0 // indirect
)

require github.com/single/dep v1.2.3
"#;
        let f = write_tmp(content, ".mod");
        let out = parse("github.com/a/b", f.path(), "go.mod").unwrap();
        let s = sigs(&out);
        assert!(s.contains(&"pkg:github.com/pkg/errors@v0.9.1"));
        assert!(
            s.contains(&"pkg:golang.org/x/net@v0.17.0"),
            "// indirect stripped"
        );
        assert!(
            s.contains(&"pkg:github.com/single/dep@v1.2.3"),
            "single-line require"
        );
        assert!(out.nodes[1..].iter().all(|n| n.vname.corpus == "go.dev"));
        // Name-recognized manifest with an unmapped extension: honest label.
        assert_eq!(out.nodes[0].vname.language, "go");
    }

    #[test]
    fn csproj_packagereference_attr_and_child_forms() {
        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageReference Include="Newtonsoft.Json" Version="13.0.3" />
    <PackageReference Include="Serilog">
      <Version>3.1.1</Version>
    </PackageReference>
  </ItemGroup>
</Project>"#;
        let f = write_tmp(content, ".csproj");
        let out = parse("github.com/a/b", f.path(), "src/App.csproj").unwrap();
        let s = sigs(&out);
        assert!(
            s.contains(&"pkg:Newtonsoft.Json@13.0.3"),
            "self-closing attr form"
        );
        assert!(s.contains(&"pkg:Serilog@3.1.1"), "child <Version> form");
        assert!(out.nodes[1..].iter().all(|n| n.vname.corpus == "nuget.org"));
        assert_eq!(out.nodes[0].vname.language, "xml");
    }

    #[test]
    fn new_manifests_malformed_degrade_to_file_node_only() {
        for (content, vname) in [
            ("{ not json", "composer.json"),
            ("[not valid\ntoml = {", "pyproject.toml"),
            (": : bad: yaml: {{{{", "pubspec.yaml"),
        ] {
            let f = write_tmp(content, "x");
            let out = parse("c", f.path(), vname).unwrap();
            assert_eq!(out.nodes.len(), 1, "{vname} degrades to file node only");
            assert_eq!(out.edges.len(), 0, "{vname} emits no edges when malformed");
        }
        // go.mod / csproj are best-effort text/XML: junk yields no edges, no panic.
        let f = write_tmp("garbage not a require line", "x");
        let out = parse("c", f.path(), "go.mod").unwrap();
        assert_eq!(out.edges.len(), 0);
        let f = write_tmp("<not valid xml ><<", "x");
        let out = parse("c", f.path(), "App.csproj").unwrap();
        assert_eq!(out.nodes[0].kind, "file");
    }
}
