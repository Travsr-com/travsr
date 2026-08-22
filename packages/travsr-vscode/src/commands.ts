/**
 * VSCODE-247 — CLI↔UI parity commands.
 *
 * Surfaces CLI-only features in the extension:
 *   travsr.askSymbol         live ranked symbol search (Quick Pick)
 *   travsr.manageSynonyms    synonym editor webview (multi-chip add)
 *   travsr.showDependencies  direct + transitive imports, click-navigable
 *   travsr.showExecutionPath PCST path between two symbols, rendered in the graph
 *   travsr.showRepos         registry manager webview
 *   travsr.showGraphStats    graph metrics dashboard webview
 *   travsr.showLanguages     indexed + available languages, install from UI
 *
 * Pure helpers (stripEnvelope, parsers, openAtLine) are exported for unit tests.
 */

import * as cp from "child_process";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import type { McpClient } from "./mcp";
import { ActiveRepo } from "./activeRepo";
import { GraphPanel, type GraphData, type GraphNode } from "./graph";
import {
  buildSynonymsHtml,
  buildReposHtml,
  buildStatsHtml,
  buildLanguagesHtml,
  buildPanelLoadingHtml,
  type RepoRow,
  type StatsView,
  type LangCount,
  type LangInfo,
} from "./webviews";
import type { Diagnostic, LogEntry } from "./webviews";

// ── Pure helpers (unit-testable) ────────────────────────────────────────────

/**
 * Strip the `<travsr-data>…</travsr-data>` envelope the MCP server wraps around
 * repo-derived tool output (SEC-001). Returns the inner text, or the input
 * unchanged when no envelope is present. An empty envelope yields "".
 */
export function stripEnvelope(raw: string): string {
  const m = /^<travsr-data>\n?([\s\S]*?)\n?<\/travsr-data>$/.exec(raw.trim());
  return m ? m[1] : raw;
}

/** A ranked symbol search result row. */
export interface SymbolItem extends vscode.QuickPickItem {
  path: string;
  line?: number;
}

/** Map a graph node `kind` to a VS Code codicon id. */
export function kindCodicon(kind: string): string {
  switch (kind) {
    case "function":
    case "method":
      return "symbol-method";
    case "class":
      return "symbol-class";
    case "interface":
      return "symbol-interface";
    case "struct":
      return "symbol-structure";
    case "enum":
      return "symbol-enum";
    case "var":
    case "variable":
      return "symbol-variable";
    case "file":
      return "symbol-file";
    default:
      return "symbol-misc";
  }
}

/**
 * Parse a `get_graph_json` payload into ranked Quick Pick items. Non-symbol
 * (file) nodes are dropped so the search returns navigable definitions. Returns
 * an empty array on malformed JSON — never throws.
 */
export function parseGraphSymbols(raw: string): SymbolItem[] {
  if (!raw) return [];
  let data: GraphData;
  try {
    data = JSON.parse(raw) as GraphData;
  } catch {
    return [];
  }
  if (!Array.isArray(data.nodes)) return [];
  return data.nodes
    .filter((n) => n.kind !== "file")
    .map((n) => ({
      label: `$(${kindCodicon(n.kind)}) ${n.label}`,
      description: n.path,
      detail: typeof n.score === "number" ? `score ${n.score.toFixed(3)}` : undefined,
      path: n.path,
      line: n.line,
    }));
}

/** A single synonym pair. */
export interface SynonymPair {
  term: string;
  alias: string;
}

/** Parse the `synonym_list` output (`term => alias` per line) into pairs. */
export function parseSynonymList(raw: string): SynonymPair[] {
  const inner = stripEnvelope(raw);
  return inner
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean)
    .map((l) => {
      const idx = l.indexOf(" => ");
      if (idx < 0) return null;
      return { term: l.slice(0, idx), alias: l.slice(idx + 4) };
    })
    .filter((p): p is SynonymPair => p !== null);
}

/**
 * Parse `get_execution_path` prose (`signature (kind) — path`, one node per
 * line) into a synthetic GraphData: a node per line flagged `root` (so the graph
 * highlights it) chained source→sink by `flows` edges.
 */
export function parseExecutionPath(raw: string): GraphData {
  const inner = stripEnvelope(raw);
  const lineRe = /^(?:\[[^\]]+\]\s*)?(.+?)\s+\((\w+)\)\s+—\s+(.+)$/;
  const nodes: GraphNode[] = [];
  for (const line of inner.split("\n")) {
    const t = line.trim();
    if (!t) continue;
    const m = lineRe.exec(t);
    const node: GraphNode = m
      ? { id: m[1], label: m[1], kind: m[2], path: m[3], package: "", score: 0, root: true }
      : { id: t, label: t, kind: "symbol", path: "", package: "", score: 0, root: true };
    nodes.push(node);
  }
  const edges = nodes.slice(1).map((n, i) => ({
    source: nodes[i].id,
    target: n.id,
    kind: "flows",
  }));
  return { nodes, edges };
}

/** Parse `repos_list` TSV output (`name\tdb_path\t{0|1}`) into rows. */
export function parseReposList(raw: string): RepoRow[] {
  const inner = stripEnvelope(raw);
  return inner
    .split("\n")
    .map((l) => l.replace(/\r$/, ""))
    .filter((l) => l.trim())
    .map((l) => {
      const parts = l.split("\t");
      return { name: parts[0] ?? "", path: parts[1] ?? "", exists: parts[2] === "1" };
    });
}

/** Parse `repo_languages` TSV output (`lang\tcount`) into LangCount rows. */
export function parseLanguageCounts(raw: string): LangCount[] {
  const inner = stripEnvelope(raw);
  return inner
    .split("\n")
    .filter((l) => l.trim())
    .map((l) => {
      const [lang, cnt] = l.split("\t");
      return { language: lang ?? "", count: parseInt(cnt ?? "0", 10) };
    })
    .filter((l) => l.language);
}

