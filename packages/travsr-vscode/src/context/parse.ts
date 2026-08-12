/**
 * Pure parser for `get_context` text output.
 *
 * Pinned to the stable format emitted by tools.rs (QUERY_PROTOCOL_VERSION).
 * On any parse failure returns { raw } so the UI can fall back to raw text.
 *
 * Output format (from tools.rs):
 *   [index commit: {sha}, embeddings: {on|off}]
 *   [retrieval: {tier} | coverage {n}/{m} | confidence: {label} ]
 *   <blank>
 *   {sig} ({kind}) — {path}[:{line}][ [package: {pkg}]] [via: {role}][ [score: {N.NN}]]
 *     optional indented snippet lines…
 *   <blank>
 *   [{N} nodes, {M} with snippets, ~{X} metadata-tokens + ~{Y} snippet-tokens ({budget} budget)]
 *   [note: …]
 */

export interface ContextHeader {
  indexCommit: string;
  embeddings: "on" | "off" | "degraded";
  tier: string;
  coverage: [number, number];
  confidence: string;
}

/**
 * RFC-022 §14 match-source bucket a node is grouped under (flag-on only).
 * `docs` (#376) and `tests` (#479) are non-code lanes rendered below the
 * implementation sections.
 */
export type MatchSource = "exact" | "semantic" | "docs" | "tests" | "relevant";

export interface ContextNode {
  sig: string;
  kind: string;
  path: string;
  line?: number;
  pkg?: string;
  /**
   * Provenance badge (`seed`, `caller of X`, …). Optional: RFC-022 §14 grouped
   * mode hoists the source of Exact/Semantic seed rows into the section header,
   * so those rows carry no `[via:]` badge (`via === undefined`).
   */
  via?: string;
  /** RFC-022 §14 section this node was grouped under (flag-on only). */
  matchSource?: MatchSource;
  score?: number;
  snippet?: string;
}

export interface ContextFooter {
  nodes: number;
  withSnippets: number;
  metaTokens: number;
  snippetTokens: number;
  budget: string;
}

export interface ParsedContext {
  header: ContextHeader;
  nodes: ContextNode[];
  footer: ContextFooter;
  notes: string[];
}

export interface RawFallback {
  raw: string;
}

export type ContextResult = ParsedContext | RawFallback;

export function isParsed(r: ContextResult): r is ParsedContext {
  return "header" in r;
}

// ── Line-level parsers ────────────────────────────────────────────────────────

const FRESHNESS_RE = /^\[index commit:\s*([^,]+),\s*embeddings:\s*(on|off|degraded)\]$/;
const RETRIEVAL_RE = /^\[retrieval:\s*(.+?)\s*\|\s*coverage\s+(\d+)\/(\d+)\s*\|\s*confidence:\s*(.+?)\s*\]$/;
// Full snippet footer: [N nodes, M with snippets, ~X metadata-tokens + ~Y snippet-tokens (shared budget)]
const FOOTER_RE =
  /^\[(\d+)\s+nodes?,\s*(\d+)\s+with\s+snippets?,\s*~(\d+)\s+metadata-tokens\s*\+\s*~(\d+)\s+snippet-tokens\s+\((.+?)\s+budget\)\]$/;
// Simple metadata-only footer: [N nodes, ~T tokens] or [N nodes, ~T tokens — …]
const FOOTER_RE_SIMPLE = /^\[(\d+)\s+nodes?,\s*~(\d+)\s+tokens[^\]]*\]$/;
const NOTE_RE = /^\[note:\s*.+\]$/;
// RFC-022 §14 match-source section header: "## exact — literal symbol / FTS …".
// Consumed (not emitted); stamps matchSource on the node rows that follow it.
// Includes the docs (#376) and tests (#479) lanes so their rows are bucketed
// under the right section instead of inheriting the preceding one.
const SECTION_RE = /^##\s+(exact|semantic|docs|tests|relevant)\b/;

function parseHeader(line1: string, line2: string): ContextHeader | null {
  const m1 = FRESHNESS_RE.exec(line1.trim());
  const m2 = RETRIEVAL_RE.exec(line2.trim());
  if (!m1 || !m2) return null;
  return {
    indexCommit: m1[1].trim(),
    embeddings: m1[2] as "on" | "off" | "degraded",
    tier: m2[1].trim(),
    coverage: [parseInt(m2[2], 10), parseInt(m2[3], 10)],
    confidence: m2[4].trim(),
  };
}

