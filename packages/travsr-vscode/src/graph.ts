/**
 * GraphPanel — VS Code WebviewPanel for the Travsr code graph.
 *
 * This file is intentionally a thin host shell. All rendering logic lives in
 * media/graph.js (plain ES, no modules) and media/graph.css, loaded via
 * asWebviewUri so the CSP never needs 'unsafe-inline'.
 *
 * Public surface (unchanged from VSCODE-245):
 *   GraphPanel.show()   — create or reveal the singleton panel
 *   panel.query()       — trigger a new graph query
 *   panel.renderPath()  — render a pre-built PCST path
 *   panel.dispose()     — destroy the panel
 *
 * WebviewMessage (webview → extension):
 *   query          — user submitted a new symbol search
 *   goToDefinition — jump to source file:line in the editor
 *   showBlastRadius — delegate to travsr.showBlastRadius command
 *   showDependencies — delegate to travsr.showDependencies command
 *   exportDot      — copy DOT to clipboard
 *   exportJson     — save JSON via save dialog
 *   exportPng      — save PNG via save dialog (NEW)
 *   requestPeek    — read source lines for the peek panel (NEW)
 *
 * Extension → webview messages:
 *   render         — new graph data
 *   renderPeek     — source lines for the peek panel
 *   freshness      — graph stats (nodeCount, state) for the status bar
 *   diagnosticsOverlay — live LSP diagnostics per node (#688)
 */

import * as vscode from "vscode";
import type { McpClient } from "./mcp";

// ── Public types (consumed by commands.ts) ────────────────────────────────

export interface GraphNode {
  id: string;
  label: string;
  kind: string;
  path: string;
  package: string;
  score: number;
  root?: boolean;
  line?: number;
}

export interface GraphEdge {
  source: string;
  target: string;
  kind: string;
}

export interface GraphData {
  nodes: GraphNode[];
  edges: GraphEdge[];
}

// ── Diagnostics overlay (#688) ────────────────────────────────────────────

/** Worst severity found for a node. Info and Hint are deliberately ignored. */
export type NodeDiagnosticSeverity = "error" | "warning";

export interface NodeDiagnostic {
  severity: NodeDiagnosticSeverity;
  count: number;
}

export interface DiagnosticsOverlay {
  /** Node id → worst severity + count. Clean nodes are absent, not zeroed. */
  byNode: Record<string, NodeDiagnostic>;
  /**
   * Attribution granularity. `"file"` means a diagnostic anywhere in a file
   * badges every node from that file, because `get_graph_json` emits `line`
   * but no `end_line`, so there is no symbol span to test a range against.
   * Symbol-level attribution is the follow-up (#688 Option A).
   */
  scope: "file";
  /**
   * Graph file paths no provider has published diagnostics for. Absence of
   * diagnostics is not evidence of correctness — a file whose language has no
   * extension installed looks identical to a clean one — so the panel says
   * "not diagnosed" rather than implying "clean".
   */
  unknownCoverage: string[];
}

/** Debounce window for diagnostics recomputation. */
export const DIAGNOSTICS_DEBOUNCE_MS = 200;

// ── Message protocol ──────────────────────────────────────────────────────

type WebviewMessage =
  | { command: "query"; query: string; direction: string; depth: number; kind_filter?: string; mode?: string; path_prefix?: string; reqId?: number }
  | { command: "goToDefinition"; path: string; line?: number }
  | { command: "showBlastRadius"; path: string }
  | { command: "showDependencies"; path: string }
  | { command: "exportDot"; dot: string }
  | { command: "exportJson"; json: string }
  | { command: "exportPng"; dataUrl: string }
  | { command: "requestPeek"; path: string; line: number };

// ── Security helpers ──────────────────────────────────────────────────────

/**
 * Resolve a graph node path to a VS Code URI, rejecting anything that falls
 * outside the open workspace folders (prevents the peek/goto panel from
 * being used to read arbitrary files off the developer's filesystem).
 */
function resolveWorkspacePath(path: string): vscode.Uri | null {
  const folders = vscode.workspace.workspaceFolders ?? [];
  const root = folders[0]?.uri;
  const uri = path.startsWith("/")
    ? vscode.Uri.file(path)
    : root
    ? vscode.Uri.joinPath(root, path)
    : null;
  if (!uri) return null;
  const fsPath = uri.fsPath;
  const inWorkspace = folders.some((wf) => fsPath.startsWith(wf.uri.fsPath));
  if (!inWorkspace) {
    // eslint-disable-next-line no-console
    console.warn(`Travsr: blocked path outside workspace: ${fsPath}`);
    return null;
  }
  return uri;
}

