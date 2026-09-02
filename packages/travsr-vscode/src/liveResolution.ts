/**
 * RFC-027 live semantic resolution — the editor half (daemon-driven positions).
 *
 * Phase B (SCIP) is commit-gated, so between commits the graph knows *that*
 * `user.save()` is a call but not *which* `save`. The daemon closes most of
 * that gap on its own (an unambiguous callee needs no help), but a reference on
 * a typed receiver — a method or field on `user`, an `implements` clause — is
 * exactly what lexical matching cannot settle. That is the one question a
 * language server is authoritative for, so we ask the one the developer is
 * already running.
 *
 * ## Daemon-driven, not editor-scanned
 *
 * The editor no longer guesses which references exist. It **asks the daemon**
 * (`requestLiveResolutionTargets`), which runs the real parser over the dirty
 * file, keeps the references its own lexical lane cannot settle, and answers
 * with a position, a name, an edge kind, and which provider to run. Reference
 * detection therefore lives with the parser and the graph, not in an
 * English-shaped regex here — which is what lets the lane reach fields,
 * implements clauses, and every language the native extractor covers, rather
 * than only `identifier(` in the handful the old scan understood.
 *
 * The editor's residual job is small and mechanical: find the column of the
 * named reference on its line, run the provider, and report the answer back.
 *
 * ## What this does and does not do
 *
 * It calls `vscode.executeDefinitionProvider` / `executeImplementationProvider`,
 * which route to whatever extension owns the language. **No server is spawned
 * and none is bundled** — this reuses a process the developer already trusts and
 * already pays for (RFC-027 section 7.6), which is why it costs nothing and
 * needs no new trust decision.
 *
 * It reports **positions**, never identities. It never names a graph node,
 * never mints a VName, and never asserts a relationship. The daemon maps both
 * endpoints to nodes itself against SCIP-owned identity. This is the line that
 * separates it from the #688 editor plane, where the editor's own *claim*
 * (a diagnostic) is what is being reported and so must stay out of the graph.
 *
 * Everything is bounded and best-effort: a capped number of queries per file,
 * a stale-buffer check before reporting, and every failure path is silent. A
 * freshness improvement is never worth a word of the user's attention.
 */

import * as vscode from "vscode";

import {
  DependentTargetsItem,
  LiveResolutionItem,
  LiveResolutionTargetItem,
  reportLiveResolution,
  requestLiveResolutionTargets,
} from "./daemonIpc";

/**
 * Cap on provider queries for one save, across the saved file **and** every
 * dependent the interface-edit closure pulls in (RFC-027 section 8.7.5).
 *
 * Each is an IPC round trip into another extension host. The daemon keeps the
 * set surgical (only references its lexical lane could not settle), so a
 * hand-written file is far below this; the cap only bounds a pathological
 * generated file or a rename in a hot utility whose closure is large. It bounds
 * the *total*, not each file, so one save can never fan out into an unbounded
 * storm of queries (§10.1). Truncation degrades recall, which the commit-gated
 * path repairs.
 */
const MAX_QUERIES_PER_SAVE = 200;

/**
 * Languages the daemon's live lane detects references for. A cheap pre-filter
 * kept in step with the daemon's own gate (`live_language` in travsr-daemon):
 * the daemon is authoritative and returns no targets for anything else, so this
 * only avoids a wasted round trip on a save the daemon could not act on.
 * Widening it beyond the daemon's set costs one empty request, never
 * correctness.
 *
 * The first block runs the daemon's native extractor. The rest run the generic
 * tree-sitter detector and reach this lane through the editor **alone**
 * (RFC-027 section 8.3), so omitting one here disables it outright: this filter
 * gates the request itself, not just the round trip.
 *
 * These are VS Code `languageId` values, which are not always the name of the
 * language: C# is `csharp`, Objective-C is `objective-c` / `objective-cpp`.
 * Whether a given language resolves anything depends on the developer having
 * its extension installed; with no provider the request simply returns nothing,
 * which is the section 7.3c floor.
 */
const SUPPORTED_LANGUAGES = new Set([
  // Native extractor.
  "typescript",
  "typescriptreact",
  "javascript",
  "javascriptreact",
  "rust",
  "python",
  // Generic detector, editor lane only.
  "go",
  "java",
  "csharp",
  "cpp",
  "c",
  "objective-c",
  "objective-cpp",
  "ruby",
  "php",
  "kotlin",
  "swift",
  "dart",
  "scala",
]);

/** VS Code provider command for each daemon-named provider. */
const PROVIDER_COMMAND: Record<string, string> = {
  definition: "vscode.executeDefinitionProvider",
  implementation: "vscode.executeImplementationProvider",
};

