/**
 * VSCODE-247 — interactive management webviews.
 *
 * Pure HTML builders for the Synonyms editor, Repos manager, Graph Stats
 * dashboard, and Languages panel. Each builder returns a complete document via
 * `webviewShell`, styled with the canonical travsr-designer tokens and a strict
 * CSP. The builders are framework-free (no React) and pure so they can be
 * unit-tested without a real webview.
 *
 * Controllers (panel lifecycle + MCP wiring) live in commands.ts.
 */

import type { SynonymPair } from "./commands";

function esc(s: string): string {
  return String(s ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/**
 * Wrap body HTML in a full document with CSP, canonical travsr-designer tokens,
 * and the `acquireVsCodeApi()` bridge exposed as `const vscode`.
 *
 * Tokens match the designer skill exactly:
 *   bg       charcoal-800  #141414
 *   bg-elev  charcoal-700  #1a1a1a
 *   bg-input charcoal-600  #333333
 *   border   charcoal-500  #4d4d4d
 *   fg       linen-100     #f6f1ed
 *   accent   green-300     #86df86
 *   stale    orange-400    #fb923c
 *   error                  #ef4444
 */
export function webviewShell(title: string, body: string, script: string): string {
  return `<!DOCTYPE html><html lang="en"><head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
<title>${esc(title)}</title>
<style>
  :root {
    --bg: #141414; --bg-elev: #1a1a1a; --bg-input: #333333;
    --border: #4d4d4d;
    --fg: #f6f1ed; --fg-muted: #c8b7ab; --fg-subtle: #8f7a6c;
    --green: #86df86; --green-deep: #17340e; --gold: #fcd053;
    --orange: #fb923c; --orange-deep: #4f2000;
    --error: #ef4444;
  }
  @media (prefers-color-scheme: light) {
    :root {
      --bg: #f6f1ed; --bg-elev: #fbfaf9; --bg-input: #eee5de;
      --border: #e2d4ca;
      --fg: #1a1a1a; --fg-muted: #705f54; --fg-subtle: #8f7a6c;
      --green: #429429; --green-deep: #dbf6db; --gold: #d89a02;
      --orange: #b35500; --orange-deep: #fff7ed;
      --error: #b91c1c;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    * { animation-duration: 0.01ms !important; transition-duration: 0.01ms !important; }
  }
  * { box-sizing: border-box; }
  body { font-family: var(--vscode-font-family,'Segoe UI',system-ui,sans-serif);
    background: var(--bg); color: var(--fg); margin: 0; padding: 16px; font-size: 13px; }
  h2 { margin: 0 0 4px; font-size: 15px; display: flex; align-items: center; gap: 8px; }
  .sub { color: var(--fg-subtle); margin: 0 0 16px; font-size: 12px; }
  .btn { background: var(--bg-input); color: var(--fg); border: 1px solid var(--border);
    border-radius: 6px; padding: 5px 10px; cursor: pointer; font-size: 12px; transition: background 120ms; }
  .btn:hover:not(:disabled) { background: var(--bg-elev); border-color: var(--fg-muted); }
  .btn:disabled { opacity: 0.5; cursor: not-allowed; }
  .btn.primary { background: var(--green-deep); border-color: var(--green); color: var(--green); }
  .btn.primary:hover:not(:disabled) { background: #1e4a1e; }
  .btn.danger:hover:not(:disabled) { background: var(--orange-deep); border-color: var(--orange); color: var(--orange); }
  input, select { background: var(--bg-input); color: var(--fg); border: 1px solid var(--border);
    border-radius: 6px; padding: 5px 8px; font-size: 12px; font-family: inherit; }
  input:focus, select:focus { outline: 2px solid var(--green); outline-offset: 1px; }
  table { width: 100%; border-collapse: collapse; }
  th { text-align: left; color: var(--fg-subtle); font-weight: 600; padding: 6px 8px;
    border-bottom: 1px solid var(--border); font-size: 11px; text-transform: uppercase; }
  td { padding: 6px 8px; border-bottom: 1px solid var(--border); vertical-align: middle; }
  tr:hover td { background: var(--bg-elev); }
  .mono { font-family: var(--vscode-editor-font-family, ui-monospace, monospace); font-size: 12px; }
  .muted { color: var(--fg-muted); }
  .empty { color: var(--fg-subtle); font-style: italic; padding: 12px 0; }
  .chip { display: inline-flex; align-items: center; gap: 4px; background: var(--green-deep);
    color: var(--green); border-radius: 12px; padding: 2px 6px 2px 10px; margin: 2px; font-size: 12px; }
  .chip .x { cursor: pointer; opacity: 0.7; padding: 0 2px; line-height: 1; }
  .chip .x:hover { opacity: 1; color: var(--orange); }
  .chip-area { min-height: 28px; display: flex; flex-wrap: wrap; align-items: center;
    gap: 2px; padding: 4px; background: var(--bg-elev); border: 1px solid var(--border);
    border-radius: 6px; margin-bottom: 6px; }
  .badge { border-radius: 10px; padding: 2px 8px; font-size: 11px; font-weight: 600; }
  .badge.ok { background: var(--green-deep); color: var(--green); }
  .badge.stale { background: var(--orange-deep); color: var(--orange); }
  .badge.elevated { background: #3b2000; color: var(--gold); }
  .toolbar { display: flex; gap: 8px; align-items: center; margin-bottom: 12px; flex-wrap: wrap; }
  .addrow { display: flex; gap: 8px; margin: 14px 0; align-items: flex-start; flex-wrap: wrap; }
  .cards { display: flex; gap: 12px; flex-wrap: wrap; }
  .card { background: var(--bg-elev); border: 1px solid var(--border); border-radius: 10px;
    padding: 14px 18px; min-width: 120px; }
  .card .k { color: var(--fg-subtle); font-size: 11px; text-transform: uppercase; }
  .card .v { font-size: 22px; color: var(--green); margin-top: 4px; font-variant-numeric: tabular-nums; }
  .x-btn { cursor: pointer; color: var(--fg-subtle); background: none; border: none;
    padding: 2px 4px; border-radius: 4px; font-size: 13px; }
  .x-btn:hover { color: var(--orange); }
  section { margin-top: 20px; }
  section h3 { font-size: 13px; font-weight: 600; color: var(--fg-muted); text-transform: uppercase;
    letter-spacing: 0.05em; margin: 0 0 8px; }
  .lang-dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%;
    background: var(--green); margin-right: 6px; }
  /* Recent activity: one row per lifecycle event. */
  .activity { width: 100%; border-collapse: collapse; }
  .activity td { padding: 5px 8px; border-bottom: 1px solid var(--border);
    vertical-align: top; font-size: 12px; }
  .activity td:first-child { white-space: nowrap; width: 1%; }
  .activity .detail { font-size: 11px; }
  /* Whole row tinted, not just the label: a warning should read as one thing. */
  tr.lvl-WARN td { color: var(--gold); }
  tr.lvl-ERROR td { color: var(--error); }

  /* Daemon log: fixed height so the panel stays scannable, scrolls on both axes
     so a long line never widens the page. */
  .log { max-height: 340px; overflow: auto; background: var(--bg-elev);
    border: 1px solid var(--border); border-radius: 6px; padding: 8px; }
  .log-line { display: flex; gap: 8px; align-items: baseline; padding: 2px 0;
    font-family: var(--vscode-editor-font-family, ui-monospace, monospace);
    font-size: 11.5px; white-space: pre; }
  .log-line .t { flex: 0 0 auto; }
  .log-line .lv { flex: 0 0 44px; font-weight: 600; }
  .log-line .tg { flex: 0 0 88px; }
  .log-line .msg { flex: 1 1 auto; color: var(--fg); }
  .log-line .detail { flex: 0 0 auto; }
  /* Only level and message take the colour; time and target stay muted so the
     tint marks the line without shouting the whole row. */
  .lvl-WARN .lv, .lvl-WARN .msg { color: var(--gold); }
  .lvl-ERROR .lv, .lvl-ERROR .msg { color: var(--error); }
  .badge.dim { background: var(--bg-elev); color: var(--fg-subtle); }
  .consent { margin-top: 6px; }
  .consent summary { cursor: pointer; font-size: 12px; color: var(--gold); }
  .consent-body { display: flex; flex-direction: column; gap: 6px; margin-top: 8px; padding: 10px;
    background: var(--bg-elev); border: 1px solid var(--border); border-radius: 6px; }
  .consent-body label { font-size: 11px; color: var(--fg-subtle); }
  .consent-body input { width: 100%; }
  .not-here summary { cursor: pointer; font-size: 11px; color: var(--fg-subtle); list-style: none; }
  .not-here summary::after { content: ' ↓'; }
  .not-here[open] summary::after { content: ' ↑'; }
  .not-here-body { margin-top: 6px; }
  .spinner { display: inline-block; animation: spin 0.8s linear infinite; }
  @keyframes spin { to { transform: rotate(360deg); } }
  #status-bar { display:none; align-items:center; gap:6px; padding:7px 12px;
    border-radius:6px; margin-bottom:12px; font-size:12px; }
  #status-bar.s-loading { display:flex; background:var(--bg-elev); color:var(--fg-muted); border:1px solid var(--border); }
  #status-bar.s-error   { display:flex; background:#2d0000; color:var(--error); border:1px solid var(--error); }
  #status-bar.s-ok      { display:flex; background:var(--green-deep); color:var(--green); border:1px solid var(--green); }
</style></head><body>
<div id="status-bar"></div>
${body}
<script>
const vscode = acquireVsCodeApi();
function setLoading(btn, loading, label) {
  btn.disabled = loading;
  btn.innerHTML = loading ? '<span class="spinner">⟳</span> …' : label;
  if (loading) btn.dataset.label = label;
}
window.addEventListener('message', function(ev) {
  const d = ev.data;
  if (!d) return;
  if (d.command === 'unlockButtons') {
    document.querySelectorAll('button.btn').forEach(function(b) {
      b.disabled = false;
      if (b.dataset.label) { b.innerHTML = b.dataset.label; delete b.dataset.label; }
    });
  } else if (d.command === 'setStatus') {
    const bar = document.getElementById('status-bar');
    if (!bar) return;
    if (!d.text) { bar.className = ''; bar.textContent = ''; return; }
    bar.className = 's-' + (d.type || 'loading');
    const t = document.createTextNode(d.text);
    bar.textContent = '';
    if (d.type !== 'error' && d.type !== 'ok') {
      const s = document.createElement('span');
      s.className = 'spinner'; s.textContent = '⟳';
      bar.appendChild(s);
    }
    bar.appendChild(t);
  }
});
${script}
</script>
</body></html>`;
}

/** Instant loading placeholder — shown before the first async render completes. */
export function buildPanelLoadingHtml(title: string): string {
  return webviewShell(
    title,
    `<p class="sub" style="margin-top:32px;text-align:center">
       <span class="spinner" style="font-size:18px">⟳</span>&nbsp; Loading…
     </p>`,
    ""
  );
}

/** Synonyms editor: grouped by term, alias chips with remove, a staged multi-chip add row, reset. */
export function buildSynonymsHtml(pairs: SynonymPair[]): string {
  const byTerm = new Map<string, string[]>();
  for (const p of pairs) {
    const list = byTerm.get(p.term) ?? [];
    list.push(p.alias);
    byTerm.set(p.term, list);
  }
  const terms = [...byTerm.keys()].sort();
  const rows = terms
    .map((term) => {
      const chips = (byTerm.get(term) ?? [])
        .map(
          (a) =>
            `<span class="chip">${esc(a)}<span class="x" title="Remove" onclick="removePair('${esc(term)}','${esc(a)}')">✕</span></span>`
        )
        .join("");
      return `<tr><td class="mono">${esc(term)}</td><td>${chips}</td>
<td><button class="x-btn" title="Remove all aliases for this term" onclick="removeTerm('${esc(term)}')">🗑</button></td></tr>`;
    })
    .join("\n");

  const body = `
<h2>Synonyms</h2>
<p class="sub">Query terms expanded during search. ${pairs.length} pair(s).</p>
<div class="addrow">
  <div style="display:flex;flex-direction:column;gap:6px;flex:1;min-width:200px">
    <input id="term" placeholder="term" style="width:100%">
    <div class="chip-area" id="staged" aria-label="Staged aliases"></div>
    <div style="display:flex;gap:6px;align-items:center">
      <input id="alias" placeholder="alias — press Enter to stage" style="flex:1">
      <button class="btn primary" id="addBtn" onclick="commitAdd()">Add</button>
    </div>
    <p style="margin:2px 0;font-size:11px;color:var(--fg-subtle)">Press Enter in alias field to stage; Add commits all staged aliases.</p>
  </div>
  <button class="btn danger" style="align-self:flex-start" onclick="resetAll()">Reset to defaults</button>
</div>
<table><thead><tr><th>Term</th><th>Aliases</th><th></th></tr></thead>
<tbody>${rows || '<tr><td colspan="3" class="empty">No synonyms defined.</td></tr>'}</tbody></table>`;

  const script = `
let _staged = [];
function renderStaged() {
  const area = document.getElementById('staged');
  area.innerHTML = _staged.map((a,i) =>
    '<span class="chip">' + a + '<span class="x" onclick="removeStaged(' + i + ')">✕</span></span>'
  ).join('');
}
function removeStaged(i) { _staged.splice(i, 1); renderStaged(); }
function stageAlias() {
  const inp = document.getElementById('alias');
  const v = inp.value.trim();
  if (!v || _staged.includes(v)) return;
  _staged.push(v);
  renderStaged();
  inp.value = '';
}
function commitAdd() {
  const term = document.getElementById('term').value.trim();
  if (!term) return;
  if (_staged.length === 0) {
    const a = document.getElementById('alias').value.trim();
    if (!a) return;
    _staged.push(a);
    document.getElementById('alias').value = '';
  }
  const btn = document.getElementById('addBtn');
  setLoading(btn, true, 'Add');
  vscode.postMessage({command:'addBatch', term, aliases:[..._staged]});
  _staged = []; renderStaged();
}
function removePair(t,a){ vscode.postMessage({command:'removePair', term:t, alias:a}); }
function removeTerm(t){ vscode.postMessage({command:'removeTerm', term:t}); }
function resetAll(){ vscode.postMessage({command:'reset'}); }
document.getElementById('alias').addEventListener('keydown', e => { if(e.key==='Enter'){ e.preventDefault(); stageAlias(); } });`;

  return webviewShell("Travsr Synonyms", body, script);
}

/** One registry row for the repos manager. */
export interface RepoRow {
  name: string;
  path: string;
  exists: boolean;
}

/** Repos manager: table with status badges, prune-stale, and per-row remove. */
export function buildReposHtml(rows: RepoRow[]): string {
  const staleCount = rows.filter((r) => !r.exists).length;
  const tableRows = rows
    .map(
      (r) =>
        `<tr><td class="mono">${esc(r.name)}</td>
<td class="mono muted" style="max-width:260px;overflow:hidden;text-overflow:ellipsis" title="${esc(r.path)}">${esc(r.path)}</td>
<td>${r.exists ? '<span class="badge ok">active</span>' : '<span class="badge stale">stale</span>'}</td>
<td><button class="x-btn" title="Remove from registry" onclick="removeRepo(this,'${esc(r.name)}')">✕</button></td></tr>`
    )
    .join("\n");

  const body = `
<h2>Registered repos</h2>
<p class="sub">${rows.length} repo(s) in ~/.travsr/registry.json · ${staleCount} stale.</p>
<div class="toolbar">
  <button class="btn ${staleCount > 0 ? "primary" : ""}" id="pruneBtn" onclick="prune(this)">Prune stale (${staleCount})</button>
  <button class="btn" id="refreshBtn" onclick="doRefresh(this)">Refresh</button>
</div>
<table><thead><tr><th>Name</th><th>DB Path</th><th>Status</th><th></th></tr></thead>
<tbody>${tableRows || '<tr><td colspan="4" class="empty">No repos registered.</td></tr>'}</tbody></table>`;

  const script = `
function prune(btn){ setLoading(btn,true,'Prune stale (${staleCount})'); vscode.postMessage({command:'prune'}); }
function removeRepo(btn,n){ setLoading(btn,true,'✕'); vscode.postMessage({command:'remove', name:n}); }
function doRefresh(btn){ setLoading(btn,true,'Refresh'); vscode.postMessage({command:'refresh'}); }`;

  return webviewShell("Travsr Repos", body, script);
}

/** One daemon log line, as the stats panel renders it. */
export interface LogEntry {
  time: string;
  level: string;
  target: string;
  message: string;
  /** Stable machine key, present on lifecycle events only. */
  event?: string;
  /** Remaining structured fields, already flattened to `k=v`. */
  detail: string;
}

/**
 * Human labels for the stable event keys the daemon emits.
 *
 * Keyed on `event`, never on the message, which is the point of the keys: a
 * message can be reworded for clarity without breaking anything built on it.
 * An event with no entry here is simply not lifecycle, so it does not belong in
 * the activity feed.
 */
const EVENT_LABELS: Record<string, string> = {
  "daemon.session.start": "Daemon started",
  "daemon.ready": "Daemon ready",
  "daemon.socket.bound": "Control socket bound",
  "daemon.session.stop": "Daemon stopped",
  "head.drift.detected": "HEAD moved, reconciling",
  "head.reconcile.complete": "Reindexed after HEAD moved",
  "head.reconcile.pruned": "Pruned deleted files",
  "phase_b.start": "Semantic indexing started",
  "phase_b.indexed": "Semantic indexing complete",
  "phase_b.complete": "Semantic refresh complete",
  "kcore.updated": "Graph centrality updated",
  "embed.text.updated": "Embedding text updated",
  "embed.text.fts_backfill": "Search index backfilled",
  "store.fts_words.backfill": "Word index updated",
  "lsif.spawn": "Analyzer started",
  "lsif.complete": "Analyzer finished",
  "query.failed": "Query failed",
};

/** Stats for the dashboard card. Fields are pre-formatted strings. */
export interface StatsView {
  nodes: string;
  edges: string;
  schemaVersion: string;
  dbSize: string;
  lastIndexed: string;
}

/** Graph stats dashboard: metric cards, recent activity, and the log tail. */
export function buildStatsHtml(stats: StatsView, log: LogEntry[] = []): string {
  const card = (k: string, v: string): string =>
    `<div class="card"><div class="k">${esc(k)}</div><div class="v">${esc(v)}</div></div>`;

  // The feed is lifecycle, not traffic. `query.served` fires on every query and
  // would bury a Phase B failure under a hundred cache hits, so only events with
  // a label reach it; everything else stays in the stream below.
  const labelled = log.filter(
    (e) => e.event !== undefined && EVENT_LABELS[e.event] !== undefined
  );

  // Collapse consecutive runs of the same event. The embed tick fires on every
  // reindex, so without this four `embed.text.updated` rows push a Phase B
  // failure off the bottom of an eight-row feed. Runs are collapsed rather than
  // deduplicated outright, so a repeated failure still reads as repeated.
  const runs: Array<{ entry: LogEntry; count: number }> = [];
  for (const e of labelled) {
    const last = runs[runs.length - 1];
    if (last && last.entry.event === e.event) {
      last.entry = e; // keep the newest of the run
      last.count += 1;
    } else {
      runs.push({ entry: e, count: 1 });
    }
  }
  const activity = runs.slice(-8).reverse();

  const activityRows = activity.length
    ? activity
        .map(
          ({ entry: e, count }) =>
            `<tr class="lvl-${esc(e.level)}">
<td class="mono muted">${esc(e.time)}</td>
<td>${esc(EVENT_LABELS[e.event as string])}${count > 1 ? ` <span class="muted">&times;${count}</span>` : ""}</td>
<td class="mono muted detail">${esc(e.detail)}</td></tr>`
        )
        .join("\n")
    : `<tr><td colspan="3" class="empty">No lifecycle events yet. Start the daemon to see activity.</td></tr>`;

  const logRows = log.length
    ? log
        .slice(-200)
        .reverse()
        .map(
          (e) =>
            `<div class="log-line lvl-${esc(e.level)}"><span class="mono muted t">${esc(e.time)}</span>` +
            `<span class="lv">${esc(e.level)}</span>` +
            `<span class="mono muted tg">${esc(e.target)}</span>` +
            `<span class="msg">${esc(e.message)}</span>` +
            `<span class="mono muted detail">${esc(e.detail)}</span></div>`
        )
        .join("\n")
    : `<div class="empty">No daemon log yet.</div>`;

  const body = `
<h2>Graph stats</h2>
<p class="sub">Live metrics for the indexed graph.</p>
<div class="cards">
  ${card("Nodes", stats.nodes)}
  ${card("Edges", stats.edges)}
  ${card("Schema", stats.schemaVersion)}
  ${card("DB size", stats.dbSize)}
  ${card("Last indexed", stats.lastIndexed)}
</div>
<div class="toolbar" style="margin-top:16px">
  <button class="btn" id="refreshBtn" onclick="doRefresh(this)">Refresh</button>
</div>

<h2 style="margin-top:28px">Recent activity</h2>
<p class="sub">Lifecycle events from the daemon, newest first.</p>
<table class="activity"><tbody>
${activityRows}
</tbody></table>

<h2 style="margin-top:28px">Daemon log</h2>
<p class="sub">Last ${log.length} lines, newest first.</p>
<div class="log">
${logRows}
</div>`;

  const script = `function doRefresh(btn){ setLoading(btn,true,'Refresh'); vscode.postMessage({command:'refresh'}); }`;
  return webviewShell("Travsr Stats", body, script);
}

/** A per-language node count from the graph. */
export interface LangCount {
  language: string;
  count: number;
}

/** An available language tool from `travsr lang list --json`. */
export interface LangInfo {
  language: string;
  package: string;
  sandbox: "Standard" | "Elevated";
  installed: boolean;
  registered: boolean;
  builtin: boolean;
  needsApproval: boolean;
  scipInstallType: "GithubBinary" | "Command" | "Manual";
  installHint: string;
  underlyingToolHint: string;
  elevatedHosts: string[];
}

/** Languages panel: indexed section + available section with install/approve actions. */
export function buildLanguagesHtml(indexed: LangCount[], available: LangInfo[]): string {
  // ── Indexed section ──────────────────────────────────────────────────────────
  const indexedRows = indexed.length
    ? indexed
        .map(
          (l) =>
            `<tr><td><span class="lang-dot"></span><span class="mono">${esc(l.language)}</span></td>
<td style="text-align:right;color:var(--fg-muted)">${l.count.toLocaleString()}</td></tr>`
        )
        .join("\n")
    : `<tr><td colspan="2" class="empty" style="font-style:normal">No language metadata yet.&nbsp; <button class="btn primary" id="initBtn" onclick="initRepo(this)">Initialize this repo</button></td></tr>`;
  const indexedNote = indexed.length
    ? `<p style="font-size:11px;color:var(--fg-subtle);margin:4px 0 0">Node counts from structural (tree-sitter) analysis — includes test &amp; fixture files.</p>`
    : "";

  // ── Available section ────────────────────────────────────────────────────────
  const detectedLangs = new Set(indexed.map((l) => l.language));

  const availRows = available
    .map((l) => {
      const detected = detectedLangs.has(l.language);
      // Builtins bypass lang.toml registration — their semantic analysis runs whenever
      // the underlying tool is installed, regardless of the registered field.
      const active = l.builtin ? l.installed : (l.registered && l.installed);

      // Sandbox badge
      const sandboxBadge =
        l.sandbox === "Elevated"
          ? `<span class="badge elevated">Elevated</span>`
          : `<span class="badge ok">Standard</span>`;

      // Semantic column: single badge reflecting Phase B SCIP/LSIF registration state.
      // Tree-sitter structural analysis is unconditional and not shown here.
      const semCls = active ? "ok" : l.registered ? "stale" : "dim";
      const semTitle = active
        ? "Semantic analysis enabled (SCIP/LSIF active)"
        : l.registered
          ? "Registered but underlying tool not found — reinstall to fix"
          : "Semantic analysis not enabled";
      const analysisBadges = `<span class="badge ${semCls}" title="${semTitle}">${active ? "enabled" : l.registered ? "partial" : "disabled"}</span>`;

      // Raw action HTML (used directly when detected or active; wrapped otherwise)
      let rawAction: string;
      if (l.builtin) {
        rawAction = `<span class="badge ok" title="Built-in to the travsr binary — always active, cannot be disabled">Built-in</span>`;
      } else if (active) {
        rawAction = `<button class="btn danger" onclick="removeLang(this,'${esc(l.language)}')">Disable</button>`;
      } else if (l.needsApproval) {
        rawAction = `<details class="consent">
  <summary>Grant access &amp; Install</summary>
  <div class="consent-body">
    <label>Approver GitHub handle</label>
    <input id="by_${esc(l.language)}" placeholder="your-github-handle">
    <label>Reason (one sentence)</label>
    <input id="reason_${esc(l.language)}" placeholder="e.g. Need Java SCIP for service indexing">
    <label>Permitted hosts (comma-separated)</label>
    <input id="hosts_${esc(l.language)}" value="${esc(l.elevatedHosts.join(","))}" placeholder="${esc(l.elevatedHosts.join(","))}">
    <button class="btn primary" onclick="approveLang(this,'${esc(l.language)}')">Grant &amp; Install</button>
  </div>
</details>`;
      } else if (l.scipInstallType === "Manual") {
        // underlyingToolHint may contain trailing prose ("https://... — description").
        // Extract only the URL token (up to the first whitespace).
        const rawHint = l.underlyingToolHint ?? "";
        const docsUrl = rawHint.startsWith("http") ? rawHint.split(/\s/)[0] : "";
        rawAction = docsUrl
          ? `<a href="${esc(docsUrl)}" style="color:var(--green);font-size:12px">Install guide ↗</a>`
          : `<span style="font-size:11px;color:var(--fg-subtle)">Manual — run:<br><code style="color:var(--fg-muted)">${esc(l.installHint)}</code></span>`;
      } else {
        rawAction = `<button class="btn primary" onclick="installLang(this,'${esc(l.language)}')">Install</button>`;
      }

      // Gate: undetected + inactive non-builtins get a disclosure instead of a direct button.
      // Builtins always show their badge directly — they're always available regardless of repo.
      const actionCell =
        !detected && !active && !l.builtin
          ? `<details class="not-here"><summary>Not in this repo</summary><div class="not-here-body">${rawAction}</div></details>`
          : rawAction;

      return `<tr>
<td><span class="mono">${esc(l.language)}</span></td>
<td class="mono muted">${esc(l.package)}</td>
<td>${sandboxBadge}</td>
<td>${analysisBadges}</td>
<td>${actionCell}</td></tr>`;
    })
    .join("\n");

  const body = `
<h2>Languages</h2>
<p class="sub">Indexed languages in this repo and available semantic analysis tools.</p>
<div class="toolbar">
  <button class="btn" id="detectBtn" onclick="detectLangs(this)">Detect &amp; install</button>
  <button class="btn" id="refreshBtn" onclick="doRefresh(this)" title="Refresh indexed counts (fast)">Refresh</button>
  <button class="btn" id="reloadBtn" onclick="reloadAvail(this)" title="Re-run travsr lang list to refresh tool status">Reload available tools</button>
</div>
<section>
  <h3>Indexed in this repo</h3>
  <table><thead><tr><th>Language</th><th style="text-align:right">Nodes</th></tr></thead>
  <tbody>${indexedRows}</tbody></table>
  ${indexedNote}
</section>
<section>
  <h3>Available tools</h3>
  <table><thead><tr><th>Language</th><th>Package</th><th>Sandbox</th><th>Semantic</th><th>Action</th></tr></thead>
  <tbody>${availRows || '<tr><td colspan="5" class="empty">No language tools found. Is the travsr binary on PATH?</td></tr>'}</tbody></table>
</section>`;

  const script = `
function installLang(btn, lang) {
  setLoading(btn, true, 'Install');
  vscode.postMessage({command:'installLang', language:lang});
}
function approveLang(btn, lang) {
  setLoading(btn, true, 'Grant & Install');
  vscode.postMessage({command:'approveLang', language:lang,
    approvedBy: (document.getElementById('by_'+lang)||{}).value||'',
    reason: (document.getElementById('reason_'+lang)||{}).value||'',
    permittedHosts: (document.getElementById('hosts_'+lang)||{}).value||''
  });
}
function removeLang(btn, lang) {
  setLoading(btn, true, 'Disable');
  vscode.postMessage({command:'removeLang', language:lang});
}
function detectLangs(btn) {
  setLoading(btn, true, 'Detect &amp; install');
  vscode.postMessage({command:'detectLangs'});
}
function doRefresh(btn) { setLoading(btn, true, 'Refresh'); vscode.postMessage({command:'refresh'}); }
function reloadAvail(btn) { setLoading(btn, true, 'Reload available tools'); vscode.postMessage({command:'reloadAvailable'}); }
function initRepo(btn) { setLoading(btn, true, btn.innerText || 'Initialize this repo'); vscode.postMessage({command:'initRepo'}); }`;

  return webviewShell("Travsr Languages", body, script);
}