/** Parse `travsr lang list --json` output into LangInfo rows. Tolerates empty/error. */
export function parseAvailableLanguages(raw: string): LangInfo[] {
  try {
    const parsed = JSON.parse(raw.trim() || "[]") as LangInfo[];
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

/** Human-readable "time ago" for a timestamp in ms. */
export function timeAgo(ms: number): string {
  const secs = Math.floor(ms / 1000);
  if (secs < 60) return `${secs}s ago`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  return `${Math.floor(hrs / 24)}d ago`;
}

/**
 * Read the tail of the daemon log.
 *
 * `daemon.log.<UTC-DATE>` is JSON lines, one object per line. Only the last
 * `maxBytes` are read, by seeking rather than by loading the file and slicing:
 * the log is capped at 50 MB across rotations and the newest file is never
 * pruned even when it alone exceeds that, so reading it whole to show 200 lines
 * is a bounded-looking call that is not bounded.
 *
 * The first line of the window is dropped when the window did not start at byte
 * zero, because a seek lands mid-line.
 */
export function readDaemonLogTail(repoRoot: string, maxLines = 500): LogEntry[] {
  const dir = path.join(repoRoot, ".travsr");
  let newest: string;
  try {
    const files = fs
      .readdirSync(dir)
      .filter((f) => f.startsWith("daemon.log"))
      .sort(); // ISO date suffix sorts chronologically
    if (files.length === 0) return [];
    newest = path.join(dir, files[files.length - 1]);
  } catch {
    return [];
  }

  // Generous enough that maxLines is the binding limit, not the byte window.
  const MAX_BYTES = 512 * 1024;
  let text: string;
  try {
    const size = fs.statSync(newest).size;
    const start = Math.max(0, size - MAX_BYTES);
    const len = size - start;
    const fd = fs.openSync(newest, "r");
    try {
      const buf = Buffer.alloc(len);
      fs.readSync(fd, buf, 0, len, start);
      text = buf.toString("utf8");
    } finally {
      fs.closeSync(fd);
    }
    if (start > 0) text = text.slice(text.indexOf("\n") + 1);
  } catch {
    return [];
  }

  return text
    .split("\n")
    .filter((l) => l.trim() !== "")
    .slice(-maxLines)
    .map(parseLogLine);
}

/**
 * One log line as the panel needs it.
 *
 * Rotated files written before the log became JSON are still on disk and are
 * still the only record of what happened then, so a line that does not parse is
 * carried through as its own text rather than dropped.
 */
export function parseLogLine(line: string): LogEntry {
  try {
    const e = JSON.parse(line) as {
      timestamp?: string;
      level?: string;
      target?: string;
      fields?: Record<string, unknown>;
    };
    if (typeof e.timestamp === "string" && typeof e.level === "string") {
      const fields = e.fields ?? {};
      // `repo` is dropped for the same reason the CLI renderer drops it: the
      // panel belongs to one repo and the reader opened it from inside that
      // repo, so restating the path on every line is spent width.
      const { message, event, repo: _repo, ...rest } = fields as Record<string, unknown>;
      return {
        // 24-hour, fixed width. `toLocaleTimeString` defaults to 12-hour with a
        // meridiem in most locales, which is wider and does not sort by eye.
        time: new Date(e.timestamp).toTimeString().slice(0, 8),
        level: e.level,
        target: shortTarget(e.target ?? ""),
        message: typeof message === "string" ? message : "",
        event: typeof event === "string" ? event : undefined,
        detail: Object.entries(rest)
          .map(([k, v]) => `${k}=${String(v)}`)
          .join(" "),
        iso: e.timestamp,
        raw: line,
      };
    }
  } catch {
    // fall through
  }
  return { time: "", level: "", target: "", message: line, detail: "", iso: "", raw: line };
}

/** `travsr_plugin_host::registry` is 29 characters that say "plugin host". */
function shortTarget(target: string): string {
  return target.split("::")[0].replace(/^travsr_/, "").replace(/_/g, "-");
}

/** Build the stats dashboard view from `get_graph_stats` + local graph.db. */
export function buildStatsView(raw: string): StatsView {
  const lines = stripEnvelope(raw).split("\n");
  const field = (key: string): string => {
    for (const l of lines) {
      const m = new RegExp(`^${key}:\\s*(.+)$`).exec(l.trim());
      if (m) return m[1];
    }
    return "—";
  };
  let dbSize = "—";
  let lastIndexed = "—";
  const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  if (root) {
    try {
      const st = fs.statSync(path.join(root, ".travsr", "graph.db"));
      dbSize = `${(st.size / 1_048_576).toFixed(1)} MB`;
      lastIndexed = timeAgo(Date.now() - st.mtimeMs);
    } catch {
      // graph.db absent — leave dashes.
    }
  }
  return {
    nodes: field("nodes"),
    edges: field("edges"),
    schemaVersion: field("schema_version"),
    dbSize,
    lastIndexed,
  };
}

/** Open a file (repo-relative or absolute) at an optional 1-based line. */
export async function openAtLine(filePath: string, line?: number): Promise<void> {
  const root = vscode.workspace.workspaceFolders?.[0]?.uri;
  const uri = filePath.startsWith("/")
    ? vscode.Uri.file(filePath)
    : root
      ? vscode.Uri.joinPath(root, filePath)
      : vscode.Uri.file(filePath);
  if (line != null) {
    const doc = await vscode.workspace.openTextDocument(uri);
    const lineIdx = Math.max(0, line - 1);
    await vscode.window.showTextDocument(doc, {
      selection: new vscode.Range(lineIdx, 0, lineIdx, 0),
    });
  } else {
    await vscode.commands.executeCommand("vscode.open", uri);
  }
}

function escHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

/**
 * Resolve an import specifier string from `get_dependencies` output to a
 * workspace-relative file path, or `undefined` when not resolvable (external
 * package, stdlib, crate, or local-but-missing).
 *
 * Spec format: `kind:specifier` (e.g. `import:./status`, `use:std::io::Write`).
 * Only `.`-relative and `/`-absolute specifiers are attempted — everything else
 * is an external dependency that has no local file to open.
 *
 * `existsCheck` is injectable so this function can be unit-tested without disk.
 */
export function resolveDepSpec(
  spec: string,
  sourceAbsPath: string,
  existsCheck: (p: string) => boolean = fs.existsSync
): string | undefined {
  const colonIdx = spec.indexOf(":");
  const raw = colonIdx >= 0 ? spec.slice(colonIdx + 1) : spec;
  if (!raw.startsWith(".") && !raw.startsWith("/")) return undefined;

  const dir = path.dirname(sourceAbsPath);
  const candidates = [
    "",
    ".ts",
    ".tsx",
    ".js",
    ".jsx",
    "/index.ts",
    "/index.tsx",
    "/index.js",
    "/index.jsx",
  ];
  for (const ext of candidates) {
    const abs = path.resolve(dir, raw + ext);
    if (existsCheck(abs)) {
      const wsRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      return wsRoot ? path.relative(wsRoot, abs) : abs;
    }
  }
  return undefined;
}

/** An entry in the dep list webview — resolved path is clickable, absent = dimmed. */
export interface DepEntry {
  display: string;
  path?: string;
}

/**
 * Build a clickable dep list webview with resolved paths. Entries with a `path`
 * are clickable; entries without are shown dimmed (external/stdlib/crate deps).
 * `transitive` entries are collapsed under a `<details>` summary.
 */
export function buildDepListHtml(
  title: string,
  direct: DepEntry[],
  transitive: DepEntry[]
): string {
  const li = (e: DepEntry): string => {
    if (e.path) {
      return `<li class="dep" data-path="${escHtml(e.path)}">${escHtml(e.display)}</li>`;
    }
    return `<li class="dep-ext" title="External / stdlib — no local file">${escHtml(e.display)}</li>`;
  };
  const directRows = direct.map(li).join("\n") || "<li><em>none</em></li>";
  const transitiveBlock = transitive.length
    ? `<details><summary>Transitive (${transitive.length})</summary>
<ul class="deps">${transitive.map(li).join("\n")}</ul></details>`
    : "";
  return `<!DOCTYPE html><html><head>
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
<style>
  :root { --bg:#141414; --bg-elev:#1a1a1a; --border:#4d4d4d; --fg:#f6f1ed; --fg-muted:#c8b7ab; --green:#86df86; }
  @media (prefers-color-scheme: light) {
    :root { --bg:#f6f1ed; --bg-elev:#fbfaf9; --border:#e2d4ca; --fg:#1a1a1a; --fg-muted:#705f54; --green:#429429; }
  }
  body { font-family: var(--vscode-font-family); padding: 16px; color: var(--fg); background: var(--bg); }
  h3 { margin: 0 0 12px; font-size: 14px; }
  ul.deps { list-style: none; margin: 0; padding: 0; }
  li.dep { font-family: var(--vscode-editor-font-family, monospace); padding: 3px 6px;
    cursor: pointer; border-radius: 4px; font-size: 12px; }
  li.dep:hover { background: var(--bg-elev); color: var(--green); }
  li.dep-ext { font-family: var(--vscode-editor-font-family, monospace); padding: 3px 6px;
    border-radius: 4px; font-size: 12px; color: var(--fg-muted); cursor: default; }
  summary { cursor: pointer; margin: 12px 0 6px; font-weight: 600; font-size: 12px; color: var(--fg-muted); }
</style></head><body>
<h3>${title}</h3>
<ul class="deps">${directRows}</ul>
${transitiveBlock}
<script>
  const vscode = acquireVsCodeApi();
  document.querySelectorAll('li.dep').forEach(function(el){
    el.addEventListener('click', function(){
      vscode.postMessage({ command: 'open', path: el.getAttribute('data-path') });
    });
  });
</script>
</body></html>`;
}

/**
 * Build a clickable file-list webview (used by blast radius etc.). Each entry
 * posts `{command:'open',path}` back to the extension.
 */
export function buildClickableFileListHtml(
  title: string,
  direct: string[],
  transitive: string[]
): string {
  const li = (f: string): string => {
    const clean = f.replace(/^\s*↳\s*/, "").trim();
    return `<li class="dep" data-path="${escHtml(clean)}">${escHtml(clean)}</li>`;
  };
  const directRows = direct.map(li).join("\n") || "<li><em>none</em></li>";
  const transitiveBlock = transitive.length
    ? `<details><summary>Transitive (${transitive.length})</summary>
<ul class="deps">${transitive.map(li).join("\n")}</ul></details>`
    : "";
  return `<!DOCTYPE html><html><head>
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
<style>
  body { font-family: var(--vscode-font-family); padding: 16px; color: var(--vscode-foreground); }
  h3 { margin: 0 0 12px; }
  ul.deps { list-style: none; margin: 0; padding: 0; }
  li.dep { font-family: var(--vscode-editor-font-family, monospace); padding: 3px 6px; cursor: pointer; border-radius: 4px; }
  li.dep:hover { background: var(--vscode-list-hoverBackground); }
  summary { cursor: pointer; margin: 12px 0 6px; font-weight: 600; }
</style></head><body>
<h3>${title}</h3>
<ul class="deps">${directRows}</ul>
${transitiveBlock}
<script>
  const vscode = acquireVsCodeApi();
  document.querySelectorAll('li.dep').forEach(function(el){
    el.addEventListener('click', function(){
      vscode.postMessage({ command: 'open', path: el.getAttribute('data-path') });
    });
  });
</script>
</body></html>`;
}

// ── Command registrations ───────────────────────────────────────────────────

// Language tokens to strip from symbol queries so `fn foo` → `foo` matches backend inference.
// Mirrors the set in travsr-mcp's infer_language_from_query (tools.rs).
const LANG_TOKENS = new Set([
  "fn","func","function","def","class","struct","trait","impl","interface",
  "type","const","let","var","mod","module","pub","pub(crate)","async","static",
]);

/** Strip leading language tokens from a query string (client-side ranking parity). */
export function stripLangTokens(query: string): string {
  const words = query.trim().split(/\s+/);
  const stripped = words.filter((w) => !LANG_TOKENS.has(w.toLowerCase()));
  return (stripped.length > 0 ? stripped : words).join(" ");
}

// Session-level rate-limit for synonym suggestions: one per (query, symbolName) pair.
const synonymPromptedPairs = new Set<string>();

/**
 * travsr.askSymbol — live ranked symbol search. Reuses `get_graph_json` (its
 * nodes already carry path + line), debounced 250ms, with a stale-response
 * guard so out-of-order daemon replies never clobber newer input.
 *
 * Applies client-side language-token stripping (ITEM 4) before the query so
 * `fn foo` ranks the same as `foo` (mirrors travsr-mcp's infer_language_from_query).
 *
 * On accept: if the typed token differs from the selected symbol name, offers
 * a one-click synonym add (ITEM 5) gated by travsr.suggestSynonyms.
 */
export function registerAskSymbol(client: McpClient): vscode.Disposable {
  return vscode.commands.registerCommand("travsr.askSymbol", () => {
    const qp = vscode.window.createQuickPick<SymbolItem>();
    qp.placeholder = "Search symbols by name or natural language…";
    qp.matchOnDescription = true;
    let debounce: ReturnType<typeof setTimeout> | undefined;
    let queryAbort: AbortController | undefined;

    const run = (value: string): void => {
      if (!value.trim()) {
        qp.items = [];
        return;
      }
      queryAbort?.abort();
      queryAbort = new AbortController();
      const signal = queryAbort.signal;
      const normalised = stripLangTokens(value);
      qp.busy = true;
      void client
        .callTool(
          "get_graph_json",
          { query: normalised, direction: "both", depth: "1", kind_filter: "" },
          signal
        )
        .then((raw) => {
          if (signal.aborted || qp.value !== value) return;
          qp.items = parseGraphSymbols(raw);
          qp.busy = false;
        });
    };

    qp.onDidChangeValue((value) => {
      clearTimeout(debounce);
      debounce = setTimeout(() => run(value), 250);
    });

    qp.onDidAccept(() => {
      const sel = qp.selectedItems[0];
      if (!sel) { qp.hide(); return; }
      void openAtLine(sel.path, sel.line);
      qp.hide();

      // ITEM 5: synonym learning — offer to teach the backend when query ≠ selected name.
      const cfg = vscode.workspace.getConfiguration("travsr");
      if (!cfg.get<boolean>("suggestSynonyms", true)) return;
      const typedToken = stripLangTokens(qp.value).split(/\s+/)[0] ?? "";
      // Strip codicon prefix from label (e.g. "$(symbol-method) barFn" → "barFn")
      const selectedName = sel.label.replace(/^\$\([^)]+\)\s*/, "").trim();
      if (!typedToken || typedToken === selectedName) return;
      const pairKey = `${typedToken}\x00${selectedName}`;
      if (synonymPromptedPairs.has(pairKey)) return;
      synonymPromptedPairs.add(pairKey);
      void vscode.window
        .showInformationMessage(
          `Add synonym: "${typedToken}" → "${selectedName}"?`,
          "Add",
          "Skip"
        )
        .then((choice) => {
          if (choice === "Add") {
            void client.callTool("synonym_add", { term: typedToken, alias: selectedName });
          }
        });
    });

    qp.onDidHide(() => {
      clearTimeout(debounce);
      queryAbort?.abort();
      qp.dispose();
    });
    qp.show();
  });
}

// ── Managed webview panels (singleton per viewType) ─────────────────────────

/** Messages posted from the management webviews back to the extension. */
type PanelMessage =
  | { command: "add"; term: string; alias: string }
  | { command: "addBatch"; term: string; aliases: string[] }
  | { command: "removePair"; term: string; alias: string }
  | { command: "removeTerm"; term: string }
  | { command: "reset" }
  | { command: "prune" }
  | { command: "remove"; name: string }
  | { command: "installLang"; language: string }
  | { command: "removeLang"; language: string }
  | { command: "enableWithPermission"; language: string }
  | { command: "detectLangs" }
  | { command: "reloadAvailable" }
  | { command: "pickRepo" }
  | { command: "initRepo" }
  | { command: "openFile"; path: string }
  | { command: "refreshLog" }
  | { command: "refresh" };

const managedPanels = new Map<string, { panel: vscode.WebviewPanel; refresh: () => Promise<void> }>();

/** Re-render every open managed panel — call after an external `travsr init` updates graph.db. */
export function refreshOpenPanels(): void {
  for (const { refresh } of managedPanels.values()) {
    void refresh();
  }
}

/**
 * Open (or reveal) a singleton management webview. `render` produces the HTML;
 * `handle` reacts to a posted message and may call the provided `refresh`.
 */
type RefreshFn = (override?: string) => Promise<void>;
/** Posts a status bar update to the active webview (`type` defaults to `'loading'`). */
type PostStatus = (text: string, type?: 'loading' | 'error' | 'ok') => void;

function openManagedPanel(
  viewType: string,
  title: string,
  render: () => Promise<string>,
  handle: (msg: PanelMessage, refresh: RefreshFn, postStatus: PostStatus) => Promise<void>
): void {
  const existing = managedPanels.get(viewType);
  if (existing) {
    existing.panel.reveal(vscode.ViewColumn.Active);
    existing.panel.webview.html = buildPanelLoadingHtml(title);
    void existing.refresh();
    return;
  }
  const panel = vscode.window.createWebviewPanel(viewType, title, vscode.ViewColumn.Active, {
    enableScripts: true,
    localResourceRoots: [],
  });
  const refresh: RefreshFn = async (override?: string): Promise<void> => {
    try {
      panel.webview.html = override ?? await render();
    } catch {
      // Render failed — show an error state so the status bar is never orphaned.
      panel.webview.html = buildPanelLoadingHtml(`${title} (error — try reopening)`);
    }
  };
  // Sends a status update into the live webview HTML (cleared on next full re-render).
  const postStatus: PostStatus = (text, type = 'loading') =>
    void panel.webview.postMessage({ command: 'setStatus', text, type });

  managedPanels.set(viewType, { panel, refresh: () => refresh() });
  panel.webview.onDidReceiveMessage((msg: PanelMessage) => {
    // Unlock all button spinners IMMEDIATELY — before any async work.
    void panel.webview.postMessage({ command: 'unlockButtons' });
    void handle(msg, refresh, postStatus);
  });
  panel.onDidDispose(() => managedPanels.delete(viewType));
  panel.webview.html = buildPanelLoadingHtml(title);
  void refresh();
}

/**
 * travsr.manageSynonyms — interactive synonym editor webview backed by the
 * synonym_* MCP tools. Supports multi-chip staged batch add.
 */
export function registerManageSynonyms(client: McpClient): vscode.Disposable {
  const render = async (): Promise<string> =>
    buildSynonymsHtml(parseSynonymList(await client.callTool("synonym_list")));

  const warnIfError = (result: string): void => {
    const trimmed = result.trim();
    if (trimmed && trimmed !== "ok") {
      void vscode.window.showWarningMessage(`Travsr: ${trimmed}`);
    }
  };

  const handle = async (msg: PanelMessage, refresh: RefreshFn, _postStatus: PostStatus): Promise<void> => {
    switch (msg.command) {
      case "add":
        warnIfError(await client.callTool("synonym_add", { term: msg.term, alias: msg.alias }));
        break;
      case "addBatch":
        // synonym_set is atomic: replaces all aliases for the term in one write.
        warnIfError(await client.callTool("synonym_set", { term: msg.term, aliases: msg.aliases.join(",") }));
        break;
      case "removePair":
        await client.callTool("synonym_remove", { term: msg.term, alias: msg.alias });
        break;
      case "removeTerm":
        await client.callTool("synonym_remove_term", { term: msg.term });
        break;
      case "reset": {
        const confirm = await vscode.window.showWarningMessage(
          "Reset all synonyms to the built-in defaults? Custom entries will be lost.",
          { modal: true },
          "Reset"
        );
        if (confirm !== "Reset") return;
        warnIfError(await client.callTool("synonym_reset"));
        break;
      }
      default:
        break;
    }
    await refresh();
  };

  return vscode.commands.registerCommand("travsr.manageSynonyms", () =>
    openManagedPanel("travsrSynonyms", "Travsr: Synonyms", render, handle)
  );
}

/**
 * travsr.showRepos — registry manager webview (status badges, prune, remove)
 * backed by the repos_* MCP tools.
 */
export function registerShowRepos(client: McpClient): vscode.Disposable {
  const render = async (): Promise<string> =>
    buildReposHtml(parseReposList(await client.callTool("repos_list")));

  const handle = async (msg: PanelMessage, refresh: RefreshFn, _postStatus: PostStatus): Promise<void> => {
    if (msg.command === "prune") {
      const result = stripEnvelope(await client.callTool("repos_prune"));
      const m = /^pruned:\s*(\d+)/.exec(result.trim());
      void vscode.window.showInformationMessage(
        `Pruned ${m ? m[1] : "0"} stale repo(s).`
      );
    } else if (msg.command === "remove") {
      await client.callTool("repos_remove", { name: (msg as { command: "remove"; name: string }).name });
    }
    await refresh();
  };

  return vscode.commands.registerCommand("travsr.showRepos", () =>
    openManagedPanel("travsrRepos", "Travsr: Repos", render, handle)
  );
}

/**
 * travsr.showGraphStats — read-only metrics dashboard webview.
 */
export function registerShowGraphStats(client: McpClient): vscode.Disposable {
  // Kept so a follow tick can redraw the log without re-running the two
  // expensive halves of a render. Undefined until the first full pass, so a
  // log-only refresh before then falls back to doing the work.
  let lastStats: StatsView | undefined;
  let lastDiags: Diagnostic[] = [];
  let logOnly = false;

  const render = async (): Promise<string> => {
    const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const reuse = logOnly && lastStats !== undefined;
    const stats = reuse ? (lastStats as StatsView) : buildStatsView(await client.callTool("get_graph_stats"));
    // Read straight from the log file rather than asking the daemon: it works
    // after a crash, which is when the panel is worth opening. This is the
    // cheap half, and the only half a follow tick needs.
    const log = root ? readDaemonLogTail(root) : [];
    const bin = vscode.workspace.getConfiguration("travsr").get<string>("binaryPath") || "travsr";
    // readDiagnostics spawns `travsr status`.
    const diags = reuse ? lastDiags : root ? await readDiagnostics(bin, root) : [];
    lastStats = stats;
    lastDiags = diags;
    return buildStatsHtml(stats, log, diags);
  };

  const handle = async (msg: PanelMessage, refresh: RefreshFn, _postStatus: PostStatus): Promise<void> => {
    if (msg.command === "openFile") {
      // The log writes absolute paths in some places and repo-relative in
      // others, so both resolve against the repo root. The result must stay
      // inside it: the panel renders whatever the log file says, and a log file
      // is not a trusted input just because it is local. Without this a `path=`
      // field naming anything on disk becomes a click that opens it.
      const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      if (root === undefined) return;
      const target = path.resolve(root, msg.path);
      const rel = path.relative(root, target);
      if (rel.startsWith("..") || path.isAbsolute(rel)) {
        void vscode.window.showWarningMessage(
          `Travsr: ${msg.path} is outside the workspace, not opening it`
        );
        return;
      }
      try {
        const doc = await vscode.workspace.openTextDocument(target);
        await vscode.window.showTextDocument(doc, { preview: true });
      } catch {
        // The file the log complained about may be the file that is gone.
        void vscode.window.showWarningMessage(`Travsr: cannot open ${msg.path}`);
      }
      return;
    }
    if (msg.command === "refreshLog") {
      logOnly = true;
      try {
        await refresh();
      } finally {
        logOnly = false;
      }
      return;
    }
    await refresh();
  };

  return vscode.commands.registerCommand("travsr.showGraphStats", () =>
    openManagedPanel("travsrStats", "Travsr: Graph Stats", render, handle)
  );
}

/**
 * travsr.showDependencies — direct + transitive imports of a file, with
 * click-to-navigate for resolvable local imports. External/stdlib/crate deps
 * are shown dimmed and non-clickable.
 */
export function registerShowDependencies(client: McpClient): vscode.Disposable {
  return vscode.commands.registerCommand("travsr.showDependencies", async (file?: string) => {
    const activeFile = vscode.window.activeTextEditor?.document.fileName;
    const target =
      file ??
      (activeFile
        ? vscode.workspace.asRelativePath(activeFile)
        : undefined);
    if (!target) {
      void vscode.window.showInformationMessage("Open a file to see its dependencies.");
      return;
    }

    const raw = stripEnvelope(
      await client.callTool("get_dependencies", { file: target, transitive: "true", depth: "3" })
    );
    const lines = raw.split("\n").map((l) => l.replace(/\s+$/, "")).filter((l) => l.trim());
    const directLines = lines.filter((l) => !l.startsWith(" ") && !l.includes("↳"));
    const transitiveLines = lines.filter((l) => l.startsWith(" ") || l.includes("↳"));

    // Resolve specifiers to file paths when possible.
    const sourceAbsPath = activeFile ?? (
      vscode.workspace.workspaceFolders?.[0]?.uri.fsPath
        ? path.join(vscode.workspace.workspaceFolders[0].uri.fsPath, target)
        : target
    );

    const toEntry = (spec: string): import("./commands").DepEntry => {
      const clean = spec.replace(/^\s*↳\s*/, "").trim();
      const resolved = resolveDepSpec(clean, sourceAbsPath);
      const colon = clean.indexOf(":");
      const display = colon >= 0 ? clean.slice(colon + 1) : clean;
      return { display, path: resolved };
    };

    const direct = directLines.map(toEntry);
    const transitive = transitiveLines.map(toEntry);

    const panel = vscode.window.createWebviewPanel(
      "travsrDependencies",
      `Dependencies — ${target}`,
      vscode.ViewColumn.Beside,
      { enableScripts: true, localResourceRoots: [] }
    );
    panel.webview.html = buildDepListHtml(
      `Dependencies of <code>${escHtml(target)}</code>`,
      direct,
      transitive
    );
    panel.webview.onDidReceiveMessage((msg: { command?: string; path?: string }) => {
      if (msg.command === "open" && msg.path) void openAtLine(msg.path);
    });
  });
}

/**
 * travsr.showExecutionPath — prompt for source + sink (source seeded from the
 * word under the cursor), then render the PCST path in the graph panel.
 */
export function registerShowExecutionPath(
  client: McpClient,
  context: vscode.ExtensionContext
): vscode.Disposable {
  return vscode.commands.registerCommand("travsr.showExecutionPath", async () => {
    const editor = vscode.window.activeTextEditor;
    const seed = editor
      ? editor.document.getText(editor.document.getWordRangeAtPosition(editor.selection.active))
      : "";

    const source = await vscode.window.showInputBox({
      prompt: "Source symbol",
      value: seed,
    });
    if (!source) return;
    const sink = await vscode.window.showInputBox({ prompt: "Sink symbol" });
    if (!sink) return;

    const raw = await client.callTool("get_execution_path", { source, sink });
    const data = parseExecutionPath(raw);
    if (data.nodes.length === 0) {
      void vscode.window.showInformationMessage(`No path found from ${source} to ${sink}.`);
      return;
    }
    const panel = GraphPanel.show(client, context);
    panel.renderPath(data, `${source} → ${sink}`);
  });
}

/**
 * What is wrong with this repo's index, as `travsr status` reports it.
 *
 * The CLI already phrases every one of these for a person and names the command
 * that fixes it, so this parses that rather than reimplementing the mapping in
 * TypeScript and letting the two drift.
 *
 * These are all repo-scoped: an analyzer that crashed, a language with no tool
 * registered, an approval that was never given. None of them belong to a file,
 * which is why they are cards here rather than entries in the Problems panel,
 * which wants a file to attach to.
 */
export async function readDiagnostics(binary: string, cwd: string): Promise<Diagnostic[]> {
  const out = await spawnLangCommand(binary, ["status"], cwd);
  const found: Diagnostic[] = [];
  for (const line of out.split("\n")) {
    const m = /^\s*warning:\s*(.+)$/.exec(line);
    if (!m) continue;
    const text = m[1].trim();
    // The CLI writes the fix in backticks. Lift it out so it can be copied
    // without the reader having to pick it out of the sentence.
    const cmd = /`([^`]+)`/.exec(text);
    const severity: Diagnostic["severity"] = /crashed|failed|not usable/i.test(text)
      ? "error"
      : "warn";
    found.push({
      severity,
      title: text.replace(/\s*[-—.]?\s*(re-?run|run)\s+`[^`]+`.*$/i, "").trim(),
      hint: text,
      command: cmd ? cmd[1] : undefined,
    });
  }
  return found;
}