/**
 * Repo-relative, forward-slash path, matching the graph's own path keys, or
 * `null` when the file is outside the repo.
 *
 * The boundary test is on a separator, not a bare prefix: `startsWith(repoRoot)`
 * alone accepts a *sibling* directory whose name merely extends the root, so
 * `/work/repo-vendor/x.ts` would be reported to the daemon as the repo-relative
 * `-vendor/x.ts` and mapped against a path that means something else in the
 * graph.
 */
function repoRelative(repoRoot: string, uri: vscode.Uri): string | null {
  const full = uri.fsPath;
  const root = repoRoot.replace(/[/\\]+$/, "");
  const inside =
    full === root ||
    full.startsWith(root + "/") ||
    full.startsWith(root + "\\");
  if (!inside) return null;
  return full.slice(root.length).replace(/^[/\\]/, "").replace(/\\/g, "/");
}

/** Escape a name for use inside a `RegExp`. */
function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Column of `name` on 1-based `line` in `doc`, or `null` if it is not there.
 *
 * The native extractor records a reference's line but not its column, so the
 * editor recovers the column here. A whole-word match avoids resolving a
 * substring of a longer identifier; if the name has moved off the line (a race
 * against an edit) we return `null` and skip, never a wrong position.
 */
function columnOf(
  doc: vscode.TextDocument,
  line: number,
  name: string
): number | null {
  if (line < 1 || line > doc.lineCount) return null;
  const text = doc.lineAt(line - 1).text;
  const m = new RegExp(`\\b${escapeRegExp(name)}\\b`).exec(text);
  return m ? m.index : null;
}

/** A shared, mutable query budget spent across the saved file and dependents. */
interface Budget {
  remaining: number;
}

/**
 * Resolve the references the daemon asked about and publish the answers.
 *
 * Resolves the saved document's own references, then the dependents whose live
 * edges this save can restore (RFC-027 section 8.7.5): a rename in one file
 * heals the files that reference it without each being saved in turn. All of it
 * shares one query budget so a large closure cannot balloon a save.
 *
 * Returns the total number of resolutions reported, for tests and logging.
 * Resolves to 0 on every uninteresting path (unsupported language, file outside
 * the repo, no targets, no provider, nothing resolved) and never rejects.
 */
export async function publishLiveResolutions(
  repoRoot: string,
  doc: vscode.TextDocument
): Promise<number> {
  if (!SUPPORTED_LANGUAGES.has(doc.languageId)) return 0;
  const file = repoRelative(repoRoot, doc.uri);
  if (!file) return 0;

  const version = doc.version;
  const { own, dependents } = await requestLiveResolutionTargets(
    repoRoot,
    file,
    version
  );
  if (own.length === 0 && dependents.length === 0) return 0;
  // The buffer may have moved while the daemon was parsing; the target lines
  // would no longer describe this file.
  if (doc.version !== version) return 0;

  const budget: Budget = { remaining: MAX_QUERIES_PER_SAVE };
  let total = 0;

  // The saved file, resolved against its live buffer.
  total += await resolveAndReport(repoRoot, doc, file, own, version, budget);

  // Each dependent, resolved against its own file (RFC-027 section 8.7.5).
  for (const dep of dependents) {
    if (budget.remaining <= 0) break;
    total += await resolveDependent(repoRoot, dep, budget);
  }

  return total;
}

/**
 * Resolve one dependent file's targets against its own document.
 *
 * A dependent dirty in another tab is **skipped**: its buffer differs from the
 * text the daemon parsed from disk, so the target lines would not describe it,
 * and resolving against stale text is exactly the wrong-edge risk the lane must
 * avoid (RFC-027 section 10.1). Only the saved document's version is trustworthy
 * here; for a clean or freshly opened dependent, disk equals what the daemon saw.
 */
async function resolveDependent(
  repoRoot: string,
  dep: DependentTargetsItem,
  budget: Budget
): Promise<number> {
  const uri = vscode.Uri.joinPath(vscode.Uri.file(repoRoot), dep.file);
  const open = vscode.workspace.textDocuments.find(
    (d) => d.uri.fsPath === uri.fsPath
  );
  let depDoc: vscode.TextDocument;
  if (open) {
    if (open.isDirty) return 0; // unsaved edits — its buffer is not what the daemon parsed.
    depDoc = open;
  } else {
    try {
      depDoc = await vscode.workspace.openTextDocument(uri);
    } catch {
      return 0; // gone from disk or unreadable — skip, never guess.
    }
  }
  return resolveAndReport(repoRoot, depDoc, dep.file, dep.targets, depDoc.version, budget);
}

/**
 * Resolve `targets` against `doc` under the shared `budget` and report the
 * answers for `file`. Returns the number reported. Drops the whole batch if the
 * buffer moves mid-flight (RFC-027 section 11) — answers against old text no
 * longer describe the file.
 */
