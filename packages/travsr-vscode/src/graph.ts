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
import {
  detachSession,
  reportLspDiagnostics,
  REPORT_TTL_SECS,
  type FileDiagnostics,
  type LspDiagnosticsReport,
} from "./daemonIpc";
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

/**
 * One diagnostic, as the detail panel lists it. The badge answers "is this
 * broken"; this answers "broken how, and where", which is the thing a reader
 * can act on.
 *
 * `line` is 1-based, matching the graph's own `line` and what
 * `goToDefinition` expects, rather than the 0-based `Range` it comes from.
 */
export interface NodeDiagnosticItem {
  severity: NodeDiagnosticSeverity;
  line: number;
  message: string;
  /** Producer (`ts`, `eslint`, …) when the provider set one. */
  source?: string;
}

/**
 * Cap on listed diagnostics per file. A generated or badly broken file can
 * hold thousands, and the panel is a place to start reading, not a second
 * Problems view. The overflow is counted, never silently dropped.
 */
export const MAX_DIAGNOSTIC_ITEMS_PER_FILE = 50;

/** Cap on a single diagnostic message. Long type errors run to kilobytes. */
export const MAX_DIAGNOSTIC_MESSAGE_CHARS = 300;

export interface DiagnosticsOverlay {
  /** Node id → worst severity + count. Clean nodes are absent, not zeroed. */
  byNode: Record<string, NodeDiagnostic>;
  /**
   * Graph file path → the individual diagnostics in it, worst first.
   *
   * Keyed by file rather than by node id because attribution is file-scoped
   * (see `scope`): a graph routinely holds dozens of symbols from one file,
   * and keying by node would copy the same list under each of them. The
   * webview maps a node to its list through the node's own `path`.
   *
   * Message text lives here and only here. It never reaches the daemon's
   * editor plane (`broken`), which stays counts-only: that plane is queried
   * over a socket and persisted in another process, while this one is posted
   * to a webview in the same extension host that already holds the text.
   */
  itemsByFile: Record<string, NodeDiagnosticItem[]>;
  /**
   * Graph file path → how many diagnostics were dropped by
   * `MAX_DIAGNOSTIC_ITEMS_PER_FILE`. Absent when nothing was dropped.
   */
  itemsTruncated: Record<string, number>;
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
  /**
   * Files with something wrong, for the daemon's editor plane. Per file rather
   * than aggregated, because "which files are broken" composes with the graph
   * and "how many errors" does not.
   */
  broken: FileDiagnostics[];
  /** Distinct files across the rendered nodes. */
  filesSeen: number;
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

/** Error and warning tallies for one file. Info and Hint are ignored. */
function tally(diags: readonly vscode.Diagnostic[]): {
  errors: number;
  warnings: number;
} {
  let errors = 0;
  let warnings = 0;
  for (const d of diags) {
    if (d.severity === vscode.DiagnosticSeverity.Error) errors++;
    else if (d.severity === vscode.DiagnosticSeverity.Warning) warnings++;
  }
  return { errors, warnings };
}

/**
 * Reduce a tally to the one badge worth drawing: errors outrank warnings, and
 * the count is of the winning severity only (a file with 2 errors and 9
 * warnings reads "2 errors", not "11 problems"). Null when there is nothing at
 * either level, so callers omit the node rather than post a zero.
 */
function worstDiagnostic(t: {
  errors: number;
  warnings: number;
}): NodeDiagnostic | null {
  if (t.errors > 0) return { severity: "error", count: t.errors };
  if (t.warnings > 0) return { severity: "warning", count: t.warnings };
  return null;
}
/**
 * The listable diagnostics of one file, errors first and then by line.
 *
 * Same severity policy as the badge: Info and Hint are dropped, so the list
 * and the ring can never disagree about whether a file is a problem. Sorting
 * puts errors first because a reader scanning the panel wants the thing that
 * breaks the build before the thing that lints.
 */
function listItems(diags: readonly vscode.Diagnostic[]): NodeDiagnosticItem[] {
  const items: NodeDiagnosticItem[] = [];
  for (const d of diags) {
    const severity: NodeDiagnosticSeverity | null =
      d.severity === vscode.DiagnosticSeverity.Error
        ? "error"
        : d.severity === vscode.DiagnosticSeverity.Warning
          ? "warning"
          : null;
    if (!severity) continue;
    const message = d.message.replace(/\s+/g, " ").trim();
    items.push({
      severity,
      // `Range` is 0-based; every consumer here (the graph's `line`,
      // `goToDefinition`) is 1-based.
      line: d.range.start.line + 1,
      message:
        message.length > MAX_DIAGNOSTIC_MESSAGE_CHARS
          ? message.slice(0, MAX_DIAGNOSTIC_MESSAGE_CHARS - 1) + "…"
          : message,
      ...(d.source ? { source: d.source } : {}),
    });
  }
  items.sort((a, b) =>
    a.severity === b.severity
      ? a.line - b.line
      : a.severity === "error"
        ? -1
        : 1
  );
  return items;
}

/**
 * Workspace `fsPath`s of `nodes`, for deciding whether a diagnostic change is
 * about this graph at all (#698 review, P2).
 *
 * Resolved through `resolveWorkspacePath`, the same gate the overlay uses, so
 * a node the overlay would drop cannot make the listener wake up either.
 */
export function resolvedFsPaths(nodes: GraphNode[]): Set<string> {
  const out = new Set<string>();
  for (const node of nodes) {
    if (!node.path) continue;
    const uri = resolveWorkspacePath(node.path);
    if (uri) out.add(uri.fsPath);
  }
  return out;
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
  const broken: FileDiagnostics[] = [];
  const itemsByFile: Record<string, NodeDiagnosticItem[]> = {};
  const itemsTruncated: Record<string, number> = {};
  const perFile = new Map<string, NodeDiagnostic | null>();

  for (const node of nodes) {
    if (!node.path) continue;
    if (!perFile.has(node.path)) {
      const uri = resolveWorkspacePath(node.path);
      if (!uri) {
        perFile.set(node.path, null);
      } else {
        if (!published.has(uri.fsPath)) unknownCoverage.push(node.path);
        const diags = vscode.languages.getDiagnostics(uri);
        const t = tally(diags);
        if (t.errors > 0 || t.warnings > 0) {
          broken.push({ path: node.path, errors: t.errors, warnings: t.warnings });
          const items = listItems(diags);
          if (items.length > MAX_DIAGNOSTIC_ITEMS_PER_FILE) {
            itemsTruncated[node.path] =
              items.length - MAX_DIAGNOSTIC_ITEMS_PER_FILE;
          }
          itemsByFile[node.path] = items.slice(
            0,
            MAX_DIAGNOSTIC_ITEMS_PER_FILE
          );
        }
        perFile.set(node.path, worstDiagnostic(t));
      }
    }
    const worst = perFile.get(node.path);
    if (worst) byNode[node.id] = worst;
  }

  return {
    byNode,
    itemsByFile,
    itemsTruncated,
    scope: "file",
    unknownCoverage,
    broken,
    filesSeen: perFile.size,
  };
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
  /** Last report sent to the daemon, so an unchanged one is not resent. */
  private lastReportedDiagnostics = "";
  /** Last overlay posted to the webview, so an unchanged one is not reposted. */
  private lastPostedOverlay = "";
  /** Last report sent, replayed by the lease renewal without recomputing. */
  private lastReport: LspDiagnosticsReport | undefined;
  /**
   * Workspace `fsPath`s of the rendered nodes, so a diagnostic change outside
   * the graph can be ignored without recomputing anything (#698 review, P2).
   */
  private renderedFsPaths: Set<string> = new Set();
  /** Lease renewal timer; see the constructor. Cleared in `dispose`. */
  private leaseRenewal: ReturnType<typeof setInterval> | undefined;
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
    //
    // Filtered on `e.uris` (#698 review, P2): a change anywhere in the
    // workspace used to schedule a full recompute, and the recompute calls the
    // argument-less `getDiagnostics()`, whose cost tracks total workspace
    // breakage rather than graph size. A noisy linter in another editor group
    // then rebuilt the Problems list every 200ms for an overlay that never
    // changed. A newly-diagnosed file still appears in `e.uris`, so
    // `unknownCoverage` stays correct.
    vscode.languages.onDidChangeDiagnostics(
      (e) => {
        if (!e.uris.some((u) => this.renderedFsPaths.has(u.fsPath))) return;
        this.diagnosticsDebouncer.schedule();
      },
      null,
      this.disposables
    );

    // #698 review P2: the lease is renewed on a timer, not by change. Reports
    // are skipped when the reduction is identical, so a window whose
    // diagnostics settle stops renewing and is dropped at `REPORT_TTL_SECS`,
    // leaving `travsr daemon lsp` saying "no editor attached" while the panel
    // is open and current. Renewing at a third of the lease tolerates two
    // missed ticks.
    this.leaseRenewal = setInterval(
      () => this.renewLease(),
      (REPORT_TTL_SECS * 1000) / 3
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
    this.postDiagnosticsOverlay(true);
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
    this.postDiagnosticsOverlay(true);
  }

  /**
   * Recompute the overlay for the rendered node set and push it. Cheap enough
   * to run un-debounced on re-render (the node set just changed and the old
   * overlay is addressed to node ids that may no longer exist); the debounce
   * exists for the keystroke-driven path.
   */
  /**
   * Renew the daemon lease without touching the webview.
   *
   * The lease and the overlay are different concerns on different clocks: the
   * lease has to be refreshed on a timer whether or not anything changed,
   * while the webview should only be told when something did. Routing the
   * renewal through `postDiagnosticsOverlay(true)` conflated them, so every
   * renewal tick reposted a byte-identical overlay and `refreshOpenDetailProblems`
   * rebuilt the Problems list, destroying a text selection the reader was in
   * the middle of making (#698 review, P3). A smaller version of the P2 above,
   * on a five-minute period instead of 200ms, and the only remaining path to it.
   */
  private renewLease(): void {
    const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    if (!root || !this.lastReport) return;
    void reportLspDiagnostics(root, this.lastReport);
  }

  private postDiagnosticsOverlay(force = false): void {
    // No early return on an empty node set (#698 review, P3): the webview
    // keeps the previous graph's badge and Problems data until it is told
    // otherwise, so a query that matches nothing would leave "3 errors" over
    // an empty canvas. `computeDiagnosticsOverlay([])` is trivially cheap and
    // returns exactly the empty state the webview should be shown.
    const overlay = computeDiagnosticsOverlay(this.renderedNodes);
    this.renderedFsPaths = resolvedFsPaths(this.renderedNodes);

    // Deduped like the daemon report below (#698 review, P2): an unchanged
    // overlay still made the webview replace the Problems list, which destroys
    // a text selection the reader may be in the middle of making.
    const overlayKey = JSON.stringify(overlay);
    if (force || overlayKey !== this.lastPostedOverlay) {
      this.lastPostedOverlay = overlayKey;
      void this.panel.webview.postMessage({
        command: "diagnosticsOverlay",
        ...overlay,
      });
    }

    // #688: mirror the reduction into the daemon log at DEBUG. Fire and
    // forget — `reportLspDiagnostics` swallows every failure, so a stopped
    // daemon or an older one costs nothing here.
    const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    if (!root) return;

    const report = {
      files: overlay.broken,
      seen: overlay.filesSeen,
      undiagnosed: overlay.unknownCoverage.length,
    };

    // Renewing an unchanged view is not free of meaning — the lease depends on
    // it — but a keystroke that changes nothing does not need to say so twice.
    // `force` marks the render path, which is user-driven and rare, so the
    // lease is renewed whenever the user actually looks at the graph.
    const key = JSON.stringify(report);
    if (!force && key === this.lastReportedDiagnostics) return;
    this.lastReportedDiagnostics = key;
    this.lastReport = report;
    void reportLspDiagnostics(root, report);
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
    if (this.leaseRenewal) clearInterval(this.leaseRenewal);
    // Withdraw this window's view rather than leave it asserting what it saw
    // for the rest of the lease.
    const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    if (root) void detachSession(root);
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

<!-- ── Breadcrumb nav (P3 repo-map LOD) ─────────────────────────────────── -->
<nav id="breadcrumb" aria-label="Graph navigation level">
  <!-- Populated by graph.js renderBreadcrumb() -->
</nav>

<!-- ── Disambiguation bar (multiple implementations of same symbol) ─────── -->
<div id="disambig-bar" role="navigation" aria-label="Implementation selector"></div>
<!-- Hover popup for truncated implementation chips. Outside the bar because
     the chip row clips on both axes once it scrolls. -->
<div id="db-tip" role="tooltip" aria-hidden="true"></div>

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

  <!-- Node search overlay. Inside #main so it is positioned against the canvas
       rather than the window: the toolbar above wraps to two rows on a narrow
       panel, and a window-anchored overlay landed on top of it. -->
  <div id="node-search" style="display:none" role="search" aria-label="Search nodes">
    <input id="node-search-input" type="text" placeholder="Search nodes…" autocomplete="off" spellcheck="false">
    <ul id="node-search-results" role="listbox"></ul>
  </div>

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
  <span id="statusGraph">,</span>
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
