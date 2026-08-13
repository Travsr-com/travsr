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
    --blue: #7dd3fc;
  }
  @media (prefers-color-scheme: light) {
    :root {
      --bg: #f6f1ed; --bg-elev: #fbfaf9; --bg-input: #eee5de;
      --border: #e2d4ca;
      --fg: #1a1a1a; --fg-muted: #705f54; --fg-subtle: #8f7a6c;
      --green: #429429; --green-deep: #dbf6db; --gold: #d89a02;
      --orange: #b35500; --orange-deep: #fff7ed;
      --error: #b91c1c;
      --blue: #0369a1;
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
  /* Only the message takes the colour; time and target stay muted so the tint
     marks the line without shouting the whole row. The pill carries severity as
     shape as well as hue, so it still reads without colour vision. */
  .lvl-WARN .msg { color: var(--gold); }
  .lvl-ERROR .msg { color: var(--error); }
  .log-line:hover { background: var(--bg-input); border-radius: 3px; }

  .pill { flex: 0 0 auto; min-width: 42px; text-align: center; font-size: 9.5px;
    font-weight: 700; letter-spacing: 0.06em; padding: 1px 5px; border-radius: 3px;
    border: 1px solid transparent; text-transform: uppercase; }
  .p-INFO  { color: var(--green); border-color: var(--green); background: var(--green-deep); }
  .p-DEBUG, .p-TRACE { color: var(--fg-subtle); border-color: var(--border); opacity: .7; }
  .p-WARN  { color: var(--gold);  border-color: var(--gold);  background: var(--orange-deep); }
  .p-ERROR { color: var(--error); border-color: var(--error); }

  /* Toolbar: search on the left, severity threshold in the middle, count on the
     right, so the eye lands on the control it needs without hunting. */
  .log-bar { display: flex; align-items: center; gap: 10px; flex-wrap: wrap;
    margin: 0 0 8px; }
  .log-bar input[type=search] { flex: 1 1 220px; min-width: 160px; padding: 4px 8px;
    background: var(--bg-input); color: var(--fg); border: 1px solid var(--border);
    border-radius: 5px; font-size: 12px; font-family: inherit; }
  .log-bar input[type=search]::placeholder { color: var(--fg-subtle); }
  .chips { display: flex; gap: 4px; }
  .chip-btn { cursor: pointer; padding: 3px 9px; font-size: 11px; border-radius: 5px;
    background: var(--bg-elev); color: var(--fg-muted);
    border: 1px solid var(--border); font-family: inherit; }
  .chip-btn:hover { color: var(--fg); }
  .chip-btn.on { background: var(--green-deep); color: var(--green); border-color: var(--green); }
  .chip-n { opacity: .65; font-variant-numeric: tabular-nums; }
  .count { font-size: 11px; color: var(--fg-subtle); font-variant-numeric: tabular-nums;
    margin-left: auto; }
  .count.filtered { color: var(--gold); }
  mark { background: var(--gold); color: var(--bg); border-radius: 2px; padding: 0 1px; }

  /* Health banner: the answer to "is something wrong", before any detail. */
  .banner { display: flex; align-items: baseline; gap: 8px; flex-wrap: wrap;
    padding: 8px 12px; border-radius: 6px; margin: 14px 0 0; font-size: 12px;
    border: 1px solid transparent; }
  .banner strong { font-size: 12px; }
  .banner span { color: var(--fg-muted); }
  .banner.good { background: var(--green-deep); border-color: var(--green); color: var(--green); }
  .banner.bad  { background: var(--orange-deep); border-color: var(--orange); color: var(--orange); }
  .banner.idle { background: var(--bg-elev); border-color: var(--border); color: var(--fg-muted); }

  /* One card per problem: what broke, what it costs, what to run. */
  .diags { display: flex; flex-direction: column; gap: 8px; margin-top: 10px; }
  .diag { padding: 10px 12px; border-radius: 6px; border: 1px solid var(--border);
    background: var(--bg-elev); border-left-width: 3px; }
  .diag.error { border-left-color: var(--error); }
  .diag.warn  { border-left-color: var(--gold); }
  .diag-t { font-size: 12px; font-weight: 600; margin-bottom: 3px; }
  .diag.error .diag-t { color: var(--error); }
  .diag.warn  .diag-t { color: var(--gold); }
  .diag-h { font-size: 11.5px; color: var(--fg-muted); line-height: 1.5; }
  .diag-a { margin-top: 7px; }
  .diag-cmd { display: inline-block; padding: 3px 7px; border-radius: 4px;
    background: var(--bg-input); border: 1px solid var(--border); color: var(--fg);
    font-family: var(--vscode-editor-font-family, ui-monospace, monospace); font-size: 11px;
    user-select: all; }

  /* Subsystem tint. A categorical axis, separate from severity, so a run of
     indexer lines is findable by eye without reading any of them. Kept low
     saturation: severity is the signal, this is only grouping. */
  .log-line[data-tg="daemon"]      .tg { color: var(--green); opacity: .75; }
  .log-line[data-tg="store"]       .tg { color: var(--gold);  opacity: .7; }
  .log-line[data-tg="indexer"]     .tg { color: var(--orange); opacity: .8; }
  .log-line[data-tg="plugin-host"] .tg { color: #a78bfa; opacity: .85; }
  .log-line[data-tg="mcp"]         .tg { color: #7dd3fc; opacity: .85; }

  /* Second toolbar row: the modes that mirror the CLI flags. */
  .log-bar.modes { margin: -2px 0 8px; font-size: 11px; color: var(--fg-subtle); }
  .sel { display: inline-flex; align-items: center; gap: 4px; }
  .sel select { background: var(--bg-input); color: var(--fg); border: 1px solid var(--border);
    border-radius: 4px; padding: 2px 4px; font-size: 11px; font-family: inherit; }
  .tog { display: inline-flex; align-items: center; gap: 4px; cursor: pointer; }
  .tog input { margin: 0; }

  /* JSON mode makes each row expandable rather than replacing the list with a
     wall of objects. The column line is already the summary, so collapsed costs
     nothing and 200 entries stay scannable; the stored line is one click away.
     (No backticks in this stylesheet: it is a template literal.) */
  .caret { display: none; flex: 0 0 10px; color: var(--fg-subtle); user-select: none; }
  .caret::before { content: "\u25B8"; }
  .log.json-mode .caret { display: inline-block; }
  .log.json-mode .log-line { cursor: pointer; padding: 3px 0; }
  .log.json-mode .log-line.open .caret::before { content: "\u25BE"; }
  .log.json-mode .log-line.open { background: var(--bg-input); border-radius: 3px; }
  .jsonline { display: none; }
  .log.json-mode .log-line.open .jsonline { display: block; white-space: pre;
    overflow-x: auto; line-height: 1.5; margin: 6px 0 4px 18px;
    padding-left: 10px; border-left: 2px solid var(--border); }
  .log.json-mode .log-line.open { display: block; }
  .log.json-mode .log-line.open > span:not(.jsonline) { display: inline; }

  .j-k { color: var(--blue); }
  .j-s { color: var(--green); }
  .j-n { color: var(--gold); }
  .j-b { color: var(--orange); }
  .j-p { color: var(--fg-subtle); }
  .j-raw { color: var(--fg-muted); }
  :focus-visible { outline: 2px solid var(--green); outline-offset: 1px; }
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

/**
 * Pretty-print one stored log line the way a JSON viewer does: indented, one
 * field per line, each kind of value its own colour.
 *
 * Raw JSON in a panel is a wall. Every line is the same width and the same
 * weight, so the eye has nothing to land on and the format buys nothing over
 * the column view.
 *
 * Built by walking the parsed value rather than running regexes over the text.
 * Chained replacements cannot work here: a pass for numbers matches the digits
 * inside an already-marked timestamp string, and the spans nest. Walking the
 * value knows what each token is.
 *
 * A line that does not parse is shown as itself, escaped. Rotations from before
 * the log was JSON are still on disk.
 */
export function highlightJson(line: string): string {
  let parsed: unknown;
  try {
    parsed = JSON.parse(line);
  } catch {
    return `<span class="j-raw">${esc(line)}</span>`;
  }
  return renderJsonValue(parsed, 0);
}

function renderJsonValue(v: unknown, depth: number): string {
  const pad = (n: number): string => "  ".repeat(n);
  if (v === null) return `<span class="j-b">null</span>`;
  if (typeof v === "boolean") return `<span class="j-b">${v}</span>`;
  if (typeof v === "number") return `<span class="j-n">${v}</span>`;
  if (typeof v === "string") return `<span class="j-s">"${esc(v)}"</span>`;
  if (Array.isArray(v)) {
    if (v.length === 0) return `<span class="j-p">[]</span>`;
    const items = v
      .map((x) => `${pad(depth + 1)}${renderJsonValue(x, depth + 1)}`)
      .join(`<span class="j-p">,</span>\n`);
    return `<span class="j-p">[</span>\n${items}\n${pad(depth)}<span class="j-p">]</span>`;
  }
  const entries = Object.entries(v as Record<string, unknown>);
  if (entries.length === 0) return `<span class="j-p">{}</span>`;
  const rows = entries
    .map(
      ([k, val]) =>
        `${pad(depth + 1)}<span class="j-k">"${esc(k)}"</span><span class="j-p">:</span> ` +
        renderJsonValue(val, depth + 1)
    )
    .join(`<span class="j-p">,</span>\n`);
  return `<span class="j-p">{</span>\n${rows}\n${pad(depth)}<span class="j-p">}</span>`;
}

/** One thing that is wrong, phrased for the person who has to fix it. */
export interface Diagnostic {
  severity: "error" | "warn";
  /** What is wrong, in a few words. */
  title: string;
  /** What it costs, and how to fix it. */
  hint: string;
  /** A command to run, pulled out of the hint so it can be copied. */
  command?: string;
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
  /** Raw RFC3339 stamp, so the panel can retime to UTC without a round trip. */
  iso: string;
  /** The stored line verbatim, for the JSON view. */
  raw: string;
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
export function buildStatsHtml(
  stats: StatsView,
  log: LogEntry[] = [],
  diags: Diagnostic[] = []
): string {
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

  // Severity threshold, the same semantics `travsr daemon logs --level` uses:
  // warn means warn and above, not warn alone.
  const RANK: Record<string, number> = { TRACE: 0, DEBUG: 1, INFO: 2, WARN: 3, ERROR: 4 };
  const rankOf = (lvl: string): number => RANK[lvl] ?? 2;
  const counts = { all: log.length, info: 0, warn: 0, error: 0 };
  for (const e of log) {
    const r = rankOf(e.level);
    if (r >= 2) counts.info += 1;
    if (r >= 3) counts.warn += 1;
    if (r >= 4) counts.error += 1;
  }

  const chip = (id: string, label: string, n: number): string =>
    `<button class="chip-btn" data-level="${id}" onclick="setLevel('${id}',this)">` +
    `${esc(label)} <span class="chip-n">${n}</span></button>`;

  const logRows = log.length
    ? log
        .slice(-200)
        .reverse()
        .map(
          (e) =>
            `<div class="log-line lvl-${esc(e.level)}" data-rank="${rankOf(e.level)}"` +
            ` data-iso="${esc(e.iso)}" data-json="${esc(e.raw)}" data-tg="${esc(e.target)}">` +
            `<span class="caret" aria-hidden="true"></span>` +
            `<span class="mono muted t" data-local="${esc(e.time)}">${esc(e.time)}</span>` +
            `<span class="pill p-${esc(e.level)}">${esc(e.level || "\u2014")}</span>` +
            `<span class="mono muted tg">${esc(e.target)}</span>` +
            `<span class="msg" data-raw="${esc(e.message)}">${esc(e.message)}</span>` +
            `<span class="mono muted detail" data-raw="${esc(e.detail)}">${esc(e.detail)}</span>` +
            `<span class="jsonline mono">${highlightJson(e.raw)}</span></div>`
        )
        .join("\n")
    : `<div class="empty">No daemon log yet. Run <span class="mono">travsr daemon start</span>.</div>`;

  // Health reads before anything else, because "is something wrong" is the
  // question the panel is opened with. All clear is its own state, not an empty
  // list: nothing found and nothing checked look identical otherwise.
  const errs = diags.filter((d) => d.severity === "error").length;
  const warns = diags.length - errs;
  const health = diags.length
    ? `<div class="banner bad">
<strong>${errs ? `${errs} error${errs > 1 ? "s" : ""}` : ""}${errs && warns ? " &middot; " : ""}${warns ? `${warns} warning${warns > 1 ? "s" : ""}` : ""}</strong>
<span>affecting what the graph can answer</span></div>`
    : log.length
      ? `<div class="banner good"><strong>All clear</strong><span>no analyzer or index problems reported</span></div>`
      : `<div class="banner idle"><strong>Daemon not running</strong>
<span>start it to keep the graph fresh: <span class="mono">travsr daemon start</span></span></div>`;

  const diagCards = diags.length
    ? `<div class="diags">` +
      diags
        .map(
          (d) =>
            `<div class="diag ${d.severity}">` +
            `<div class="diag-t">${d.severity === "error" ? "&#10007;" : "&#9888;"} ${esc(d.title)}</div>` +
            `<div class="diag-h">${esc(d.hint)}</div>` +
            (d.command
              ? `<div class="diag-a"><code class="diag-cmd">${esc(d.command)}</code></div>`
              : "") +
            `</div>`
        )
        .join("\n") +
      `</div>`
    : "";

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

${health}
${diagCards}

<h2 style="margin-top:28px">Recent activity</h2>
<p class="sub">Lifecycle events from the daemon, newest first.</p>
<table class="activity"><tbody>
${activityRows}
</tbody></table>

<h2 style="margin-top:28px">Daemon log</h2>
<div class="log-bar">
  <input id="logSearch" type="search" placeholder="Filter lines\u2026" oninput="filterLog()"
         aria-label="Filter log lines">
  <div class="chips" role="group" aria-label="Minimum severity">
    ${chip("all", "All", counts.all)}
    ${chip("info", "Info+", counts.info)}
    ${chip("warn", "Warn+", counts.warn)}
    ${chip("error", "Error", counts.error)}
  </div>
  <span class="count" id="logCount">${log.length} lines</span>
</div>
<div class="log-bar modes">
  <label class="sel">Lines
    <select id="logLines" onchange="filterLog()">
      <option value="100">100</option>
      <option value="200" selected>200</option>
      <option value="500">500</option>
    </select>
  </label>
  <label class="sel">Since
    <select id="logSince" onchange="filterLog()">
      <option value="0" selected>All</option>
      <option value="5">5m</option>
      <option value="60">1h</option>
      <option value="1440">24h</option>
    </select>
  </label>
  <label class="tog"><input type="checkbox" id="logUtc" onchange="filterLog()"> UTC</label>
  <label class="tog"><input type="checkbox" id="logJson" onchange="filterLog()"> JSON</label>
  <label class="tog"><input type="checkbox" id="logFollow" onchange="toggleFollow(this)"> Follow</label>
</div>
<div class="log" id="logBox" onclick="toggleRow(event)">
<div class="empty" id="logEmpty" style="display:none">No lines match this filter.</div>
${logRows}
</div>`;

  const script = `
function doRefresh(btn){ setLoading(btn,true,'Refresh'); vscode.postMessage({command:'refresh'}); }

var minRank = 0;
function setLevel(id, btn) {
  minRank = { all: 0, info: 2, warn: 3, error: 4 }[id];
  var all = document.querySelectorAll('.chip-btn');
  for (var i = 0; i < all.length; i++) all[i].classList.remove('on');
  btn.classList.add('on');
  filterLog();
}

// Rebuilt from text nodes rather than innerHTML. The query is user input and
// this runs on every keystroke, so there is no point at which it could be
// parsed as markup.
function mark(el, q) {
  if (!el) return;
  var raw = el.getAttribute('data-raw') || '';
  el.textContent = '';
  if (!q) { el.textContent = raw; return; }
  var lower = raw.toLowerCase(), i = 0, at;
  while ((at = lower.indexOf(q, i)) !== -1) {
    if (at > i) el.appendChild(document.createTextNode(raw.slice(i, at)));
    var m = document.createElement('mark');
    m.textContent = raw.slice(at, at + q.length);
    el.appendChild(m);
    i = at + q.length;
  }
  el.appendChild(document.createTextNode(raw.slice(i)));
}

var followTimer = null;
function toggleFollow(cb) {
  if (followTimer) { clearInterval(followTimer); followTimer = null; }
  // Re-reads the file on a timer, which is what --follow does. 3s is slower
  // than a tail and fast enough for a panel you glance at.
  if (cb.checked) followTimer = setInterval(function () {
    vscode.postMessage({ command: 'refresh' });
  }, 3000);
}

function toggleRow(ev) {
  var box = document.getElementById('logBox');
  if (!box || !box.classList.contains('json-mode')) return;
  var row = ev.target.closest('.log-line');
  if (row) row.classList.toggle('open');
}

function filterLog() {
  var box = document.getElementById('logBox');
  if (!box) return;
  var q = (document.getElementById('logSearch').value || '').toLowerCase();
  var maxLines = Number(document.getElementById('logLines').value);
  var sinceMin = Number(document.getElementById('logSince').value);
  var utc = document.getElementById('logUtc').checked;
  var asJson = document.getElementById('logJson').checked;
  var cutoff = sinceMin ? Date.now() - sinceMin * 60000 : 0;
  box.classList.toggle('json-mode', asJson);

  var lines = box.querySelectorAll('.log-line');
  var shown = 0;
  for (var i = 0; i < lines.length; i++) {
    var line = lines[i];

    // Retime from the stored stamp rather than re-reading the file: the raw
    // value is on the row, so local and UTC are the same data rendered twice.
    var iso = line.getAttribute('data-iso');
    var tEl = line.querySelector('.t');
    if (iso && tEl) {
      var d = new Date(iso);
      tEl.textContent = utc
        ? d.toISOString().slice(11, 19)
        : (tEl.getAttribute('data-local') || '');
    }

    var okLevel = Number(line.getAttribute('data-rank')) >= minRank;
    var okAge = !cutoff || (iso && new Date(iso).getTime() >= cutoff);
    var okCount = shown < maxLines;
    var okText = !q || line.textContent.toLowerCase().indexOf(q) !== -1;
    var vis = okLevel && okAge && okText && okCount;
    line.style.display = vis ? '' : 'none';
    if (vis) {
      shown++;
      mark(line.querySelector('.msg'), q);
      mark(line.querySelector('.detail'), q);
    }
  }
  var total = lines.length;
  var label = document.getElementById('logCount');
  // Say when a filter is hiding something. A short list with no explanation
  // reads as "the daemon logged almost nothing".
  label.textContent = shown === total ? total + ' lines' : shown + ' of ' + total + ' lines';
  label.classList.toggle('filtered', shown !== total);

  var empty = document.getElementById('logEmpty');
  if (empty) empty.style.display = shown === 0 && total > 0 ? '' : 'none';
}

(function () {
  var first = document.querySelector('.chip-btn');
  if (first) first.classList.add('on');
})();
`;
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