/** Spawn a travsr CLI command and return its combined stdout+stderr.
 *
 *  For fast, local, read-only commands (`lang list`, `lang remove`): a short
 *  wall-clock timeout is fine here because these do no network I/O, so a hang
 *  means something is wrong and killing it is the right move. Network installs
 *  must NOT use this — see `spawnManagedInstall`. */
/** Run a short lang command and resolve its combined output plus exit code.
 *  `code` is `null` on timeout or spawn error, so a caller that must confirm a
 *  command actually succeeded (a security-consent grant) can check `code === 0`
 *  rather than trust empty output. */
function spawnLangCommandResult(
  binary: string,
  args: string[],
  cwd?: string,
  timeoutMs = 4_000
): Promise<{ out: string; code: number | null }> {
  return new Promise((resolve) => {
    let out = "";
    let resolved = false;
    const done = (v: { out: string; code: number | null }): void => { if (!resolved) { resolved = true; resolve(v); } };
    const proc = cp.spawn(binary, args, { env: { ...process.env, TERM: "dumb", NO_COLOR: "1" }, ...(cwd ? { cwd } : {}) });
    proc.stdout?.on("data", (d: Buffer) => { out += d.toString(); });
    proc.stderr?.on("data", (d: Buffer) => { out += d.toString(); });
    const timer = setTimeout(() => { try { proc.kill(); } catch { /* ignore */ } done({ out, code: null }); }, timeoutMs);
    proc.on("close", (code) => { clearTimeout(timer); done({ out, code }); });
    proc.on("error", (e) => { clearTimeout(timer); done({ out: `error: ${e.message}`, code: null }); });
  });
}

