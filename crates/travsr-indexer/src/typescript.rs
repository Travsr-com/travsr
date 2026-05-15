use std::path::Path;

use anyhow::Context as _;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::emit;
use crate::ParseOutput;

// One combined query covers all definition and import patterns we care about in Sprint 1.
// Sprint 2 will extend this with LSIF-derived call/ref edges.
const QUERIES: &str = r"
(class_declaration name: (type_identifier) @class.name)
(function_declaration name: (identifier) @fn.name)
(method_definition name: (property_identifier) @method.name)
(lexical_declaration (variable_declarator name: (identifier) @var.name))
(variable_declaration (variable_declarator name: (identifier) @var.name))
(import_statement source: (string (string_fragment) @import.source))
";

pub fn parse(path: &Path) -> anyhow::Result<ParseOutput> {
    let source = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;

    let is_tsx = path.extension().and_then(|e| e.to_str()) == Some("tsx");
    let language = if is_tsx {
        tree_sitter_typescript::language_tsx()
    } else {
        tree_sitter_typescript::language_typescript()
    };

    let path_str = path.to_string_lossy().replace('\\', "/");
    let file_node = emit::file_node(&path_str);
    let file_id = file_node.id;

    let mut output = ParseOutput {
        nodes: vec![file_node],
        edges: vec![],
    };

    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .context("loading TypeScript grammar")?;

    // A parse failure (None) is not an I/O error; still emit the file node.
    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => return Ok(output),
    };

    let query = Query::new(&language, QUERIES).context("compiling tree-sitter query")?;

    // Collect owned names before the cursor borrows the query.
    let capture_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let mut cursor = QueryCursor::new();
    let captures = cursor.captures(&query, tree.root_node(), source.as_slice());

    for (m, cap_idx) in captures {
        let capture = m.captures[cap_idx];
        let Some(cap_name) = capture_names.get(capture.index as usize) else {
            continue;
        };
        let text = match capture.node.utf8_text(source.as_slice()) {
            Ok(t) => t,
            Err(_) => continue,
        };

        match cap_name.as_str() {
            "class.name" => {
                let node = emit::class_node(&path_str, text);
                let edge = emit::defines_edge(file_id, node.id);
                output.nodes.push(node);
                output.edges.push(edge);
            }
            "fn.name" => {
                let node = emit::fn_node(&path_str, text);
                let edge = emit::defines_edge(file_id, node.id);
                output.nodes.push(node);
                output.edges.push(edge);
            }
            "method.name" => {
                // Edge hierarchy (Tech Lead sign-off): class→method, not file→method.
                let class_name = find_parent_class_name(capture.node, source.as_slice())
                    .unwrap_or_else(|| "<anonymous>".to_string());
                let class_id = emit::class_node(&path_str, &class_name).id;
                let node = emit::method_node(&path_str, &class_name, text);
                let edge = emit::defines_edge(class_id, node.id);
                output.nodes.push(node);
                output.edges.push(edge);
            }
            "var.name" => {
                let node = emit::var_node(&path_str, text);
                let edge = emit::defines_edge(file_id, node.id);
                output.nodes.push(node);
                output.edges.push(edge);
            }
            "import.source" => {
                let node = emit::import_node(&path_str, text);
                let edge = emit::depends_edge(file_id, node.id);
                output.nodes.push(node);
                output.edges.push(edge);
            }
            _ => {}
        }
    }

    Ok(output)
}

fn find_parent_class_name(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut current = node.parent()?;
    loop {
        if matches!(current.kind(), "class_declaration" | "class") {
            let name = (0..current.child_count())
                .filter_map(|i| current.child(i))
                .find(|child| child.kind() == "type_identifier")
                .and_then(|n| n.utf8_text(source).ok())
                .map(str::to_string);
            return name;
        }
        match current.parent() {
            Some(p) => current = p,
            None => return None,
        }
    }
}