async function resolveAndReport(
  repoRoot: string,
  doc: vscode.TextDocument,
  file: string,
  targets: LiveResolutionTargetItem[],
  version: number,
  budget: Budget
): Promise<number> {
  if (targets.length === 0) return 0;
  const resolutions: LiveResolutionItem[] = [];
  for (const target of targets) {
    if (budget.remaining <= 0) break;
    if (doc.version !== version) return 0;
    const item = await resolveTarget(repoRoot, doc, target, version);
    budget.remaining -= 1;
    if (item) resolutions.push(item);
  }
  if (resolutions.length === 0) return 0;
  if (doc.version !== version) return 0;
  await reportLiveResolution(repoRoot, file, resolutions);
  return resolutions.length;
}

/**
 * Resolve one daemon-named target through its provider, or `null` to skip.
 *
 * Every skip is safe: the daemon is fail-closed, so a reference we cannot pin, a
 * provider that throws, or a target outside the workspace simply does not become
 * an edge. A sloppy skip costs recall, never correctness.
 */
async function resolveTarget(
  repoRoot: string,
  doc: vscode.TextDocument,
  target: LiveResolutionTargetItem,
  version: number
): Promise<LiveResolutionItem | null> {
  const command = PROVIDER_COMMAND[target.provider];
  if (!command) return null;

  // RFC-027 #813 P1: prefer the column the daemon pinned against the file text,
  // which resolves the reference at its exact position (including a target whose
  // name is not literally on the line, e.g. `s.node.0`). Fall back to searching
  // the line for `name` when the daemon sent no column (an older daemon, or a
  // name it could not pin).
  const col = target.ref_col ?? columnOf(doc, target.ref_line, target.name);
  if (col === null || col === undefined) return null;
  const pos = new vscode.Position(target.ref_line - 1, col);

  let locations: unknown;
  try {
    locations = await vscode.commands.executeCommand(command, doc.uri, pos);
  } catch {
    return null; // no provider, or it threw. Neither is worth reporting.
  }

  const found = soleLocation(locations);
  if (!found) return null;
  const targetPath = repoRelative(repoRoot, found.uri);
  // Outside the workspace (node_modules, a .d.ts in the SDK) — dropped here so
  // the live lane stays intra-corpus (RFC-027 section 8.2). The daemon would
  // abstain anyway; not sending it saves the round trip.
  if (!targetPath) return null;

  return {
    ref_line: target.ref_line,
    ref_col: col,
    name: target.name,
    target_path: targetPath,
    target_line: found.range.start.line + 1,
    buffer_version: version,
    edge_kind: target.edge_kind,
  };
}

/**
 * Normalize what a definition/implementation provider returned, or `null` when
 * it did not name exactly one target.
 *
 * Providers answer with `Location[]`, `LocationLink[]`, or a bare `Location`,
 * and a single reference can resolve to several: an overload set, TypeScript
 * declaration merging across files, clangd's declaration plus its out-of-line
 * definition. Taking `list[0]` of that *is* the guess the fail-closed contract
 * forbids, just relocated into the extension where the daemon cannot see it —
 * the daemon then receives one confident-looking target with no signal that a
 * choice was made, and every downstream gate passes on an arbitrary pick.
 *
 * So more than one *distinct* target abstains and the reference stays `pending`,
 * which is honest and which the commit-gated path then resolves. Distinctness is
 * by URI plus start position, so a provider that repeats one location (or names
 * the same symbol through both a `Location` and a `LocationLink`) still counts
 * as one answer.
 */
function soleLocation(
  raw: unknown
): { uri: vscode.Uri; range: vscode.Range } | null {
  const list = Array.isArray(raw) ? raw : raw ? [raw] : [];
  const found: { uri: vscode.Uri; range: vscode.Range }[] = [];
  const seen = new Set<string>();
  for (const entry of list) {
    const e = entry as Partial<vscode.Location> & Partial<vscode.LocationLink>;
    let one: { uri: vscode.Uri; range: vscode.Range } | null = null;
    if (e.uri && e.range) {
      one = { uri: e.uri, range: e.range as vscode.Range };
    } else if (e.targetUri && e.targetRange) {
      one = { uri: e.targetUri, range: e.targetRange };
    }
    if (!one) continue;
    const key = `${one.uri.toString()}:${one.range.start.line}:${one.range.start.character}`;
    if (seen.has(key)) continue;
    seen.add(key);
    found.push(one);
    // Two distinct targets is already ambiguous; nothing later can undo that.
    if (found.length > 1) return null;
  }
  return found.length === 1 ? found[0] : null;
}
