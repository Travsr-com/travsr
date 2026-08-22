//! Phase-A indexing for prose (#376): a heading-scoped chunker for Markdown/MDX.
//!
//! A doc section is a node — that's the entire feature (see
//! `docs/plans/376-doc-prose-retrieval.md` §1). Every existing node-level
//! capability (embedding eligibility, FTS, RBAC, snippets, incremental delta)
//! already works on any node with a distinct signature, so this module's only
//! job is turning markdown bytes into a deterministic, non-overlapping list of
//! heading-anchored chunks. No embedding, no retrieval, no subprocess — pure
//! string processing, strictly safer than every existing Phase A parser.
//!
//! Malformed-input policy matches `data_format.rs`: anything that can't be
//! chunked (non-UTF-8, excluded path) degrades to a `file` node only, never
//! a panic.

use std::path::Path;

use travsr_core::{Edge, EdgeKind, Language, Node, VName};

use crate::ParseOutput;

// ── Tunables (§3.2-3.3 of the plan; values are bench-measured, not guesses) ──

/// A childless heading's body is merged into the preceding section once its
/// own content (excluding the heading line) is under this length. Calibrated
/// against two bench corpora: 400 chars gives 646 chunks (k8s) / 1498 (travsr)
/// at p50 765-789 chars; 200 over-fragmented to ~11 chunks/file.
const MERGE_UP_CHARS: usize = 400;
/// Reuses `EmbedRichness::Standard`'s cap (`skeleton.rs::embed_standard`) so a
/// bigger embedding model automatically gets bigger chunks without a second
/// constant to keep in sync by hand.
const SPLIT_CHARS: usize = 2600;
/// A single paragraph (no internal blank-line break) this size or larger is
/// hard-split at the character level, ignoring paragraph boundaries. Without
/// this a chunk containing one giant table or unbroken fence can survive
/// whole — a bench run on kubernetes produced a single 362,082-char chunk.
const HARD_SPLIT_CHARS: usize = SPLIT_CHARS + SPLIT_CHARS / 2;
/// A fenced code block spanning this many lines or more has its *embed text*
/// (not its stored span) elided to the first [`FENCE_KEEP_LINES`] lines — a
/// doc chunk must not become a code dump.
const FENCE_ELIDE_THRESHOLD_LINES: usize = 40;
const FENCE_KEEP_LINES: usize = 10;
/// Per-segment and total slug length caps (§3.2).
const SLUG_SEGMENT_MAX_CHARS: usize = 48;
const SLUG_TRAIL_MAX_CHARS: usize = 160;

// ── Public entry point ───────────────────────────────────────────────────────

/// Parse a single Markdown/MDX file into a `file` node plus one `doc-chunk`
/// node per retrievable heading section, with `DefinesBinding` edges from the
/// file to each chunk (mirrors `data_format::parse`'s file-node shape so
/// `travsr graph <path>.md` works the same way it does for any other file).
///
/// `extra_excludes` are additional path substrings from `docs.exclude` config
/// (env-var driven — see `travsr-indexer`'s `Indexer::with_doc_excludes`);
/// empty by default.
pub fn parse(
    corpus: &str,
    abs_path: &Path,
    vname_path: &str,
    extra_excludes: &[String],
) -> anyhow::Result<ParseOutput> {
    let lang_str = Language::Markdown.as_str();
    let file_vname = VName::new(corpus, "", vname_path, lang_str, "file");
    let file_node = Node::new(file_vname, "file");
    let file_id = file_node.id;

    let mut out = ParseOutput {
        nodes: vec![file_node],
        ..Default::default()
    };

    if is_excluded(vname_path, extra_excludes) {
        return Ok(out);
    }

    let Ok(bytes) = std::fs::read(abs_path) else {
        return Ok(out);
    };
    let Ok(text) = String::from_utf8(bytes) else {
        tracing::debug!(
            "markdown: non-UTF-8 content at {:?}, file node only, no chunks",
            abs_path
        );
        return Ok(out);
    };

    for chunk in chunk_markdown(&text, vname_path) {
        let sig = format!("doc:{}", chunk.anchor);
        let vname = VName::new(corpus, "", vname_path, lang_str, sig);
        let node = Node::new(vname, "doc-chunk")
            .with_line(chunk.line_start as u32)
            .with_end_line(chunk.line_end as u32);
        out.edges
            .push(Edge::new(file_id, node.id, EdgeKind::DefinesBinding));
        out.nodes.push(node);
    }
    Ok(out)
}

/// Reconstruct the `embed_text` for a doc-chunk node without re-parsing the
/// whole file into `ParseOutput`. `chunk_markdown` is pure and deterministic
/// (same bytes in, same chunks out — the property the proptest in `tests`
/// below asserts), so this is the single source of truth for embed text: it
/// is called both here (discarded, since `ParseOutput`/`Node` carry no text
/// field — matching `data_format.rs`'s precedent) and from
/// `skeleton::embed_texts_for_file`'s doc-chunk branch, which matches chunks
/// back to requested nodes by re-deriving each chunk's `NodeId` from
/// `(corpus, vname_path, anchor)` and comparing.
pub fn chunk_markdown(text: &str, vname_path: &str) -> Vec<Chunk> {
    let stripped = strip_bom(text);
    let doc = ScanResult::scan(stripped);
    build_chunks(&doc, stripped, vname_path)
}

/// One retrievable doc section: a heading trail anchor, its 1-based inclusive
/// line span in the source file, and its ready-to-embed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Slugified heading trail, e.g. `"retrieval-design/token-budget"`, or
    /// `"_preamble"` for content before the first heading. Split pieces of an
    /// oversized section get a `#2`, `#3`, ... suffix; a genuine duplicate
    /// heading trail elsewhere in the file gets a `~2`, `~3`, ... suffix
    /// (§3.2) — the two suffix characters are kept distinct so a NodeId
    /// collision (identical anchor) can never arise from two different causes.
    pub anchor: String,
    pub line_start: usize,
    pub line_end: usize,
    pub embed_text: String,
}

// ── Exclusions (§3.3) ────────────────────────────────────────────────────────