// ── Diagnostics helpers (#688) ────────────────────────────────────────────

/**
 * Reduce a file's diagnostics to the one badge worth drawing: errors outrank
 * warnings, and the count is of the winning severity only (a file with 2
 * errors and 9 warnings reads "2 errors", not "11 problems"). Returns null
 * when nothing at error/warning level is present, so callers can omit the
 * node entirely rather than post a zero.
 */
function worstDiagnostic(
  diags: readonly vscode.Diagnostic[]
): NodeDiagnostic | null {
  let errors = 0;
  let warnings = 0;
  for (const d of diags) {
    if (d.severity === vscode.DiagnosticSeverity.Error) errors++;
    else if (d.severity === vscode.DiagnosticSeverity.Warning) warnings++;
  }
  if (errors > 0) return { severity: "error", count: errors };
  if (warnings > 0) return { severity: "warning", count: warnings };
  return null;
}

/**
 * Map the diagnostics VS Code already holds onto the nodes currently rendered.
 *
 * Reads `vscode.languages.getDiagnostics` only — Travsr spawns no language
 * server and hosts none. Diagnostics are looked up once per distinct file, not
 * once per node, because a graph routinely holds dozens of symbols from the
 * same file.
 *
 * Nodes whose path escapes the workspace are dropped by `resolveWorkspacePath`
 * and never appear in the result.
 */
export function computeDiagnosticsOverlay(
  nodes: GraphNode[]
): DiagnosticsOverlay {
  // A URI appears in the global list once some provider has published for it,
  // even if it published an empty array. That is the only signal available for
  // "was this file looked at at all" — getDiagnostics(uri) returns [] both for
  // a clean file and for one nothing has ever diagnosed.
  const published = new Set(
    vscode.languages.getDiagnostics().map(([uri]) => uri.fsPath)
  );

  const byNode: Record<string, NodeDiagnostic> = {};
  const unknownCoverage: string[] = [];
  const perFile = new Map<string, NodeDiagnostic | null>();

  for (const node of nodes) {
    if (!node.path) continue;
    if (!perFile.has(node.path)) {
      const uri = resolveWorkspacePath(node.path);
      if (!uri) {
        perFile.set(node.path, null);
      } else {
        if (!published.has(uri.fsPath)) unknownCoverage.push(node.path);
        perFile.set(node.path, worstDiagnostic(vscode.languages.getDiagnostics(uri)));
      }
    }
    const worst = perFile.get(node.path);
    if (worst) byNode[node.id] = worst;
  }

  return { byNode, scope: "file", unknownCoverage };
}

/**
 * Collapse a burst of calls into one deferred call. Diagnostics fire per
 * keystroke once several servers are running; recomputing the whole overlay
 * on each would burn the extension host for frames nobody sees.
 */
export function makeDebouncer(
  fn: () => void,
  ms: number
): { schedule(): void; dispose(): void } {
  let timer: ReturnType<typeof setTimeout> | undefined;
  return {
    schedule(): void {
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = undefined;
        fn();
      }, ms);
    },
    dispose(): void {
      if (timer) clearTimeout(timer);
      timer = undefined;
    },
  };
}

// ── Nonce helper ──────────────────────────────────────────────────────────

function getNonce(): string {
  const chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  let text = "";
  for (let i = 0; i < 32; i++) {
    text += chars.charAt(Math.floor(Math.random() * chars.length));
  }
  return text;
}

/** Parse node count from get_graph_stats output. */
function parseNodeCount(raw: string): number {
  const m = raw.match(/Nodes:\s*([\d,]+)/i);
  if (!m) return 0;
  return parseInt(m[1].replace(/,/g, ""), 10);
}

// ── GraphPanel ────────────────────────────────────────────────────────────

export class GraphPanel {
  static readonly viewType = "travsrGraphPanel";
  private static current: GraphPanel | undefined;

  private readonly panel: vscode.WebviewPanel;
  private disposables: vscode.Disposable[] = [];
  /** Nodes in the graph as currently rendered — the overlay's input set. */
  private renderedNodes: GraphNode[] = [];
  private readonly diagnosticsDebouncer = makeDebouncer(
    () => this.postDiagnosticsOverlay(),
    DIAGNOSTICS_DEBOUNCE_MS
  );

