/**
 * ITEM 1 — Context Explorer panel.
 *
 * Surfaces `get_context` (PPR + knapsack + KNN + Tier 0 trust signals) as a
 * first-class UI panel. Visual design mirrors docs/mockups/context-explorer-mockup.html
 * exactly — canonical travsr-designer tokens, three states (strong/abstain/degraded),
 * per-node expandable snippets, provenance legend, token-saved bar, copy button.
 *
 * Command: travsr.openContextExplorer  Keybinding: Cmd+Shift+K / Ctrl+Shift+K
 */

import * as vscode from "vscode";
import type { McpClient } from "./mcp";
import { parseContextResult, isParsed, parseSnippetsResult } from "./context/parse";
import { stripEnvelope } from "./commands";

// ── Keyword set (salvaged from contextProvider.ts) ────────────────────────────

const KEYWORDS = new Set([
  "import","export","from","const","let","var","function","class","return",
  "if","else","for","while","do","switch","case","break","continue","new",
  "this","super","extends","implements","interface","type","enum","namespace",
  "async","await","try","catch","finally","throw","typeof","instanceof","void",
  "null","undefined","true","false","default","static","public","private",
  "protected","readonly","abstract","override","declare","module","require",
  "fn","mut","use","mod","pub","struct","impl","trait","where","match",
  "ref","in","as","is","of","with","yield","delete","get","set",
]);

/** Resolve the graph-query token at a document position, filtering language
 *  keywords. Shared by the Context Explorer (cursor) and the "add context to
 *  chat" CodeAction (invocation range) so both agree on what counts as a symbol. */
export function getSymbolAt(document: vscode.TextDocument, position: vscode.Position): string {
  const range = document.getWordRangeAtPosition(position, /[\w:.]+/);
  if (!range) return "";
  const word = document.getText(range);
  return KEYWORDS.has(word) ? "" : word;
}

export function getSymbolAtCursor(editor: vscode.TextEditor): string {
  return getSymbolAt(editor.document, editor.selection.active);
}

// ── Panel singleton ───────────────────────────────────────────────────────────

export class ContextExplorerPanel {
  private static current: ContextExplorerPanel | undefined;

  private readonly panel: vscode.WebviewPanel;
  // The Rust MCP server is single-threaded sequential: it ignores $/cancelRequest
  // and runs every get_context to completion before reading the next line.
  // Aborting the client-side RPC only removes it from the pending map; the server
  // still takes 20-30 s on large repos before the next query can start, causing
  // the second callTool to either time out or appear blank.
  //
  // Solution: generation counter + single pending slot.
  //   - New query arrives → saved in pendingQuery, loading posted immediately.
  //   - Server finishes the old query → response discarded (gen mismatch).
  //   - drainQueryQueue() immediately fires the pending new query.
  //   - Phase 2 (snippets) uses a real AbortSignal so it IS cancelled on new query.
  private queryGeneration = 0;
  private queryInFlight = false;
  private pendingQuery: { query: string; budget: number } | null = null;
  private snippetAbort: AbortController | undefined;
  private disposed = false;
  private disposables: vscode.Disposable[] = [];

  static show(client: McpClient, context: vscode.ExtensionContext): ContextExplorerPanel {
    if (ContextExplorerPanel.current) {
      ContextExplorerPanel.current.panel.reveal(vscode.ViewColumn.Beside);
      return ContextExplorerPanel.current;
    }
    const panel = vscode.window.createWebviewPanel(
      "travsrContextExplorer",
      "Travsr Context",
      vscode.ViewColumn.Beside,
      { enableScripts: true, localResourceRoots: [], retainContextWhenHidden: true }
    );
    const instance = new ContextExplorerPanel(panel, client, context);
    ContextExplorerPanel.current = instance;
    return instance;
  }

  private constructor(
    panel: vscode.WebviewPanel,
    private readonly client: McpClient,
    context: vscode.ExtensionContext
  ) {
    this.panel = panel;
    panel.webview.html = buildShellHtml();

    panel.onDidDispose(() => { this.dispose(); }, null, this.disposables);

    panel.webview.onDidReceiveMessage(
      (msg: { command: string; query?: string; budget?: number; path?: string; line?: string; raw?: string; sig?: string; mode?: string }) => {
        if (msg.command === "query" && msg.query !== undefined) {
          this.scheduleQuery(msg.query.trim(), msg.budget ?? 2000);
        } else if (msg.command === "fetchSnippet" && msg.sig) {
          void this.fetchSingleSnippet(msg.sig, msg.mode ?? "auto");
        } else if (msg.command === "open" && msg.path) {
          const line = msg.line ? parseInt(msg.line, 10) : undefined;
          void openAtLine(msg.path, line);
        } else if (msg.command === "copy" && msg.raw) {
          void vscode.env.clipboard.writeText(msg.raw).then(() =>
            void vscode.window.showInformationMessage("Travsr: context copied to clipboard.")
          );
        } else if (msg.command === "runLever" && msg.raw) {
          // Degradation lever — show the CLI command to the user
          void vscode.window.showInformationMessage(
            `Run in terminal: ${msg.raw}`,
            "Copy command"
          ).then((action) => {
            if (action === "Copy command") void vscode.env.clipboard.writeText(msg.raw ?? "");
          });
        }
      },
      null,
      this.disposables
    );

    context.subscriptions.push({ dispose: () => this.dispose() });
  }

  /** Seed the query box and optionally provide the source file token estimate. */
  seedQuery(seed: string, sourceFileTok?: number): void {
    void this.panel.webview.postMessage({ command: "seed", seed, sourceFileTok });
  }

  /** Enqueue a new get_context query. Shows loading immediately; the actual
   *  callTool is deferred until the server finishes any in-flight query. */
  private scheduleQuery(query: string, budget: number): void {
    if (!query || this.disposed) return;
    // Abort Phase 2 (snippets) for the superseded query immediately — get_snippets
    // IS a cheap call the server can cancel, unlike get_context.
    this.snippetAbort?.abort();
    this.pendingQuery = { query, budget };
    void this.panel.webview.postMessage({ command: "loading" });
    if (!this.queryInFlight) {
      void this.drainQueryQueue();
    }
    // If queryInFlight, the drainQueryQueue while-loop will process pendingQuery
    // as soon as the server responds to the current get_context.
  }

