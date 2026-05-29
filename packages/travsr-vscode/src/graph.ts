/**
 * GraphPanel — Cytoscape.js WebviewPanel for the Travsr code graph.
 *
 * Lifecycle: one singleton panel per window. Callers invoke `GraphPanel.show()`
 * then `panel.query(symbol)`. The panel sends `{command:"query"}` messages back
 * when the toolbar search form is submitted so the extension can call the MCP
 * daemon and post `{command:"render", data}` into the webview.
 */

import * as vscode from "vscode";
import type { McpClient } from "./mcp";

export interface GraphNode {
  id: string;
  label: string;
  kind: string;
  path: string;
  package: string;
  score: number;
  root?: boolean;
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

type WebviewMessage =
  | { command: "query"; query: string; direction: string; depth: number; kind_filter?: string }
  | { command: "goToDefinition"; path: string }
  | { command: "showBlastRadius"; path: string };

export class GraphPanel {
  static readonly viewType = "travsrGraphPanel";
  private static current: GraphPanel | undefined;

  private readonly panel: vscode.WebviewPanel;
  private disposables: vscode.Disposable[] = [];

  private constructor(
    private readonly client: McpClient,
    _context: vscode.ExtensionContext
  ) {
    this.panel = vscode.window.createWebviewPanel(
      GraphPanel.viewType,
      "Travsr: Graph",
      vscode.ViewColumn.One,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
        localResourceRoots: [],
      }
    );

    this.panel.webview.html = buildLoadingHtml();

    this.panel.onDidDispose(() => this.dispose(), null, this.disposables);

    this.panel.webview.onDidReceiveMessage(
      (msg: WebviewMessage) => this.handleMessage(msg),
      null,
      this.disposables
    );
  }

  static show(
    client: McpClient,
    context: vscode.ExtensionContext
  ): GraphPanel {
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
    kindFilter = ""
  ): Promise<void> {
    this.panel.title = kindFilter === "file" ? "Travsr: File Graph" : `Travsr: ${query}`;
    const raw = await this.client.callTool("get_graph_json", {
      query,
      direction,
      depth: String(depth),
      kind_filter: kindFilter,
    });
    let data: GraphData = { nodes: [], edges: [] };
    try {
      if (raw) {
        data = JSON.parse(raw) as GraphData;
      }
    } catch {
      // malformed JSON from daemon → render empty graph
    }
    void this.panel.webview.postMessage({ command: "render", data, query });
  }

  private handleMessage(msg: WebviewMessage): void {
    if (msg.command === "query" && (msg.query || msg.kind_filter === "file")) {
      void this.query(msg.query, msg.direction, msg.depth, msg.kind_filter ?? "");
    } else if (msg.command === "goToDefinition" && msg.path) {
      const root = vscode.workspace.workspaceFolders?.[0]?.uri;
      const uri = msg.path.startsWith("/")
        ? vscode.Uri.file(msg.path)
        : root ? vscode.Uri.joinPath(root, msg.path) : vscode.Uri.file(msg.path);
      void vscode.commands.executeCommand("vscode.open", uri);
    } else if (msg.command === "showBlastRadius" && msg.path) {
      void vscode.commands.executeCommand("travsr.showBlastRadius", msg.path);
    }
  }

  dispose(): void {
    GraphPanel.current = undefined;
    this.panel.dispose();
    for (const d of this.disposables) {
      d.dispose();
    }
    this.disposables = [];
  }
}

// ── Webview HTML ──────────────────────────────────────────────────────────────