/// Substring match for a directory-scoped exclusion pattern (one ending in
/// `/`), anchored to a path-segment boundary on **both** ends: the character
/// before the match must be `/` or start-of-string, matching the trailing
/// `/` already in `pattern`. Plain [`str::contains`] only anchors the
/// trailing side, so `"api/"` would also match inside `"cri-api/"` or
/// `"downwardapi/"` — real kubernetes directories, not the generated-OpenAPI-
/// reference `api/` the pattern is meant to catch. Patterns with no trailing
/// `/` (filename substrings like `"pull_request_template"`) are unambiguous
/// without this and fall through to a plain substring check.
fn contains_path_segment(lower: &str, pattern: &str) -> bool {
    if !pattern.ends_with('/') {
        return lower.contains(pattern);
    }
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(pos) = lower[from..].find(pattern) {
        let abs = from + pos;
        if abs == 0 || bytes[abs - 1] == b'/' {
            return true;
        }
        from = abs + 1;
    }
    false
}

/// Default filename/path exclusions. Bench-measured to remove 93.3% of doc
/// bytes on kubernetes (mostly `CHANGELOG/`) and 12.2% on travsr, both with
/// near-zero decision-relevant content. Excluded files still get their `file`
/// node (so `travsr graph` on the path works) — they just produce no chunks.
fn is_excluded(vname_path: &str, extra: &[String]) -> bool {
    let lower = vname_path.to_lowercase();
    let file_name = Path::new(&lower)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let stem = file_name
        .strip_suffix(".mdx")
        .or_else(|| file_name.strip_suffix(".markdown"))
        .or_else(|| file_name.strip_suffix(".md"))
        .unwrap_or(file_name);

    const EXCLUDED_STEMS: &[&str] = &[
        "changelog",
        "changes",
        "history",
        "news",
        "releases",
        "license",
        "code_of_conduct",
        "security_contacts",
        "maintainers",
        "owners",
    ];
    if EXCLUDED_STEMS.contains(&stem) {
        return true;
    }

    // Directory-scoped patterns, matched via `contains_path_segment` so the
    // trailing "/" anchors each to a real path-segment boundary — "api/"
    // fires on "pkg/api/README.md" but not "cri-api/README.md" or
    // "downwardapi/README.md". ".app/" specifically catches downloaded app
    // bundles vendored into a repo (travsr's own worst false-positive hit
    // was a VS Code.app SKILL.md dragged in by `.vscode-test/`).
    const EXCLUDED_PATH_SUBSTRINGS: &[&str] = &[
        "changelog/",
        "pull_request_template",
        "issue_template",
        "api/",
        "reference/",
        "generated/",
        ".vscode-test/",
        ".app/",
        "bench/",
    ];
    if EXCLUDED_PATH_SUBSTRINGS
        .iter()
        .any(|p| contains_path_segment(&lower, p))
    {
        return true;
    }

    extra
        .iter()
        .any(|p| !p.is_empty() && contains_path_segment(&lower, p.to_lowercase().as_str()))
}

fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

// ── Scanning: front matter, headings, fences ─────────────────────────────────

#[derive(Debug, Clone)]
struct RawHeading {
    level: u8,
    text: String,
    /// 1-based line number of the heading itself (for Setext, the text line).
    line: usize,
}

struct ScanResult {
    front_matter_title: Option<String>,
    /// 0-based index into `lines()` of the first line after front matter
    /// (0 if there is no front matter).
    content_start: usize,
    headings: Vec<RawHeading>,
    total_lines: usize,
}

impl ScanResult {
    fn scan(text: &str) -> Self {
        let lines: Vec<&str> = text.lines().collect();
        let total_lines = lines.len();

        let (front_matter_title, content_start) = scan_front_matter(&lines);

        let mut headings = Vec::new();
        let mut fence: Option<(char, usize)> = None;
        // Most recent non-blank, non-fence, non-heading line — a Setext
        // underline is only valid immediately after one of these, with no
        // blank line in between (otherwise "---" is a thematic break, not
        // an underline; this is the classic markdown-chunker fence/heading
        // ambiguity the plan calls out).
        let mut pending_para: Option<(usize, String)> = None;

        let mut i = content_start;
        while i < lines.len() {
            let line = lines[i];
            let trimmed = line.trim();

            if let Some((fchar, flen)) = fence {
                if is_fence_close(trimmed, fchar, flen) {
                    fence = None;
                }
                pending_para = None;
                i += 1;
                continue;
            }
            if let Some((fchar, flen)) = fence_open(trimmed) {
                fence = Some((fchar, flen));
                pending_para = None;
                i += 1;
                continue;
            }
            if trimmed.is_empty() {
                pending_para = None;
                i += 1;
                continue;
            }
            if let Some((level, text)) = atx_heading(trimmed) {
                headings.push(RawHeading {
                    level,
                    text,
                    line: i + 1,
                });
                pending_para = None;
                i += 1;
                continue;
            }
            if let Some(level) = setext_underline(trimmed) {
                if let Some((para_line, para_text)) = pending_para.take() {
                    headings.push(RawHeading {
                        level,
                        text: para_text,
                        line: para_line,
                    });
                    i += 1;
                    continue;
                }
                // "---"/"===" with no preceding paragraph: thematic break,
                // not an underline. Falls through to the plain-line case.
            }
            pending_para = Some((i + 1, trimmed.to_string()));
            i += 1;
        }

        Self {
            front_matter_title,
            content_start,
            headings,
            total_lines,
        }
    }
}

/// Front matter is only recognised when `---` is the literal first line
/// (Jekyll/Hugo/Docusaurus convention) — anywhere else `---` is a thematic
/// break or Setext underline, handled by the main scan loop. Returns the
/// extracted `title:` value (if present) and the 0-based index of the first
/// line after the closing delimiter. Bounded scan so a stray leading `---`
/// with no closing delimiter degrades to "no front matter" rather than
/// swallowing the whole file.
fn scan_front_matter(lines: &[&str]) -> (Option<String>, usize) {
    const MAX_SCAN: usize = 200;
    if lines.first().map(|l| l.trim()) != Some("---") {
        return (None, 0);
    }
    let mut title = None;
    for (offset, line) in lines.iter().enumerate().skip(1).take(MAX_SCAN) {
        let t = line.trim();
        if t == "---" || t == "..." {
            return (title, offset + 1);
        }
        if let Some(rest) = t
            .strip_prefix("title:")
            .or_else(|| t.strip_prefix("Title:"))
        {
            let v = rest.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                title = Some(v.to_string());
            }
        }
    }
    // No closing delimiter found — not front matter after all.
    (None, 0)
}

fn fence_open(trimmed: &str) -> Option<(char, usize)> {
    let fchar = trimmed.chars().next()?;
    if fchar != '`' && fchar != '~' {
        return None;
    }
    let run = trimmed.chars().take_while(|&c| c == fchar).count();
    if run >= 3 {
        Some((fchar, run))
    } else {
        None
    }
}