function spawnLangCommand(binary: string, args: string[], cwd?: string, timeoutMs = 4_000): Promise<string> {
  return spawnLangCommandResult(binary, args, cwd, timeoutMs).then((r) => r.out);
}

/** The last non-empty line of CLI output — the final status the command printed
 *  (e.g. "'rust' is active — full cross-file analysis is on."). Empty when the
 *  command printed nothing. */
function lastLine(s: string): string {
  const lines = s.split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
  return lines.length ? lines[lines.length - 1] : "";
}

/** Run a network-bound install command (`lang install`, `lang detect --yes`,
 *  `init`) under a cancellable progress notification.
 *
 *  Deliberately imposes NO wall-clock timeout. A fixed timer is the wrong tool
 *  here: on a slow connection it SIGKILLs the CLI mid-download and — because the
 *  killed process resolves with empty output — the panel would report a false
 *  success while leaving a half-finished install behind. Instead the worst case
 *  is bounded by the CLI's own per-download network timeouts, and the user can
 *  stop it at any time from the notification's Cancel button. The result says
 *  whether the user cancelled so the caller can report an honest outcome. */
function spawnManagedInstall(
  binary: string,
  args: string[],
  cwd: string,
  title: string
): Thenable<{ out: string; cancelled: boolean; code: number | null }> {
  return vscode.window.withProgress(
    { location: vscode.ProgressLocation.Notification, title, cancellable: true },
    (_progress, token) =>
      new Promise((resolve) => {
        let out = "";
        let settled = false;
        const finish = (r: { out: string; cancelled: boolean; code: number | null }): void => {
          if (!settled) { settled = true; resolve(r); }
        };
        const proc = cp.spawn(binary, args, {
          env: { ...process.env, TERM: "dumb", NO_COLOR: "1" },
          cwd,
        });
        proc.stdout?.on("data", (d: Buffer) => { out += d.toString(); });
        proc.stderr?.on("data", (d: Buffer) => { out += d.toString(); });
        token.onCancellationRequested(() => {
          try { proc.kill(); } catch { /* ignore */ }
          finish({ out, cancelled: true, code: null });
        });
        // Exit code carries meaning: `lang install` exits 2 when the language was
        // set up but the project build tool it needs (e.g. Gradle, sbt, composer)
        // is not installed — a partial success the caller reports honestly.
        proc.on("close", (code) => finish({ out, cancelled: false, code }));
        proc.on("error", (e) =>
          finish({ out: `${out}\nerror: ${e.message}`, cancelled: false, code: null })
        );
      })
  );
}

