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
 *   travsr.showLanguages     indexed + available languages, install/approve from UI
 *
 * Pure helpers (stripEnvelope, parsers, openAtLine) are exported for unit tests.
 */

import * as cp from "child_process";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import type { McpClient } from "./mcp";
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

/**
 * travsr.askSymbol — live ranked symbol search. Reuses `get_graph_json` (its
 * nodes already carry path + line), debounced 250ms, with a stale-response
 * guard so out-of-order daemon replies never clobber newer input.
 */
export function registerAskSymbol(client: McpClient): vscode.Disposable {
  return vscode.commands.registerCommand("travsr.askSymbol", () => {
    const qp = vscode.window.createQuickPick<SymbolItem>();
    qp.placeholder = "Search symbols by name or natural language…";
    qp.matchOnDescription = true;
    let debounce: ReturnType<typeof setTimeout> | undefined;

    const run = (value: string): void => {
      if (!value.trim()) {
        qp.items = [];
        return;
      }
      qp.busy = true;
      void client
        .callTool("get_graph_json", {
          query: value,
          direction: "both",
          depth: "1",
          kind_filter: "",
        })
        .then((raw) => {
          if (qp.value !== value) return;
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
      if (sel) void openAtLine(sel.path, sel.line);
      qp.hide();
    });
    qp.onDidHide(() => {
      clearTimeout(debounce);
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
  | { command: "approveLang"; language: string; approvedBy: string; reason: string; permittedHosts: string }
  | { command: "removeLang"; language: string }
  | { command: "detectLangs" }
  | { command: "reloadAvailable" }
  | { command: "initRepo" }
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
        for (const alias of msg.aliases) {
          const result = await client.callTool("synonym_add", { term: msg.term, alias });
          const trimmed = result.trim();
          if (trimmed && trimmed !== "ok") {
            void vscode.window.showWarningMessage(`Travsr synonyms: ${trimmed} (alias "${alias}" and later aliases skipped)`);
            break;
          }
        }
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
  const render = async (): Promise<string> =>
    buildStatsHtml(buildStatsView(await client.callTool("get_graph_stats")));

  const handle = async (_msg: PanelMessage, refresh: RefreshFn, _postStatus: PostStatus): Promise<void> => {
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

/** Spawn a travsr CLI command and return its combined stdout+stderr. */
function spawnLangCommand(binary: string, args: string[], cwd?: string, timeoutMs = 4_000): Promise<string> {
  return new Promise((resolve) => {
    let out = "";
    let resolved = false;
    const done = (v: string): void => { if (!resolved) { resolved = true; resolve(v); } };
    const proc = cp.spawn(binary, args, { env: { ...process.env, TERM: "dumb", NO_COLOR: "1" }, ...(cwd ? { cwd } : {}) });
    proc.stdout?.on("data", (d: Buffer) => { out += d.toString(); });
    proc.stderr?.on("data", (d: Buffer) => { out += d.toString(); });
    const timer = setTimeout(() => { try { proc.kill(); } catch { /* ignore */ } done(""); }, timeoutMs);
    proc.on("close", () => { clearTimeout(timer); done(out); });
    proc.on("error", (e) => { clearTimeout(timer); done(`error: ${e.message}`); });
  });
}

/**
 * travsr.showLanguages — Languages panel: indexed node counts from the graph +
 * available SCIP tools from `travsr lang list --json`, with one-click install,
 * elevated consent form, and disable.
 */
export function registerShowLanguages(
  client: McpClient,
  binary: string,
  onAfterInit?: () => void
): vscode.Disposable {
  const getCorpus = (): string =>
    path.basename(vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? "");

  // Read the configured binary path at call time so we always use the value
  // written by checkBinaryAndPrompt, which runs async after activation.
  const getBinary = (): string =>
    vscode.workspace.getConfiguration("travsr").get<string>("binaryPath") || binary;

  let cachedAvailable: LangInfo[] = [];
  let availableLoaded = false;

  const render = async (): Promise<string> => {
    const langsRaw = await client.callTool("repo_languages");
    if (!availableLoaded) {
      const raw = await spawnLangCommand(getBinary(), ["lang", "list", "--json"]);
      cachedAvailable = parseAvailableLanguages(raw);
      availableLoaded = true;
    }
    return buildLanguagesHtml(parseLanguageCounts(langsRaw), cachedAvailable);
  };

  // Buttons are unlocked immediately by openManagedPanel's 'unlockButtons' postMessage
  // sent before handle() is ever called. postStatus drives the in-panel status bar for
  // operations that take >1s (install, detect, reload).
  const handle = async (msg: PanelMessage, refresh: RefreshFn, postStatus: PostStatus): Promise<void> => {
    const corpus = getCorpus();

    if (msg.command === "reloadAvailable") {
      availableLoaded = false;
      postStatus('Reloading available tools…');
      void spawnLangCommand(getBinary(), ["lang", "list", "--json"]).then((raw) => {
        cachedAvailable = parseAvailableLanguages(raw);
        availableLoaded = true;
        postStatus(""); // clear immediately — never couple clear to render()/callTool
        void refresh();
      });
      return;
    }

    switch (msg.command) {
      case "installLang": {
        const args = ["lang", "install", msg.language, "--no-interactive", "--yes"];
        if (corpus) args.push("--corpus", corpus);
        postStatus(`Installing ${msg.language} tool…`);
        void spawnLangCommand(getBinary(), args).then(() => {
          availableLoaded = false;
          postStatus("");
          void refresh();
          void vscode.window.showInformationMessage(`${msg.language} tool installed.`);
        });
        return;
      }
      case "approveLang": {
        const m = msg as { command: "approveLang"; language: string; approvedBy: string; reason: string; permittedHosts: string };
        if (!m.approvedBy || !m.reason) {
          void vscode.window.showWarningMessage("Approver handle and reason are required.");
          return;
        }
        const approveArgs = [
          "lang", "approve", m.language,
          "--approved-by", m.approvedBy,
          "--reason", m.reason,
          "--permitted-hosts", m.permittedHosts || "",
        ];
        const installArgs = ["lang", "install", m.language, "--no-interactive", "--yes"];
        if (corpus) installArgs.push("--corpus", corpus);
        postStatus(`Installing ${m.language} with elevated approval…`);
        void spawnLangCommand(getBinary(), approveArgs)
          .then(() => spawnLangCommand(getBinary(), installArgs))
          .then(() => {
            availableLoaded = false;
            postStatus("");
            void refresh();
            void vscode.window.showInformationMessage(`${m.language} installed with elevated approval.`);
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
      case "initRepo": {
        const wsRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
        if (!wsRoot) return;
        postStatus("Initializing repo…");
        void spawnLangCommand(getBinary(), ["init"], wsRoot, 120_000).then(() => {
          postStatus("");
          // Graph rebuilt — evict stale blast-radius and caller counts.
          onAfterInit?.();
          refreshOpenPanels();
        });
        return;
      }
      case "detectLangs":
        postStatus('Detecting languages…');
        void spawnLangCommand(getBinary(), ["lang", "detect"]).then((out) => {
          void vscode.window.showInformationMessage(
            out.trim() ? `Detect: ${out.trim().slice(0, 120)}` : "Detection complete."
          );
          availableLoaded = false;
          postStatus("");
          void refresh();
        });
        return;
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
  context.subscriptions.push(
    registerAskSymbol(client),
    registerManageSynonyms(client),
    registerShowDependencies(client),
    registerShowExecutionPath(client, context),
    registerShowRepos(client),
    registerShowGraphStats(client),
    registerShowLanguages(client, binary, onAfterInit)
  );
}