fn is_fence_close(trimmed: &str, fchar: char, open_len: usize) -> bool {
    if !trimmed.starts_with(fchar) {
        return false;
    }
    let run = trimmed.chars().take_while(|&c| c == fchar).count();
    run >= open_len && trimmed.chars().skip(run).all(char::is_whitespace)
}

/// ATX heading: 1-6 `#` followed by a space (or end of line), optional
/// trailing closed-ATX `#`s stripped. Returns `None` for `#######` (7+, not
/// a valid heading) or a bare `#word` with no separating space.
fn atx_heading(trimmed: &str) -> Option<(u8, String)> {
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None; // "#foo" is not a heading, "#[attr]"-style content isn't either
    }
    let text = rest.trim();
    // Closed ATX heading: "## Title ##" — strip a trailing run of '#'.
    let text = text.trim_end_matches('#').trim_end();
    Some((hashes as u8, text.to_string()))
}

/// Setext underline: a line consisting solely of `=` (level 1) or `-`
/// (level 2) characters, one or more, no other content.
fn setext_underline(trimmed: &str) -> Option<u8> {
    if !trimmed.is_empty() && trimmed.chars().all(|c| c == '=') {
        Some(1)
    } else if !trimmed.is_empty() && trimmed.chars().all(|c| c == '-') {
        Some(2)
    } else {
        None
    }
}

// ── Section building: preamble + heading trail + merge-up ───────────────────

struct Section {
    /// Human heading trail, e.g. `["Retrieval Design", "Token Budget"]`.
    /// Empty for the preamble.
    trail: Vec<String>,
    line_start: usize,
    line_end: usize,
    is_preamble: bool,
}