  private constructor(
    private readonly client: McpClient,
    context: vscode.ExtensionContext
  ) {
    const extUri = context.extensionUri;
    this.panel = vscode.window.createWebviewPanel(
      GraphPanel.viewType,
      "Travsr: Graph",
      vscode.ViewColumn.One,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
        localResourceRoots: [extUri],
      }
    );

    this.panel.webview.html = this.buildHtml(extUri);

    this.panel.onDidDispose(() => this.dispose(), null, this.disposables);
    this.panel.webview.onDidReceiveMessage(
      (msg: WebviewMessage) => this.handleMessage(msg),
      null,
      this.disposables
    );

    // #688: the language servers the user already runs are the only source of
    // live correctness data. Tied to the panel so it dies with it.
    vscode.languages.onDidChangeDiagnostics(
      () => this.diagnosticsDebouncer.schedule(),
      null,
      this.disposables
    );
  }

  static show(client: McpClient, context: vscode.ExtensionContext): GraphPanel {
    if (GraphPanel.current) {
      GraphPanel.current.panel.reveal(vscode.ViewColumn.One);
      return GraphPanel.current;
    }
    GraphPanel.current = new GraphPanel(client, context);
    return GraphPanel.current;
  }

  async query(
    query: string,
    direction = "both",
    depth = 2,
    kindFilter = "",
    reqId?: number,
    mode = "",
    pathPrefix = ""
  ): Promise<void> {
    if (mode === "overview") {
      this.panel.title = pathPrefix
        ? `Travsr: ${pathPrefix}`
        : "Travsr: Repo Map";
    } else {
      this.panel.title =
        kindFilter === "file" ? "Travsr: File Graph" : `Travsr: ${query}`;
    }

    // Fire stats concurrently — don't block render on it.
    void this.client.callTool("get_graph_stats").then((raw) => {
      const nodeCount = parseNodeCount(raw);
      void this.panel.webview.postMessage({
        command: "freshness",
        nodeCount,
        state: nodeCount > 0 ? "fresh" : "stale",
      });
    });

    const raw = await this.client.callTool("get_graph_json", {
      query,
      direction,
      depth: String(depth),
      kind_filter: kindFilter,
      mode,
      path_prefix: pathPrefix,
    });

    let data: GraphData = { nodes: [], edges: [] };
    try {
      if (raw) {
        data = JSON.parse(raw) as GraphData;
      }
    } catch {
      // Malformed JSON from daemon → render empty graph
    }

    void this.panel.webview.postMessage({
      command: "render",
      data,
      query,
      mode,
      pathPrefix,
      ...(reqId !== undefined ? { reqId } : {}),
    });

    this.renderedNodes = data.nodes ?? [];
    this.postDiagnosticsOverlay();
  }

  /**
   * Render a pre-built graph (e.g. PCST execution path) directly,
   * bypassing get_graph_json. Root-flagged nodes are highlighted.
   */
  renderPath(data: GraphData, query: string): void {
    this.panel.reveal(vscode.ViewColumn.One);
    this.panel.title = `Travsr: ${query}`;
    void this.panel.webview.postMessage({ command: "render", data, query });
    this.renderedNodes = data.nodes ?? [];
    this.postDiagnosticsOverlay();
  }

  /**
   * Recompute the overlay for the rendered node set and push it. Cheap enough
   * to run un-debounced on re-render (the node set just changed and the old
   * overlay is addressed to node ids that may no longer exist); the debounce
   * exists for the keystroke-driven path.
   */
  private postDiagnosticsOverlay(): void {
    if (this.renderedNodes.length === 0) return;
    const overlay = computeDiagnosticsOverlay(this.renderedNodes);
    void this.panel.webview.postMessage({
      command: "diagnosticsOverlay",
      ...overlay,
    });
  }

  private handleMessage(msg: WebviewMessage): void {
    switch (msg.command) {
      case "query":
        if (msg.query || msg.kind_filter === "file" || msg.mode === "overview") {
          void this.query(
            msg.query,
            msg.direction,
            msg.depth,
            msg.kind_filter ?? "",
            msg.reqId,
            msg.mode ?? "",
            msg.path_prefix ?? ""
          );
        }
        break;

      case "goToDefinition":
        if (msg.path) {
          const uri = resolveWorkspacePath(msg.path);
          if (!uri) break;
          if (msg.line != null) {
            void (async () => {
              const doc = await vscode.workspace.openTextDocument(uri);
              const lineIdx = msg.line! - 1;
              await vscode.window.showTextDocument(doc, {
                selection: new vscode.Range(lineIdx, 0, lineIdx, 0),
              });
            })();
          } else {
            void vscode.commands.executeCommand("vscode.open", uri);
          }
        }
        break;

      case "showBlastRadius":
        if (msg.path) {
          void vscode.commands.executeCommand(
            "travsr.showBlastRadius",
            msg.path
          );
        }
        break;

      case "showDependencies":
        if (msg.path) {
          void vscode.commands.executeCommand(
            "travsr.showDependencies",
            msg.path
          );
        }
        break;

      case "exportDot":
        void vscode.env.clipboard
          .writeText(msg.dot)
          .then(() =>
            vscode.window.showInformationMessage("Graph DOT copied to clipboard")
          );
        break;

      case "exportJson":
        void (async () => {
          const uri = await vscode.window.showSaveDialog({
            filters: { JSON: ["json"] },
            saveLabel: "Save graph JSON",
          });
          if (uri) {
            await vscode.workspace.fs.writeFile(
              uri,
              Buffer.from(msg.json, "utf8")
            );
          }
        })();
        break;

      case "exportPng":
        void (async () => {
          if (msg.dataUrl.length > 50 * 1024 * 1024) {
            vscode.window.showErrorMessage("Travsr: PNG too large to export.");
            return;
          }
          const uri = await vscode.window.showSaveDialog({
            filters: { PNG: ["png"] },
            saveLabel: "Save graph PNG",
          });
          if (uri) {
            // dataUrl = "data:image/png;base64,..."
            const base64 = msg.dataUrl.replace(/^data:image\/png;base64,/, "");
            await vscode.workspace.fs.writeFile(
              uri,
              Buffer.from(base64, "base64")
            );
            vscode.window.showInformationMessage("Graph PNG saved.");
          }
        })();
        break;

      case "requestPeek":
        void (async () => {
          const { path, line } = msg;
          const uri = resolveWorkspacePath(path);
          if (!uri) return;
          try {
            const doc = await vscode.workspace.openTextDocument(uri);
            const defLine = line - 1; // 0-indexed
            const start = Math.max(0, defLine - 8);
            const end = Math.min(doc.lineCount - 1, defLine + 25);
            const lines: Array<{ no: number; text: string }> = [];
            for (let i = start; i <= end; i++) {
              lines.push({ no: i + 1, text: doc.lineAt(i).text });
            }
            void this.panel.webview.postMessage({
              command: "renderPeek",
              path,
              line,
              lines,
            });
          } catch {
            // File not found or binary — silently fall back to go-to-def
            void this.panel.webview.postMessage({
              command: "renderPeek",
              path,
              line,
              lines: [{ no: line, text: `// ${path}:${line}` }],
            });
          }
        })();
        break;
    }
  }

  dispose(): void {
    GraphPanel.current = undefined;
    this.diagnosticsDebouncer.dispose();
    this.panel.dispose();
    for (const d of this.disposables) {
      d.dispose();
    }
    this.disposables = [];
  }

  // ── HTML scaffold ─────────────────────────────────────────────────────

  private buildHtml(extUri: vscode.Uri): string {
    const webview = this.panel.webview;
    const nonce = getNonce();
    const csp = webview.cspSource;

    const cssUri = webview
      .asWebviewUri(vscode.Uri.joinPath(extUri, "media", "graph.css"))
      .toString();
    const cyUri = webview
      .asWebviewUri(
        vscode.Uri.joinPath(extUri, "media", "vendor", "cytoscape.min.js")
      )
      .toString();
    const jsUri = webview
      .asWebviewUri(vscode.Uri.joinPath(extUri, "media", "graph.js"))
      .toString();
    const logoUri = webview
      .asWebviewUri(vscode.Uri.joinPath(extUri, "icon.png"))
      .toString();

    return buildHtmlContent(nonce, csp, cssUri, cyUri, jsUri, logoUri);
  }
}