/**
 * Parse a non-indented node line.
 *
 * Format (tools.rs format_node_line):
 *   {sig} ({kind}) — {path}[:{line}][ [package: {pkg}]] [via: {role}][ [score: {N.NN}]]
 *
 * Parsing strategy: strip optional trailing bracketed tags from right-to-left,
 * then split sig+kind from path on the em-dash separator.
 */
function parseNodeLine(raw: string): Omit<ContextNode, "snippet"> | null {
  let rest = raw.trim();

  // Extract optional [score: N.NN]
  let score: number | undefined;
  const scoreM = /\s+\[score:\s*([\d.]+)\]$/.exec(rest);
  if (scoreM) { score = parseFloat(scoreM[1]); rest = rest.slice(0, -scoreM[0].length); }

  // Extract optional [via: role]. RFC-022 §14: grouped Exact/Semantic seed rows
  // hoist their source into the section header and carry no badge, so a missing
  // `[via:]` is valid (via = undefined) — it must not drop the row.
  let via: string | undefined;
  const viaM = /\s+\[via:\s*([^\]]+)\]$/.exec(rest);
  if (viaM) { via = viaM[1].trim(); rest = rest.slice(0, -viaM[0].length); }

  // Extract optional [package: pkg]
  let pkg: string | undefined;
  const pkgM = /\s+\[package:\s*([^\]]+)\]$/.exec(rest);
  if (pkgM) { pkg = pkgM[1].trim(); rest = rest.slice(0, -pkgM[0].length); }

  // Split on em-dash separator: '{sig} ({kind}) — {path}[:{line}]'
  // The em-dash character is U+2014.
  const dashIdx = rest.indexOf(" — ");
  if (dashIdx < 0) return null;

  const leftPart = rest.slice(0, dashIdx);
  const rightPart = rest.slice(dashIdx + 3); // skip ' — '

  // Extract kind from left part: last ' (word)' — greedy match for sig
  const kindM = /^(.+)\s+\((\w+)\)$/.exec(leftPart.trim());
  if (!kindM) return null;
  const sig = kindM[1].trim();
  const kind = kindM[2];

  // Extract optional line number from path
  let line: number | undefined;
  let path = rightPart.trim();
  const lineM = /:(\d+)$/.exec(path);
  if (lineM) { line = parseInt(lineM[1], 10); path = path.slice(0, -lineM[0].length); }

  return { sig, kind, path, line, pkg, via, score };
}

// ── get_snippets output parser ────────────────────────────────────────────────

/**
 * Parse `get_snippets` output into a sig→snippet map.
 *
 * Output format (tools.rs get_snippets_body):
 *   {sig} ({kind}) — {path} [package: {pkg}]
 *   ───
 *   code lines…
 *
 *   {sig2} ({kind2}) — {path2} [package: {pkg2}]
 *   ───
 *   code lines…
 *
 *   [N symbols, M with snippets, ~T tokens]
 *
 * Scanned line-by-line so internal blank lines in snippet bodies don't split blocks.
 */

/** True if a line looks like a get_snippets block header (not snippet code). */
function looksLikeSnippetHeader(line: string): boolean {
  if (!line || line.startsWith(" ") || line.startsWith("\t")) return false;
  if (line.trim().startsWith("[") || line.trim() === "───") return false;
  return line.includes(" — ") && /\s+\(\w+\)/.test(line);
}

/** True only for the trailing summary footer "[N symbols, …]". Must NOT match a
 *  skeleton body's opening "[skeleton: …]" line, nor a code line like "[1, 2]". */
