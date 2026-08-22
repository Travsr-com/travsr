//! Fuzz target: markdown doc-chunker (#376 Phase 1, plan §10).
//!
//! Unlike the per-language Tree-sitter targets, this one drives the pure
//! `chunk_markdown` function directly rather than going through a temp file:
//! the chunker is hand-written string processing with no grammar and no
//! subprocess, so the interesting surface is the chunking logic itself, not
//! file IO.
//!
//! Asserts the three properties plan §10 requires of the chunker, on arbitrary
//! input:
//!
//! 1. **No panic.** The chunker slices by byte offset in several places; any
//!    index that is not on a char boundary is a crash on the indexing path,
//!    reachable by committing a `.md` file.
//! 2. **The partition property.** Spans are 1-based, well-formed, ordered,
//!    non-overlapping, and inside the file. A violated span is a `get_snippets`
//!    read of the wrong lines, or of lines past EOF.
//! 3. **Anchor uniqueness.** An anchor is the VName `signature`, so two chunks
//!    sharing one collapse to a single `NodeId`: the second silently overwrites
//!    the first and a whole doc section becomes unretrievable. The `~2` / `#2`
//!    disambiguators exist to prevent exactly this, and adversarial headings
//!    (duplicate trails, slug-truncation collisions, literal `~2` in a heading)
//!    are precisely what would defeat them.
#![no_main]

use libfuzzer_sys::fuzz_target;
use std::collections::HashSet;
use travsr_analysis::markdown::chunk_markdown;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        // Non-UTF-8 never reaches the chunker: the caller degrades to a file
        // node with no chunks. Nothing to assert.
        return;
    };

    let chunks = chunk_markdown(text, "docs/fuzz.md");

    // Determinism: the same bytes must yield the same chunks, since NodeId
    // stability across re-indexes depends on it.
    assert_eq!(
        chunks,
        chunk_markdown(text, "docs/fuzz.md"),
        "chunk_markdown is not deterministic"
    );

    let total_lines = text.lines().count();
    let mut prev_end = 0usize;
    let mut anchors: HashSet<&str> = HashSet::new();

    for c in &chunks {
        assert!(
            c.line_start >= 1,
            "line_start is 1-based, got {}",
            c.line_start
        );
        assert!(
            c.line_start <= c.line_end,
            "inverted span {}..={}",
            c.line_start,
            c.line_end
        );
        assert!(
            c.line_end <= total_lines,
            "span {}..={} runs past EOF ({total_lines} lines)",
            c.line_start,
            c.line_end
        );
        assert!(
            c.line_start > prev_end,
            "span {}..={} overlaps the previous chunk ending at {prev_end}",
            c.line_start,
            c.line_end
        );
        assert!(
            anchors.insert(c.anchor.as_str()),
            "duplicate anchor {:?}; the two chunks collapse to one NodeId",
            c.anchor
        );
        prev_end = c.line_end;
    }
});