  private async drainQueryQueue(): Promise<void> {
    while (this.pendingQuery && !this.disposed) {
      const { query, budget } = this.pendingQuery;
      this.pendingQuery = null;
      const gen = ++this.queryGeneration;
      this.queryInFlight = true;

      try {
        // No AbortSignal: the Rust server ignores $/cancelRequest (sequential loop).
        // 120 s covers worst-case: server finishes a prior get_context (~30 s) then
        // this one (~30 s) before the client gives up.
        const raw = await this.client.callTool(
          "get_context",
          { query, token_budget: String(budget) },
          undefined,
          120_000
        );

        // A newer query arrived while we waited — discard this result and loop.
        if (gen !== this.queryGeneration || this.disposed) continue;

        const stripped = stripEnvelope(raw);

        const result = parseContextResult(stripped);
        if (!isParsed(result)) {
          void this.panel.webview.postMessage({ command: "renderRaw", raw: stripped || result.raw });
          continue;
        }

        // RFC-022 §14: when the backend groups by match-source (flag-on), keep the
        // Exact → Semantic → Docs → Tests → Relevant section order and sort by
        // score *within* a section — a flat score-sort would shuffle the sections
        // together. Order mirrors the backend trust_rank (seed.rs). Flag-off (no
        // matchSource) keeps the original flat score-sort.
        if (result.nodes.some((n) => n.matchSource)) {
          const rank: Record<string, number> = {
            exact: 0,
            semantic: 1,
            docs: 2,
            tests: 3,
            relevant: 4,
          };
          result.nodes.sort(
            (a, b) =>
              (rank[a.matchSource ?? "relevant"] ?? 5) - (rank[b.matchSource ?? "relevant"] ?? 5) ||
              (b.score ?? 0) - (a.score ?? 0)
          );
        } else {
          result.nodes.sort((a, b) => (b.score ?? 0) - (a.score ?? 0));
        }
        void this.panel.webview.postMessage({ command: "render", data: result, rawText: stripped });

        // Phase 2: snippets — use a real AbortSignal so a new query cancels this.
        const SNIPPET_KINDS = new Set([
          "function", "method", "constructor",
          "class", "struct", "interface", "trait", "enum", "impl",
          "type", "type_alias",
        ]);
        const snippetBudget = Math.min(budget * 2, 6000);
        const snippetLimit = Math.min(50, Math.max(12, Math.ceil(snippetBudget / 300)));
        const topSigs = result.nodes
          .filter(n => SNIPPET_KINDS.has(n.kind) && !n.sig.startsWith("scip:"))
          .slice(0, snippetLimit)
          .map(n => n.sig);

        if (topSigs.length === 0) {
          void this.panel.webview.postMessage({ command: "enrichSnippets", snippets: {}, snippetCount: 0 });
          continue;
        }

        this.snippetAbort = new AbortController();
        const snippetSignal = this.snippetAbort.signal;
        const snippetRaw = await this.client.callTool(
          "get_snippets",
          { symbols: topSigs.join("\n"), token_budget: String(snippetBudget) },
          snippetSignal,
          30_000
        );
        if (!snippetSignal.aborted && gen === this.queryGeneration && !this.disposed) {
          const snippetMap = parseSnippetsResult(stripEnvelope(snippetRaw ?? ""));
          void this.panel.webview.postMessage({
            command: "enrichSnippets",
            snippets: Object.fromEntries(snippetMap),
            snippetCount: snippetMap.size,
          });
        }
      } finally {
        this.queryInFlight = false;
      }
    }
  }

  /** On-demand single-symbol fetch for a node the user expanded or pinned.
   *  Bypasses the score/budget top-N cap entirely — one symbol, generous budget,
   *  in the requested representation (auto | full | skeleton). */
  private async fetchSingleSnippet(sig: string, mode: string): Promise<void> {
    if (this.disposed) return;
    try {
      // Large budget so a `full` definition is never truncated. The server always
      // includes the first (only) symbol, so even an oversized body comes back whole.
      const raw = await this.client.callTool(
        "get_snippets",
        { symbols: sig, token_budget: "30000", mode },
        undefined,
        30_000
      );
      if (this.disposed) return;
      const map = parseSnippetsResult(stripEnvelope(raw ?? ""));
      const snippet = map.get(sig) ?? [...map.values()][0] ?? "";
      void this.panel.webview.postMessage({ command: "snippetFetched", sig, mode, snippet });
    } catch {
      void this.panel.webview.postMessage({ command: "snippetFetched", sig, mode, snippet: "" });
    }
  }