function looksLikeSnippetFooter(line: string): boolean {
  return /^\[\d+ symbols?\b/.test(line.trim());
}

export function parseSnippetsResult(text: string): Map<string, string> {
  const map = new Map<string, string>();
  if (!text.trim()) return map;

  const lines = text.split("\n");
  let i = 0;

  while (i < lines.length) {
    const line = lines[i];
    const trimmed = line.trim();

    // Skip blank lines, footer lines ([…]), orphan separators (───)
    if (!trimmed || trimmed.startsWith("[") || trimmed === "───") { i++; continue; }

    if (!looksLikeSnippetHeader(line)) { i++; continue; }

    // Parse header: "{sig} ({kind}) — {path} [package: {pkg}]"
    const dashIdx = trimmed.indexOf(" — ");
    if (dashIdx < 0) { i++; continue; }
    const kindM = /^(.+)\s+\(\w+\)$/.exec(trimmed.slice(0, dashIdx).trim());
    if (!kindM) { i++; continue; }
    const sig = kindM[1].trim();
    i++;

    // Collect snippet body if separator follows
    if (i < lines.length && lines[i].trim() === "───") {
      i++; // skip ───
      const snippetLines: string[] = [];
      while (i < lines.length) {
        const sl = lines[i];
        // Stop at the next block header or the trailing summary footer. Do NOT
        // stop on a bare "[" — skeleton bodies open with "[skeleton: …]" and code
        // lines can start with "[" (arrays, decorators). Blank lines are fine.
        if (looksLikeSnippetHeader(sl) || looksLikeSnippetFooter(sl)) break;
        snippetLines.push(sl);
        i++;
      }
      // Trim trailing blank lines
      while (snippetLines.length > 0 && !snippetLines[snippetLines.length - 1].trim()) {
        snippetLines.pop();
      }
      if (snippetLines.length > 0) map.set(sig, snippetLines.join("\n"));
    }
  }

  return map;
}

// ── Main parser ───────────────────────────────────────────────────────────────

export function parseContextResult(text: string): ContextResult {
  if (!text.trim()) return { raw: text };

  const lines = text.split("\n");
  if (lines.length < 2) return { raw: text };

  const header = parseHeader(lines[0], lines[1]);
  if (!header) return { raw: text };

  const nodes: ContextNode[] = [];
  const notes: string[] = [];
  let footer: ContextFooter | undefined;
  // RFC-022 §14: the match-source section the current node rows belong to, set by
  // the most recent "## <source>" header. undefined for flag-off/flat output.
  let currentSection: MatchSource | undefined;

  let i = 2; // skip the two header lines
  while (i < lines.length) {
    const line = lines[i];
    const trimmed = line.trim();

    if (!trimmed) { i++; continue; }

    // Match-source section header (flag-on): switch section, consume the line.
    const sectionM = SECTION_RE.exec(trimmed);
    if (sectionM) {
      currentSection = sectionM[1] as MatchSource;
      i++;
      continue;
    }

    // Footer line — full snippet format
    const footerM = FOOTER_RE.exec(trimmed);
    if (footerM) {
      footer = {
        nodes: parseInt(footerM[1], 10),
        withSnippets: parseInt(footerM[2], 10),
        metaTokens: parseInt(footerM[3], 10),
        snippetTokens: parseInt(footerM[4], 10),
        budget: footerM[5].trim(),
      };
      i++;
      continue;
    }

    // Footer line — metadata-only format: [N nodes, ~T tokens] or [N nodes, ~T tokens — …]
    const footerSimpleM = FOOTER_RE_SIMPLE.exec(trimmed);
    if (footerSimpleM) {
      footer = {
        nodes: parseInt(footerSimpleM[1], 10),
        withSnippets: 0,
        metaTokens: parseInt(footerSimpleM[2], 10),
        snippetTokens: 0,
        budget: "—",
      };
      i++;
      continue;
    }

    // Note line
    if (NOTE_RE.test(trimmed)) {
      notes.push(trimmed);
      i++;
      continue;
    }

    // Skip the snippet separator line emitted by the backend (SNIPPET_SEP = "───").
    if (trimmed === "───") { i++; continue; }

    // Try to parse a node line (non-indented, not a sep/footer/note)
    if (!line.startsWith(" ") && !line.startsWith("\t")) {
      const node = parseNodeLine(trimmed);
      if (node) {
        i++;
        // Check whether the next line is the snippet separator.
        let snippet: string | undefined;
        if (i < lines.length && lines[i].trim() === "───") {
          i++; // skip separator
          const snippetLines: string[] = [];
          // Collect snippet body until blank line, footer, or note.
          while (i < lines.length) {
            const sl = lines[i];
            const st = sl.trim();
            if (!st) break;
            if (FOOTER_RE.exec(st) || FOOTER_RE_SIMPLE.exec(st) || NOTE_RE.test(st)) break;
            snippetLines.push(sl);
            i++;
          }
          if (snippetLines.length > 0) snippet = snippetLines.join("\n");
        }
        nodes.push({ ...node, snippet, matchSource: currentSection });
        continue;
      }
    }

    i++;
  }

  if (!footer) return { raw: text };

  return { header, nodes, footer, notes };
}