export function buildLoadingHtml(): string {
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src https://cdnjs.cloudflare.com https://cdn.jsdelivr.net 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'none';">
<title>Travsr Graph</title>
<script src="https://cdnjs.cloudflare.com/ajax/libs/cytoscape/3.29.2/cytoscape.min.js"></script>
<script src="https://cdnjs.cloudflare.com/ajax/libs/dagre/0.8.5/dagre.min.js"></script>
<script src="https://cdn.jsdelivr.net/npm/cytoscape-dagre@2.5.0/cytoscape-dagre.min.js"></script>
<style>
  /* Travsr design tokens — charcoal surfaces, green-300 accent, gold-300 vars, orange-400 hot */
  :root {
    --bg:          #141414;  /* charcoal-800 */
    --bg-elev:     #1a1a1a;  /* charcoal-700 */
    --bg-input:    #252525;  /* charcoal-650 */
    --border:      #333333;  /* charcoal-600 */
    --border-mid:  #4d4d4d;  /* charcoal-500 */
    --fg:          #f6f1ed;  /* linen-100 */
    --fg-muted:    #c8b7ab;  /* linen-400 */
    --fg-subtle:   #8f7a6c;  /* linen-600 */
    --green:       #86df86;  /* green-300 — node, accent, fresh */
    --green-dim:   #429429;  /* green-500 — active bg */
    --green-deep:  #17340e;  /* green-800 — active bg deep */
    --gold:        #fcd053;  /* gold-300 — var nodes */
    --gold-dim:    #573700;  /* gold-800 — var bg */
    --orange:      #fb923c;  /* orange-400 — blast radius, hot */
    --orange-dim:  #703000;  /* orange-800 — orange bg */
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body {
    font-family: 'Segoe UI', -apple-system, system-ui, sans-serif;
    background: var(--bg); color: var(--fg);
    height: 100vh; display: flex; flex-direction: column;
    overflow: hidden; font-size: 12px;
  }
  .titlebar {
    background: #0a0a0a; border-bottom: 1px solid var(--border);
    padding: 0 12px; height: 34px;
    display: flex; align-items: center; gap: 8px;
    flex-shrink: 0; user-select: none;
  }
  .titlebar-icon { font-size: 13px; color: var(--green); }
  .titlebar-title { font-size: 12px; color: var(--fg); font-weight: 500; letter-spacing: 0.1px; }
  .titlebar-badge {
    background: #cf6a0a; color: #fbfaf9; font-size: 9px;
    padding: 1px 6px; border-radius: 10px; font-weight: 700; letter-spacing: 0.4px;
  }
  .toolbar {
    background: var(--bg-elev); border-bottom: 1px solid var(--border);
    padding: 5px 12px; display: flex; align-items: center;
    gap: 8px; flex-shrink: 0; flex-wrap: wrap;
  }
  .search-wrap { position: relative; flex: 0 0 200px; }
  .search-wrap .search-icon {
    position: absolute; left: 8px; top: 50%;
    transform: translateY(-50%); color: var(--fg-subtle); font-size: 11px; pointer-events: none;
  }
  .search-input {
    width: 100%; background: var(--bg-input); border: 1px solid var(--border-mid);
    border-radius: 4px; color: var(--fg); font-size: 11px;
    padding: 4px 8px 4px 24px; outline: none; transition: border-color 120ms;
  }
  .search-input:focus { border-color: var(--green); }
  .search-input::placeholder { color: var(--fg-subtle); }
  .sep { width: 1px; height: 18px; background: var(--border); flex-shrink: 0; }
  .btn-group { display: flex; gap: 1px; }
  .btn {
    background: var(--bg-input); border: none; color: var(--fg-muted);
    font-size: 11px; padding: 4px 9px; cursor: pointer;
    transition: background 120ms, color 120ms; white-space: nowrap;
  }
  .btn:first-child { border-radius: 4px 0 0 4px; }
  .btn:last-child  { border-radius: 0 4px 4px 0; }
  .btn:only-child  { border-radius: 4px; }
  .btn:hover { background: var(--border-mid); color: var(--fg); }
  .btn.active { background: var(--green-deep); color: var(--green); border: 1px solid #86df8633; }
  .btn.active-var { background: var(--gold-dim); color: var(--gold); border: 1px solid #fcd05333; }
  .label { font-size: 10px; color: var(--fg-subtle); white-space: nowrap; text-transform: uppercase; letter-spacing: 0.5px; }
  .depth-wrap { display: flex; align-items: center; gap: 6px; }
  .depth-slider {
    -webkit-appearance: none; appearance: none;
    width: 64px; height: 3px; border-radius: 2px;
    background: var(--border-mid); outline: none; cursor: pointer;
  }
  .depth-slider::-webkit-slider-thumb {
    -webkit-appearance: none; width: 12px; height: 12px;
    border-radius: 50%; background: var(--green); cursor: pointer;
  }
  .depth-val { font-size: 11px; color: var(--green); font-weight: 700; min-width: 10px; }
  .toolbar-right { margin-left: auto; display: flex; align-items: center; gap: 6px; }
  .main { display: flex; flex: 1; overflow: hidden; }
  #cy {
    flex: 1; background: var(--bg);
    background-image: radial-gradient(circle, #252525 1px, transparent 1px);
    background-size: 28px 28px;
  }
  .empty-hint {
    position: absolute; top: 50%; left: 50%; transform: translate(-50%,-50%);
    color: var(--fg-subtle); font-size: 12px; text-align: center; pointer-events: none;
  }
  .empty-hint kbd {
    background: var(--bg-input); border: 1px solid var(--border-mid); border-radius: 3px;
    padding: 1px 5px; font-family: monospace; font-size: 10px; color: var(--fg-muted);
  }
  .detail {
    width: 248px; background: var(--bg);
    border-left: 1px solid var(--border);
    display: flex; flex-direction: column;
    overflow: hidden; flex-shrink: 0;
  }
  .detail-header {
    background: var(--bg-elev); border-bottom: 1px solid var(--border);
    padding: 7px 12px; font-size: 10px; font-weight: 700;
    color: var(--fg-subtle); text-transform: uppercase; letter-spacing: 0.8px;
    display: flex; align-items: center; gap: 6px;
  }
  .detail-header span { color: var(--green); }
  .detail-body { padding: 12px; overflow-y: auto; flex: 1; }
  .node-icon-lg {
    width: 30px; height: 30px; border-radius: 6px;
    display: flex; align-items: center; justify-content: center;
    font-size: 14px; flex-shrink: 0;
  }
  .node-sig {
    font-size: 12px; font-weight: 600; color: var(--fg);
    word-break: break-all; margin-bottom: 3px;
    font-family: 'Consolas', 'Courier New', monospace;
  }
  .node-kind-tag {
    display: inline-block; font-size: 9px;
    padding: 1px 6px; border-radius: 3px;
    font-weight: 700; margin-bottom: 12px; letter-spacing: 0.4px;
  }
  .detail-section { margin-bottom: 12px; }
  .detail-section-title {
    font-size: 9px; font-weight: 700; text-transform: uppercase;
    letter-spacing: 0.9px; color: var(--fg-subtle); margin-bottom: 5px;
    border-bottom: 1px solid var(--border); padding-bottom: 4px;
    display: flex; align-items: center; justify-content: space-between;
    cursor: pointer; user-select: none; transition: color 120ms;
  }
  .detail-section-title:hover { color: var(--fg-muted); }
  .detail-section-title .collapse-icon { font-size: 9px; transition: transform 150ms; }
  .detail-section-title.collapsed .collapse-icon { transform: rotate(-90deg); }
  .collapsible-body { overflow: hidden; transition: max-height 0.2s ease; }
  .collapsible-body.collapsed { max-height: 0 !important; }
  .detail-row {
    display: flex; justify-content: space-between;
    align-items: baseline; padding: 2px 0; gap: 8px;
  }
  .detail-key { color: var(--fg-subtle); font-size: 10px; white-space: nowrap; }
  .detail-val { color: var(--fg-muted); font-size: 10px; text-align: right; word-break: break-all; }
  .detail-val.mono { font-family: 'Consolas', monospace; color: var(--fg-muted); font-size: 10px; }
  .detail-val.score { color: var(--green); font-weight: 700; }
  .detail-val.path  { color: var(--green); opacity: 0.8; }
  .detail-val.var-score { color: var(--gold); font-weight: 700; }
  .edge-list { list-style: none; }
  .edge-list li {
    font-size: 10px; padding: 3px 0; color: var(--fg-muted);
    display: flex; align-items: center; gap: 5px;
    border-bottom: 1px solid var(--border); cursor: pointer; transition: color 120ms;
  }
  .edge-list li:hover { color: var(--green); }
  .edge-arrow { color: var(--green); font-size: 9px; opacity: 0.7; }
  .edge-type {
    font-size: 8px; color: var(--fg-subtle); border: 1px solid var(--border);
    padding: 0 4px; border-radius: 2px; white-space: nowrap; margin-left: auto;
  }
  .edge-type.reads { color: var(--gold); border-color: #fcd05333; }
  .var-list { list-style: none; }
  .var-row {
    display: flex; align-items: center; gap: 6px;
    padding: 4px 0; border-bottom: 1px solid var(--border);
    cursor: pointer; font-size: 10px; transition: background 120ms;
  }
  .var-row:hover { background: var(--bg-elev); margin: 0 -4px; padding: 4px 4px; border-radius: 3px; }
  .var-tag {
    font-size: 8px; background: var(--gold-dim); color: var(--gold);
    border: 1px solid #fcd05333; padding: 0 4px; border-radius: 2px;
    white-space: nowrap; flex-shrink: 0;
  }
  .var-name { color: var(--gold); font-family: 'Consolas', monospace; flex: 1; }
  .var-refs { color: var(--fg-subtle); font-size: 9px; white-space: nowrap; }
  .btn-action {
    width: 100%; background: var(--green-deep); border: 1px solid #86df8622; color: var(--green);
    padding: 6px 10px; border-radius: 4px; font-size: 10px;
    cursor: pointer; margin-bottom: 5px; text-align: left;
    display: flex; align-items: center; gap: 6px; transition: background 120ms;
  }
  .btn-action:hover { background: #255317; }
  .btn-action.hot { background: var(--orange-dim); border-color: #fb923c22; color: var(--orange); }
  .btn-action.hot:hover { background: #4f2000; }
  .btn-action.secondary { background: var(--bg-input); border-color: var(--border); color: var(--fg-muted); }
  .btn-action.secondary:hover { background: var(--border); color: var(--fg); }
  .statusbar {
    background: #0a0a0a; height: 22px; border-top: 1px solid var(--border);
    display: flex; align-items: center;
    padding: 0 12px; gap: 16px; flex-shrink: 0;
  }
  .status-item {
    font-size: 10px; color: var(--fg-subtle);
    display: flex; align-items: center; gap: 4px; white-space: nowrap;
  }
  .status-item.brand { color: var(--green); font-weight: 600; }
  .status-item .dot { width: 5px; height: 5px; border-radius: 50%; background: var(--green); }
  .status-right { margin-left: auto; display: flex; gap: 16px; }
  #tooltip {
    position: fixed; background: var(--bg-elev); border: 1px solid var(--border-mid);
    border-radius: 5px; padding: 7px 10px; font-size: 11px; color: var(--fg-muted);
    pointer-events: none; display: none; z-index: 1000;
    box-shadow: 0 6px 16px rgba(0,0,0,0.6); max-width: 230px;
  }
  #tooltip .tt-sig { color: var(--fg); font-family: monospace; font-weight: 600; margin-bottom: 2px; font-size: 11px; }
  #tooltip .tt-kind { font-size: 10px; margin-bottom: 2px; color: var(--fg-subtle); }
  #tooltip .tt-kind.var-kind { color: var(--gold); }
  #tooltip .tt-path { color: var(--green); font-size: 10px; opacity: 0.8; }
  #tooltip .tt-score { color: var(--green); font-size: 10px; font-weight: 600; }
  #tooltip .tt-refs  { color: var(--gold); font-size: 10px; }
  .legend {
    position: absolute; bottom: 10px; left: 12px;
    background: rgba(26,26,26,0.95); border: 1px solid var(--border);
    border-radius: 5px; padding: 8px 10px; font-size: 10px;
    pointer-events: none; backdrop-filter: blur(6px);
  }
  .legend-title { color: var(--fg-subtle); text-transform: uppercase; letter-spacing: 0.8px; font-weight: 700; margin-bottom: 6px; font-size: 9px; }
  .legend-row { display: flex; align-items: center; gap: 6px; margin-bottom: 4px; color: var(--fg-muted); }
  .legend-dot  { width: 9px; height: 9px; border-radius: 50%; flex-shrink: 0; }
  .legend-tag  { width: 12px; height: 7px; background: var(--gold); border-radius: 2px 4px 4px 2px; flex-shrink: 0; }
  .legend-diamond { width: 8px; height: 8px; transform: rotate(45deg); flex-shrink: 0; border-radius: 1px; }
  .legend-rect { width: 11px; height: 7px; border-radius: 2px; flex-shrink: 0; }
  .legend-line { width: 16px; height: 2px; flex-shrink: 0; }
  .legend-line.dashed { background: repeating-linear-gradient(90deg, var(--border-mid) 0, var(--border-mid) 3px, transparent 3px, transparent 6px); height: 1px; }
  .legend-line.dotted { background: repeating-linear-gradient(90deg, var(--gold) 0, var(--gold) 2px, transparent 2px, transparent 5px); height: 1px; }
  .legend-row.var-row-legend { opacity: 0.4; }
  .legend-row.var-row-legend.visible { opacity: 1; }
  .legend-row.non-file-legend { opacity: 1; transition: opacity 0.2s; }
  .legend-row.non-file-legend.dimmed-file { opacity: 0.25; }
  #statusMode { color: var(--green); font-weight: 600; }
  .graph-wrap { position: relative; flex: 1; overflow: hidden; display: flex; }
  .minimap {
    position: absolute; bottom: 10px; right: 12px;
    width: 108px; height: 78px;
    background: rgba(26,26,26,0.95); border: 1px solid var(--border);
    border-radius: 5px; backdrop-filter: blur(6px); overflow: hidden;
  }
  .minimap-label { position: absolute; bottom: 3px; left: 6px; font-size: 8px; color: var(--border-mid); pointer-events: none; }
  .var-banner {
    position: absolute; top: 10px; left: 50%; transform: translateX(-50%);
    background: var(--gold-dim); border: 1px solid #fcd05344; border-radius: 4px;
    padding: 4px 14px; font-size: 11px; color: var(--gold);
    pointer-events: none; opacity: 0; transition: opacity 0.25s;
    white-space: nowrap;
  }
  .var-banner.show { opacity: 1; }
</style>
</head>
<body>

<div class="titlebar">
  <span class="titlebar-icon">⬡</span>
  <span class="titlebar-title">Travsr: Code Graph</span>
  <span class="titlebar-badge">MCP</span>
</div>

<div class="toolbar">
  <div class="search-wrap">
    <span class="search-icon">⌕</span>
    <input class="search-input" id="searchInput" type="text" placeholder="Search symbol… (Enter)">
  </div>
  <div class="sep"></div>
  <span class="label">Direction</span>
  <div class="btn-group">
    <button class="btn" id="btn-deps"    onclick="setDirection('deps')">Deps</button>
    <button class="btn active" id="btn-both" onclick="setDirection('both')">Both</button>
    <button class="btn" id="btn-callers" onclick="setDirection('callers')">Callers</button>
  </div>
  <div class="sep"></div>
  <span class="label">Depth</span>
  <div class="depth-wrap">
    <input class="depth-slider" type="range" min="1" max="4" value="2" id="depthSlider"
           oninput="document.getElementById('depthVal').textContent=this.value; debouncedQuery()">
    <span class="depth-val" id="depthVal">2</span>
  </div>
  <div class="sep"></div>
  <span class="label">Layout</span>
  <div class="btn-group">
    <button class="btn active" id="btn-dagre"  onclick="setLayout('dagre')">Dagre</button>
    <button class="btn"        id="btn-force"  onclick="setLayout('cose')">Force</button>
    <button class="btn"        id="btn-circle" onclick="setLayout('circle')">Circle</button>
  </div>
  <div class="sep"></div>
  <button class="btn" id="btn-vars" onclick="toggleVars()" title="Show / hide exported variable nodes">
    ⬡ Vars <span id="varCount" style="color:var(--gold);margin-left:2px">0</span>
  </button>
  <div class="sep"></div>
  <button class="btn" id="btn-files" onclick="setFilesMode(!filesMode)" title="Show file dependency graph — imports only">
    ▭ Files
  </button>
  <div class="toolbar-right">
    <button class="btn" onclick="cy.fit(null,30)">⊡ Fit</button>
    <button class="btn" onclick="resetLayout()">↺ Layout</button>
  </div>
</div>

<div class="main">
  <div class="graph-wrap">
    <div id="cy"></div>
    <div class="empty-hint" id="emptyHint">
      Type a symbol name and press <kbd>Enter</kbd> to explore the graph
    </div>
    <div class="var-banner" id="varBanner"></div>
    <div class="legend">
      <div class="legend-title">Node types</div>
      <div class="legend-row non-file-legend"><div class="legend-dot"     style="background:#86df86"></div> function</div>
      <div class="legend-row non-file-legend"><div class="legend-diamond" style="background:#fcd053"></div> class</div>
      <div class="legend-row"><div class="legend-rect"    style="background:#b3b3b3"></div> file</div>
      <div class="legend-row non-file-legend"><div class="legend-dot"     style="background:#e2d4ca"></div> interface</div>
      <div class="legend-row var-row-legend" id="legend-var"><div class="legend-tag"></div> variable</div>
      <div style="margin-top:8px" class="legend-title">Edges</div>
      <div class="legend-row non-file-legend"><div class="legend-line" style="background:#86df86"></div> calls</div>
      <div class="legend-row non-file-legend"><div class="legend-line" style="background:#c8b7ab"></div> defines</div>
      <div class="legend-row"><div class="legend-line dashed"></div> imports</div>
      <div class="legend-row var-row-legend"><div class="legend-line dotted"></div> reads (var)</div>
    </div>
    <div class="minimap">
      <canvas id="minimapCanvas" width="110" height="80"></canvas>
      <span class="minimap-label">minimap</span>
    </div>
  </div>

  <div class="detail">
    <div class="detail-header"><span>⬡</span> Node Detail</div>
    <div class="detail-body" id="detailBody">
      <div style="color:#555;font-size:11px;margin-top:20px;text-align:center">
        Click a node to see details
      </div>
    </div>
  </div>
</div>

<div class="statusbar">
  <div class="status-item"><div class="dot"></div> travsr</div>
  <div class="status-right">
    <div class="status-item" id="statusGraph">— nodes · — edges</div>
    <div class="status-item" id="statusDepth">depth 2</div>
    <div class="status-item" id="statusMode"></div>
    <div class="status-item" id="statusQuery">—</div>
  </div>
</div>

<div id="tooltip">
  <div class="tt-sig"  id="ttSig"></div>
  <div class="tt-kind" id="ttKind"></div>
  <div class="tt-path" id="ttPath"></div>
  <div class="tt-score" id="ttScore"></div>
  <div class="tt-refs"  id="ttRefs" style="display:none"></div>
</div>

<script>
const vscode = acquireVsCodeApi();
cytoscape.use(cytoscapeDagre);

// ── State ─────────────────────────────────────────────────────────────────────
let varsVisible = false;
let filesMode = false;
let currentDirection = 'both';
let currentLayout = 'dagre';

// ── Cytoscape init (empty) ────────────────────────────────────────────────────
// Travsr palette: green-300 nodes, gold-300 vars, orange-400 hot, charcoal surfaces
const VAR_COLOR = '#fcd053';  // gold-300
function nodeColor(kind) {
  return { function:'#86df86', class:'#fcd053', file:'#b3b3b3', interface:'#e2d4ca', var:VAR_COLOR }[kind] || '#8f7a6c';
}
function nodeShape(kind) {
  return { function:'ellipse', class:'diamond', file:'round-rectangle', interface:'triangle', var:'round-tag' }[kind] || 'ellipse';
}
function edgeColor(kind) {
  return { calls:'#86df86', defines:'#c8b7ab', imports:'#4d4d4d', reads:VAR_COLOR }[kind] || '#4d4d4d';
}

const cy = cytoscape({
  container: document.getElementById('cy'),
  elements: [],
  style: [
    {
      selector: 'node',
      style: {
        'label':            'data(label)',
        'shape':            ele => nodeShape(ele.data('kind')),
        'background-color': ele => nodeColor(ele.data('kind')),
        'border-width':     ele => ele.data('root') ? 3 : 0,
        'border-color':     '#ffffff',
        'color':            '#cccccc',
        'font-size':        '10px',
        'text-valign':      'bottom',
        'text-halign':      'center',
        'text-margin-y':    4,
        'min-zoomed-font-size': 8,
        'width':            ele => ele.data('kind') === 'var' ? 38 : 20 + Math.round((ele.data('score') || 0.3) * 28),
        'height':           ele => ele.data('kind') === 'var' ? 18 : 20 + Math.round((ele.data('score') || 0.3) * 28),
        'overlay-padding':  4,
        'transition-property': 'background-color, border-width, opacity',
        'transition-duration': '0.15s',
      }
    },
    { selector: 'node[kind = "file"]', style: { 'width': 50, 'height': 32, 'font-size': '9px' } },
    { selector: 'node[kind = "var"]', style: {
        'opacity': 0.55, 'font-size': '9px', 'text-margin-y': 5,
        'border-width': 1, 'border-color': VAR_COLOR + '88', 'display': 'none',
      }
    },
    { selector: 'edge[kind = "reads"]', style: { 'display': 'none' } },
    { selector: 'node:selected', style: {
        'border-width': 3, 'border-color': '#86df86',
        'overlay-color': '#86df86', 'overlay-opacity': 0.08, 'opacity': 1,
      }
    },
    { selector: 'node[kind = "var"]:selected', style: { 'opacity': 1, 'border-width': 2, 'border-color': VAR_COLOR } },
    { selector: 'node.dimmed', style: { 'opacity': 0.12 } },
    { selector: 'node[kind = "var"].dimmed', style: { 'opacity': 0.07 } },
    { selector: 'edge', style: {
        'line-color':         ele => edgeColor(ele.data('kind')),
        'target-arrow-color': ele => edgeColor(ele.data('kind')),
        'target-arrow-shape': 'triangle',
        'curve-style':        'bezier',
        'width':              1.5,
        'line-style':         ele => ['imports','reads'].includes(ele.data('kind')) ? 'dashed' : 'solid',
        'line-dash-pattern':  ele => ele.data('kind') === 'reads' ? [3,3] : [5,3],
        'arrow-scale':        0.9,
        'opacity':            0.7,
      }
    },
    { selector: 'edge.dimmed',   style: { 'opacity': 0.08 } },
    { selector: 'edge:selected', style: { 'opacity': 1, 'width': 2.5 } },
  ],
  layout: { name:'dagre', rankDir:'TB', nodeSep:60, rankSep:80, padding:30 },
  wheelSensitivity: 0.3, minZoom: 0.2, maxZoom: 4,
});

// ── Message bridge: receive render commands from extension ────────────────────
window.addEventListener('message', event => {
  const msg = event.data;
  if (msg.command !== 'render') return;

  const data = msg.data || { nodes: [], edges: [] };
  const nodes = (data.nodes || []).map(n => ({
    data: { id: n.id, label: n.label, kind: n.kind, path: n.path, pkg: n.package, score: n.score, root: n.root || false }
  }));
  const edges = (data.edges || []).map(e => ({
    data: { id: e.source + '->' + e.target, source: e.source, target: e.target, kind: e.kind }
  }));

  // Reset var visibility state before loading new data
  varsVisible = false;
  document.getElementById('btn-vars').classList.remove('active-var');
  document.querySelectorAll('.var-row-legend').forEach(el => el.classList.remove('visible'));

  cy.elements().remove();
  cy.add({ nodes, edges });

  // Re-apply hidden state for var nodes after adding
  cy.nodes('[kind = "var"]').style('display', 'none');
  cy.edges('[kind = "reads"]').style('display', 'none');

  const varCount = cy.nodes('[kind = "var"]').length;
  document.getElementById('varCount').textContent = String(varCount);

  const hint = document.getElementById('emptyHint');
  if (hint) hint.style.display = nodes.length > 0 ? 'none' : 'block';

  resetLayout();

  if (msg.query) {
    document.getElementById('statusQuery').textContent = msg.query;
    document.getElementById('searchInput').value = msg.query;
  }
  updateStatusBar();
});

// ── Search: send query to extension ──────────────────────────────────────────
function submitQuery() {
  const query = document.getElementById('searchInput').value.trim();
  if (!query && !filesMode) return;
  const depth = parseInt(document.getElementById('depthSlider').value, 10);
  document.getElementById('statusDepth').textContent = 'depth ' + depth;
  vscode.postMessage({ command: 'query', query, direction: currentDirection, depth, kind_filter: filesMode ? 'file' : '' });
}

document.getElementById('searchInput').addEventListener('keydown', e => {
  if (e.key === 'Enter') submitQuery();
});

let _debounceTimer = null;
function debouncedQuery() {
  clearTimeout(_debounceTimer);
  _debounceTimer = setTimeout(() => {
    if (document.getElementById('searchInput').value.trim() || filesMode) submitQuery();
  }, 400);
}

function setFilesMode(active) {
  filesMode = active;
  document.getElementById('btn-files').classList.toggle('active', active);
  document.getElementById('statusMode').textContent = active ? '· file graph' : '';
  // Dim non-file legend rows when in file mode (same pattern as vars)
  document.querySelectorAll('.non-file-legend').forEach(el => el.classList.toggle('dimmed-file', active));
  if (active) {
    flashBanner('File dependency graph · imports only');
    document.getElementById('emptyHint').innerHTML = 'File graph active — showing module imports<br><span style="color:var(--fg-subtle);font-size:10px">Clear Files mode to search symbols</span>';
  } else {
    document.getElementById('emptyHint').innerHTML = 'Type a symbol name and press <kbd>Enter</kbd> to explore the graph';
  }
  if (active || document.getElementById('searchInput').value.trim()) submitQuery();
}

// ── Status bar (counts only visible nodes/edges) ──────────────────────────────
function updateStatusBar() {
  const nodes = cy.nodes(':visible').length;
  const edges = cy.edges(':visible').length;
  const tokens = cy.nodes(':visible').reduce((sum, n) => {
    const d = n.data();
    return sum + Math.max(1, Math.floor(((d.label || '').length + (d.kind || '').length + (d.path || '').length) / 4));
  }, 0);
  document.getElementById('statusGraph').textContent = nodes + ' nodes · ' + edges + ' edges · ~' + tokens + ' tokens';
}

// ── Vars toggle ───────────────────────────────────────────────────────────────
function toggleVars() {
  varsVisible = !varsVisible;
  const btn = document.getElementById('btn-vars');
  const varNodes = cy.nodes('[kind = "var"]');
  const readsEdges = cy.edges('[kind = "reads"]');
  if (varsVisible) {
    varNodes.style('display', 'element');
    readsEdges.style('display', 'element');
    btn.classList.add('active-var');
    flashBanner('Showing ' + varNodes.length + ' exported variable nodes');
    document.querySelectorAll('.var-row-legend').forEach(el => el.classList.add('visible'));
    resetLayout();
  } else {
    varNodes.style('display', 'none');
    readsEdges.style('display', 'none');
    btn.classList.remove('active-var');
    flashBanner('Variable nodes hidden');
    document.querySelectorAll('.var-row-legend').forEach(el => el.classList.remove('visible'));
    cy.elements().removeClass('dimmed');
    resetLayout();
  }
  updateStatusBar();
}

function flashBanner(msg) {
  const b = document.getElementById('varBanner');
  b.textContent = msg;
  b.classList.add('show');
  setTimeout(() => b.classList.remove('show'), 1800);
}

// ── Select a var node ─────────────────────────────────────────────────────────
function selectVarNode(id) {
  if (!varsVisible) toggleVars();
  const node = cy.getElementById(id);
  if (node.length) {
    cy.elements().removeClass('dimmed');
    node.closedNeighborhood().removeClass('dimmed');
    cy.elements().not(node.closedNeighborhood()).addClass('dimmed');
    cy.animate({ fit: { eles: node.closedNeighborhood(), padding: 60 } }, { duration: 300 });
    node.select();
    updateDetailPanel(node.data(), node);
  }
}

// ── Interactions ──────────────────────────────────────────────────────────────
const tooltip = document.getElementById('tooltip');

cy.on('mouseover', 'node', evt => {
  const d = evt.target.data();
  document.getElementById('ttSig').textContent   = d.label;
  document.getElementById('ttKind').textContent  = d.varKind ? d.varKind + ' (exported)' : d.kind;
  document.getElementById('ttKind').className    = 'tt-kind' + (d.kind === 'var' ? ' var-kind' : '');
  document.getElementById('ttPath').textContent  = d.path;
  document.getElementById('ttScore').textContent = 'score: ' + d.score;
  const refsEl = document.getElementById('ttRefs');
  if (d.kind === 'var' && d.refs) {
    refsEl.textContent = 'read in ' + d.refs + ' places';
    refsEl.style.display = 'block';
  } else {
    refsEl.style.display = 'none';
  }
  tooltip.style.display = 'block';
});
cy.on('mousemove', evt => {
  tooltip.style.left = (evt.originalEvent.clientX + 14) + 'px';
  tooltip.style.top  = (evt.originalEvent.clientY + 14) + 'px';
});
cy.on('mouseout', 'node', () => { tooltip.style.display = 'none'; });

cy.on('tap', 'node', evt => {
  const d = evt.target.data();
  updateDetailPanel(d, evt.target);
  const neighbourhood = evt.target.closedNeighborhood();
  cy.elements().addClass('dimmed');
  neighbourhood.removeClass('dimmed');
});
cy.on('tap', evt => {
  if (evt.target === cy) cy.elements().removeClass('dimmed');
});

// ── Detail panel ──────────────────────────────────────────────────────────────
function updateDetailPanel(d, ele) {
  const color   = nodeColor(d.kind);
  const iconMap = { function:'f', class:'◈', file:'▭', interface:'◻', var:'x' };
  const connected = ele ? ele.connectedEdges() : cy.edges();

  const edgeItems = connected
    .filter(e => e.data('kind') !== 'reads')
    .map(e => {
      const isOut = e.data('source') === d.id;
      const other = isOut ? e.data('target') : e.data('source');
      return '<li><span class="edge-arrow">' + (isOut?'→':'←') + '</span> ' + other + ' <span class="edge-type">' + e.data('kind') + '</span></li>';
    }).join('');

  const readsOut = connected.filter(e => e.data('kind') === 'reads' && e.data('source') === d.id);
  const readsIn  = connected.filter(e => e.data('kind') === 'reads' && e.data('target') === d.id);

  const sameFileVars = d.kind !== 'var'
    ? cy.nodes('[kind = "var"]').filter(n => n.data('path') === d.path)
    : cy.collection();

  const varsInFileSection = sameFileVars.length > 0 ? \`
    <div class="detail-section">
      <div class="detail-section-title" onclick="toggleSection(this)" style="color:#d7ba7daa">
        Variables in file (\${sameFileVars.length}) <span class="collapse-icon">▾</span>
      </div>
      <div class="collapsible-body" style="max-height:300px">
        <ul class="var-list">
          \${sameFileVars.map(n => '<li class="var-row" onclick="selectVarNode(\\'' + n.data('id') + '\\')">' +
            '<span class="var-tag">' + (n.data('varKind')||'var') + '</span>' +
            '<span class="var-name">' + n.data('label') + '</span>' +
            '<span class="var-refs">' + (n.data('refs')||0) + ' refs</span></li>').join('')}
        </ul>
        <div style="font-size:10px;color:#555;margin-top:6px;font-style:italic">Click to focus in graph</div>
      </div>
    </div>\` : '';

  const readsSection = d.kind === 'var' ? \`
    <div class="detail-section">
      <div class="detail-section-title" onclick="toggleSection(this)" style="color:#d7ba7daa">
        Read by (\${readsIn.length || d.refs || 0}) <span class="collapse-icon">▾</span>
      </div>
      <div class="collapsible-body" style="max-height:200px">
        <ul class="edge-list">
          \${readsIn.length
            ? readsIn.map(e => '<li><span class="edge-arrow" style="color:#d7ba7d">←</span> ' + e.data('source') + ' <span class="edge-type reads">reads</span></li>').join('')
            : '<li style="color:#555;font-style:italic">expand graph depth to see readers</li>'}
        </ul>
      </div>
    </div>\` : '';

  const readsOutSection = readsOut.length > 0 ? \`
    <div class="detail-section">
      <div class="detail-section-title" onclick="toggleSection(this)" style="color:#d7ba7daa">
        Reads variables (\${readsOut.length}) <span class="collapse-icon">▾</span>
      </div>
      <div class="collapsible-body" style="max-height:200px">
        <ul class="edge-list">
          \${readsOut.map(e => '<li><span class="edge-arrow" style="color:#d7ba7d">→</span> ' + e.data('target') + ' <span class="edge-type reads">reads</span></li>').join('')}
        </ul>
        \${!varsVisible ? '<div style="font-size:10px;color:#555;margin-top:4px;font-style:italic">Enable Vars toggle to see these nodes</div>' : ''}
      </div>
    </div>\` : '';

  const edgeCount = connected.filter(e => e.data('kind') !== 'reads').length;
  const tokenCost = Math.max(1, Math.floor(((d.label||'').length + (d.kind||'').length + (d.path||'').length) / 4));

  document.getElementById('detailBody').innerHTML = \`
    <div style="display:flex;align-items:center;gap:10px;margin-bottom:12px">
      <div class="node-icon-lg" style="background:\${color}1a;border:2px solid \${color};color:\${color};font-size:15px">
        \${iconMap[d.kind] || '●'}
      </div>
      <div>
        <div class="node-sig">\${d.label}</div>
        <span class="node-kind-tag" style="background:\${color}1a;color:\${color}">\${d.varKind || d.kind}</span>
        \${d.kind==='var' ? '<div style="font-size:10px;color:#858585;margin-top:2px">read in <span style="color:#d7ba7d;font-weight:600">' + (d.refs||0) + '</span> places</div>' : ''}
      </div>
    </div>
    <div class="detail-section">
      <div class="detail-section-title" onclick="toggleSection(this)">Identity <span class="collapse-icon">▾</span></div>
      <div class="collapsible-body" style="max-height:200px">
        <div class="detail-row"><span class="detail-key">path</span><span class="detail-val path">\${d.path||'—'}</span></div>
        <div class="detail-row"><span class="detail-key">package</span><span class="detail-val mono">\${d.pkg||'—'}</span></div>
      </div>
    </div>
    <div class="detail-section">
      <div class="detail-section-title" onclick="toggleSection(this)">Graph metrics <span class="collapse-icon">▾</span></div>
      <div class="collapsible-body" style="max-height:200px">
        <div class="detail-row"><span class="detail-key">score</span><span class="detail-val \${d.kind==='var'?'var-score':'score'}">\${d.score}</span></div>
        \${d.kind !== 'var' ? '<div class="detail-row"><span class="detail-key">callers</span><span class="detail-val">' + connected.filter(e=>e.data('target')===d.id&&e.data('kind')!=='reads').length + '</span></div>' +
          '<div class="detail-row"><span class="detail-key">deps</span><span class="detail-val">' + connected.filter(e=>e.data('source')===d.id&&e.data('kind')!=='reads').length + '</span></div>' : ''}
        <div class="detail-row"><span class="detail-key">token cost</span><span class="detail-val">\${tokenCost}</span></div>
      </div>
    </div>
    \${edgeCount > 0 ? '<div class="detail-section"><div class="detail-section-title" onclick="toggleSection(this)">Edges (' + edgeCount + ') <span class="collapse-icon">▾</span></div><div class="collapsible-body" style="max-height:200px"><ul class="edge-list">' + (edgeItems||'<li style="color:#555">No structural edges</li>') + '</ul></div></div>' : ''}
    \${readsSection}
    \${readsOutSection}
    \${varsInFileSection}
    <div class="detail-section">
      <div class="detail-section-title" onclick="toggleSection(this)">Actions <span class="collapse-icon">▾</span></div>
      <div class="collapsible-body" style="max-height:200px">
        \${d.path ? '<button class="btn-action" onclick="vscode.postMessage({command:\\'goToDefinition\\',path:\\'' + d.path + '\\'})"><span>↗</span> Go to definition</button>' : ''}
        \${d.kind !== 'var' ? '<button class="btn-action hot" onclick="vscode.postMessage({command:\\'showBlastRadius\\',path:\\'' + (d.path||'') + '\\'})"><span>💥</span> Show blast radius</button>' : ''}
        <button class="btn-action secondary" onclick="navigator.clipboard.writeText(\\'' + d.id + '\\').then(()=>flashBanner('VName copied'))"><span>⧉</span> Copy VName</button>
      </div>
    </div>
  \`;
}

// ── Collapsible sections ──────────────────────────────────────────────────────
function toggleSection(titleEl) {
  const body = titleEl.nextElementSibling;
  const collapsed = body.classList.toggle('collapsed');
  titleEl.classList.toggle('collapsed', collapsed);
}

// ── Toolbar controls ──────────────────────────────────────────────────────────
function setDirection(dir) {
  currentDirection = dir;
  ['deps','both','callers'].forEach(d => {
    document.getElementById('btn-'+d).classList.toggle('active', d === dir);
  });
  if (document.getElementById('searchInput').value.trim()) submitQuery();
}
function setLayout(name) {
  currentLayout = name;
  ['dagre','force','circle'].forEach(l => {
    const btn = document.getElementById('btn-'+l);
    if (btn) btn.classList.toggle('active', l === name);
  });
  resetLayout();
}
function resetLayout() {
  if (cy.nodes().length === 0) return;
  const opts = currentLayout === 'dagre'
    ? { name:'dagre', rankDir:'TB', nodeSep:55, rankSep:75, padding:30 }
    : currentLayout === 'circle'
    ? { name:'circle', padding:40 }
    : { name:'cose', animate:true, randomize:false, padding:40, nodeRepulsion:8000 };
  cy.layout(opts).run();
}

// ── Minimap ───────────────────────────────────────────────────────────────────
function drawMinimap() {
  const canvas = document.getElementById('minimapCanvas');
  if (!canvas) return;
  const ctx = canvas.getContext('2d');
  const W = canvas.width, H = canvas.height;
  ctx.clearRect(0,0,W,H);
  ctx.fillStyle = '#1e1e1e';
  ctx.fillRect(0,0,W,H);
  const visible = cy.elements().filter(e => e.style('display') !== 'none');
  const bb = visible.boundingBox();
  if (!bb || bb.w === 0) return;
  const scale = Math.min((W-8)/bb.w, (H-8)/bb.h) * 0.9;
  const ox = (W - bb.w*scale)/2, oy = (H - bb.h*scale)/2;
  visible.edges().forEach(e => {
    const s = e.source().position(), t = e.target().position();
    ctx.beginPath();
    ctx.moveTo(ox+(s.x-bb.x1)*scale, oy+(s.y-bb.y1)*scale);
    ctx.lineTo(ox+(t.x-bb.x1)*scale, oy+(t.y-bb.y1)*scale);
    ctx.strokeStyle = e.data('kind') === 'reads' ? '#d7ba7d44' : '#444';
    ctx.lineWidth = 0.7; ctx.stroke();
  });
  visible.nodes().forEach(n => {
    const p = n.position();
    const x = ox+(p.x-bb.x1)*scale, y = oy+(p.y-bb.y1)*scale;
    ctx.beginPath();
    ctx.arc(x, y, n.data('root') ? 4 : n.data('kind')==='var' ? 2 : 2.5, 0, Math.PI*2);
    ctx.globalAlpha = n.data('kind') === 'var' ? 0.55 : 1;
    ctx.fillStyle = nodeColor(n.data('kind'));
    ctx.fill();
    ctx.globalAlpha = 1;
  });
}
cy.on('layoutstop', () => { drawMinimap(); updateStatusBar(); });
cy.on('position', 'node', drawMinimap);
</script>
</body>
</html>`;
}
