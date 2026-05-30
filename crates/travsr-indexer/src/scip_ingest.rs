//! SCIP protobuf ingestion — one function covers all 8 SCIP-emitting tools.
//! SCIP schema: https://github.com/sourcegraph/scip

use std::path::Path;
use anyhow::Context as _;
use prost::Message as _;
use travsr_core::{Language, Node, VName};
use crate::ParseOutput;

pub fn ingest_scip(path: &Path, corpus: &str) -> anyhow::Result<ParseOutput> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading SCIP output {}", path.display()))?;
    let index = scip::types::Index::decode(bytes.as_slice())
        .context("decoding SCIP protobuf")?;

    let mut nodes = Vec::new();

    for doc in &index.documents {
        let lang_str = doc.language.to_lowercase();
        let Some(language) = Language::from_str(&lang_str) else { continue };
        let lang_s = language.as_str();
        let rel_path = &doc.relative_path;

        // File node
        let file_vname = VName::new(corpus, "", rel_path, lang_s, "file");
        nodes.push(Node::new(file_vname, "file"));

        // Build a line-number index from occurrences with Definition role (bit 0)
        let mut def_lines: std::collections::HashMap<&str, u32> =
            std::collections::HashMap::new();
        for occ in &doc.occurrences {
            if occ.symbol_roles & 1 == 1 {
                if let Some(line) = occ.range.first() {
                    def_lines.insert(&occ.symbol, (*line as u32) + 1);
                }
            }
        }

        for sym in &doc.symbols {
            if sym.symbol.is_empty() || sym.symbol.starts_with("local ") {
                continue;
            }
            let kind = scip_kind_to_node_kind(sym.kind);
            if kind == "local" {
                continue; // skip locals
            }
            let line = def_lines.get(sym.symbol.as_str()).copied().unwrap_or(0);
            let vname = VName::new(corpus, "", rel_path, lang_s, &sym.symbol);
            let mut node = Node::new(vname, kind);
            if line > 0 {
                node = node.with_line(line);
            }
            nodes.push(node);
        }
    }

    Ok(ParseOutput { nodes, edges: vec![], ffi_markers: vec![] })
}

fn scip_kind_to_node_kind(kind: i32) -> &'static str {
    // scip::types::SymbolKind values (generated from proto enum)
    match kind {
        8 => "class",       // Class
        14 => "enum",       // Enum
        15 => "class",      // EnumMember (group under class)
        25 => "interface",  // Interface
        35 => "module",     // Module
        36 => "module",     // Namespace
        41 => "module",     // Package
        46 => "interface",  // Protocol
        60 => "class",      // Struct
        65 => "interface",  // Trait
        67 => "class",      // Type
        68 => "class",      // TypeAlias
        73 => "class",      // Union
        1 | 32 | 56 | 21 | 10 | 22 | 49 | 70 | 66 | 52 | 51 => "function", // methods/functions/constructors
        30 => "local",      // Local — skip
        _ => "symbol",
    }
}
