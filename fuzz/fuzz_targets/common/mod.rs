//! Shared driver for the per-language grammar fuzz targets.
//!
//! These grammars are reachable only through `PluginIndexer`, which dispatches
//! to the in-process plugins registered by ADR-017 Rule 4.
//! `travsr_indexer::Indexer` handles TypeScript, Rust, Python, Go, the data
//! formats and Markdown, and returns an empty `ParseOutput` for every other
//! extension, so a target driving it would fuzz a no-op.

use std::cell::RefCell;

use travsr_plugin_host::PluginIndexer;

const CORPUS: &str = "github.com/travsr/fuzz";

thread_local! {
    /// Built once per process: `PluginIndexer::new` compiles the Tree-sitter
    /// query of every registered grammar, which costs far more than one parse.
    static INDEXER: RefCell<PluginIndexer> = RefCell::new(PluginIndexer::new(CORPUS));
}

/// Write `data` to a temp file named `input.<ext>` and parse it with the
/// in-process grammar registered for that extension.
pub fn parse_bytes_as(data: &[u8], ext: &str) {
    // Unwrap is intentional: tempfile creation failing here is a system-level
    // failure unrelated to the fuzz input, not a bug in the parser.
    let dir = tempfile::tempdir().unwrap();
    let name = format!("input.{ext}");
    let path = dir.path().join(&name);
    std::fs::write(&path, data).unwrap();
    INDEXER.with(|indexer| {
        let _ = indexer.borrow_mut().parse_file_with_vname(&path, &name);
    });
}