  private dispose(): void {
    this.disposed = true;
    this.snippetAbort?.abort();
    ContextExplorerPanel.current = undefined;
    for (const d of this.disposables) d.dispose();
    this.disposables = [];
  }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async function openAtLine(filePath: string, line?: number): Promise<void> {
  const root = vscode.workspace.workspaceFolders?.[0]?.uri;
  const uri = filePath.startsWith("/")
    ? vscode.Uri.file(filePath)
    : root ? vscode.Uri.joinPath(root, filePath) : vscode.Uri.file(filePath);
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

// ── Webview shell (matches mockup exactly) ────────────────────────────────────

function buildShellHtml(): string {
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
<style>
*,*::before,*::after{box-sizing:border-box;margin:0;padding:0}
:root{
  --ch-950:#0a0a0a;--ch-900:#0f0f0f;--ch-800:#141414;--ch-750:#181818;
  --ch-700:#1a1a1a;--ch-650:#212121;--ch-600:#2c2c2c;--ch-550:#333333;
  --ch-500:#4d4d4d;--ch-400:#808080;--ch-300:#b3b3b3;
  --linen-100:#f6f1ed;--linen-200:#e8ddd5;--linen-300:#e2d4ca;
  --linen-400:#c8b7ab;--linen-500:#ad998b;--linen-600:#8f7a6c;
  --green-300:#86df86;--green-400:#5fce5f;--green-500:#429429;--green-deep:#102910;
  --gold-300:#fcd053;--gold-500:#d89a02;--gold-deep:#332100;
  --orange-300:#ffb970;--orange-400:#fb923c;--orange-deep:#3a1c00;
  --red-400:#ef4444;--red-deep:#2d0b0b;
  --bg:var(--ch-800);--bg-elev:var(--ch-700);--bg-input:var(--ch-600);
  --border:var(--ch-550);--fg:var(--linen-100);--fg-muted:var(--linen-400);
  --fg-subtle:var(--linen-600);--node:var(--green-300);--accent:var(--green-300);
  --via-seed:var(--green-300);--via-seed-bg:var(--green-deep);
  --via-caller:var(--orange-400);--via-caller-bg:var(--orange-deep);
  --via-dependency:var(--gold-300);--via-dependency-bg:var(--gold-deep);
  --via-context:var(--linen-400);--via-context-bg:var(--ch-600);
  --font-mono:ui-monospace,Menlo,Monaco,"Cascadia Code",monospace;
  --font-sans:system-ui,-apple-system,sans-serif;
  --r-sm:4px;--r-md:8px;--r-full:9999px;
}
html,body{height:100%}
body{background:var(--bg);color:var(--fg);font-family:var(--font-sans);
  font-size:13px;line-height:1.5;padding:14px 16px 24px;
  display:flex;flex-direction:column;gap:10px;overflow-x:hidden}

/* ── query bar ── */
.query-row{display:flex;gap:8px;align-items:center;flex-shrink:0}
.query-input{
  flex:1;display:flex;align-items:center;gap:8px;background:var(--bg-input);
  border:1px solid var(--border);border-radius:var(--r-md);padding:8px 12px;
  min-width:0}
.query-input:focus-within{border-color:var(--node);box-shadow:0 0 0 2px rgba(134,223,134,.15)}
.query-input .mag{color:var(--fg-subtle);font-size:14px;flex-shrink:0;cursor:pointer}
.query-input .mag:hover{color:var(--node)}
.query-input input{
  flex:1;background:none;border:none;outline:none;color:var(--fg);
  font-family:var(--font-mono);font-size:13px;min-width:0}
.seeded-badge{
  font-size:10px;color:var(--fg-subtle);background:var(--ch-550);
  padding:2px 8px;border-radius:var(--r-full);white-space:nowrap;flex-shrink:0}
.budget-wrap{display:flex;flex-direction:column;gap:2px;min-width:140px}
.budget-wrap label{
  font-size:10px;color:var(--fg-subtle);
  display:flex;justify-content:space-between}
.budget-wrap input[type=range]{width:100%;accent-color:var(--node);height:4px}
.btn-primary{
  display:flex;align-items:center;gap:6px;background:var(--green-deep);color:var(--green-300);
  border:1px solid var(--green-500);border-radius:var(--r-md);padding:8px 14px;
  font-size:12px;font-weight:600;cursor:pointer;white-space:nowrap;font-family:var(--font-sans)}
.btn-primary:hover{background:var(--green-500);color:#0a1a0a}
.btn-primary:disabled{opacity:.45;cursor:not-allowed}
.btn-search{
  display:flex;align-items:center;gap:6px;background:var(--ch-650);color:var(--fg-muted);
  border:1px solid var(--border);border-radius:var(--r-md);padding:8px 14px;
  font-size:12px;font-weight:600;cursor:pointer;white-space:nowrap;font-family:var(--font-sans)}
.btn-search:hover{border-color:var(--node);color:var(--node)}

/* ── signals pills ── */
.signals{display:flex;align-items:center;gap:8px;flex-wrap:wrap;flex-shrink:0}
.pill{
  display:inline-flex;align-items:center;gap:5px;font-size:11px;font-weight:600;
  padding:4px 10px;border-radius:var(--r-full);border:1px solid transparent}
.pill .dot{width:7px;height:7px;border-radius:50%}
.pill.conf-strong{background:var(--green-deep);color:var(--green-300);border-color:var(--green-500)}
.pill.conf-strong .dot{background:var(--green-300)}
.pill.conf-partial{background:var(--gold-deep);color:var(--gold-300);border-color:var(--gold-500)}
.pill.conf-partial .dot{background:var(--gold-300)}
.pill.conf-weak,.pill.conf-none{background:var(--red-deep);color:var(--red-400);border-color:var(--red-400)}
.pill.conf-weak .dot,.pill.conf-none .dot{background:var(--red-400)}
.pill.meta{background:var(--ch-650);color:var(--fg-muted);border-color:var(--border)}
.pill.meta .k{color:var(--fg-subtle)}

/* ── token-saved bar ── */
.tokenbar{
  background:var(--bg-elev);border:1px solid var(--border);border-radius:var(--r-md);
  padding:10px 12px;flex-shrink:0}
.tokenbar .top{display:flex;justify-content:space-between;align-items:baseline;margin-bottom:7px}
.tokenbar .lhs{font-size:12px;color:var(--fg)}
.tokenbar .lhs b{color:var(--node);font-variant-numeric:tabular-nums}
.tokenbar .rhs{font-size:11px;color:var(--fg-muted)}
.tokenbar .rhs .save{color:var(--green-300);font-weight:700}
.track{height:8px;background:var(--ch-600);border-radius:var(--r-full);overflow:hidden;position:relative}
.track .fill{height:100%;background:linear-gradient(90deg,var(--green-500),var(--green-300));border-radius:var(--r-full)}
.tok-legend{display:flex;gap:14px;margin-top:7px;font-size:10px;color:var(--fg-subtle)}
.tok-legend span::before{content:"■ "}
.tok-legend .used::before{color:var(--green-300)}
.tok-legend .base::before{color:var(--fg-subtle)}

/* ── abstain banner ── */
.abstain{
  background:var(--red-deep);border:1px solid var(--red-400);border-radius:var(--r-md);
  padding:14px 16px;flex-shrink:0}
.abstain h3{font-size:14px;color:var(--red-400);display:flex;align-items:center;gap:8px;margin-bottom:6px}
.abstain p{font-size:12px;color:var(--linen-300);margin-bottom:4px}
.abstain .hint{font-size:11px;color:var(--fg-muted)}
.abstain code{font-family:var(--font-mono);background:var(--ch-700);padding:1px 5px;border-radius:var(--r-sm);color:var(--linen-200)}
/* weak match — softer than a true abstain: results WERE returned, just loosely related */
.abstain.weak{background:var(--orange-deep);border-color:var(--orange-400)}
.abstain.weak h3{color:var(--orange-400)}

/* ── degradation note → lever ── */
.lever-note{
  display:flex;align-items:center;gap:12px;background:var(--gold-deep);
  border:1px solid var(--gold-500);border-radius:var(--r-md);padding:11px 14px;flex-shrink:0}
.lever-note .ico{font-size:16px}
.lever-note .txt{flex:1;font-size:12px;color:var(--gold-300)}
.lever-note .txt b{color:var(--linen-100)}
.lever-note .txt small{display:block;color:var(--linen-500);font-size:10.5px;margin-top:2px}
.lever-btn{
  background:var(--gold-500);color:#221500;border:none;border-radius:var(--r-md);
  padding:7px 13px;font-size:11.5px;font-weight:700;cursor:pointer;white-space:nowrap;
  font-family:var(--font-mono)}
.lever-btn:hover{background:var(--gold-300)}

/* ── results ── */
.scroll-area{flex:1;overflow-y:auto;display:flex;flex-direction:column;gap:8px;min-height:0}
.results-hdr{
  font-size:11px;text-transform:uppercase;letter-spacing:.07em;color:var(--fg-subtle);
  display:flex;justify-content:space-between;flex-shrink:0}

/* ── filter bar: top-k + snippets-only ── */
.filterbar{display:flex;align-items:center;gap:12px;flex-wrap:wrap;flex-shrink:0}
.fb-seg{display:inline-flex;align-items:center;gap:2px;background:var(--ch-700);
  border:1px solid var(--border);border-radius:var(--r-md);padding:2px}
.fb-seg .fb-lbl{font-size:10px;text-transform:uppercase;letter-spacing:.06em;
  color:var(--fg-subtle);padding:0 6px 0 5px}
.fb-seg button{background:transparent;border:none;color:var(--fg-muted);
  font-family:var(--font-mono);font-size:11px;padding:3px 9px;border-radius:var(--r-sm);cursor:pointer}
.fb-seg button:hover{color:var(--fg)}
.fb-seg button.on{color:var(--node);font-weight:700;box-shadow:inset 0 0 0 1px var(--node)}
.fb-toggle{display:inline-flex;align-items:center;gap:6px;font-size:11px;
  color:var(--fg-muted);cursor:pointer;user-select:none}
.fb-toggle input{accent-color:var(--node);cursor:pointer}
.fb-count{margin-left:auto;font-size:10.5px;color:var(--fg-subtle);font-variant-numeric:tabular-nums}
.scroll-area>#node-list{display:flex;flex-direction:column;gap:8px}
/* ── RFC-022 §14 match-source section header ── */
.section-head{
  font-family:var(--font-sans);font-size:10px;font-weight:700;letter-spacing:.07em;
  text-transform:uppercase;color:var(--fg-muted);
  padding:8px 2px 5px;border-bottom:1px solid var(--border);
  display:flex;align-items:center;gap:7px}
.section-head::before{content:'';width:6px;height:6px;border-radius:var(--r-full);
  background:var(--accent);flex-shrink:0}
.section-head:first-child{padding-top:2px}
.node{
  border:1px solid var(--border);border-radius:var(--r-md);background:var(--bg-elev);
  overflow:hidden;transition:border-color .15s;flex-shrink:0}
.node:hover{border-color:var(--ch-400)}
.node .row{display:flex;align-items:center;gap:10px;padding:10px 12px;cursor:pointer;user-select:none}
.node .kind-ico{font-size:15px;width:20px;text-align:center;flex-shrink:0;color:var(--fg-subtle)}
.node .main{flex:1;min-width:0}
.node .sig{font-family:var(--font-mono);font-size:12.5px;color:var(--fg);
  white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.node .loc{font-family:var(--font-mono);font-size:10.5px;color:var(--fg-muted);
  margin-top:2px;cursor:pointer;display:inline-block}
.node .loc:hover{color:var(--node);text-decoration:underline}
.via{
  display:inline-block;vertical-align:middle;font-size:8px;font-weight:700;
  text-transform:uppercase;letter-spacing:.04em;padding:2px 7px;
  border-radius:var(--r-full);flex-shrink:0;
  max-width:220px;overflow:hidden;white-space:nowrap;text-overflow:ellipsis}
.via.seed{color:var(--via-seed);background:var(--via-seed-bg)}
.via.caller{color:var(--via-caller);background:var(--via-caller-bg)}
.via.dependency{color:var(--via-dependency);background:var(--via-dependency-bg)}
.via.context,.via.other{color:var(--via-context);background:var(--via-context-bg)}
.score{display:flex;flex-direction:column;align-items:flex-end;gap:3px;width:60px;flex-shrink:0}
.score .num{font-family:var(--font-mono);font-size:11px;color:var(--fg-muted);font-variant-numeric:tabular-nums}
.score .sbar{width:52px;height:4px;background:var(--ch-600);border-radius:var(--r-full);overflow:hidden}
.score .sfill{height:100%;background:var(--node);border-radius:var(--r-full)}
.chev{color:var(--fg-subtle);font-size:11px;flex-shrink:0;transition:transform .15s}
.node.open .chev{transform:rotate(90deg)}
.snippet{display:none;border-top:1px solid var(--border);background:var(--ch-750);padding:10px 14px 12px}
.node.open .snippet{display:block}
.snippet pre{font-family:var(--font-mono);font-size:11.5px;line-height:1.55;
  color:var(--linen-200);overflow-x:auto;white-space:pre-wrap;word-break:break-all}
.snippet .cap{font-size:9.5px;color:var(--fg-subtle);margin-top:6px}
.node.dim{opacity:.45}

/* ── per-node snippet controls: mode selector + add-to-context ── */
.snip-ctl{display:flex;align-items:center;gap:10px;flex-wrap:wrap;margin-bottom:9px}
.mode-seg{display:inline-flex;align-items:center;gap:2px;background:var(--ch-700);
  border:1px solid var(--border);border-radius:var(--r-md);padding:2px}
.mode-seg button{background:transparent;border:none;color:var(--fg-muted);
  font-family:var(--font-mono);font-size:10.5px;padding:3px 9px;border-radius:var(--r-sm);cursor:pointer}
.mode-seg button:hover{color:var(--fg)}
.mode-seg button.on{color:var(--node);font-weight:700;box-shadow:inset 0 0 0 1px var(--node)}
.pin-btn{background:var(--ch-650);color:var(--fg-muted);border:1px solid var(--border);
  border-radius:var(--r-md);padding:4px 11px;font-size:10.5px;font-weight:600;cursor:pointer;
  font-family:var(--font-sans);white-space:nowrap}
.pin-btn:hover{border-color:var(--node);color:var(--node)}
.pin-btn.on{background:var(--green-deep);color:var(--green-300);border-color:var(--green-500)}
.pin-badge{font-size:11px;flex-shrink:0}
.snip-badge{display:inline-flex;align-items:center;font-size:11px;color:var(--green-300);flex-shrink:0}
.snip-loading{font-size:11px;color:var(--fg-subtle);font-style:italic;padding:4px 0}

/* ── provenance legend ── */
.legend-box{
  margin-top:4px;padding:10px 12px;background:var(--ch-750);
  border:1px dashed var(--border);border-radius:var(--r-md);flex-shrink:0}
.legend-box .t{font-size:10px;text-transform:uppercase;letter-spacing:.07em;
  color:var(--fg-subtle);margin-bottom:7px}
.legend-box .items{display:flex;gap:14px;flex-wrap:wrap}
.legend-box .it{font-size:11px;color:var(--fg-muted);display:flex;align-items:center;gap:6px}
.legend-box .why{color:var(--fg-subtle)}
.ms-tag{display:inline-block;font-size:8px;font-weight:700;text-transform:uppercase;
  letter-spacing:.04em;padding:2px 7px;border-radius:var(--r-full);flex-shrink:0;
  color:var(--accent);background:var(--green-deep)}

/* ── misc states ── */
.empty{color:var(--fg-subtle);font-size:12px;padding:28px 0;text-align:center;flex-shrink:0}
.loading{color:var(--fg-subtle);font-size:12px;font-style:italic;padding:12px 0;flex-shrink:0}
pre.raw-fallback{
  font-family:var(--font-mono);font-size:11px;background:var(--bg-elev);border-radius:var(--r-md);
  padding:12px;overflow-x:auto;white-space:pre-wrap;word-break:break-all;color:var(--fg-muted)}
</style>
</head>
<body>

<!-- query bar: input + budget + copy button (matches mockup layout) -->
<div class="query-row">
  <div class="query-input" id="qi">
    <span class="mag" id="mag-btn" title="Search (Enter)">⌕</span>
    <input id="q" placeholder="Symbol or question — e.g. PaymentService.charge" autofocus spellcheck="false">
    <span class="seeded-badge" id="seeded" style="display:none"></span>
  </div>
  <div class="budget-wrap">
    <label>token budget <span id="bv">2000</span></label>
    <input type="range" id="budget" min="500" max="8000" step="500" value="2000">
  </div>
  <button class="btn-search" id="run-btn" title="Search (Enter)">⌕ Search</button>
  <button class="btn-primary" id="copy-btn" disabled title="Run a query first">⧉ Copy context</button>
</div>

<!-- signals pills (hidden until results arrive) -->
<div class="signals" id="signals" style="display:none"></div>

<!-- token-saved bar (hidden until results arrive) -->
<div class="tokenbar" id="tokenbar" style="display:none">
  <div class="top">
    <div class="lhs"><b id="used-tok">0</b> tokens of context selected</div>
    <div class="rhs"><span class="save" id="save-pct"></span> <span id="base-tok-txt"></span></div>
  </div>
  <div class="track"><div class="fill" id="tok-fill" style="width:0%"></div></div>
  <div class="tok-legend"><span class="used">selected context</span><span class="base">whole-file baseline</span></div>
</div>

<!-- scrollable results area -->
<div class="scroll-area" id="scroll">
  <p class="empty">Type a symbol or question and press Enter ⏎</p>
</div>

<!-- provenance legend (shown once results arrive) -->
<div class="legend-box" id="legend-box" style="display:none">
  <div class="t">How each result was matched, then how it connects in the graph</div>
  <div class="items">
    <div class="it"><span class="ms-tag">Exact</span><span class="why">literal symbol / name match</span></div>
    <div class="it"><span class="ms-tag">Semantic</span><span class="why">ranked by relevance</span></div>
    <div class="it"><span class="ms-tag">Docs</span><span class="why">documentation prose (verify against code)</span></div>
    <div class="it"><span class="ms-tag">Tests</span><span class="why">test entry points &amp; fixtures</span></div>
    <div class="it"><span class="ms-tag">Relevant</span><span class="why">nearby in the graph (see via)</span></div>
    <div class="it"><span class="via caller">caller</span><span class="why">calls a match</span></div>
    <div class="it"><span class="via dependency">dependency</span><span class="why">imported or used by a match</span></div>
    <div class="it"><span class="via context">context</span><span class="why">on the path between matches</span></div>
  </div>
</div>

<script>
const vscode = acquireVsCodeApi();
const qEl = document.getElementById('q');
const budgetEl = document.getElementById('budget');
const bvEl = document.getElementById('bv');
const copyBtn = document.getElementById('copy-btn');
const scroll = document.getElementById('scroll');
const signalsEl = document.getElementById('signals');
const tokenbar = document.getElementById('tokenbar');
const legendBox = document.getElementById('legend-box');
let sourceFileTok = 0;
let lastRawText = '';
// Client-side model so filter changes and async snippet enrichment can re-render
// the node list without a fresh get_context round-trip.
let state = { nodes: [], snippets: {}, isAbstain: false, topK: 0, snippetsOnly: false, openSigs: new Set(), mode: {}, pinned: new Set() };

budgetEl.addEventListener('input', () => { bvEl.textContent = budgetEl.value; });

function submit() {
  const q = qEl.value.trim();
  if (!q) return;
  vscode.postMessage({ command:'query', query:q, budget:parseInt(budgetEl.value,10) });
}
document.getElementById('mag-btn').addEventListener('click', submit);
document.getElementById('run-btn').addEventListener('click', submit);
qEl.addEventListener('keydown', e => { if(e.key==='Enter'&&!e.shiftKey){e.preventDefault();submit();} });

copyBtn.addEventListener('click', () => {
  // Base context = get_context metadata. Pinned nodes append their full/skeleton
  // definitions so they actually land in the copied AI context (the metadata
  // alone carries no bodies).
  let payload = lastRawText || '';
  if(state.pinned.size>0){
    const blocks=[];
    for(const sig of state.pinned){
      const body = state.snippets[sig];
      if(body) blocks.push('### '+sig+' ['+(state.mode[sig]||'auto')+']\\n'+body);
    }
    if(blocks.length) payload += '\\n\\n## Pinned definitions\\n\\n'+blocks.join('\\n\\n');
  }
  if(payload) vscode.postMessage({ command:'copy', raw:payload });
});

function esc(s){ return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;'); }

function kindIco(kind){
  switch(kind){
    case 'function': case 'method': return 'ƒ';
    case 'class': return '◆';
    case 'interface': return '◇';
    case 'struct': return '◼';
    case 'enum': return '⊞';
    case 'var': case 'variable': return '$';
    case 'file': return '⬤';
    default: return '·';
  }
}

function viaClass(via){
  // RFC-019 renders relevant rows as "caller of X" / "dependency of X", so match
  // on the leading role, not exact equality. Tolerant of undefined (hoisted seed).
  const v = String(via||'').toLowerCase();
  if(v.startsWith('seed')) return 'seed';
  if(v.startsWith('caller')) return 'caller';
  if(v.startsWith('dep')) return 'dependency';
  return 'context';
}

// RFC-022 §14: labelled section header for a match-source group. Real heading for
// the a11y outline; the source it names is dropped from each row's via badge below.
function sectionHead(ms){
  const label = ms==='exact' ? 'Exact · literal / FTS match'
    : ms==='semantic' ? 'Semantic · cross-encoder ranked'
    : ms==='docs' ? 'Docs · documentation prose (verify against code)'
    : ms==='tests' ? 'Tests · test entry points & fixtures'
    : 'Relevant · graph-adjacent context';
  return \`<div class="section-head" role="heading" aria-level="3">\${label}</div>\`;
}

// Strip structural type prefixes (fn:, method:, class:, etc.) so the badge
// shows "CALLER OF CONTAINSPREFIX" instead of "CALLER OF FN:CONTAINSPREFIX".
// Full string is still shown in the title tooltip.
function shortVia(via) {
  return String(via).replace(/\b(fn|class|method|interface|var|import):/gi, '');
}

function confPillClass(conf){
  const c = conf.toLowerCase();
  if(c==='strong') return 'conf-strong';
  if(c==='moderate'||c==='partial') return 'conf-partial';
  return 'conf-none';
}

// Extract actionable CLI command from a degradation note, if present.
function extractLever(note){
  const m = /run \`(travsr [^']+?)\`/.exec(note);
  return m ? m[1].trim() : null;
}

// Format note text into a human-readable title + small detail.
function noteTitle(note){
  // strip outer [note: ...] wrapper
  const inner = note.replace(/^\[note:\s*/,'').replace(/\]$/,'');
  const dashIdx = inner.indexOf(' — ');
  if(dashIdx>=0) return { title: inner.slice(0,dashIdx), detail: inner.slice(dashIdx+3) };
  return { title: inner, detail: '' };
}

function renderParsed(data, rawText){
  lastRawText = rawText;
  const h = data.header;
  const f = data.footer;
  const totalTok = f.metaTokens + f.snippetTokens;
  const isAbstain = data.notes.some(n => n.includes('no strong match')||n.includes('abstain'));

  // reset client model for the new result set
  state.nodes = data.nodes;
  state.snippets = {};
  state.isAbstain = isAbstain;
  state.topK = 0;
  state.snippetsOnly = false;
  state.openSigs = new Set();
  state.mode = {};
  state.pinned = new Set();
  updateCopyLabel();

  // signals pills
  signalsEl.innerHTML =
    \`<span class="pill \${confPillClass(h.confidence)}"><span class="dot"></span>confidence: \${esc(h.confidence)}</span>\` +
    \`<span class="pill meta"><span class="k">coverage</span> \${h.coverage[0]} / \${h.coverage[1]}</span>\` +
    \`<span class="pill meta"><span class="k">tier</span> \${esc(h.tier)}</span>\`;
  signalsEl.style.display = 'flex';

  // enable copy button now that we have results
  copyBtn.disabled = false;
  copyBtn.title = 'Copy context to clipboard';

  // token bar
  if(sourceFileTok>0&&totalTok>0){
    const pct = Math.min(100,Math.round(totalTok/sourceFileTok*100));
    const saving = Math.round((1-totalTok/sourceFileTok)*100);
    document.getElementById('used-tok').textContent = totalTok.toLocaleString();
    document.getElementById('save-pct').textContent = saving>0 ? saving+'% smaller' : '';
    document.getElementById('base-tok-txt').textContent = saving>0
      ? \`than opening the file (\${sourceFileTok.toLocaleString()} tok)\` : '';
    document.getElementById('tok-fill').style.width = pct+'%';
    tokenbar.style.display = 'block';
  } else if(totalTok>0){
    const budget = parseInt(budgetEl.value,10);
    const over = totalTok > budget;
    document.getElementById('used-tok').textContent = totalTok.toLocaleString();
    document.getElementById('save-pct').textContent = over ? '↑ over budget' : '';
    document.getElementById('save-pct').style.color = over ? 'var(--orange-400)' : '';
    document.getElementById('base-tok-txt').textContent = over
      ? 'exact text matches are always shown in full — the budget only limits the ranked-by-relevance results'
      : 'tokens of context';
    document.getElementById('tok-fill').style.width = Math.min(100,Math.round(totalTok/budget*100))+'%';
    document.getElementById('tok-fill').style.background = over
      ? 'linear-gradient(90deg,var(--orange-400),var(--gold-300))' : '';
    tokenbar.style.display = 'block';
  }

  // build scroll content
  let html = '';

  // weak-match banner — this path always returned real seeds (a true None
  // abstention is handled separately in the renderRaw branch), so the copy is an
  // honest "loosely related" warning, not a false "it abstained".
  if(isAbstain){
    html += \`<div class="abstain weak">
  <h3>◐ Weak match</h3>
  <p>Travsr didn't find a strong graph anchor for this query, so the results below are loosely related — structural neighbours rather than a confident answer.</p>
  <p class="hint">For a precise result, try a concrete symbol — e.g. <code>PaymentService.charge</code> — or narrow to one concept per query.</p>
</div>\`;
  }

  // degradation levers (non-abstain notes)
  for(const note of data.notes){
    if(note.includes('no strong match')||note.includes('abstain')) continue;
    const lever = extractLever(note);
    const {title,detail} = noteTitle(note);
    html += \`<div class="lever-note">
  <span class="ico">◐</span>
  <div class="txt"><b>\${esc(title)}</b>\${detail?'<small>'+esc(detail)+'</small>':''}</div>
  \${lever?\`<button class="lever-btn" data-cmd="\${esc(lever)}">▶ \${esc(lever)}</button>\`:''}
</div>\`;
  }

  // results header + filter bar (node rows are rendered by renderNodeList)
  const tierLabel = h.tier==='embed' ? 'via graph traversal — not vector similarity' : 'via lexical + graph traversal';
  html += \`<div class="results-hdr">
  <span>\${f.nodes} node\${f.nodes!==1?'s':''} · <span id="snip-count">loading…</span> with snippets</span>
  <span>\${esc(tierLabel)}</span>
</div>\`;
  html += \`<div class="filterbar" id="filterbar">
  <div class="fb-seg" id="fb-topk">
    <span class="fb-lbl">show</span>
    <button data-k="10">10</button>
    <button data-k="25">25</button>
    <button data-k="50">50</button>
    <button data-k="0" class="on">all</button>
  </div>
  <label class="fb-toggle"><input type="checkbox" id="fb-snip"> snippets only</label>
  <span class="fb-count" id="fb-count"></span>
</div>\`;
  html += '<div id="node-list"></div>';

  scroll.innerHTML = html;
  legendBox.style.display = 'block';

  // wire lever buttons (static across filter changes / snippet enrichment)
  scroll.querySelectorAll('.lever-btn').forEach(el => {
    el.addEventListener('click', () => {
      const cmd = el.getAttribute('data-cmd');
      if(cmd) vscode.postMessage({ command:'runLever', raw:cmd });
    });
  });

  // wire top-k segmented control
  scroll.querySelectorAll('#fb-topk button').forEach(el => {
    el.addEventListener('click', () => {
      state.topK = parseInt(el.getAttribute('data-k'),10) || 0;
      scroll.querySelectorAll('#fb-topk button').forEach(b => b.classList.toggle('on', b===el));
      renderNodeList();
    });
  });
  // wire snippets-only toggle
  const snipToggle = document.getElementById('fb-snip');
  if(snipToggle) snipToggle.addEventListener('change', () => {
    state.snippetsOnly = snipToggle.checked;
    renderNodeList();
  });

  renderNodeList();
}

function nodeHasSnippet(n){
  return !!(state.snippets[n.sig] || n.snippet) || state.pinned.has(n.sig);
}

// Render the node rows from the client model, applying the top-k and
// snippets-only filters. Called on first render, on filter change, and when
// Phase-2 snippets arrive — expand state is preserved via state.openSigs.
function renderNodeList(){
  const listEl = document.getElementById('node-list');
  if(!listEl) return;

  const grandTotal = state.nodes.length;
  let nodes = state.snippetsOnly ? state.nodes.filter(nodeHasSnippet) : state.nodes;
  if(state.topK>0) nodes = nodes.slice(0, state.topK);

  // default-open the first visible node once (non-abstain)
  if(state.openSigs.size===0 && !state.isAbstain && nodes.length>0){
    state.openSigs.add(nodes[0].sig);
  }

  let html = '';
  if(nodes.length===0){
    html = state.snippetsOnly
      ? '<p class="empty">No snippets loaded yet — they arrive a moment after the results.</p>'
      : '<p class="empty">No nodes.</p>';
  }
  let lastSection = null;
  for(let i=0;i<nodes.length;i++){
    const n = nodes[i];
    // RFC-022 §14: emit a labelled header when the match-source section changes.
    // Absent on flag-off/flat results, so the existing single list is unchanged.
    if(n.matchSource && n.matchSource !== lastSection){
      lastSection = n.matchSource;
      html += sectionHead(n.matchSource);
    }
    const sig = n.sig;
    const mode = state.mode[sig] || 'auto';
    // undefined = never fetched, '' = fetched but no body, string = snippet.
    const loaded = state.snippets[sig] !== undefined ? state.snippets[sig] : n.snippet;
    const has = !!loaded;
    const pinned = state.pinned.has(sig);
    const dimClass = state.isAbstain ? 'dim' : '';
    const openClass = state.openSigs.has(sig) ? 'open' : '';
    const pathLine = n.line ? \`\${esc(n.path)}:\${n.line}\` : esc(n.path);
    const scorePct = n.score!=null ? Math.round(n.score*100) : 0;
    const scoreHtml = n.score!=null
      ? \`<div class="score"><span class="num">\${n.score.toFixed(2)}</span><div class="sbar"><div class="sfill" style="width:\${scorePct}%"></div></div></div>\`
      : '';
    const snipBadge = has ? '<span class="snip-badge" title="code snippet available">◇</span>' : '';
    const pinBadge  = pinned ? '<span class="pin-badge" title="added to AI context">📌</span>' : '';
    // Per-node controls (shown when expanded): mode selector + add-to-context toggle.
    const modeSeg = '<div class="mode-seg">' +
      ['auto','full','skeleton'].map(m =>
        \`<button data-mode="\${m}" class="\${m===mode?'on':''}" title="\${m==='full'?'complete definition':m==='skeleton'?'AST summary only':'auto (raw or skeleton)'}">\${m}</button>\`
      ).join('') + '</div>';
    const pinBtn = \`<button class="pin-btn \${pinned?'on':''}">\${pinned?'✓ in AI context':'＋ add to AI context'}</button>\`;
    const bodyInner = loaded===undefined
      ? '<div class="snip-loading">loading…</div>'
      : loaded===''
        ? '<div class="snip-loading">no snippet for this mode — try another mode or open the file.</div>'
        : \`<pre>\${esc(loaded)}</pre>\`;
    const expanded = \`<div class="snippet">
    <div class="snip-ctl">\${modeSeg}\${pinBtn}</div>
    \${bodyInner}
    <div class="cap">\${mode} · via get_snippets</div>
  </div>\`;
    html += \`<div class="node \${openClass} \${dimClass}" data-sig="\${esc(sig)}">
  <div class="row">
    <span class="kind-ico">\${kindIco(n.kind)}</span>
    <div class="main">
      <div class="sig">\${esc(sig)}</div>
      <span class="loc" data-path="\${esc(n.path)}" data-line="\${n.line??''}">\${pathLine}</span>
    </div>
    \${pinBadge}\${snipBadge}
    \${n.via ? \`<span class="via \${viaClass(n.via)}" title="\${esc(n.via)}">\${esc(shortVia(n.via))}</span>\` : ''}
    \${scoreHtml}
    <span class="chev">▸</span>
  </div>
  \${expanded}
</div>\`;
  }
  listEl.innerHTML = html;

  const countEl = document.getElementById('fb-count');
  if(countEl) countEl.textContent = 'showing '+nodes.length+' of '+grandTotal;

  // wire node row toggle — every node is expandable; fetch on first open if empty
  listEl.querySelectorAll('.node').forEach(el => {
    const sig = el.getAttribute('data-sig');
    el.querySelector('.row')?.addEventListener('click', (e) => {
      const t = e.target;
      if(t instanceof Element && t.classList.contains('loc')) return; // loc link handles itself
      const nowOpen = el.classList.toggle('open');
      if(nowOpen){ state.openSigs.add(sig); ensureSnippet(sig); } else { state.openSigs.delete(sig); }
    });

    // mode selector → refetch in the chosen representation
    el.querySelectorAll('.mode-seg button').forEach(b => {
      b.addEventListener('click', ev => {
        ev.stopPropagation();
        const m = b.getAttribute('data-mode');
        if(state.mode[sig]===m && state.snippets[sig]) return;
        state.mode[sig] = m;
        delete state.snippets[sig];   // force reload — different mode, different body
        state.openSigs.add(sig);
        renderNodeList();
        vscode.postMessage({ command:'fetchSnippet', sig, mode:m });
      });
    });

    // add-to-context toggle
    el.querySelector('.pin-btn')?.addEventListener('click', ev => {
      ev.stopPropagation();
      if(state.pinned.has(sig)) state.pinned.delete(sig);
      else { state.pinned.add(sig); state.openSigs.add(sig); ensureSnippet(sig); }
      renderNodeList();
      updateCopyLabel();
    });
  });

  // wire loc clicks → open file
  listEl.querySelectorAll('.loc').forEach(el => {
    el.addEventListener('click', e => {
      e.stopPropagation();
      const path = el.getAttribute('data-path');
      const line = el.getAttribute('data-line');
      if(path) vscode.postMessage({ command:'open', path, line:line||undefined });
    });
  });
}

// Request a snippet for a node if we don't already have one loaded, in that
// node's currently-selected mode (default auto).
function ensureSnippet(sig){
  if(state.snippets[sig]!==undefined) return;   // already fetched (even if empty)
  vscode.postMessage({ command:'fetchSnippet', sig, mode: state.mode[sig] || 'auto' });
}

// Reflect the number of pinned nodes on the copy button.
function updateCopyLabel(){
  const n = state.pinned.size;
  copyBtn.textContent = n>0 ? \`⧉ Copy context +\${n} pinned\` : '⧉ Copy context';
}

window.addEventListener('message', event => {
  const msg = event.data;

  if(msg.command==='enrichSnippets'){
    // Merge Phase-2 batch snippets under any on-demand fetches the user already made.
    state.snippets = Object.assign({}, msg.snippets || {}, state.snippets);
    renderNodeList();
    // Update "X with snippets" count now that Phase 2 is done
    const countEl = document.getElementById('snip-count');
    if(countEl) countEl.textContent = String(Object.keys(msg.snippets||{}).length);
    return;
  }

  if(msg.command==='snippetFetched'){
    // On-demand single-symbol result. Store even when empty ('') so we don't
    // re-request, then re-render (open state preserved via openSigs).
    if(state.mode[msg.sig]===undefined && msg.mode) state.mode[msg.sig] = msg.mode;
    state.snippets[msg.sig] = msg.snippet || '';
    renderNodeList();
    return;
  }

  if(msg.command==='seed'){
    if(msg.seed&&!qEl.value) qEl.value = msg.seed;
    if(msg.seed){
      document.getElementById('seeded').textContent = 'seeded · '+msg.seed;
      document.getElementById('seeded').style.display = '';
    }
    if(msg.sourceFileTok) sourceFileTok = msg.sourceFileTok;
    return;
  }

  if(msg.command==='loading'){
    scroll.innerHTML = '<p class="loading">Querying the graph…</p>';
    return;
  }

  if(msg.command==='render'){
    renderParsed(msg.data, msg.rawText||'');
  } else if(msg.command==='renderRaw'){
    lastRawText = msg.raw||'';
    const raw = msg.raw||'';
    // Detect non-standard abstain format from seed.rs abstain_message():
    // no [index commit:] header → falls through to raw fallback in the parser.
    const isAbstainRaw = raw.startsWith('[note: no grounded match');
    if(isAbstainRaw){
      signalsEl.innerHTML = '<span class="pill conf-none"><span class="dot"></span>confidence: none</span>';
      signalsEl.style.display = 'flex';
      tokenbar.style.display = 'none';
      copyBtn.disabled = false;
      scroll.innerHTML = \`<div class="abstain">
  <h3>⚠ No confident match</h3>
  <p>Travsr couldn't resolve your query to a graph anchor with confidence. Rather than return a plausible-looking but unrelated set of nodes, it abstained.</p>
  <p class="hint">Try a concrete symbol — e.g. <code>PaymentService.charge</code> or "retry logic for failed charges". The resolution detail is shown below.</p>
</div><pre class="raw-fallback">\${esc(raw)}</pre>\`;
      legendBox.style.display = 'none';
    } else {
      signalsEl.style.display = 'none';
      tokenbar.style.display = 'none';
      copyBtn.disabled = !lastRawText;
      scroll.innerHTML = '<pre class="raw-fallback">'+esc(raw)+'</pre>';
      legendBox.style.display = 'block';
    }
  }
});
</script>
</body>
</html>`;
}