fn build_chunks(doc: &ScanResult, text: &str, vname_path: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let sections = build_sections(doc, &lines);
    if sections.is_empty() {
        return Vec::new();
    }

    let title = doc
        .front_matter_title
        .clone()
        .or_else(|| {
            doc.headings
                .iter()
                .find(|h| h.level == 1)
                .map(|h| h.text.clone())
        })
        .unwrap_or_else(|| {
            Path::new(vname_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("untitled")
                .to_string()
        });

    let mut anchor_counts: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    let mut chunks = Vec::new();

    for section in &sections {
        let base_anchor = if section.is_preamble {
            "_preamble".to_string()
        } else {
            slugify_trail(&section.trail)
        };
        // Duplicate-trail ordinal on the second+ occurrence only (§3.2) — the
        // first occurrence's anchor, and therefore its NodeId, never changes
        // when a later duplicate is added.
        let count = anchor_counts.entry(base_anchor.clone()).or_insert(0);
        *count += 1;
        let anchor = if *count == 1 {
            base_anchor
        } else {
            format!("{base_anchor}~{count}")
        };

        let header = if section.trail.is_empty() {
            format!("doc: {title} | path: {vname_path}")
        } else {
            format!(
                "doc: {} > {} | path: {vname_path}",
                title,
                section.trail.join(" > ")
            )
        };

        // Body excludes the section's own heading line (the trail in `header`
        // already carries it) but includes any merged-in child headings —
        // they're real prose context, not noise.
        let body_from = if section.is_preamble {
            section.line_start
        } else {
            section.line_start + 1
        };
        let pieces = split_section(&lines, body_from, section.line_end);
        for (piece_idx, (start, end)) in pieces.iter().enumerate() {
            let piece_anchor = if piece_idx == 0 {
                anchor.clone()
            } else {
                format!("{anchor}#{}", piece_idx + 1)
            };
            let body = elided_body(&lines, *start, *end);
            let embed_text = if body.is_empty() {
                header.clone()
            } else {
                format!("{header}\n{body}")
            };
            // The stored span always covers the *first* piece's full section
            // start..end_of_this_piece's original range — for a split section
            // each piece owns its own sub-range (§10: contiguous, non-overlapping).
            let node_start = if piece_idx == 0 {
                section.line_start
            } else {
                *start
            };
            chunks.push(Chunk {
                anchor: piece_anchor,
                line_start: node_start,
                line_end: *end,
                embed_text,
            });
        }
    }

    chunks
}

/// `a` is a non-empty, strictly-shorter prefix of `b` — i.e. `a` names a
/// genuine ancestor heading of whatever `b` names, not the same heading and
/// not an unrelated branch.
fn is_strict_prefix(a: &[String], b: &[String]) -> bool {
    !a.is_empty() && a.len() < b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

fn build_sections(doc: &ScanResult, lines: &[&str]) -> Vec<Section> {
    let mut output: Vec<Section> = Vec::new();

    // Preamble: content_start+1 (1-based) .. first heading - 1, or to EOF if
    // there are no headings at all.
    let preamble_start = doc.content_start + 1;
    let preamble_end = doc
        .headings
        .first()
        .map(|h| h.line.saturating_sub(1))
        .unwrap_or(doc.total_lines);
    let preamble_has_content = preamble_end >= preamble_start
        && preamble_start <= doc.total_lines
        && (preamble_start..=preamble_end).any(|ln| !lines[ln - 1].trim().is_empty());
    if preamble_has_content {
        output.push(Section {
            trail: Vec::new(),
            line_start: preamble_start,
            line_end: preamble_end,
            is_preamble: true,
        });
    }

    let mut stack: Vec<(u8, String)> = Vec::new();
    for (i, h) in doc.headings.iter().enumerate() {
        while stack.last().is_some_and(|(lvl, _)| *lvl >= h.level) {
            stack.pop();
        }
        let mut trail: Vec<String> = stack.iter().map(|(_, t)| t.clone()).collect();
        trail.push(h.text.clone());
        stack.push((h.level, h.text.clone()));

        let is_leaf = doc
            .headings
            .get(i + 1)
            .map(|next| next.level <= h.level)
            .unwrap_or(true);
        let section_end = doc
            .headings
            .get(i + 1)
            .map(|next| next.line.saturating_sub(1))
            .unwrap_or(doc.total_lines);
        // Body length excludes the heading line itself.
        let body_len = if h.line < section_end {
            raw_char_len(lines, h.line + 1, section_end)
        } else {
            0
        };

        let merge_target = output
            .last()
            .filter(|s| !s.is_preamble && is_strict_prefix(&s.trail, &trail));
        if is_leaf && body_len < MERGE_UP_CHARS && merge_target.is_some() {
            // Absorb into the immediately preceding output section — but
            // only when that section is a genuine ancestor (its trail is a
            // strict prefix of this heading's trail), never a sibling or
            // cousin. A section with children is never itself a merge
            // candidate (guarded by `is_leaf`), so the nearest preceding
            // output entry is always either this leaf's true parent or an
            // ancestor several levels up whose intervening levels were
            // themselves already absorbed — walking further up than that
            // would require a non-contiguous span, which the partition
            // invariant (§10) forbids. Rejecting a same-depth or unrelated
            // predecessor (e.g. two short top-level sections in a row) is
            // what stops one section's prose from being mislabeled under an
            // unrelated sibling's heading trail.
            output.last_mut().unwrap().line_end = section_end;
        } else {
            output.push(Section {
                trail,
                line_start: h.line,
                line_end: section_end,
                is_preamble: false,
            });
        }
    }

    output
}

// ── Splitting (oversized sections) + fence elision ───────────────────────────

/// Split a section's raw line range into pieces sized for embedding.
/// Operates on the *raw* (non-elided) text so line boundaries always map to
/// real file lines — elision only ever shrinks `embed_text`, never the
/// stored span. Splits at paragraph boundaries (blank lines outside fences);
/// a single paragraph that alone exceeds [`HARD_SPLIT_CHARS`] is hard-split
/// at the character level as a last resort (kubernetes bench run: one section
/// with no paragraph breaks and no huge fence produced a 362,082-char chunk
/// without this).
fn split_section(lines: &[&str], start_1based: usize, end_1based: usize) -> Vec<(usize, usize)> {
    if start_1based > end_1based {
        return vec![(start_1based, end_1based)]; // empty body — nothing to split
    }
    let total_chars = raw_char_len(lines, start_1based, end_1based);
    if total_chars <= SPLIT_CHARS {
        return vec![(start_1based, end_1based)];
    }

    let blocks = paragraph_blocks(lines, start_1based, end_1based);
    let mut pieces: Vec<(usize, usize)> = Vec::new();
    let mut bin_start: Option<usize> = None;
    let mut bin_chars = 0usize;

    for &(bstart, bend) in &blocks {
        let blen = raw_char_len(lines, bstart, bend);
        if blen >= HARD_SPLIT_CHARS {
            // Flush current bin, then hard-split this oversized block alone.
            if let Some(s) = bin_start.take() {
                pieces.push((s, bstart.saturating_sub(1).max(s)));
                bin_chars = 0;
            }
            pieces.extend(hard_split_block(lines, bstart, bend));
            continue;
        }
        match bin_start {
            None => {
                bin_start = Some(bstart);
                bin_chars = blen;
            }
            Some(s) => {
                if bin_chars + blen > SPLIT_CHARS {
                    pieces.push((s, bstart.saturating_sub(1).max(s)));
                    bin_start = Some(bstart);
                    bin_chars = blen;
                } else {
                    bin_chars += blen;
                }
            }
        }
    }
    if let Some(s) = bin_start {
        pieces.push((s, end_1based));
    }
    // Stretch/clean boundaries so pieces exactly partition [start,end] with
    // no gaps regardless of how blank-line separators were assigned above.
    stitch_ranges(pieces, start_1based, end_1based)
}

/// Re-anchor a list of (possibly gappy) ranges so they exactly partition
/// `[start,end]`: each piece's end is stretched to just before the next
/// piece's start, the first starts at `start`, the last ends at `end`.
fn stitch_ranges(mut pieces: Vec<(usize, usize)>, start: usize, end: usize) -> Vec<(usize, usize)> {
    if pieces.is_empty() {
        return vec![(start, end)];
    }
    pieces[0].0 = start;
    for i in 0..pieces.len() - 1 {
        let next_start = pieces[i + 1].0;
        pieces[i].1 = next_start.saturating_sub(1).max(pieces[i].0);
    }
    pieces.last_mut().unwrap().1 = end;
    pieces
}

/// Group a line range into paragraph blocks: maximal runs of non-blank lines,
/// blank lines used only as separators (never inside a fence — a fence's
/// internal blank lines don't split it). Returned ranges are gap-tolerant;
/// [`stitch_ranges`] closes the gaps afterward.
fn paragraph_blocks(lines: &[&str], start_1based: usize, end_1based: usize) -> Vec<(usize, usize)> {
    let mut blocks = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut block_start: Option<usize> = None;

    for ln in start_1based..=end_1based {
        let line = lines[ln - 1];
        let trimmed = line.trim();

        if let Some((fchar, flen)) = fence {
            if is_fence_close(trimmed, fchar, flen) {
                fence = None;
            }
            if block_start.is_none() {
                block_start = Some(ln);
            }
            continue;
        }
        if let Some((fchar, flen)) = fence_open(trimmed) {
            fence = Some((fchar, flen));
            if block_start.is_none() {
                block_start = Some(ln);
            }
            continue;
        }
        if trimmed.is_empty() {
            if let Some(s) = block_start.take() {
                blocks.push((s, ln.saturating_sub(1)));
            }
            continue;
        }
        if block_start.is_none() {
            block_start = Some(ln);
        }
    }
    if let Some(s) = block_start {
        blocks.push((s, end_1based));
    }
    if blocks.is_empty() {
        blocks.push((start_1based, end_1based));
    }
    blocks
}

/// Character-level fallback for a single block that has no internal
/// paragraph breaks and is still >= [`HARD_SPLIT_CHARS`]. Cuts at line
/// boundaries close to [`SPLIT_CHARS`] — ignores fence/paragraph semantics
/// entirely, by design (this only fires when nothing structural is left to
/// split on).
fn hard_split_block(lines: &[&str], start_1based: usize, end_1based: usize) -> Vec<(usize, usize)> {
    let mut pieces = Vec::new();
    let mut cur_start = start_1based;
    let mut cur_chars = 0usize;
    for ln in start_1based..=end_1based {
        let llen = lines[ln - 1].chars().count() + 1; // +1 for the newline
        if cur_chars + llen > SPLIT_CHARS && ln > cur_start {
            pieces.push((cur_start, ln - 1));
            cur_start = ln;
            cur_chars = 0;
        }
        cur_chars += llen;
    }
    pieces.push((cur_start, end_1based));
    pieces
}

fn raw_char_len(lines: &[&str], start_1based: usize, end_1based: usize) -> usize {
    if start_1based > end_1based {
        return 0;
    }
    lines[start_1based - 1..end_1based]
        .iter()
        .map(|l| l.chars().count() + 1)
        .sum()
}

/// Build the final embed-text body for `[start,end]`: identical to the raw
/// text except any fenced block spanning >= [`FENCE_ELIDE_THRESHOLD_LINES`]
/// lines has its interior truncated to the first [`FENCE_KEEP_LINES`] lines
/// plus a marker. The stored span is untouched — a snippet read still shows
/// the whole real fence; only the embedding payload shrinks.
fn elided_body(lines: &[&str], start_1based: usize, end_1based: usize) -> String {
    if start_1based > end_1based {
        return String::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut i = start_1based;
    while i <= end_1based {
        let line = lines[i - 1];
        let trimmed = line.trim();
        if let Some((fchar, flen)) = fence_open(trimmed) {
            // Find the matching close within [i+1, end_1based] (or EOF-of-range).
            let mut close = None;
            for j in (i + 1)..=end_1based {
                if is_fence_close(lines[j - 1].trim(), fchar, flen) {
                    close = Some(j);
                    break;
                }
            }
            let close = close.unwrap_or(end_1based);
            let fence_lines = close - i + 1;
            out.push(line.to_string());
            if fence_lines >= FENCE_ELIDE_THRESHOLD_LINES {
                let keep_end = (i + FENCE_KEEP_LINES).min(close.saturating_sub(1));
                for k in (i + 1)..=keep_end {
                    out.push(lines[k - 1].to_string());
                }
                let elided = close.saturating_sub(1).saturating_sub(keep_end);
                if elided > 0 {
                    out.push(format!("… ({elided} more lines elided)"));
                }
                if close > i {
                    out.push(lines[close - 1].to_string());
                }
            } else {
                for k in (i + 1)..close {
                    out.push(lines[k - 1].to_string());
                }
                if close > i {
                    out.push(lines[close - 1].to_string());
                }
            }
            i = close + 1;
        } else {
            out.push(line.to_string());
            i += 1;
        }
    }
    out.join("\n").trim().to_string()
}

// ── Slugification ─────────────────────────────────────────────────────────────

/// Slugify a heading trail: each segment lowercased, non-alphanumerics
/// collapsed to `-`, capped at [`SLUG_SEGMENT_MAX_CHARS`]; segments joined by
/// `/`; the whole trail capped at [`SLUG_TRAIL_MAX_CHARS`] (tail-truncated —
/// leading segments carry more context than trailing ones).
fn slugify_trail(trail: &[String]) -> String {
    let joined = trail
        .iter()
        .map(|seg| slugify_segment(seg))
        .collect::<Vec<_>>()
        .join("/");
    if joined.chars().count() <= SLUG_TRAIL_MAX_CHARS {
        joined
    } else {
        joined.chars().take(SLUG_TRAIL_MAX_CHARS).collect()
    }
}

fn slugify_segment(text: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("section");
    }
    if slug.chars().count() > SLUG_SEGMENT_MAX_CHARS {
        slug = slug.chars().take(SLUG_SEGMENT_MAX_CHARS).collect();
        while slug.ends_with('-') {
            slug.pop();
        }
    }
    slug
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchors(text: &str) -> Vec<String> {
        chunk_markdown(text, "docs/x.md")
            .into_iter()
            .map(|c| c.anchor)
            .collect()
    }

    fn spans(text: &str) -> Vec<(usize, usize)> {
        chunk_markdown(text, "docs/x.md")
            .into_iter()
            .map(|c| (c.line_start, c.line_end))
            .collect()
    }

    // ── heading trail: levels, skipped levels, Setext ────────────────────────

    #[test]
    fn heading_trail_nests_by_level() {
        // A short "Token Budget" child would merge up into its parent (that
        // behavior has its own dedicated tests below), which would collapse
        // the compound trail this test wants to demonstrate. Use a body long
        // enough (>400 chars) that Token Budget survives as its own node, so
        // the anchor/trail nesting is exercised in isolation from merge-up.
        let text = format!(
            "## Retrieval Design\nintro\n### Token Budget\n{}\n",
            "word ".repeat(90)
        );
        let chunks = chunk_markdown(&text, "docs/x.md");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].anchor, "retrieval-design");
        assert_eq!(chunks[1].anchor, "retrieval-design/token-budget");
        assert!(chunks[1]
            .embed_text
            .starts_with("doc: x > Retrieval Design > Token Budget"));
    }

    #[test]
    fn heading_trail_skipped_levels_still_nest() {
        // H2 straight to H4 with a real (non-leaf) shape: give the H4 its own
        // child so neither merges away, proving the trail still nests
        // correctly across a skipped level.
        let text = "## A\n#### B\n##### C\nbody long enough to avoid merge-up. ".to_string()
            + &"x".repeat(410);
        let chunks = chunk_markdown(&text, "docs/x.md");
        let anchor = &chunks.last().unwrap().anchor;
        assert!(anchor.starts_with("a/b/c"), "got {anchor}");
    }

    #[test]
    fn setext_headings_recognized_as_level_1_and_2() {
        let text = "Title One\n=========\nintro text long enough to survive merge-up. ".to_string()
            + &"x".repeat(410)
            + "\nSubtitle\n--------\nmore body text here too, also long enough. "
            + &"y".repeat(410);
        let chunks = chunk_markdown(&text, "docs/x.md");
        assert_eq!(anchors(&text), vec!["title-one", "title-one/subtitle"]);
        assert!(chunks[0].embed_text.contains("intro text"));
    }

    #[test]
    fn setext_underline_without_preceding_paragraph_is_not_a_heading() {
        // A "---" thematic break right after a blank line (no paragraph text
        // immediately above it) must not be misread as a Setext underline.
        let text = "## Heading\n\n---\n\nSome text after a thematic break.\n";
        let chunks = chunk_markdown(text, "docs/x.md");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].anchor, "heading");
    }

    // ── the classic markdown-chunker bug: fences ──────────────────────────────

    #[test]
    fn hash_lines_inside_fence_are_not_read_as_headings() {
        let text = "## Real Heading\n```\n# not a heading\n## also not\n```\nbody\n";
        assert_eq!(anchors(text), vec!["real-heading"]);
    }

    #[test]
    fn fence_with_tildes_also_suppresses_headings() {
        let text = "## Real Heading\n~~~\n# not a heading\n~~~\nbody\n";
        assert_eq!(anchors(text), vec!["real-heading"]);
    }

    // ── slug: collapsing, truncation ──────────────────────────────────────────

    #[test]
    fn slugify_collapses_non_alphanumerics_and_lowercases() {
        assert_eq!(slugify_segment("Retrieval Design!!"), "retrieval-design");
        assert_eq!(slugify_segment("  Leading/Trailing  "), "leading-trailing");
        assert_eq!(slugify_segment("A_B--C"), "a-b-c");
    }

    #[test]
    fn slugify_segment_truncates_at_max_chars() {
        let long = "word ".repeat(30); // way over 48 chars
        let slug = slugify_segment(&long);
        assert!(slug.chars().count() <= SLUG_SEGMENT_MAX_CHARS);
        assert!(!slug.ends_with('-'));
    }

    #[test]
    fn slugify_trail_truncates_at_total_max_chars() {
        let trail: Vec<String> = (0..10)
            .map(|i| format!("segment-{i}-with-padding"))
            .collect();
        let joined = slugify_trail(&trail);
        assert!(joined.chars().count() <= SLUG_TRAIL_MAX_CHARS);
    }

    // ── duplicate-trail ordinal ────────────────────────────────────────────────

    #[test]
    fn duplicate_heading_trail_gets_ordinal_suffix_on_second_occurrence_only() {
        let text = "## Decision\nfirst\n## Other\nfiller\n## Decision\nsecond\n";
        assert_eq!(anchors(text), vec!["decision", "other", "decision~2"]);
    }

    #[test]
    fn adding_a_third_duplicate_does_not_rename_the_first_two() {
        let text = "## D\na\n## D\nb\n## D\nc\n";
        assert_eq!(anchors(text), vec!["d", "d~2", "d~3"]);
    }

    // ── merge-up: leaf vs children, ancestor-only, siblings never merge ──────

    #[test]
    fn short_childless_leaf_merges_into_parent() {
        let text = "## Parent\nintro\n### Leaf\nshort\n";
        let chunks = chunk_markdown(text, "docs/x.md");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].anchor, "parent");
        assert!(chunks[0].embed_text.contains("Leaf"));
        assert!(chunks[0].embed_text.contains("short"));
    }

    #[test]
    fn section_with_children_never_merges_regardless_of_body_length() {
        // "Parent" has a child ("Child") and an empty own-body, which would
        // make it a merge *candidate* if body length were the only test —
        // but a section with children is never itself a leaf, so it must
        // survive as its own node under "Grandparent" rather than merging
        // away. "Child" (genuinely childless, short body) merges into it as
        // usual, which is what leaves "grandparent/parent" as the deepest
        // surviving anchor rather than three separate nodes.
        let text = "## Grandparent\nintro\n### Parent\n\n#### Child\nbody\n";
        assert_eq!(anchors(text), vec!["grandparent", "grandparent/parent"]);
    }

    #[test]
    fn two_short_top_level_siblings_do_not_merge_into_each_other() {
        // Regression: an earlier implementation merged into "whatever
        // immediately precedes it in document order", which silently folded
        // Section B's prose under Section A's heading trail. Only a genuine
        // ancestor (trail is a strict prefix) may absorb a leaf.
        let text = "## Section A\nshort a\n## Section B\nshort b\n";
        let chunks = chunk_markdown(text, "docs/x.md");
        assert_eq!(anchors(text), vec!["section-a", "section-b"]);
        assert!(chunks[0].embed_text.contains("short a"));
        assert!(!chunks[0].embed_text.contains("short b"));
        assert!(chunks[1].embed_text.contains("short b"));
    }

    #[test]
    fn short_leaf_under_a_title_h1_merges_into_the_title() {
        let text = "# Title\n## Intro\nshort\n## Body\n".to_string() + &"x".repeat(500);
        let chunks = chunk_markdown(&text, "docs/x.md");
        // "Intro" is a short leaf whose parent ("Title") is the immediately
        // preceding, still-standing ancestor — it should merge.
        assert_eq!(chunks[0].anchor, "title");
        assert!(chunks[0].embed_text.contains("Intro"));
    }

    // ── splitting: paragraph boundaries, fence-safety, hard-split fallback ────

    #[test]
    fn oversized_section_splits_at_paragraph_boundaries() {
        let para_a = "a".repeat(1500);
        let para_b = "b".repeat(1500);
        let text = format!("## Big\n{para_a}\n\n{para_b}\n");
        let chunks = chunk_markdown(&text, "docs/x.md");
        assert_eq!(
            chunks.len(),
            2,
            "1500+1500 > 2600 cap, splits into two pieces"
        );
        assert_eq!(chunks[0].anchor, "big");
        assert_eq!(chunks[1].anchor, "big#2");
        assert!(chunks[0].embed_text.contains(&para_a));
        assert!(chunks[1].embed_text.contains(&para_b));
    }

    #[test]
    fn split_never_breaks_inside_a_fence() {
        let fence_body = "line\n".repeat(30); // ~150 chars, well under fence-elide threshold
        let text = format!(
            "## Big\n{}\n\n```\n{fence_body}```\n\n{}\n",
            "a".repeat(1400),
            "b".repeat(1400)
        );
        let chunks = chunk_markdown(&text, "docs/x.md");
        for c in &chunks {
            let opens = c.embed_text.matches("```").count();
            assert_eq!(
                opens % 2,
                0,
                "fence markers must pair up within one chunk: {c:?}"
            );
        }
    }

    #[test]
    fn one_giant_paragraph_with_no_blank_breaks_hard_splits_across_lines() {
        // A giant table with no blank rows (the plan's motivating example):
        // many lines, zero paragraph boundaries. Line-based hard-split must
        // still cut it into bounded pieces.
        let body: String = (0..500).map(|i| format!("| row {i} | value |\n")).collect();
        let text = format!("## Huge\n{body}");
        let chunks = chunk_markdown(&text, "docs/x.md");
        assert!(
            chunks.len() > 2,
            "expected multiple hard-split pieces, got {}",
            chunks.len()
        );
        for c in &chunks {
            assert!(
                c.embed_text.chars().count() <= SPLIT_CHARS + 200,
                "hard-split piece too large: {} chars",
                c.embed_text.chars().count()
            );
        }
    }

    #[test]
    fn one_pathological_unbroken_line_never_panics() {
        // A node's span is line-granular (the whole architecture — snippets,
        // embed-text reconstruction — keys off line numbers), so a single
        // source line with no internal newline anywhere cannot be split
        // across two nodes. The fallback degrades to "one oversized chunk"
        // rather than attempting a sub-line split; the invariant that matters
        // here is "never panic", not "always <= SPLIT_CHARS".
        let text = format!("## Huge\n{}\n", "x".repeat(10_000));
        let chunks = chunk_markdown(&text, "docs/x.md");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].line_start, 1);
        assert_eq!(chunks[0].line_end, 2);
    }

    // ── fence elision ──────────────────────────────────────────────────────────

    #[test]
    fn long_fence_is_elided_in_embed_text_but_not_in_stored_span() {
        let fence_lines: Vec<String> = (0..100).map(|i| format!("code line {i}")).collect();
        let text = format!("## Section\n```\n{}\n```\n", fence_lines.join("\n"));
        let total_lines = text.lines().count();
        let chunks = chunk_markdown(&text, "docs/x.md");
        assert_eq!(chunks.len(), 1);
        // Stored span covers the whole file including the full fence.
        assert_eq!(chunks[0].line_end, total_lines);
        // Embed text keeps only the first FENCE_KEEP_LINES content lines.
        assert!(chunks[0].embed_text.contains("code line 0"));
        assert!(chunks[0].embed_text.contains("code line 9"));
        assert!(!chunks[0].embed_text.contains("code line 50"));
        assert!(chunks[0].embed_text.contains("more lines elided"));
    }

    #[test]
    fn short_fence_is_not_elided() {
        let text = "## Section\n```\nfn f() {}\n```\n";
        let chunks = chunk_markdown(text, "docs/x.md");
        assert!(chunks[0].embed_text.contains("fn f() {}"));
        assert!(!chunks[0].embed_text.contains("elided"));
    }

    // ── front matter ─────────────────────────────────────────────────────────

    #[test]
    fn front_matter_title_is_extracted_and_block_excluded_from_chunks() {
        let text = "---\ntitle: My Doc Title\ndraft: true\n---\n## Section\nbody\n";
        let chunks = chunk_markdown(text, "docs/x.md");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0]
            .embed_text
            .starts_with("doc: My Doc Title > Section"));
        // Front matter lines never appear inside any chunk body.
        assert!(!chunks[0].embed_text.contains("draft: true"));
    }

    #[test]
    fn missing_front_matter_title_falls_back_to_first_h1_then_filename() {
        let with_h1 = "# Fallback Title\n## Section\nbody\n";
        let chunks = chunk_markdown(with_h1, "docs/x.md");
        assert!(chunks
            .last()
            .unwrap()
            .embed_text
            .contains("Fallback Title >"));

        let no_heading_at_all = "just prose, no headings anywhere in this file at all\n";
        let chunks = chunk_markdown(no_heading_at_all, "docs/getting-started.md");
        assert!(chunks[0].embed_text.starts_with("doc: getting-started"));
    }

    #[test]
    fn unterminated_front_matter_delimiter_is_not_treated_as_front_matter() {
        // "---" as line 1 with no closing delimiter anywhere — must degrade to
        // "no front matter" rather than swallowing the whole file.
        let text = "---\n## Section\nbody\n";
        let chunks = chunk_markdown(text, "docs/x.md");
        assert_eq!(chunks[0].line_start, 1); // preamble ("---") is not excluded
    }

    // ── degrade-never-panic: CRLF, BOM, tabs, non-UTF-8, empty ────────────────

    #[test]
    fn crlf_line_endings_produce_the_same_chunks_as_lf() {
        let lf = "## A\nbody one\n## B\nbody two\n";
        let crlf = lf.replace('\n', "\r\n");
        assert_eq!(anchors(lf), anchors(&crlf));
        assert_eq!(spans(lf), spans(&crlf));
    }

    #[test]
    fn bom_is_stripped_and_does_not_corrupt_the_first_heading() {
        let text = "\u{feff}## Heading\nbody\n";
        let chunks = chunk_markdown(text, "docs/x.md");
        assert_eq!(chunks[0].anchor, "heading");
    }

    #[test]
    fn tabs_after_atx_hashes_count_as_the_heading_separator() {
        let text = "##\tTabbed Heading\nbody\n";
        assert_eq!(anchors(text), vec!["tabbed-heading"]);
    }

    #[test]
    fn non_utf8_file_degrades_to_file_node_only_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.md");
        std::fs::write(&path, [0x23, 0x20, 0xff, 0xfe, 0x00, 0x0a]).unwrap();
        let out = parse("github.com/a/b", &path, "bad.md", &[]).unwrap();
        assert_eq!(out.nodes.len(), 1, "file node only, no doc-chunk nodes");
        assert_eq!(out.nodes[0].kind, "file");
    }

    #[test]
    fn empty_file_produces_no_chunks() {
        assert!(chunk_markdown("", "docs/x.md").is_empty());
    }

    #[test]
    fn headings_only_file_with_no_body_never_panics() {
        let text = "## A\n## B\n### C\n";
        // Every section is a zero-body leaf; must not panic on empty ranges.
        let chunks = chunk_markdown(text, "docs/x.md");
        assert!(!chunks.is_empty());
    }

    #[test]
    fn no_heading_file_becomes_a_single_preamble_chunk() {
        let text = "Just a plain paragraph with no headings at all.\n";
        let chunks = chunk_markdown(text, "docs/x.md");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].anchor, "_preamble");
    }

    // ── exclusions (§3.3) ────────────────────────────────────────────────────

    #[test]
    fn changelog_is_excluded_by_default() {
        assert!(is_excluded("CHANGELOG.md", &[]));
        assert!(is_excluded("project/changelog.md", &[]));
        assert!(is_excluded("project/CHANGELOG/2024.md", &[]));
    }

    #[test]
    fn pr_and_issue_templates_are_excluded() {
        assert!(is_excluded(".github/PULL_REQUEST_TEMPLATE.md", &[]));
        assert!(is_excluded(".github/ISSUE_TEMPLATE/bug.md", &[]));
    }

    #[test]
    fn vscode_test_app_bundle_is_excluded() {
        assert!(is_excluded(
            "packages/travsr-vscode/.vscode-test/vscode/Visual Studio Code.app/SKILL.md",
            &[]
        ));
    }

    #[test]
    fn ordinary_doc_is_not_excluded() {
        assert!(!is_excluded("docs/rfcs/RFC-012-plan.md", &[]));
    }

    #[test]
    fn api_dir_pattern_is_anchored_to_path_segment_boundary() {
        // A real "api/" directory segment (generated OpenAPI reference) is
        // still excluded.
        assert!(is_excluded("pkg/generated/api/README.md", &[]));
        assert!(is_excluded("api/README.md", &[]));
        // Directory names that merely end in "api" are not "api/" as a path
        // segment and must survive: staging/src/k8s.io/cri-api/README.md and
        // similar kubernetes docs were silently dropped by a plain
        // `contains("api/")` check before this was anchored.
        assert!(!is_excluded("staging/src/k8s.io/cri-api/README.md", &[]));
        assert!(!is_excluded("pkg/volume/downwardapi/README.md", &[]));
        assert!(!is_excluded(
            "staging/src/k8s.io/kube-openapi/README.md",
            &[]
        ));
    }

    #[test]
    fn extra_exclude_patterns_apply_on_top_of_defaults() {
        assert!(is_excluded(
            "internal/secret-notes.md",
            &["secret-notes".to_string()]
        ));
        assert!(!is_excluded("internal/secret-notes.md", &[]));
    }

    // ── line span invariants ─────────────────────────────────────────────────

    #[test]
    fn spans_are_contiguous_and_non_overlapping() {
        let text = "# T\nintro\n## A\n".to_string()
            + &"x".repeat(500)
            + "\n### A.1\nleaf\n## B\n"
            + &"y".repeat(500)
            + "\n";
        let s = spans(&text);
        assert!(!s.is_empty());
        for w in s.windows(2) {
            assert_eq!(
                w[0].1 + 1,
                w[1].0,
                "gap or overlap between {:?} and {:?}",
                w[0],
                w[1]
            );
        }
        assert_eq!(s[0].0, 1);
        assert_eq!(s.last().unwrap().1, text.lines().count());
    }

    // ── determinism / identity ────────────────────────────────────────────────

    #[test]
    fn same_bytes_twice_yields_byte_identical_chunks() {
        let text = "## A\nbody\n### B\nmore\n## C\n".to_string() + &"z".repeat(3000);
        assert_eq!(
            chunk_markdown(&text, "docs/x.md"),
            chunk_markdown(&text, "docs/x.md")
        );
    }

    #[test]
    fn whitespace_only_edit_inside_one_section_does_not_change_other_anchors() {
        let base = "## A\nfirst\n## B\nsecond\n## C\nthird\n";
        let edited = "## A\nfirst   \n## B\nsecond\n## C\nthird\n"; // trailing spaces added inside A
        assert_eq!(anchors(base), anchors(edited));
    }

    // ── parse(): file node + doc-chunk nodes + edges ──────────────────────────

    #[test]
    fn parse_emits_file_node_doc_chunk_nodes_and_defines_binding_edges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guide.md");
        std::fs::write(&path, "## Overview\nSome content that explains things.\n").unwrap();
        let out = parse("github.com/a/b", &path, "guide.md", &[]).unwrap();

        let file_nodes: Vec<_> = out.nodes.iter().filter(|n| n.kind == "file").collect();
        let doc_nodes: Vec<_> = out.nodes.iter().filter(|n| n.kind == "doc-chunk").collect();
        assert_eq!(file_nodes.len(), 1);
        assert_eq!(doc_nodes.len(), 1);
        assert_eq!(doc_nodes[0].vname.signature, "doc:overview");
        assert_eq!(doc_nodes[0].vname.language, "markdown");
        assert_eq!(doc_nodes[0].line, Some(1));

        assert_eq!(out.edges.len(), 1);
        assert_eq!(out.edges[0].kind, EdgeKind::DefinesBinding);
        assert_eq!(out.edges[0].src, file_nodes[0].id);
        assert_eq!(out.edges[0].dst, doc_nodes[0].id);
    }

    #[test]
    fn parse_excluded_file_emits_only_the_file_node() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("CHANGELOG.md");
        std::fs::write(&path, "## v1.0\nRelease notes.\n").unwrap();
        let out = parse("github.com/a/b", &path, "CHANGELOG.md", &[]).unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].kind, "file");
        assert!(out.edges.is_empty());
    }

    // ── proptest: partition property across generated documents ──────────────

    proptest::proptest! {
        #[test]
        fn prop_spans_always_partition_the_document(
            n_sections in 0usize..12,
            levels in proptest::collection::vec(1u8..4, 0..12),
            body_len in proptest::collection::vec(0usize..600, 0..12),
        ) {
            let mut text = String::new();
            let n = n_sections.min(levels.len()).min(body_len.len());
            for i in 0..n {
                let level = levels[i].max(1);
                text.push_str(&"#".repeat(level as usize));
                text.push_str(&format!(" Heading {i}\n"));
                if body_len[i] > 0 {
                    text.push_str(&"w ".repeat(body_len[i]));
                    text.push('\n');
                }
            }
            let total_lines = text.lines().count();
            let s = spans(&text);
            if total_lines == 0 {
                proptest::prop_assert!(s.is_empty());
            } else if !s.is_empty() {
                proptest::prop_assert_eq!(s[0].0, 1);
                proptest::prop_assert_eq!(s.last().unwrap().1, total_lines);
                for w in s.windows(2) {
                    proptest::prop_assert_eq!(w[0].1 + 1, w[1].0);
                }
                for (start, end) in &s {
                    proptest::prop_assert!(start <= end);
                }
            }
        }

        #[test]
        fn prop_chunking_is_deterministic(
            seed in proptest::collection::vec("[a-zA-Z0-9 #\n`]{0,300}", 1..5),
        ) {
            let text = seed.join("\n");
            proptest::prop_assert_eq!(
                chunk_markdown(&text, "docs/x.md"),
                chunk_markdown(&text, "docs/x.md")
            );
        }

        /// An anchor becomes the VName `signature`, so two chunks sharing one
        /// collapse to a single `NodeId`: the second overwrites the first and a
        /// whole doc section becomes permanently unretrievable, silently.
        ///
        /// `prop_spans_always_partition_the_document` cannot catch this — it
        /// generates `Heading {i}`, which is unique by construction. The pool
        /// below is chosen so collisions are the common case rather than a rare
        /// draw: exact repeats (the `~2` disambiguator), strings differing only
        /// in case/punctuation (slug collapsing), pairs that are equal after the
        /// 48-char per-segment truncation, and headings that *already contain*
        /// the `~2`/`#2` disambiguators the chunker appends.
        #[test]
        fn prop_anchors_are_unique_under_collision_pressure(
            picks in proptest::collection::vec(0usize..8, 1..14),
            levels in proptest::collection::vec(1u8..4, 1..14),
        ) {
            const POOL: [&str; 8] = [
                "Decision",
                "decision",
                "De-cision!",
                "Decision~2",
                "Decision#2",
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA-one",
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA-two",
                "_preamble",
            ];
            let mut text = String::new();
            let n = picks.len().min(levels.len());
            for i in 0..n {
                text.push_str(&"#".repeat(levels[i].max(1) as usize));
                text.push(' ');
                text.push_str(POOL[picks[i] % POOL.len()]);
                text.push('\n');
                text.push_str("body text for this section\n");
            }

            let chunks = chunk_markdown(&text, "docs/x.md");
            let mut seen = std::collections::HashSet::new();
            for c in &chunks {
                proptest::prop_assert!(
                    seen.insert(c.anchor.clone()),
                    "duplicate anchor {:?} collapses two chunks onto one NodeId; doc was:\n{}",
                    c.anchor,
                    text
                );
            }
        }
    }
}