// ── Exported HTML template (also used by tests without VS Code context) ───────

export function buildHtmlContent(
  nonce: string,
  csp: string,
  cssUri: string,
  cyUri: string,
  jsUri: string,
  logoUri: string,
): string {
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src ${csp} 'nonce-${nonce}'; style-src ${csp}; img-src ${csp} data:; font-src ${csp};">
<link rel="stylesheet" href="${cssUri}">
<title>Travsr Graph</title>
</head>
<body>

<!-- ── Top bar ─────────────────────────────────────────────────────────── -->
<div id="topbar">
  <div id="brand">
    <span class="logo"><img src="${logoUri}" alt="" width="20" height="20"></span>
    <span class="word">travsr</span>
    <span class="vtag">graph</span>
  </div>

  <span id="searchWrap">
    <input id="searchInput" type="text" placeholder="symbol name… (press / to focus)" autocomplete="off" autocorrect="off" spellcheck="false">
  </span>

  <div class="grp" id="grp-direction" aria-label="Direction">
    <span class="grp-label">show</span>
    <button class="btn" id="btn-callers" title="Callers only">← callers</button>
    <button class="btn active" id="btn-both" title="Callers and dependencies">both</button>
    <button class="btn" id="btn-deps" title="Dependencies only">deps →</button>
  </div>

  <div class="grp" id="grp-depth" aria-label="Depth">
    <span class="grp-label">depth</span>
    <input type="range" id="depthSlider" min="1" max="4" value="2"
           aria-label="Traversal depth">
    <span id="depthVal">2</span>
  </div>

  <div class="grp" id="grp-spread" aria-label="Spread">
    <span class="grp-label">spread</span>
    <input type="range" id="spreadSlider" min="1" max="5" step="0.1" value="1"
           aria-label="Node spread multiplier" style="width:72px">
    <span id="spreadVal">1×</span>
  </div>

  <div class="grp" id="grp-layout" aria-label="Layout">
    <span class="grp-label">layout</span>
    <button class="btn active" id="btn-flow" title="Semantic flow layout">⇄ flow</button>
    <button class="btn" id="btn-rings" title="Concentric ring layout">◎ rings</button>
  </div>

  <div class="grp" id="grp-toggles" aria-label="Toggles">
    <button class="btn active" id="btn-group" title="Group symbols by file">▣ files</button>
    <button class="btn" id="btn-vars" title="Show exported variable nodes">x vars</button>
    <button class="btn active-orange" id="btn-noise" title="Hide test and vendor nodes">⊘ noise</button>
  </div>

  <div class="grp" id="grp-edges" aria-label="Edge kinds">
    <span class="grp-label">edges</span>
    <span class="chip on" id="chip-calls" role="button" tabindex="0">calls</span>
    <span class="chip on" id="chip-imports" role="button" tabindex="0">imports</span>
  </div>

  <div class="grp" id="grp-fx" aria-label="Effects">
    <button class="btn active" id="btn-fx" title="Toggle visual effects">⚡ fx</button>
    <button class="btn" id="btn-pulse" title="Replay bloom animation">⟳</button>
  </div>

  <div class="grp" aria-label="Exports">
    <button class="btn" id="btn-dot" title="Copy graph as Graphviz DOT">⤓ DOT</button>
    <button class="btn" id="btn-json" title="Save graph as JSON">⤓ JSON</button>
    <button class="btn" id="btn-png" title="Save graph as PNG">PNG</button>
    <button class="btn" id="btn-fit" title="Fit graph to window" aria-label="Fit to window">⛶</button>
    <button class="btn" id="btn-search" title="Search nodes (⌘F)">⌕</button>
  </div>
</div>

<!-- ── Node search overlay ──────────────────────────────────────────────── -->
<div id="node-search" style="display:none" role="search" aria-label="Search nodes">
  <input id="node-search-input" type="text" placeholder="Search nodes…" autocomplete="off" spellcheck="false">
  <ul id="node-search-results" role="listbox"></ul>
</div>

<!-- ── Breadcrumb nav (P3 repo-map LOD) ─────────────────────────────────── -->
<nav id="breadcrumb" aria-label="Graph navigation level">
  <!-- Populated by graph.js renderBreadcrumb() -->
</nav>

<!-- ── Disambiguation bar (multiple implementations of same symbol) ─────── -->
<div id="disambig-bar" role="navigation" aria-label="Implementation selector"></div>

<!-- ── Blast bar ────────────────────────────────────────────────────────── -->
<div id="blastbar" style="display:none" role="status" aria-live="polite">
  <div class="blast-icon" aria-hidden="true">⊗</div>
  <span>Blast radius of <strong id="blastName"></strong></span>
  <span class="meta" id="blastMeta"></span>
  <button class="btn" id="blastExit">✕ exit blast view</button>
</div>

<!-- ── Main canvas area ─────────────────────────────────────────────────── -->
<div id="main">
  <div id="fxAurora" aria-hidden="true">
    <div class="blob b1"></div>
    <div class="blob b2"></div>
    <div class="blob b3"></div>
  </div>
  <canvas id="bgfx" aria-hidden="true"></canvas>
  <div id="cy" role="application" aria-label="Code dependency graph"></div>

  <!-- Tile-map for repo-map LOD overview (P3) — hidden until mode='overview' -->
  <div id="tilemap" role="grid" aria-label="Repository package overview">
    <canvas id="tilemap-edges" aria-hidden="true"></canvas>
    <div id="tilemap-tiles"></div>
  </div>
  <div id="spotlight" aria-hidden="true"></div>
  <div id="halo" aria-hidden="true"></div>

  <div class="col-head" id="colCallers" aria-hidden="true">callers · hop −2 · −1</div>
  <div class="col-head" id="colRoot"    aria-hidden="true">root</div>
  <div class="col-head" id="colDeps"    aria-hidden="true">hop +1 · +2 · dependencies</div>

  <div id="banner" role="status" aria-live="polite"></div>
  <div id="hint" aria-live="polite"></div>

  <div id="zoomCtl" aria-label="Zoom controls">
    <button class="btn" id="btn-zoom-in" aria-label="Zoom in">＋</button>
    <button class="btn" id="btn-zoom-out" aria-label="Zoom out">－</button>
  </div>

  <div id="minimapBox" aria-hidden="true">
    <canvas id="minimap" width="176" height="106"></canvas>
  </div>

  <!-- Definition peek panel (P2) -->
  <div id="peek" role="dialog" aria-label="Definition peek" aria-modal="false">
    <div id="peekHead">
      <span style="color:#86df86" aria-hidden="true">↗</span>
      <span class="pk-path" id="peekPath"></span>
      <span class="pk-note">peek · <kbd>Enter</kbd> opens editor</span>
      <button class="pk-close" id="btn-peek-close" aria-label="Close peek panel">✕</button>
    </div>
    <div id="peekBody" role="region" aria-label="Source code preview"></div>
    <div id="peekActions">
      <button class="btn-action" id="peekJumpBtn">↗ Open in editor</button>
    </div>
  </div>

  <!-- Detail panel -->
  <div id="detail" role="complementary" aria-label="Node detail"></div>
</div>

<!-- ── Status bar ───────────────────────────────────────────────────────── -->
<div id="statusbar" role="status" aria-live="polite">
  <span id="fresh"><span class="dot-pulse" aria-hidden="true"></span><span id="freshText">connecting…</span></span>
  <span id="statusGraph">—</span>
  <span id="noiseBadge" style="display:none" aria-live="polite"></span>
  <span id="diagBadge" style="display:none" aria-live="polite"></span>
  <div class="legend" aria-label="Node type legend">
    <span class="lg"><span class="dot ring" style="border-color:#86df86" aria-hidden="true"></span>function</span>
    <span class="lg"><span class="dot ring" style="border-color:#fcd053" aria-hidden="true"></span>class · var</span>
    <span class="lg"><span class="dot ring" style="border-color:#b3b3b3" aria-hidden="true"></span>file</span>
    <span class="lg"><span class="dot ring" style="border-color:#e2d4ca" aria-hidden="true"></span>interface</span>
  </div>
</div>

<!-- Bootstrap: inject logo URI and initial state, then load webview logic -->
<script nonce="${nonce}">
window.TRAVSR_INIT = {
  logoUri: ${JSON.stringify(logoUri)}
};
</script>
<script src="${cyUri}"></script>
<script src="${jsUri}"></script>
</body>
</html>`;
}

// ── Standalone loading HTML (used by other webview panels) ────────────────

export function buildLoadingHtml(logoUri?: string, cspSource?: string): string {
  const imgSrc = cspSource ? `img-src ${cspSource};` : "";
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; ${imgSrc}">
<title>Travsr</title>
<style>
  body { background: #141414; display: flex; align-items: center; justify-content: center;
    height: 100vh; margin: 0; font-family: system-ui, sans-serif; }
  .wrap { text-align: center; color: #8f7a6c; font-size: 12px; }
  img { width: 32px; height: 32px; border-radius: 6px; margin-bottom: 12px; display: block; margin: 0 auto 12px; }
</style>
</head>
<body>
<div class="wrap">
  ${logoUri ? `<img src="${logoUri}" alt="Travsr">` : ""}
  <div>Loading…</div>
</div>
</body>
</html>`;
}