/**
 * travsr.showLanguages — Languages panel: indexed node counts from the graph +
 * available SCIP tools from `travsr lang list --json`, with one-click install,
 * elevated consent form, and disable.
 */
export function registerShowLanguages(
  client: McpClient,
  binary: string,
  activeRepo: ActiveRepo,
  onAfterInit?: () => void
): vscode.Disposable {
  // `lang install` / `lang detect` / `init` run with the targeted repo as cwd, so
  // the CLI derives the corpus exactly as the daemon does (git remote) and enables
  // the right repo. The extension used to pass `--corpus <folder-basename>`, which
  // never matched the daemon's corpus, and it only ever looked at the first
  // workspace folder — wrong or ambiguous the moment several repos are open. The
  // user now picks the target (ActiveRepo), shown in the status bar.

  // Read the configured binary path at call time so we always use the value
  // written by checkBinaryAndPrompt, which runs async after activation.
  const getBinary = (): string =>
    vscode.workspace.getConfiguration("travsr").get<string>("binaryPath") || binary;

  let cachedAvailable: LangInfo[] = [];
  let availableLoaded = false;

  const render = async (): Promise<string> => {
    const langsRaw = await client.callTool("repo_languages");
    if (!availableLoaded) {
      // Run in the target repo so the CLI computes the per-repo "This repo" column
      // (enabled / not enabled / …). Without a cwd it runs outside any repo and
      // every non-builtin reads "n/a" (no_repo). `current()` never prompts.
      const raw = await spawnLangCommand(getBinary(), ["lang", "list", "--json"], activeRepo.current());
      cachedAvailable = parseAvailableLanguages(raw);
      availableLoaded = true;
    }
    // Show the target repo in the panel only when several are open — with one
    // repo there is no ambiguity to surface.
    const target = activeRepo.hasChoice() ? activeRepo.currentName() : undefined;
    return buildLanguagesHtml(parseLanguageCounts(langsRaw), cachedAvailable, target);
  };

  // Buttons are unlocked immediately by openManagedPanel's 'unlockButtons' postMessage
  // sent before handle() is ever called. postStatus drives the in-panel status bar for
  // operations that take >1s (install, detect, reload).
  const handle = async (msg: PanelMessage, refresh: RefreshFn, postStatus: PostStatus): Promise<void> => {
    if (msg.command === "reloadAvailable") {
      availableLoaded = false;
      postStatus('Reloading available tools…');
      void spawnLangCommand(getBinary(), ["lang", "list", "--json"], activeRepo.current()).then((raw) => {
        cachedAvailable = parseAvailableLanguages(raw);
        availableLoaded = true;
        postStatus(""); // clear immediately — never couple clear to render()/callTool
        void refresh();
      });
      return;
    }

    switch (msg.command) {
      case "installLang": {
        // Prompt once if which repo is ambiguous; abort if dismissed.
        const repo = await activeRepo.ensureChosen();
        if (!repo) return;
        const args = ["lang", "install", msg.language, "--no-interactive", "--yes"];
        // cwd = the chosen repo so the CLI auto-enables it. Runs under a
        // cancellable progress notification with no wall-clock kill, so a slow
        // download is never cut off mid-flight and reported as a false success.
        void spawnManagedInstall(getBinary(), args, repo, `Installing ${msg.language}…`).then(({ out, cancelled, code }) => {
          availableLoaded = false;
          void refresh();
          if (cancelled) {
            void vscode.window.showWarningMessage(
              `Install of ${msg.language} was cancelled — it may be partly done. Re-run, or run \`travsr lang install ${msg.language}\` in a terminal.`
            );
          } else if (code === 2) {
            // Set up, but the project build tool it needs is not installed, so full
            // analysis cannot run yet. The CLI already phrases this; name the tool
            // from the Prerequisites column as a fallback.
            const need = cachedAvailable.find((x) => x.language === msg.language)?.prerequisites;
            const needTxt = need && need !== "none" ? ` (${need})` : "";
            void vscode.window.showWarningMessage(
              lastLine(out) ||
                `${msg.language} is set up, but the build tool it needs${needTxt} was not found. Install it, then Reload to get full analysis.`
            );
          } else {
            void vscode.window.showInformationMessage(lastLine(out) || `${msg.language} tool installed.`);
          }
        });
        return;
      }
      case "removeLang":
        postStatus(`Disabling ${msg.language}…`);
        void spawnLangCommand(getBinary(), ["lang", "remove", msg.language]).then(() => {
          availableLoaded = false;
          postStatus("");
          void refresh();
          void vscode.window.showInformationMessage(`Disabled language tool for ${msg.language}.`);
        });
        return;
      case "enableWithPermission": {
        // A security-relevant grant: full analysis for this language will run with
        // the user's own privileges (its build tools cannot run isolated on this
        // OS). Confirm in plain language first, then record it and re-index so it
        // takes effect — no command to type.
        const ok = await vscode.window.showWarningMessage(
          `Allow full analysis for ${msg.language} to run on this machine?`,
          {
            modal: true,
            detail:
              "It will use your project's own build tools — the same as if you ran the build yourself — including downloading this project's dependencies. You can withdraw this permission later.",
          },
          "Allow"
        );
        if (ok !== "Allow") return;
        const repo = await activeRepo.ensureChosen();
        if (!repo) return;
        postStatus(`Enabling ${msg.language}…`);
        // Record the grant first, and stop if it fails. The modal above is the
        // explicit user grant, so pass `--yes`: the CLI refuses a non-interactive
        // grant without it (a VS Code spawn never has a terminal). Check the exit
        // code — a security-consent step must not report success unless the
        // permission was actually recorded.
        const grant = await spawnLangCommandResult(getBinary(), [
          "lang",
          "allow-unsandboxed",
          msg.language,
          "--yes",
        ]);
        if (grant.code !== 0) {
          postStatus("");
          void refresh();
          void vscode.window.showErrorMessage(
            `Could not enable ${msg.language}: ${lastLine(grant.out) || "the permission was not recorded."}`
          );
          return;
        }
        const { cancelled } = await spawnManagedInstall(
          getBinary(),
          ["init", "--semantic", "--force"],
          repo,
          `Enabling ${msg.language}…`
        );
        availableLoaded = false;
        postStatus("");
        void refresh();
        if (cancelled) {
          void vscode.window.showWarningMessage(
            `Enabling ${msg.language} was cancelled before analysis finished. Use Reload to try again.`
          );
        } else {
          void vscode.window.showInformationMessage(
            `${msg.language} is enabled. Full analysis will run on the next index.`
          );
        }
        return;
      }
      case "pickRepo":
        await activeRepo.pick();
        void refresh();
        return;
      case "initRepo": {
        const repo = await activeRepo.ensureChosen();
        if (!repo) return;
        // `init` rebuilds the graph and can run long on a large repo; cancellable
        // progress, no fixed kill.
        void spawnManagedInstall(getBinary(), ["init"], repo, "Initializing repo…").then(({ cancelled }) => {
          if (cancelled) {
            void vscode.window.showWarningMessage("Repo initialization was cancelled.");
            return;
          }
          // Graph rebuilt — evict stale blast-radius and caller counts.
          onAfterInit?.();
          refreshOpenPanels();
        });
        return;
      }
      case "detectLangs": {
        const repo = await activeRepo.ensureChosen();
        if (!repo) return;
        // cwd = the chosen repo so detect scans it, not the extension host's cwd.
        // `--yes` makes the button live up to its "Detect & install" label: a
        // spawned process has no terminal, so a bare `lang detect` would only ever
        // print the list and install nothing. It may download an analyzer per
        // detected language, so it runs under a cancellable notification with no
        // wall-clock kill (a fixed timer would cut a slow batch off mid-download
        // and report a false "complete"). Elevated languages that need approval
        // are skipped by the CLI, never installed silently.
        void spawnManagedInstall(getBinary(), ["lang", "detect", "--yes"], repo, "Detecting & installing languages…").then(({ cancelled }) => {
          availableLoaded = false;
          void refresh();
          void vscode.window.showInformationMessage(
            cancelled
              ? "Detect & install was cancelled — some languages may not be set up. See the Languages panel."
              : "Detect & install finished. See the Languages panel for per-language status."
          );
        });
        return;
      }
      case "refresh":
        availableLoaded = false;
        await refresh();
        return;
      default:
        break;
    }
    await refresh();
  };

  return vscode.commands.registerCommand("travsr.showLanguages", () =>
    openManagedPanel("travsrLanguages", "Travsr: Languages", render, handle)
  );
}

/** Register all VSCODE-247 commands. */
export function registerParityCommands(
  client: McpClient,
  context: vscode.ExtensionContext,
  binary: string,
  onAfterInit?: () => void
): void {
  const activeRepo = new ActiveRepo(context);
  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.selectRepository", () => activeRepo.pick()),
    registerAskSymbol(client),
    registerManageSynonyms(client),
    registerShowDependencies(client),
    registerShowExecutionPath(client, context),
    registerShowRepos(client),
    registerShowGraphStats(client),
    registerShowLanguages(client, binary, activeRepo, onAfterInit)
  );
}
