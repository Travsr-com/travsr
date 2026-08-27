/**
 * RFC-027 live semantic resolution — the editor half.
 *
 * Phase B (SCIP) is commit-gated, so between commits the graph knows *that*
 * `user.save()` is a call but not *which* `save`. The daemon closes most of
 * that gap on its own (an unambiguous callee needs no help), but a call on a
 * typed receiver is exactly what lexical matching cannot settle. That is the
 * one question a language server is authoritative for, so we ask the one the
 * developer is already running.
 *
 * ## What this does and does not do
 *
 * It calls `vscode.executeDefinitionProvider`, which routes to whatever
 * extension owns the language. **No server is spawned and none is bundled** —
 * this reuses a process the developer already trusts and already pays for
 * (RFC-027 section 7.6), which is why it costs nothing and needs no new trust
 * decision.
 *
 * It reports **positions**, never identities. It never names a graph node,
 * never mints a VName, and never asserts a relationship. The daemon maps both
 * endpoints to nodes itself against SCIP-owned identity. This is the line that
 * separates it from the #688 editor plane, where the editor's own *claim*
 * (a diagnostic) is what is being reported and so must stay out of the graph.
 *
 * ## Why a rough candidate scan is safe
 *
 * We do not have a parser here, so call sites are found with a deliberately
 * simple scan for `identifier(`. That is imprecise, and it does not need to be
 * precise: the daemon is fail-closed. A position that is not really a call, or
 * that resolves outside the graph, produces a `pending` marker, never an edge.
 * A sloppy candidate therefore costs one wasted provider call and some recall
 * — never correctness. Precision is enforced where the graph is, not here.
 *
 * Everything is bounded and best-effort: a capped number of queries per file,
 * a stale-buffer check before reporting, and every failure path is silent. A
 * freshness improvement is never worth a word of the user's attention.
 */

import * as vscode from "vscode";

import { LiveResolutionItem, reportLiveResolution } from "./daemonIpc";

/**
 * Cap on definition queries per file.
 *
 * Each is an IPC round trip into another extension host. A hand-written source
 * file is far below this; a generated one can hold thousands, and resolving all
 * of them would cost more than the freshness is worth. Truncation degrades
 * recall, which the commit-gated path then repairs.
 */
const MAX_QUERIES_PER_FILE = 200;

/** Languages whose Phase B call sites the daemon can currently ratify. */
const SUPPORTED_LANGUAGES = new Set([
  "typescript",
  "typescriptreact",
  "javascript",
  "javascriptreact",
]);

/**
 * Identifier immediately followed by `(`.
 *
 * Deliberately crude (see the module note): it matches inside strings and
 * comments, and it misses calls split across lines. Both are recall costs the
 * daemon absorbs safely.
 */
const CALL_SITE = /\b([A-Za-z_$][\w$]*)\s*\(/g;

/** Keywords that look like calls but never are. */
const NOT_CALLS = new Set([
  "if",
  "for",
  "while",
  "switch",
  "catch",
  "return",
  "function",
  "typeof",
  "await",
  "super",
  "constructor",
]);

/** Repo-relative, forward-slash path, matching the graph's own path keys. */
function repoRelative(repoRoot: string, uri: vscode.Uri): string | null {
  const full = uri.fsPath;
  if (!full.startsWith(repoRoot)) return null;
  return full.slice(repoRoot.length).replace(/^[/\\]/, "").replace(/\\/g, "/");
}

/** Candidate call-site positions in `doc`, capped. */
function callSites(doc: vscode.TextDocument): vscode.Position[] {
  const out: vscode.Position[] = [];
  const text = doc.getText();
  CALL_SITE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = CALL_SITE.exec(text)) !== null) {
    if (out.length >= MAX_QUERIES_PER_FILE) break;
    if (NOT_CALLS.has(m[1])) continue;
    out.push(doc.positionAt(m.index));
  }
  return out;
}

/**
 * Ask the running language provider where each call site resolves, and publish
 * the answers to the daemon.
 *
 * Returns the number of resolutions reported, for tests and logging. Resolves
 * to 0 on every uninteresting path (unsupported language, file outside the
 * repo, no provider, nothing resolved) and never rejects.
 */
export async function publishLiveResolutions(
  repoRoot: string,
  doc: vscode.TextDocument
): Promise<number> {
  if (!SUPPORTED_LANGUAGES.has(doc.languageId)) return 0;
  const file = repoRelative(repoRoot, doc.uri);
  if (!file) return 0;

  const version = doc.version;
  const sites = callSites(doc);
  if (sites.length === 0) return 0;

  const resolutions: LiveResolutionItem[] = [];
  for (const pos of sites) {
    // The buffer moved while we were asking. Answers computed against the old
    // text no longer describe this file, so drop the whole batch rather than
    // report positions that have shifted (RFC-027 section 11).
    if (doc.version !== version) return 0;

    let locations: unknown;
    try {
      locations = await vscode.commands.executeCommand(
        "vscode.executeDefinitionProvider",
        doc.uri,
        pos
      );
    } catch {
      continue; // no provider, or it threw. Neither is worth reporting.
    }

    const target = firstLocation(locations);
    if (!target) continue;
    const targetPath = repoRelative(repoRoot, target.uri);
    // Outside the workspace (node_modules, a .d.ts in the SDK) — dropped here
    // so the live lane stays intra-corpus (RFC-027 section 8.2). The daemon
    // would abstain anyway; not sending it saves the round trip.
    if (!targetPath) continue;

    const word = doc.getWordRangeAtPosition(pos);
    resolutions.push({
      ref_line: pos.line + 1,
      ref_col: pos.character,
      name: word ? doc.getText(word) : "",
      target_path: targetPath,
      target_line: target.range.start.line + 1,
      buffer_version: version,
    });
  }

  if (resolutions.length === 0) return 0;
  if (doc.version !== version) return 0;
  await reportLiveResolution(repoRoot, file, resolutions);
  return resolutions.length;
}

/**
 * Normalize what a definition provider returned.
 *
 * Providers may answer with `Location[]`, `LocationLink[]`, or a bare
 * `Location`, and a single call can resolve to several definitions (an
 * overload set, a merged declaration). We take the first: reporting several
 * targets for one position would ask the daemon to pick, which is precisely
 * the guess the fail-closed contract forbids.
 */
function firstLocation(
  raw: unknown
): { uri: vscode.Uri; range: vscode.Range } | null {
  const list = Array.isArray(raw) ? raw : raw ? [raw] : [];
  if (list.length === 0) return null;
  const first = list[0] as Partial<vscode.Location> &
    Partial<vscode.LocationLink>;
  if (first.uri && first.range) {
    return { uri: first.uri, range: first.range as vscode.Range };
  }
  if (first.targetUri && first.targetRange) {
    return { uri: first.targetUri, range: first.targetRange };
  }
  return null;
}
