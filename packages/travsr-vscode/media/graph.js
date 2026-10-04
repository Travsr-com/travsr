/* Travsr graph webview — P1 (semantic symbol graph) + P2 (blast + peek) + FX engine.
 * Plain ES (no modules/imports). Runs inside a VS Code webview with strict CSP.
 * Depends on: cytoscape (global) and window.TRAVSR_INIT injected by graph.ts.
 */
'use strict';

// ── VS Code API ───────────────────────────────────────────────────────────────
const vscode = acquireVsCodeApi();

// ── Palette helpers (canonical travsr-designer tokens) ────────────────────────
const C = {
  fn:    '#86df86',   // green-300
  cls:   '#fcd053',   // gold-300
  file:  '#b3b3b3',   // ch-300
  iface: '#e2d4ca',   // linen-300
  vr:    '#fcd053',   // gold-300
  noise: '#4d4d4d',   // ch-500
  pkg:   '#ffb970',   // orange-300  — directory / package tiles
  err:   '#f4645a',   // red-400     — diagnostics: error
  warn:  '#fcd053',   // gold-300    — diagnostics: warning
};
// Own-property lookup with a fallback. A plain-object map must never be indexed
// directly by a graph-supplied kind: kinds like "constructor" (real, e.g. Java)
// collide with Object.prototype members, so `map[kind] || fallback` returns the
// inherited function instead of the fallback. That bogus value was handed to
// cytoscape as a shape, `nodeShapes[it]` was undefined, and drawNode threw
// ("reading 'draw' of undefined") — killing the renderer and freezing the panel.
function fromMap(map, kind, fallback) {
  return Object.prototype.hasOwnProperty.call(map, kind) ? map[kind] : fallback;
}
function nodeColor(kind) {
  return fromMap({ function: C.fn, constructor: C.fn, method: C.fn, class: C.cls, file: C.file, interface: C.iface, var: C.vr, pkg: C.pkg, ghost: '#5a5a5a' }, kind, '#8f7a6c');
}
function nodeShape(kind) {
  return fromMap({ function: 'ellipse', constructor: 'ellipse', method: 'ellipse', class: 'diamond', file: 'round-rectangle', interface: 'triangle', var: 'round-tag', pkg: 'round-rectangle', ghost: 'round-rectangle' }, kind, 'ellipse');
}
function edgeColor(kind) {
  return fromMap({ calls: 'rgba(134,223,134,0.28)', imports: 'rgba(72,72,72,0.42)', reads: 'rgba(252,208,83,0.35)' }, kind, 'rgba(72,72,72,0.42)');
}

// ── Cytoscape init ────────────────────────────────────────────────────────────
const cy = cytoscape({
  container: document.getElementById('cy'),
  elements: [],
  style: [
    { selector: 'node', style: {
        label: 'data(label)', 'font-size': '10.5px', color: '#d8d8d8',
        'font-family': 'Segoe UI, -apple-system, sans-serif',
        'text-valign': 'bottom', 'text-halign': 'center', 'text-margin-y': 6,
        'text-outline-width': 2, 'text-outline-color': '#121212',
        'text-max-width': '120px', 'text-overflow-wrap': 'whitespace',
        shape: e => nodeShape(e.data('kind')),
        'background-color': e => nodeColor(e.data('kind')),
        'background-opacity': 0.16,
        'border-width': 2, 'border-color': e => nodeColor(e.data('kind')),
        width: e => e.data('w') || 24, height: e => e.data('h') || 24,
        'min-zoomed-font-size': 8,
        'transition-property': 'background-opacity,opacity,border-color',
        'transition-duration': '.15s',
    }},
    { selector: 'node[?root]', style: {
        'background-opacity': 0.38, 'border-width': 3, 'border-color': '#ffffff',
        // The seed node is capped at the same size as any hub, so without a
        // brighter name it looks like every other busy node in the picture.
        color: '#ffffff', 'font-size': '12px', 'font-weight': 'bold',
        'text-outline-width': 3, 'z-index': 1000,
    }},
    // ── Label level of detail ────────────────────────────────────────────────
    // Cytoscape has no label collision avoidance, so a file with 157 nodes drew
    // 157 names on top of each other. Ordinary nodes carry `.nolbl` and give up
    // their label until the view is zoomed in far enough for the text to have
    // room, or the pointer is on their neighbourhood. See applyLabelLod().
    { selector: 'node.nolbl', style: { label: '' }},
    { selector: 'node.lbl-keep', style: {
        color: '#ffffff', 'text-outline-width': 3, 'z-index': 998,
    }},
    { selector: 'node[kind="var"]', style: {
        width: 42, height: 18, 'font-size': '9px', 'background-opacity': 0.22,
        'border-color': '#fcd053', 'border-width': 1.5, opacity: 0.7, display: 'none',
    }},
    { selector: 'node[kind="file"]', style: {
        width: 54, height: 30, 'font-size': '9.5px', 'border-radius': '4px',
    }},
    // ── Repo-map LOD tile styles (P3) ────────────────────────────────────────
    { selector: 'node[kind="pkg"]', style: {
        'font-size': '12px', 'font-weight': '600',
        'text-valign': 'center', 'text-halign': 'center', 'text-margin-y': 0,
        'text-outline-width': 0,
        'background-opacity': 0.11, 'border-width': 1.5,
        'border-color': C.pkg, color: C.pkg,
        shape: 'round-rectangle',
        'text-wrap': 'wrap',
        'text-max-width': e => Math.max(40, (e.data('w') || 80) - 16) + 'px',
    }},
    { selector: 'node[kind="ghost"]', style: {
        'background-opacity': 0.04, 'border-width': 1, 'border-style': 'dashed',
        'border-color': '#5a5a5a', color: '#8f7a6c', 'font-size': '10px',
        'text-valign': 'center', 'text-margin-y': 0, 'text-wrap': 'wrap',
        shape: 'round-rectangle',
        'text-max-width': e => Math.max(30, (e.data('w') || 90) - 12) + 'px',
    }},
    { selector: 'node.noise', style: {
        'border-color': '#4d4d4d', color: '#6a6a6a', 'border-style': 'dotted', 'background-opacity': 0.06,
    }},
    { selector: ':parent', style: {
        'background-color': '#172017', 'background-opacity': 0.5,
        'border-width': 1, 'border-color': '#2d3a2d', 'border-style': 'solid',
        'text-valign': 'top', 'font-size': '9px', color: '#9ab89a',
        shape: 'round-rectangle', padding: '22px',
        'text-background-color': '#121212', 'text-background-opacity': 0.78,
        'text-background-padding': '3px', 'text-outline-width': 0,
    }},
    { selector: 'node.node-search-hit', style: {
        'border-width': 4, 'border-color': '#ffffff', 'border-opacity': 1,
        'background-opacity': 0.6, 'z-index': 999,
    }},
    { selector: 'node.dimmed', style: { opacity: 0.10 }},
    { selector: 'node.softdim', style: { opacity: 0.22 }},
    { selector: 'node:selected', style: {
        'background-opacity': 0.42, 'border-width': 3, 'border-color': '#ffffff',
    }},
    // ── Live LSP diagnostics (#688) ──────────────────────────────────────────
    // Drawn as an outline rather than a border override so the kind-coloured
    // border survives: a broken function must still read as a function. Static
    // by design — this is information, not decoration, so it does not pulse and
    // is not gated on the fx toggle.
    { selector: 'node.diag-warn', style: {
        'outline-width': 2.5, 'outline-color': C.warn, 'outline-opacity': 0.85, 'outline-offset': 2,
    }},
    { selector: 'node.diag-error', style: {
        'outline-width': 3, 'outline-color': C.err, 'outline-opacity': 0.95, 'outline-offset': 2,
    }},
    { selector: 'node.hub', style: { 'border-width': 3.5, 'border-style': 'double' }},
    { selector: 'node.wave-1', style: { 'background-opacity': 0.44 }},
    { selector: 'node.wave-2', style: { 'background-opacity': 0.28 }},
    { selector: 'edge', style: {
        // unbundled-bezier: every edge always shows a visible arc even when
        // source/target are in the same column.  45px perpendicular offset at
        // the midpoint gives a clean S-bend without looking too exaggerated.
        'curve-style': 'unbundled-bezier',
        'control-point-distances': [45],
        'control-point-weights': [0.5],
        width: e => e.data('wgt') ? Math.min(5, 1 + Math.log2(e.data('wgt')) * 0.7) : 1.3,
        'line-color': e => edgeColor(e.data('kind')),
        'line-fill': 'linear-gradient',
        'line-gradient-stop-colors': e => fromMap({
          calls:   ['rgba(134,223,134,0.10)', 'rgba(134,223,134,0.58)'],
          imports: ['rgba(72,72,72,0.18)', 'rgba(100,100,100,0.52)'],
          reads:   ['rgba(252,208,83,0.10)', 'rgba(252,208,83,0.58)'],
        }, e.data('kind'), ['rgba(72,72,72,0.18)', 'rgba(100,100,100,0.52)']),
        'line-gradient-stop-positions': [0, 100],
        'target-arrow-shape': 'triangle', 'arrow-scale': 0.75,
        'target-arrow-color': e => edgeColor(e.data('kind')),
        'line-style': e => e.data('kind') === 'calls' ? (FX.on ? 'dashed' : 'solid') : 'dashed',
        'line-dash-pattern': e => e.data('kind') === 'calls' ? [9, 5] : [6, 3],
        opacity: 0.8, display: 'element',
        label: e => e.data('wgt') ? String(e.data('wgt')) : '',
        'font-size': '9px', color: '#8a8a8a',
        'text-background-color': '#121212', 'text-background-opacity': 0.9,
        'text-background-padding': '2px',
    }},
    { selector: 'edge[kind="reads"]', style: { display: 'none' }},
    { selector: 'edge.dimmed', style: { opacity: 0.06 }},
    // Blast radius classes
    { selector: 'node.blast-root', style: {
        'background-color': '#fb923c', 'background-opacity': 0.46,
        'border-width': 3, 'border-color': '#ffffff', color: '#ffd9b3',
    }},
    { selector: 'node.blast-r1', style: {
        'background-color': '#fb923c', 'border-color': '#fb923c', 'background-opacity': 0.22, color: '#ffb970',
    }},
    { selector: 'node.blast-r2', style: {
        'background-color': '#fcd053', 'border-color': '#fcd053', 'background-opacity': 0.16, color: '#fcd053',
    }},
    { selector: 'node.blast-r3', style: {
        'background-color': '#e2d4ca', 'border-color': '#c8b7ab', 'background-opacity': 0.12, color: '#c8b7ab',
    }},
    { selector: 'edge.blast-path', style: {
        'line-color': 'rgba(251,146,60,0.55)', 'target-arrow-color': '#fb923c',
        'line-style': 'dashed', 'line-dash-pattern': [10, 6], opacity: 1, width: 2,
    }},
    { selector: 'node.commit-flash', style: {
        'border-color': '#ffffff', 'border-width': 3, 'background-opacity': 0.58,
    }},
  ],
  wheelSensitivity: 0.3, minZoom: 0.08, maxZoom: 8,
  userPanningEnabled: true,
  userZoomingEnabled: true,
  autoungrabify: false,
  boxSelectionEnabled: false,
});

// Cytoscape mishandles element ids that contain whitespace, backticks, '#' or
// brackets. Synthesized external-symbol nodes carry a raw compiler signature in
// their id (e.g. a Java constructor `...Greeter#`<init>`().`), which broke edge
// rendering and froze the panel. When any id is unsafe, remap the whole set to
// compact tokens and rewrite edge endpoints through the same map so nodes and
// edges stay consistent. Display fields (label/path/line) are untouched, so node
// names and goto/peek — which key off `path`, never `id` — are unaffected.
function sanitizeGraphIds(nodes, edges) {
  const UNSAFE = /[\s`#()[\]]/;
  if (!nodes.some(n => n && n.id != null && UNSAFE.test(String(n.id)))) {
    return { nodes, edges };
  }
  const idMap = new Map();
  let counter = 0;
  const safeId = (raw) => {
    const key = String(raw);
    let mapped = idMap.get(key);
    if (mapped === undefined) { mapped = 'g' + counter++; idMap.set(key, mapped); }
    return mapped;
  };
  return {
    // Keep the original id in `realId` so exportJson can emit the true CLI node
    // id rather than the internal `g0`/`g1` cytoscape-safe handle.
    nodes: nodes.map(n => ({ ...n, id: safeId(n.id), realId: String(n.id) })),
    edges: edges.map(e => ({ ...e, source: safeId(e.source), target: safeId(e.target) })),
  };
}

// ── State ─────────────────────────────────────────────────────────────────────
let allNodes = [];       // full payload from last render
let allEdges = [];
let loadedDepth = 2;     // depth of the last MCP query
let loadedDirection = 'both';

let direction  = 'both';
let layoutName = 'flow';
let depth      = 2;
let grouping   = true;
let varsOn     = false;
let noiseOn    = true;
let edgeKinds  = { calls: true, imports: true };

let blastOn      = false;
let blastPreNodes = null; // positions/classes to restore
let currentReqId = 0;

// When a query matches multiple symbol implementations (e.g. two files both
// define fn:ContainsPrefix), _disambigRoot holds the selected root node ID to
// isolate in the canvas. null = show all roots together (default).
let _disambigRoot = null;
let peekDefLine  = 0;
let peekDefPath  = '';

let _prog = 0; // programmatic zoom guard (prevents drill logic reacting to cy.fit)
function progZoom(fn) { _prog++; fn(); setTimeout(() => _prog--, 500); }

// _spreadMul is kept at 1 (no zoom-based spreading — that caused nodes to jump
// out of the viewport). Hover-spread is used instead; see mouseover handler.
let _spreadMul = 1;
let _blastRingState = null; // { byRing, ringR } — set by enterBlast, used by spread slider

function applyBlastPositions(mul) {
  if (!_blastRingState) return;
  const { byRing, ringR } = _blastRingState;
  const m = mul || 1;
  Object.entries(byRing).forEach(([r, ids]) => {
    const R = Number(r) * ringR * m;
    ids.forEach((id, i) => {
      const n = cy.getElementById(id);
      if (!n.length) return;
      if (Number(r) === 0) return n.position({ x: 0, y: 0 });
      const a = -Math.PI / 2 + 2 * Math.PI * i / ids.length;
      n.position({ x: Math.cos(a) * R, y: Math.sin(a) * R * 0.78 });
    });
  });
}

// ── Hover-spread state ────────────────────────────────────────────────────────
// When hovering over a crowded node, nearby nodes temporarily push apart so the
// user can tell them apart and click the right one.
let _hoverSpread = null; // { nodeId: string, orig: Map<id, {x,y}> }

// ── Label normalization ───────────────────────────────────────────────────────
// The daemon sometimes returns full VName/SCIP identifiers as the label field,
// e.g. "scip:path:lang root corpus . `pkg`/newNamedVertex()". Extract the
// terminal symbol name for display.
function shortLabel(raw, fallbackId) {
  const s = String(raw == null ? '' : raw);
  // Treat 'unknown' as absent — fall through to fallback logic below.
  const meaningful = s && s !== 'unknown';
  if (meaningful && s.length <= 48) return s;
  if (meaningful) {
    // VName/SCIP: take everything after the last '/' as the symbol name.
    const slash = s.lastIndexOf('/');
    if (slash >= 0 && slash < s.length - 1) {
      const after = s.slice(slash + 1).trim();
      if (after.length > 0 && after.length <= 80) return after;
    }
    return '…' + s.slice(-40);
  }
  // Label missing/unknown — extract the terminal segment from the node ID.
  if (fallbackId) {
    const id = String(fallbackId);
    const slash = id.lastIndexOf('/');
    const seg = slash >= 0 ? id.slice(slash + 1) : id;
    if (seg && seg.length <= 80) return seg;
    return '…' + id.slice(-32);
  }
  return '';
}

// ── Noise heuristic (client-side, #316 F3 follow-up for server flag) ─────────
function isNoise(path) {
  if (!path) return false;
  const p = path.replace(/\\/g, '/');
  return /[/](test|spec|__tests?__|testdata|fixtures|mocks?|node_modules|vendor)[/]/.test(p) ||
    /\.(spec|test)\.[jt]sx?$/.test(p) || p.endsWith('_test.go') || p.includes('/testdata/');
}

// ── Signed-hop assignment via directed BFS from root ─────────────────────────
// The daemon gives score=0.7^|hop| but not the sign. We recover it by BFS:
// outgoing structural edge (root is source) → +hop (dependency, right);
// incoming structural edge → −hop (caller, left).
// Structural edge kinds: traversed by BFS to assign signed hops.
// 'contains' handles file→symbol edges from tree-sitter indexing.
const STRUCTURAL = new Set([
  'calls', 'imports', 'defines', 'contains', 'ref',
  'is-implementation', 'overrides', 'ffi/call',
]);

function assignSignedHops(nodes, edges) {
  const byId = Object.fromEntries(nodes.map(n => [n.id, n]));
  nodes.forEach(n => { n.hop = undefined; });

  // Seed BFS from ALL root nodes simultaneously so that multi-implementation
  // queries (e.g. "ContainsPrefix" matching two files) correctly place every
  // root at hop=0 and assign signs to their distinct neighbourhoods.
  const roots = nodes.filter(n => n.root);

  // BFS to assign SIGN only: outgoing structural edge = dep (+), incoming = caller (−).
  // We deliberately do NOT use BFS hop-counts as magnitude: in dense subgraphs
  // (k8s depth=4 returns 300 nodes with many cross-edges) the shortest subgraph
  // path is often 1 hop, collapsing all nodes into hop=±1.
  // Magnitude comes from score (score ≈ 0.7^|hop|, calculated on the global graph).
  if (roots.length) {
    const sign = {};
    const queue = [];
    for (const r of roots) {
      r.hop = 0;
      sign[r.id] = 0; // 0=root, 1=dep, -1=caller
      queue.push(r.id);
    }
    while (queue.length) {
      const curId = queue.shift();
      const curSign = sign[curId];
      for (const e of edges) {
        if (!STRUCTURAL.has(e.kind)) continue;
        if (e.source === curId && sign[e.target] === undefined) {
          // Outgoing from cur → target is a dependency
          sign[e.target] = curSign >= 0 ? 1 : -1; // dep of dep = dep; dep of caller = caller
          queue.push(e.target);
        }
        if (e.target === curId && sign[e.source] === undefined) {
          // Incoming to cur → source is a caller
          sign[e.source] = curSign <= 0 ? -1 : 1;
          queue.push(e.source);
        }
      }
    }

    // Assign hops: sign from BFS, magnitude from score (global graph distance).
    nodes.forEach(n => {
      if (n.root) return; // all roots stay at hop 0
      const s = Math.min(0.99, Math.max(0.001, n.score || 0.3));
      const absHop = Math.max(1, Math.round(Math.log(s) / Math.log(0.7)));
      const sg = sign[n.id];
      if (sg !== undefined) {
        n.hop = sg >= 0 ? absHop : -absHop;
      }
      // else: handled in fallback below
    });
  }

  // Precompute edge degrees for fallback (nodes BFS didn't reach)
  const outDeg = {}, inDeg = {};
  for (const e of edges) {
    if (!STRUCTURAL.has(e.kind)) continue;
    outDeg[e.source] = (outDeg[e.source] || 0) + 1;
    inDeg[e.target]  = (inDeg[e.target]  || 0) + 1;
  }

  nodes.forEach(n => {
    if (n.hop !== undefined) return;
    const s = Math.min(0.99, Math.max(0.001, n.score || 0.3));
    const absHop = Math.max(1, Math.round(Math.log(s) / Math.log(0.7)));
    const out = outDeg[n.id] || 0;
    const inc = inDeg[n.id]  || 0;
    n.hop = out > inc ? -absHop : absHop;
  });
}

// ── Disambiguation bar ─────────────────────────────────────────────────────────
// Appears when the query returns ≥2 root nodes (same symbol name, different files).
// Each chip isolates that implementation's subgraph; "all" restores the combined view.
function renderDisambigBar() {
  const bar = document.getElementById('disambig-bar');
  if (!bar) return;
  const roots = allNodes.filter(n => n.root);
  if (roots.length <= 1) {
    bar.style.display = 'none';
    return;
  }
  bar.style.display = 'flex';

  // When every implementation lives in the same file, the path is repeated on
  // every chip and distinguishes nothing — it is pure width. Drop it and let
  // the file be stated once, in the label.
  const paths = new Set(roots.map(r => r.path || ''));
  const sharedPath = paths.size === 1 ? [...paths][0] : null;

  function chipLabel(r) {
    const base = shortLabel(r.label, r.id);
    if (sharedPath !== null) return base;
    const parts = (r.path || '').split('/');
    const sub   = parts.length >= 2 ? parts.slice(-2).join('/') : (parts[0] || '');
    return sub ? base + ' · ' + sub : base;
  }

  const allActive = _disambigRoot === null;
  // No `title`: the native tooltip is slow, truncates long paths, and would
  // fight the popup. The full text travels in data attributes instead, and
  // aria-label keeps the accessible name.
  const chipsHtml = [
    `<span class="db-chip db-all${allActive ? ' active' : ''}" data-root="" aria-label="Show all implementations">all</span>`,
    ...roots.map(r => {
      const lbl = escHtml(chipLabel(r));
      const active = _disambigRoot === r.id ? ' active' : '';
      const sym = shortLabel(r.label, r.id);
      return `<span class="db-chip${active}" data-root="${escHtml(r.id)}"` +
        ` data-sym="${escHtml(sym)}" data-path="${escHtml(r.path || '')}"` +
        ` aria-label="${escHtml(sym)}">${lbl}</span>`;
    }),
  ].join('');

  // The label carries the shared file so the chips do not have to, and the
  // count so the row reads as complete even when it is scrolled.
  const shortShared = sharedPath ? sharedPath.split('/').slice(-2).join('/') : '';
  const label = shortShared
    ? `${roots.length} in ${escHtml(shortShared)}`
    : `${roots.length} implementations`;

  // One scrolling row, always. The overflow is the scrollbar's to carry.
  bar.innerHTML =
    `<span class="db-label">${label}</span>` +
    `<div class="db-chips">${chipsHtml}</div>`;

  bar.querySelectorAll('.db-chip').forEach(chip => {
    chip.addEventListener('click', () => {
      _disambigRoot = chip.dataset.root || null;
      hideChipTip();
      renderDisambigBar();
      renderGraph(_disambigRoot || undefined);
    });
    chip.addEventListener('mouseenter', () => showChipTip(chip));
    chip.addEventListener('mouseleave', hideChipTip);
  });
  // Scrolling moves the chip out from under a popup anchored to where it was.
  const chipsBox = bar.querySelector('.db-chips');
  if (chipsBox) chipsBox.addEventListener('scroll', hideChipTip);
}

// ── Implementation chip popup ─────────────────────────────────────────────────
// Chips truncate, and the whole point of the bar is telling near-identical
// symbols apart, which is impossible from `G..`. The popup carries the full
// symbol and its path.
function showChipTip(chip) {
  const tip = document.getElementById('db-tip');
  if (!tip || !chip.dataset.sym) return;

  tip.innerHTML =
    `<div class="tip-sym">${escHtml(chip.dataset.sym)}</div>` +
    (chip.dataset.path ? `<div class="tip-path">${escHtml(chip.dataset.path)}</div>` : '');
  tip.style.display = 'block';
  tip.setAttribute('aria-hidden', 'false');

  // Measure after it is displayed, then clamp into the viewport so a chip at
  // the far right of a scrolled row does not push the popup off screen.
  const c = chip.getBoundingClientRect();
  const t = tip.getBoundingClientRect();
  const left = Math.max(8, Math.min(c.left, window.innerWidth - t.width - 8));
  // Below the chip normally; above it when there is no room underneath.
  const below = c.bottom + 6;
  const top = below + t.height > window.innerHeight - 8
    ? Math.max(8, c.top - t.height - 6)
    : below;
  tip.style.left = left + 'px';
  tip.style.top = top + 'px';
}

function hideChipTip() {
  const tip = document.getElementById('db-tip');
  if (!tip) return;
  tip.style.display = 'none';
  tip.setAttribute('aria-hidden', 'true');
}

// ── Build Cytoscape elements from filtered data ────────────────────────────────
function buildElements() {
  let visNodes = allNodes.filter(n => {
    if (!varsOn && n.kind === 'var') return false;
    if (noiseOn && isNoise(n.path)) return false;
    if (Math.abs(n.hop || 0) > depth) return false;
    if (n.root) return true;
    if (direction === 'callers' && (n.hop || 0) > 0) return false;
    if (direction === 'deps'    && (n.hop || 0) < 0) return false;
    return true;
  });

  // Disambig mode: isolate the selected root's subgraph by bidirectional BFS
  // through visible edges, then hide the other roots from this view.
  if (_disambigRoot !== null) {
    const reachable = new Set([_disambigRoot]);
    let changed = true;
    while (changed) {
      changed = false;
      allEdges.forEach(e => {
        if (reachable.has(e.source) && !reachable.has(e.target)) { reachable.add(e.target); changed = true; }
        if (reachable.has(e.target) && !reachable.has(e.source)) { reachable.add(e.source); changed = true; }
      });
    }
    visNodes = visNodes.filter(n => reachable.has(n.id));
  }

  const visIds = new Set(visNodes.map(n => n.id));
  const visById = Object.fromEntries(visNodes.map(n => [n.id, n]));

  const deg = {};
  allEdges.forEach(e => {
    if (e.kind !== 'reads' && visIds.has(e.source) && visIds.has(e.target)) {
      deg[e.source] = (deg[e.source] || 0) + 1;
      deg[e.target] = (deg[e.target] || 0) + 1;
    }
  });

  const els = [];

  // Compound file group parents
  if (grouping) {
    const files = new Set(visNodes.filter(n => n.path).map(n => n.path));
    files.forEach(f => {
      els.push({ data: { id: 'f::' + f, label: f.split('/').pop() || f, kind: 'group' } });
    });
  }

  // Nodes — detect duplicate base labels so same-name symbols from different
  // files get a disambiguating path subtitle (e.g. "v1alpha1/types.go").
  const _labelCount = {};
  visNodes.forEach(n => {
    if (n.kind !== 'file') _labelCount[n.label] = (_labelCount[n.label] || 0) + 1;
  });
  function disambigLabel(n) {
    const base = shortLabel(n.label, n.id);
    if (_labelCount[n.label] > 1 && n.path) {
      const parts = n.path.split('/');
      const subtitle = parts.length >= 2 ? parts.slice(-2).join('/') : parts[0];
      return base + '\n' + subtitle;
    }
    return base;
  }
  visNodes.forEach(n => {
    const d = deg[n.id] || 0;
    const sz = n.root ? 52 : Math.min(52, 26 + d * 5);
    const isHidden = n.kind === 'var' && !varsOn;
    els.push({
      data: {
        id: n.id, realId: n.realId, label: disambigLabel(n), kind: n.kind, path: n.path || '',
        pkg: n.package || '', score: n.score || 0, line: n.line || 0,
        hop: n.hop || 0, root: !!n.root, degree: d,
        w: n.kind === 'var' ? 42 : sz, h: n.kind === 'var' ? 18 : sz,
        ...(grouping && n.path ? { parent: 'f::' + n.path } : {}),
      },
      classes: (noiseOn && isNoise(n.path) ? 'noise ' : '') + (d >= 4 && !n.root ? 'hub' : ''),
      ...(isHidden ? { style: { display: 'none' } } : {}),
    });
  });

  // Edges
  if (grouping) {
    // Cross-file edges: aggregate by (srcFile, tgtFile, kind)
    const crossGroup = {};
    const intra = [];
    allEdges.forEach(e => {
      if (!visIds.has(e.source) || !visIds.has(e.target)) return;
      if (e.kind === 'reads' && !varsOn) return;
      if (!edgeKinds[e.kind] && e.kind !== 'reads') return;
      const sf = visById[e.source]?.path || '';
      const tf = visById[e.target]?.path || '';
      if (sf && tf && sf !== tf) {
        const key = sf + '|||' + tf + '|||' + e.kind;
        crossGroup[key] = (crossGroup[key] || 0) + 1;
      } else {
        intra.push(e);
      }
    });
    intra.forEach(e => {
      els.push({ data: { id: e.source + '->' + e.target, source: e.source, target: e.target, kind: e.kind } });
    });
    Object.entries(crossGroup).forEach(([key, wgt]) => {
      const [sf, tf, kind] = key.split('|||');
      els.push({ data: {
        id: 'grp::' + sf + '->' + tf + '::' + kind,
        source: 'f::' + sf, target: 'f::' + tf, kind, wgt,
      }});
    });
  } else {
    allEdges.forEach(e => {
      if (!visIds.has(e.source) || !visIds.has(e.target)) return;
      if (e.kind === 'reads' && !varsOn) return;
      if (!edgeKinds[e.kind] && e.kind !== 'reads') return;
      els.push({ data: { id: e.source + '->' + e.target, source: e.source, target: e.target, kind: e.kind } });
    });
  }

  return { els, noiseCount: allNodes.filter(n => noiseOn && isNoise(n.path) && Math.abs(n.hop || 0) <= depth).length };
}

// ── Flow layout: callers left, root center, deps right ────────────────────────
// Dense hop-columns are split into sub-columns so a single col never has > MAX_COL
// nodes stacked vertically.  _spreadMul doubles all spacings when the user zooms in.
function flowPositions() {
  const mul = _spreadMul || 1;
  const ROOT_X = 420, ROOT_Y = 320;
  const MAX_COL = 14;               // max nodes in one sub-column
  const VSTEP   = Math.round(130 * mul);
  const HOP_X   = Math.round(380 * mul); // x-distance per hop level
  const SUB_X   = Math.round(260 * mul); // x-gap between sub-cols of the same hop

  const cols = {};
  cy.nodes().not(':parent').forEach(n => {
    const h = n.data('hop') || 0;
    (cols[h] = cols[h] || []).push(n);
  });

  Object.entries(cols).forEach(([h, nodes]) => {
    const hop = Number(h);
    nodes.sort((a, b) =>
      (b.data('root') ? 1 : 0) - (a.data('root') ? 1 : 0) ||
      (b.data('score') || 0) - (a.data('score') || 0) ||
      (a.data('path') || '').localeCompare(b.data('path') || '')
    );

    if (hop === 0) {
      const span = (nodes.length - 1) * VSTEP;
      nodes.forEach((n, i) => n.position({ x: ROOT_X, y: ROOT_Y - span / 2 + i * VSTEP }));
      return;
    }

    const sign = hop > 0 ? 1 : -1;
    const abshop = Math.abs(hop);
    const numSubs = Math.ceil(nodes.length / MAX_COL);
    for (let s = 0; s < numSubs; s++) {
      const chunk = nodes.slice(s * MAX_COL, (s + 1) * MAX_COL);
      const x = ROOT_X + sign * (abshop * HOP_X + s * SUB_X);
      const span = (chunk.length - 1) * VSTEP;
      chunk.forEach((n, i) => n.position({ x, y: ROOT_Y - span / 2 + i * VSTEP }));
    }
  });
}

// ── Rings layout: concentric by |hop|, callers left hemisphere, deps right ────
function ringPositions() {
  const mul = _spreadMul || 1;
  const g = {};
  cy.nodes().not(':parent').forEach(n => {
    const h = n.data('hop') || 0;
    (g[h] = g[h] || []).push(n);
  });

  const maxInRing = Math.max(1, ...Object.values(g).map(ns => ns.length));
  // Ensure each node in the densest ring has at least ~70px of arc-spacing.
  // arc-spacing ≈ 2 * 0.86π * r / N → r = N * 70 / (2 * 0.86π) ≈ N * 13.
  const ringStep = Math.max(180, Math.min(360, Math.round(maxInRing * 14 + 120))) * mul;

  Object.entries(g).forEach(([h, nodes]) => {
    const hop = Number(h);
    if (hop === 0) {
      // Spread hop-0 nodes in a small cluster, not a single point.
      const s0 = Math.min(44, nodes.length * 18);
      nodes.forEach((n, i) => n.position({
        x: 420 + (nodes.length === 1 ? 0 : (i - (nodes.length - 1) / 2) * s0),
        y: 320,
      }));
      return;
    }
    // Cap |hop| at 4 so extreme-outlier nodes never blow up the fit box.
    // A single node at hop -6 would otherwise appear 1080px to the left,
    // making everything else microscopic after cy.fit().
    const clampedHop = Math.sign(hop) * Math.min(Math.abs(hop), 4);
    const r = Math.abs(clampedHop) * ringStep;
    const base = clampedHop < 0 ? Math.PI : 0;
    // Widen arc for thin rings so lone nodes aren't pinned at a single point.
    const arc = nodes.length <= 2 ? Math.PI * 0.5 : Math.PI * 0.86;
    nodes.forEach((n, i) => {
      const a = base - arc / 2 + (nodes.length === 1 ? arc / 2 : arc * i / (nodes.length - 1));
      n.position({ x: 420 + Math.cos(a) * r, y: 320 + Math.sin(a) * r * 0.72 });
    });
  });
}

// ── Render the graph with current state ────────────────────────────────────────
function renderGraph(bloomOriginId) {
  if (blastOn) exitBlast();
  closePeek();

  const { els, noiseCount } = buildElements();
  cy.batch(() => {
    cy.elements().remove();
    cy.add(els);
    cy.nodes('[kind="var"]').style('display', varsOn ? 'element' : 'none');
    cy.edges('[kind="reads"]').style('display', varsOn ? 'element' : 'none');
  });

  // Explicitly re-enable pan/grab after every render — guard against anything
  // that might reset these (layout plugins, external state, re-used instances).
  cy.userPanningEnabled(true);
  cy.userZoomingEnabled(true);
  cy.autoungrabify(false);

  (layoutName === 'flow' ? flowPositions : ringPositions)();

  // Column header visibility
  const showCols = layoutName === 'flow';
  document.querySelectorAll('.col-head').forEach(el => el.style.opacity = showCols ? '1' : '0');

  progZoom(() => {
    if (cy.nodes().length === 0) return;
    const n = cy.nodes().length;
    // Use a zoom level that keeps nodes legible regardless of graph size.
    // For small graphs (≤25 nodes) fit is fine; for larger ones lock to a
    // readable zoom and centre on the root node so the user can pan to explore.
    const rootId = _disambigRoot || (allNodes.find(nd => nd.root) || {}).id;
    const rootEl = rootId ? cy.getElementById(rootId) : null;
    if (n <= 25) {
      cy.fit(cy.elements(), 55);
    } else {
      const targetZoom = Math.max(0.18, Math.min(0.55, 12 / Math.sqrt(n)));
      cy.zoom(targetZoom);
      if (rootEl && rootEl.length) {
        cy.center(rootEl);
      } else {
        cy.center();
      }
    }
    if (FX.on) bloom(bloomOriginId || rootId || null);
  });

  // Noise badge
  const noiseBadge = document.getElementById('noiseBadge');
  if (noiseCount > 0) {
    noiseBadge.textContent = '⊘ ' + noiseCount + ' noise nodes hidden';
    noiseBadge.style.display = 'inline';
  } else {
    noiseBadge.style.display = 'none';
  }

  // #688: renderGraph rebuilds the element set, so any diagnostic classes from
  // the previous pass are gone. Repaint from the last overlay the host sent —
  // client-side re-filters (depth, noise, vars) re-render without a round trip.
  paintDiagnostics();

  // Decide which labels survive this render. progZoom sets the zoom
  // asynchronously, so the pan/zoom handler re-runs this once it settles.
  applyLabelLod();

  updateHint();
  updateStatusBar();
}

function updateHint() {
  const hintEl = document.getElementById('hint');
  if (!cy.nodes().length) {
    hintEl.innerHTML = 'Type a symbol name and press <kbd>Enter</kbd> to explore the graph';
    return;
  }
  if (layoutName === 'flow') {
    hintEl.innerHTML = '<b>Flow layout:</b> callers enter left, dependencies exit right — column = hop distance. ' +
      '<b>Click</b> a node for details · <kbd>/</kbd> focuses search · <b>⊗ blast</b> shows impact rings.';
  } else {
    hintEl.innerHTML = '<b>Rings layout:</b> concentric by hop distance, callers in left hemisphere. ' +
      '<b>Click</b> a node for details · <b>⊗ blast</b> shows impact rings.';
  }
}

// ── Live LSP diagnostics overlay (#688) ───────────────────────────────────────
// The host reads vscode.languages.getDiagnostics and posts the reduction; this
// side only paints. `_diagByNode` is the last overlay received, kept so a
// client-side re-render can repaint without asking the host again.
let _diagByNode = {};
let _diagUnknown = [];
// Per-file diagnostic lists, for the detail panel's Problems section. Keyed by
// graph path because attribution is file-scoped, so every node from one file
// shares one list.
let _diagItemsByFile = {};
let _diagTruncated = {};

function applyDiagnosticsOverlay(byNode, unknownCoverage, itemsByFile, itemsTruncated) {
  _diagByNode = byNode || {};
  _diagUnknown = unknownCoverage || [];
  _diagItemsByFile = itemsByFile || {};
  _diagTruncated = itemsTruncated || {};
  paintDiagnostics();
  updateDiagBadge();
  refreshOpenDetailProblems();
  refreshOpenPeekDiagnostics();
}

// ── Problems section of the detail panel ──────────────────────────────────────
// The ring says a node's file is broken; this says how, and clicking a row
// opens that file at that diagnostic's own line rather than the node's.

/** Build the Problems section for `path`, or '' when there is nothing to say. */
function diagProblemsHtml(path) {
  if (!path) return '';
  const items = _diagItemsByFile[path] || [];
  // A file no provider has published for is unknown, not clean. Saying nothing
  // here would let the absence read as a pass.
  if (items.length === 0) {
    if (_diagUnknown.indexOf(path) === -1) return '';
    return '<div class="d-section"><div class="d-title">Problems</div>' +
      '<div class="diag-none-note">No diagnostic provider has reported on this file, ' +
      'so it is not diagnosed rather than clean.</div></div>';
  }

  const dropped = _diagTruncated[path] || 0;
  const errors = items.filter(i => i.severity === 'error').length;
  const warnings = items.length - errors;
  const counts = [];
  if (errors > 0) counts.push(errors + (errors === 1 ? ' error' : ' errors'));
  if (warnings > 0) counts.push(warnings + (warnings === 1 ? ' warning' : ' warnings'));

  const rows = items.map(i =>
    '<button class="diag-item ' + (i.severity === 'error' ? 'is-err' : 'is-warn') + '"' +
      ' data-diag-line="' + i.line + '"' +
      ' title="' + escHtml(i.message) + '\nGo to line ' + i.line + '">' +
      '<span class="diag-item-icon">' + (i.severity === 'error' ? '⊗' : '⚠') + '</span>' +
      '<span class="diag-item-line">' + i.line + '</span>' +
      '<span class="diag-item-msg">' + escHtml(i.message) + '</span>' +
      (i.source ? '<span class="diag-item-src">' + escHtml(i.source) + '</span>' : '') +
    '</button>'
  ).join('');

  return '<div class="d-section"><div class="d-title">Problems (' + counts.join(', ') + ')</div>' +
    '<div class="diag-list">' + rows + '</div>' +
    (dropped > 0
      ? '<div class="diag-none-note">and ' + dropped + ' more, not listed</div>'
      : '') +
    '<div class="diag-none-note">Counts are per file, not per symbol.</div>' +
  '</div>';
}

/** Wire the Problems rows of an already-rendered detail panel. */
function wireDiagProblems(detailEl, path) {
  detailEl.querySelectorAll('[data-diag-line]').forEach(btn => {
    btn.addEventListener('click', () => {
      vscode.postMessage({
        command: 'goToDefinition',
        path: path,
        line: Number(btn.getAttribute('data-diag-line')) || 1,
      });
    });
  });
}

/**
 * Repaint the Problems section of the open detail panel when a new overlay
 * lands, so the list tracks the editor while the panel stays open. Only this
 * section is replaced: rebuilding the whole panel would drop scroll position
 * and the peek state on every keystroke.
 */
function refreshOpenDetailProblems() {
  const detailEl = document.getElementById('detail');
  if (!detailEl || !detailEl.classList.contains('open')) return;
  const host = detailEl.querySelector('[data-diag-host]');
  if (!host) return;
  const path = host.getAttribute('data-diag-host');
  // The list has its own scrollbar, and overlays land on every keystroke, so
  // rebuilding it would scroll the reader back to the top as they type.
  const prev = host.querySelector('.diag-list');
  const scroll = prev ? prev.scrollTop : 0;
  host.innerHTML = diagProblemsHtml(path);
  const next = host.querySelector('.diag-list');
  if (next) next.scrollTop = scroll;
  wireDiagProblems(host, path);
}

function paintDiagnostics() {
  cy.batch(() => {
    cy.nodes('.diag-error, .diag-warn').removeClass('diag-error diag-warn');
    for (const id in _diagByNode) {
      const el = cy.getElementById(id);
      if (!el || el.empty()) continue;
      el.addClass(_diagByNode[id].severity === 'error' ? 'diag-error' : 'diag-warn');
    }
  });
}

function updateDiagBadge() {
  const badge = document.getElementById('diagBadge');
  if (!badge) return;

  let errors = 0, warnings = 0;
  for (const id in _diagByNode) {
    const d = _diagByNode[id];
    if (d.severity === 'error') errors += d.count; else warnings += d.count;
  }

  const parts = [];
  if (errors > 0) parts.push('⊗ ' + errors + (errors === 1 ? ' error' : ' errors'));
  if (warnings > 0) parts.push('⚠ ' + warnings + (warnings === 1 ? ' warning' : ' warnings'));
  // Absence of diagnostics is not a clean bill of health: a file whose language
  // has no extension installed reports nothing at all. Say which it is.
  if (_diagUnknown.length > 0) {
    parts.push(_diagUnknown.length + ' not diagnosed');
  }

  if (parts.length === 0) {
    badge.style.display = 'none';
    badge.removeAttribute('title');
    return;
  }
  badge.textContent = parts.join(' · ');
  badge.className = errors > 0 ? 'diag-err' : warnings > 0 ? 'diag-warn' : 'diag-none';
  badge.style.display = 'inline';
  badge.title = _diagUnknown.length > 0
    ? 'Counts are per file, not per symbol.\nNo diagnostic provider has reported on:\n' +
      _diagUnknown.slice(0, 10).join('\n') +
      (_diagUnknown.length > 10 ? '\n…and ' + (_diagUnknown.length - 10) + ' more' : '')
    : 'Counts are per file, not per symbol.';
}

// ── Status bar ────────────────────────────────────────────────────────────────
function updateStatusBar() {
  const n = cy.nodes(':visible').not(':parent').length;
  const e = cy.edges(':visible').length;
  const tokens = cy.nodes(':visible').not(':parent').reduce((sum, nd) => {
    const d = nd.data();
    return sum + Math.max(1, Math.floor(((d.label || '').length + (d.kind || '').length + (d.path || '').length) / 4));
  }, 0);
  document.getElementById('statusGraph').textContent = n + ' nodes · ' + e + ' edges · ~' + tokens + ' tokens';
}

function setFreshness(state, nodeCount) {
  const freshEl = document.getElementById('fresh');
  const textEl  = document.getElementById('freshText');
  const pulse   = freshEl.querySelector('.dot-pulse');
  if (state === 'fresh') {
    textEl.textContent = 'fresh · ' + formatCount(nodeCount) + ' nodes';
    pulse.style.background = '#86df86';
  } else if (state === 'stale') {
    textEl.textContent = 'stale';
    pulse.style.background = '#fb923c';
  } else {
    textEl.textContent = 'connecting…';
  }
}

function formatCount(n) {
  return n >= 1e6 ? (n / 1e6).toFixed(1) + 'M' :
         n >= 1e3 ? (n / 1e3).toFixed(0) + 'k' : String(n);
}

// ── Message bridge: extension → webview ───────────────────────────────────────
window.addEventListener('message', event => {
  const msg = event.data;

  if (msg.command === 'render') {
    // Drop superseded requests
    if (msg.reqId !== undefined && msg.reqId < currentReqId) return;

    const data = msg.data || { nodes: [], edges: [] };
    // Server's data.mode takes priority — it knows whether it returned overview vs prefix.
    // msg.mode is just the query mode the client requested; fall back to it only if server
    // didn't set one (e.g. for symbol graph responses).
    const serverMode = data.mode || msg.mode || '';
    const pathPrefix = msg.pathPrefix || '';

    if (serverMode === 'overview' || serverMode === 'prefix') {
      renderOverview(data, serverMode, pathPrefix);
      return;
    }

    // Symbol graph (mode == '' or unknown)
    setViewMode(false);
    ({ nodes: allNodes, edges: allEdges } = sanitizeGraphIds(data.nodes || [], data.edges || []));
    loadedDepth = depth;
    loadedDirection = direction;
    _disambigRoot = null; // reset on every new query

    assignSignedHops(allNodes, allEdges);
    renderDisambigBar();
    renderGraph(allNodes.find(n => n.root)?.id);
    renderBreadcrumb();

    if (msg.query) {
      document.getElementById('searchInput').value = msg.query;
    }
  }

  if (msg.command === 'freshness') {
    setFreshness(msg.state, msg.nodeCount);
  }

  if (msg.command === 'renderPeek') {
    renderPeekPanel(msg.path, msg.line, msg.lines || []);
  }

  if (msg.command === 'diagnosticsOverlay') {
    applyDiagnosticsOverlay(msg.byNode, msg.unknownCoverage, msg.itemsByFile, msg.itemsTruncated);
  }
});

// ── Search ─────────────────────────────────────────────────────────────────────
function submitQuery(forceFetch) {
  const query = document.getElementById('searchInput').value.trim();
  if (!query) {
    // Empty query → back to repo overview
    _navStack = [];
    const reqId = ++currentReqId;
    vscode.postMessage({ command: 'query', query: '', direction: 'both', depth: 2, kind_filter: '', mode: 'overview', path_prefix: '', reqId });
    return;
  }

  // Client-side re-filter when within loaded depth/direction (symbol graph only)
  const depthOk = depth <= loadedDepth;
  const dirOk = direction === loadedDirection || loadedDirection === 'both';
  if (!forceFetch && !_overviewActive && allNodes.length > 0 && depthOk && dirOk) {
    _disambigRoot = null;
    renderDisambigBar();
    renderGraph();
    return;
  }

  // Switching from overview to symbol query: clear nav stack
  if (_overviewActive) _navStack = [];

  const reqId = ++currentReqId;
  vscode.postMessage({ command: 'query', query, direction, depth, kind_filter: '', reqId });
}

let _debounceTimer = null;
function debouncedQuery() {
  clearTimeout(_debounceTimer);
  _debounceTimer = setTimeout(() => {
    if (document.getElementById('searchInput').value.trim()) submitQuery(true);
  }, 380);
}

document.getElementById('searchInput').addEventListener('keydown', e => {
  if (e.key === 'Enter') submitQuery(true);
});

// ── Banner flash ───────────────────────────────────────────────────────────────
function flashBanner(msg) {
  const b = document.getElementById('banner');
  b.textContent = msg;
  b.classList.add('show');
  clearTimeout(flashBanner._t);
  flashBanner._t = setTimeout(() => b.classList.remove('show'), 2400);
}

// ── Toolbar controls ───────────────────────────────────────────────────────────
function setDirection(d) {
  direction = d;
  ['callers', 'both', 'deps'].forEach(x => {
    document.getElementById('btn-' + x).classList.toggle('active', x === d);
  });
  if (blastOn) return; // direction doesn't apply to blast view
  renderGraph();
}

function setLayout(l) {
  layoutName = l;
  ['flow', 'rings'].forEach(x => {
    document.getElementById('btn-' + x).classList.toggle('active', x === l);
  });
  if (blastOn) return; // layout preference saved; applied when blast exits
  renderGraph();
}

function onDepthSlider(v) {
  depth = Number(v);
  document.getElementById('depthVal').textContent = v;
  if (blastOn) return; // depth doesn't apply while in blast view
  if (depth <= loadedDepth) {
    renderGraph();
  } else {
    submitQuery(true); // need more data
  }
}

function toggleGrouping() {
  grouping = !grouping;
  document.getElementById('btn-group').classList.toggle('active', grouping);
  flashBanner(grouping ? 'symbols grouped by file' : 'flat view');
  if (blastOn) return;
  renderGraph();
}

function toggleVars() {
  varsOn = !varsOn;
  document.getElementById('btn-vars').classList.toggle('active-gold', varsOn);
  flashBanner(varsOn ? 'showing exported variable nodes' : 'variable nodes hidden');
  if (blastOn) return;
  renderGraph();
}

function toggleNoise() {
  noiseOn = !noiseOn;
  document.getElementById('btn-noise').classList.toggle('active-orange', noiseOn);
  flashBanner(noiseOn ? 'noise filter ON — tests & vendor hidden' : 'noise filter OFF');
  if (blastOn) return;
  renderGraph();
}

function toggleEdgeKind(k) {
  edgeKinds[k] = !edgeKinds[k];
  document.getElementById('chip-' + k).classList.toggle('on', edgeKinds[k]);
  if (blastOn) return;
  renderGraph();
}

// ── Fit / zoom ─────────────────────────────────────────────────────────────────
function fitView() { progZoom(() => cy.animate({ fit: { padding: 55 } }, { duration: 250 })); }
function zoomBy(f) {
  progZoom(() => cy.animate({
    zoom: { level: cy.zoom() * f, renderedPosition: { x: cy.width() / 2, y: cy.height() / 2 } }
  }, { duration: 200 }));
}

// ── Interactions ───────────────────────────────────────────────────────────────
const tip = document.getElementById('tip');

cy.on('mouseover', 'node', evt => {
  if (evt.target.isParent()) return;
  const d = evt.target.data();
  const hop = d.hop !== undefined ? ' · hop ' + d.hop : '';
  tip.innerHTML = '<b>' + escHtml(d.label || '') + '</b>' +
    (d.path ? '<div class="tip-path">' + escHtml(d.path) + '</div>' : '') +
    '<div class="tip-hop">' + (d.kind || '') + hop + '</div>';
  tip.style.display = 'block';

  // Soft-dim distant neighbourhood on hover (but not in blast mode)
  if (!blastOn) {
    cy.elements().not(':parent').difference(evt.target.closedNeighborhood()).addClass('softdim');
  }

  // Name the neighbourhood the pointer is on, so a label-suppressed graph can
  // still be read by sweeping across it rather than by zooming in and out.
  evt.target.closedNeighborhood().nodes().addClass('lbl-keep');
  applyLabelLod();

  if (FX.on && !blastOn) {
    const h1 = evt.target.closedNeighborhood().nodes().difference(evt.target);
    const h2 = h1.closedNeighborhood().nodes().difference(h1).difference(evt.target);
    clearTimeout(tip._wt1); clearTimeout(tip._wt2);
    tip._wt1 = setTimeout(() => h1.addClass('wave-1'), 60);
    tip._wt2 = setTimeout(() => h2.addClass('wave-2'), 175);
  }

  // Hover-spread: push nodes that crowd the hovered node out of the way so the
  // user can distinguish and click individual nodes in dense columns.
  if (!blastOn) {
    const pos = evt.target.position();
    const THRESH = 110; // graph-pixel proximity threshold
    const nearby = cy.nodes(':visible').not(':parent').filter(o => {
      if (o.id() === evt.target.id()) return false;
      const p = o.position();
      return Math.hypot(p.x - pos.x, p.y - pos.y) < THRESH;
    });
    if (nearby.length) {
      // Restore any prior spread first (instant — avoids double-animation)
      if (_hoverSpread) {
        _hoverSpread.orig.forEach((op, id) => { const o = cy.getElementById(id); if (o.length) o.position(op); });
        _hoverSpread = null;
      }
      const orig = new Map();
      nearby.forEach((o, i) => {
        orig.set(o.id(), { ...o.position() });
        const p = o.position();
        const dx = p.x - pos.x, dy = p.y - pos.y;
        const dist = Math.hypot(dx, dy);
        let nx, ny;
        if (dist < 4) {
          const a = (i / nearby.length) * Math.PI * 2;
          nx = pos.x + Math.cos(a) * (THRESH + 18);
          ny = pos.y + Math.sin(a) * (THRESH + 18);
        } else {
          const push = THRESH - dist + 22;
          nx = p.x + (dx / dist) * push;
          ny = p.y + (dy / dist) * push;
        }
        o.animate({ position: { x: nx, y: ny } }, { duration: 170, easing: 'ease-out-cubic' });
      });
      _hoverSpread = { nodeId: evt.target.id(), orig };
    }
  }
});
cy.on('mousemove', evt => {
  if (!evt.originalEvent) return;
  tip.style.left = (evt.originalEvent.clientX + 14) + 'px';
  tip.style.top  = (evt.originalEvent.clientY - 44) + 'px';
});
cy.on('mouseout', 'node', () => {
  tip.style.display = 'none';
  clearTimeout(tip._wt1); clearTimeout(tip._wt2);
  cy.elements().removeClass('wave-1 wave-2 softdim');
  cy.nodes('.lbl-keep').removeClass('lbl-keep');
  applyLabelLod();
  // Restore hover-spread
  if (_hoverSpread) {
    const s = _hoverSpread; _hoverSpread = null;
    s.orig.forEach((op, id) => {
      cy.getElementById(id).animate({ position: op }, { duration: 150, easing: 'ease-in-cubic' });
    });
  }
});

cy.on('tap', 'node', evt => {
  if (evt.target.isParent()) return;
  const n = evt.target;
  cy.batch(() => {
    // Only dim non-parent nodes/edges — compound parents must stay visible so
    // their children's opacity isn't inherited-multiplied to near-zero.
    cy.elements().not(':parent').addClass('dimmed');
    const nbhd = n.closedNeighborhood();
    nbhd.removeClass('dimmed');
  });
  showDetail(n);
  // Selection makes a node a landmark, and tapping changes no zoom, so the
  // pan/zoom handler will not run. Refresh the labels here.
  applyLabelLod();
  if (FX.on && evt.renderedPosition) shock(evt.renderedPosition.x, evt.renderedPosition.y);
});
cy.on('tap', evt => {
  if (evt.target === cy) {
    cy.elements().removeClass('dimmed');
    document.getElementById('detail').classList.remove('open');
  }
  if (FX.on && evt.renderedPosition) shock(evt.renderedPosition.x, evt.renderedPosition.y);
});

// Snap any in-flight bloom opacity fade to 1 the moment the user grabs a node.
cy.on('grab', 'node', evt => {
  evt.target.stop(true, true);
});

// ── Detail panel ────────────────────────────────────────────────────────────────
function showDetail(n) {
  const d = n.data();
  const color = nodeColor(d.kind);
  const iconMap = { function: 'ƒ', class: '◈', file: '▭', interface: '◻', var: 'x' };
  const callers = n.incomers('edge').filter(e => e.data('kind') !== 'reads').length;
  const deps    = n.outgoers('edge').filter(e => e.data('kind') !== 'reads').length;
  const tok = Math.max(1, Math.floor(((d.label || '').length + (d.path || '').length) / 4));
  const hop = d.hop !== undefined ? '<div class="d-row"><span class="d-key">hop</span><span class="d-val green">' + d.hop + '</span></div>' : '';

  const edgeItems = n.connectedEdges().map(e => {
    const isOut = e.data('source') === d.id;
    const otherId = isOut ? e.data('target') : e.data('source');
    const otherNode = cy.getElementById(otherId);
    const otherLabel = otherNode.length
      ? (otherNode.data('label') || shortLabel('', otherId))
      : shortLabel('', otherId.replace(/^(f::|grp::)/, ''));
    const wgt = e.data('wgt') ? ' <span style="color:#686868">×' + Number(e.data('wgt') || 0) + '</span>' : '';
    return '<li class="edge-li" title="' + escHtml(otherId) + '"><span class="edge-arrow">' + (isOut ? '→' : '←') + '</span> ' +
      escHtml(otherLabel) + ' <span class="edge-type">' + escHtml(e.data('kind') || '') + '</span>' + wgt + '</li>';
  }).join('');

  const detailEl = document.getElementById('detail');
  detailEl.classList.add('open');
  detailEl.innerHTML =
    '<div style="display:flex;gap:10px;align-items:center;margin-bottom:4px">' +
      '<div class="node-icon-lg" style="background:' + color + '14;border:2px solid ' + color + ';color:' + color + '">' +
        fromMap(iconMap, d.kind, '●') +
      '</div>' +
      '<div>' +
        '<div class="d-sig">' + escHtml(d.label || '') + '</div>' +
        '<span class="d-kind" style="background:' + color + '14;color:' + color + '">' + (d.kind || '') + '</span>' +
      '</div>' +
    '</div>' +
    '<div class="d-section"><div class="d-title">Identity</div>' +
      '<div class="d-row"><span class="d-key">path</span><span class="d-val" style="color:' + color + ';opacity:.85">' + escHtml(d.path || '—') + '</span></div>' +
      '<div class="d-row"><span class="d-key">package</span><span class="d-val">' + escHtml(d.pkg || '—') + '</span></div>' +
      hop +
    '</div>' +
    '<div class="d-section"><div class="d-title">Graph metrics</div>' +
      '<div class="d-row"><span class="d-key">score</span><span class="d-val green">' + (d.score || 0) + '</span></div>' +
      '<div class="d-row"><span class="d-key">callers</span><span class="d-val">' + callers + '</span></div>' +
      '<div class="d-row"><span class="d-key">deps</span><span class="d-val">' + deps + '</span></div>' +
      '<div class="d-row"><span class="d-key">token cost</span><span class="d-val gold">' + tok + '</span></div>' +
    '</div>' +
    (edgeItems ? '<div class="d-section"><div class="d-title">Edges (' + n.connectedEdges().length + ')</div><ul>' + edgeItems + '</ul></div>' : '') +
    // Problems sits above Actions and inside its own host element, so a new
    // overlay can replace just this part without rebuilding the panel.
    '<div data-diag-host="' + escHtml(d.path || '') + '">' + diagProblemsHtml(d.path) + '</div>' +
    '<div class="d-section"><div class="d-title">Actions</div>' +
      (d.path && d.line ? '<button class="btn-action" data-act="peek">↗ Definition peek</button>' : '') +
      (d.path ? '<button class="btn-action" data-act="goto">↗ Go to definition</button>' : '') +
      '<button class="btn-action hot" data-act="blast">⊗ Show blast radius</button>' +
      (d.kind === 'file' && d.path ? '<button class="btn-action" data-act="deps">⊟ Show dependencies</button>' : '') +
      '<button class="btn-action" data-act="copy">⧉ Copy VName</button>' +
    '</div>';

  // Wire actions — CSP blocks onclick= in innerHTML; must use addEventListener.
  wireDiagProblems(detailEl, d.path);
  const _peek = detailEl.querySelector('[data-act="peek"]');
  if (_peek) _peek.addEventListener('click', () => peekNode(d.path, d.line || 0));
  const _goto = detailEl.querySelector('[data-act="goto"]');
  if (_goto) _goto.addEventListener('click', () => vscode.postMessage({ command: 'goToDefinition', path: d.path, line: d.line || 0 }));
  const _blastBtn = detailEl.querySelector('[data-act="blast"]');
  if (_blastBtn) _blastBtn.addEventListener('click', () => enterBlast(d.id));
  const _deps = detailEl.querySelector('[data-act="deps"]');
  if (_deps) _deps.addEventListener('click', () => vscode.postMessage({ command: 'showDependencies', path: d.path }));
  const _copy = detailEl.querySelector('[data-act="copy"]');
  if (_copy) _copy.addEventListener('click', () => copyVName(d.id));
}

function copyVName(id) {
  navigator.clipboard.writeText(id).then(() => flashBanner('VName copied'));
}

// ── Export ─────────────────────────────────────────────────────────────────────
function escDot(s) { return String(s == null ? '' : s).replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/\n/g, ' '); }

function exportDot() {
  let out = 'digraph travsr {\n  rankdir=LR;\n  node [shape=box, style=rounded];\n';
  cy.nodes(':visible').not(':parent').forEach(n => {
    out += '  "' + escDot(n.id()) + '" [label="' + escDot(n.data('label')) + '"];\n';
  });
  cy.edges(':visible').forEach(e => {
    out += '  "' + escDot(e.data('source')) + '" -> "' + escDot(e.data('target')) +
      '" [label="' + escDot(e.data('kind')) + '"];\n';
  });
  out += '}\n';
  vscode.postMessage({ command: 'exportDot', dot: out });
}

function exportJson() {
  const payload = {
    // Emit the real CLI node id (`realId`), not the cytoscape-safe handle, so an
    // exported graph's ids line up with `travsr graph --format json`.
    nodes: cy.nodes(':visible').not(':parent').map(n => ({
      id: n.data('realId') || n.id(), label: n.data('label'), kind: n.data('kind'),
      path: n.data('path'), package: n.data('pkg'), score: n.data('score'), line: n.data('line'),
    })),
    edges: cy.edges(':visible').map(e => ({
      source: e.source().data('realId') || e.data('source'), target: e.target().data('realId') || e.data('target'), kind: e.data('kind'),
    })),
  };
  vscode.postMessage({ command: 'exportJson', json: JSON.stringify(payload, null, 2) });
}

function exportPng() {
  if (!cy.nodes().length) { flashBanner('no graph to export'); return; }
  const dataUrl = cy.png({ full: true, scale: 2, bg: '#121212' });
  vscode.postMessage({ command: 'exportPng', dataUrl });
}

// ── Minimap ────────────────────────────────────────────────────────────────────
let _minimapDirty = false;
function scheduleMinimapRedraw() { _minimapDirty = true; }

function drawMinimap() {
  if (!_minimapDirty) return;
  _minimapDirty = false;
  const cv = document.getElementById('minimap');
  if (!cv) return;
  const ctx = cv.getContext('2d');
  const W = cv.width, H = cv.height;
  ctx.clearRect(0, 0, W, H);
  ctx.fillStyle = '#0f0f0f';
  ctx.fillRect(0, 0, W, H);
  const els = cy.elements(':visible');
  if (!els.length) return;
  const bb = els.boundingBox();
  if (!bb.w) return;
  const sc = Math.min((W - 8) / bb.w, (H - 8) / bb.h) * 0.92;
  const ox = (W - bb.w * sc) / 2, oy = (H - bb.h * sc) / 2;
  const X = x => ox + (x - bb.x1) * sc;
  const Y = y => oy + (y - bb.y1) * sc;
  els.edges().forEach(e => {
    const s = e.source().position(), t = e.target().position();
    ctx.beginPath(); ctx.moveTo(X(s.x), Y(s.y)); ctx.lineTo(X(t.x), Y(t.y));
    ctx.strokeStyle = '#2c2c2c'; ctx.lineWidth = 0.6; ctx.stroke();
  });
  els.nodes().not(':parent').forEach(n => {
    const p = n.position();
    ctx.beginPath();
    ctx.arc(X(p.x), Y(p.y), n.data('root') ? 3.5 : 2.2, 0, Math.PI * 2);
    ctx.fillStyle = nodeColor(n.data('kind')); ctx.fill();
  });
  // Viewport rect
  const ex = cy.extent();
  ctx.strokeStyle = 'rgba(134,223,134,0.65)'; ctx.lineWidth = 1;
  ctx.strokeRect(X(ex.x1), Y(ex.y1), (ex.x2 - ex.x1) * sc, (ex.y2 - ex.y1) * sc);
}

// ── Label level of detail ─────────────────────────────────────────────────────
// Every node asked for its label at every zoom. At this repository's own
// exec.rs that is 157 names in the space of about twenty, which renders as an
// unreadable smear and hides the structure the graph exists to show.
//
// Zoomed out, only landmarks keep their name: the seed node and the busiest
// hubs, capped at a fixed count so the number of labels cannot grow with the
// graph. Zoomed in past LABEL_ZOOM the text has room, so everything is named.
// Hovering reveals a neighbourhood on the way through (see the mouseover
// handler), and the tooltip still answers for any single node.
//
// Package tiles, ghosts and compound parents are exempt throughout: for those
// the label is the content, not an annotation on a dot.
const LABEL_ZOOM = 1.15;
const LABEL_LANDMARKS = 10;

function labelableNodes() {
  return cy.nodes().filter(n =>
    !n.isParent() && n.data('kind') !== 'pkg' && n.data('kind') !== 'ghost');
}

function applyLabelLod() {
  const ordinary = labelableNodes();
  if (ordinary.length === 0) return;
  const showAll = cy.zoom() >= LABEL_ZOOM;

  const landmarks = new Set();
  if (!showAll) {
    ordinary.toArray()
      .filter(n => !n.data('root'))
      .sort((a, b) => (b.data('degree') || 0) - (a.data('degree') || 0))
      .slice(0, LABEL_LANDMARKS)
      .forEach(n => landmarks.add(n.id()));
  }

  cy.batch(() => {
    ordinary.forEach(n => {
      // Anything the user has singled out keeps its name whatever the zoom:
      // the blast rings, a search hit, the current selection, a hovered
      // neighbourhood. Suppressing those would break the feature that put
      // the node on screen in the first place.
      const singledOut =
        n.selected() ||
        n.hasClass('lbl-keep') ||
        n.hasClass('node-search-hit') ||
        n.hasClass('blast-root') || n.hasClass('blast-r1') ||
        n.hasClass('blast-r2') || n.hasClass('blast-r3');
      const keep = showAll || n.data('root') || landmarks.has(n.id()) || singledOut;
      n.toggleClass('nolbl', !keep);
    });
  });
}

let _lodTimer = null;
function scheduleLabelLod() {
  clearTimeout(_lodTimer);
  _lodTimer = setTimeout(applyLabelLod, 80);
}

cy.on('layoutstop pan zoom', () => {
  scheduleMinimapRedraw();
  updateStatusBar();
  scheduleLabelLod();
});

// ── P2: Blast radius mode ─────────────────────────────────────────────────────
function blastSelected() {
  if (blastOn) { exitBlast(); return; }
  let n = cy.$('node:selected').filter(x => !x.isParent());
  if (!n.length) n = cy.nodes('[?root]').filter(x => !x.isParent());
  if (!n.length) { flashBanner('click a node first, then ⊗ blast'); return; }
  enterBlast(n[0].id());
}

function enterBlast(rootId) {
  const root = cy.getElementById(rootId);
  if (!root.length || root.isParent()) return;
  blastOn = true;
  closePeek();

  // Reverse BFS: ring[id] = hop-distance upstream (callers/importers) from root
  const ring = { [rootId]: 0 };
  let frontier = [rootId], k = 0;
  while (frontier.length && k < 4) {
    k++;
    const cur = new Set(frontier); frontier = [];
    cy.edges().forEach(e => {
      if (e.data('kind') === 'reads') return;
      const s = e.data('source'), t = e.data('target');
      if (cur.has(t) && ring[s] === undefined) { ring[s] = k; frontier.push(s); }
    });
  }

  // Apply blast classes + hide unaffected
  let hidden = 0;
  cy.batch(() => {
    cy.nodes().not(':parent').forEach(n => {
      n.removeClass('blast-root blast-r1 blast-r2 blast-r3 dimmed softdim');
      const r = ring[n.id()];
      if (r === undefined) { n.style('display', 'none'); hidden++; }
      else n.addClass(r === 0 ? 'blast-root' : 'blast-r' + Math.min(r, 3));
    });
    cy.nodes(':parent').forEach(p => {
      if (p.children().filter(c => c.style('display') !== 'none').length === 0)
        p.style('display', 'none');
    });
    cy.edges().forEach(e => {
      e.removeClass('blast-path dimmed');
      const rs = ring[e.data('source')], rt = ring[e.data('target')];
      if (rs !== undefined && rt !== undefined && e.data('kind') !== 'reads')
        e.addClass('blast-path');
      else e.style('display', 'none');
    });
  });

  // Concentric ring positions — stored on _blastRingState so the spread slider can re-apply.
  const byRing = {};
  Object.entries(ring).forEach(([id, r]) => (byRing[r] = byRing[r] || []).push(id));
  const maxRingNodes = Math.max(1, ...Object.values(byRing).map(ids => ids.length));
  const ringR = Math.min(150, Math.max(80, Math.round(maxRingNodes * 11.5)));
  _blastRingState = { byRing, ringR };
  applyBlastPositions(_spreadMul);

  progZoom(() => {
    const n = cy.nodes().length;
    const targetZoom = Math.max(0.18, Math.min(0.55, 12 / Math.sqrt(n)));
    cy.zoom(targetZoom);
    cy.center(cy.getElementById(rootId).length ? cy.getElementById(rootId) : cy.elements());
  });

  document.getElementById('main').classList.add('blastmode');
  document.querySelectorAll('.col-head').forEach(el => el.style.opacity = '0');

  if (FX.on) setTimeout(() => {
    const rp = root.renderedPosition();
    shock(rp.x, rp.y, 'orange');
    setTimeout(() => shock(rp.x, rp.y, 'orange'), 175);
  }, 430);

  const direct = (byRing[1] || []).length;
  const trans  = Object.values(ring).filter(r => r >= 2).length;
  document.getElementById('blastbar').style.display = 'flex';
  document.getElementById('blastName').textContent = root.data('label') || rootId;
  document.getElementById('blastMeta').textContent = '· ' + direct + ' direct · ' + trans + ' transitive · ' + hidden + ' unaffected hidden';

  showBlastReport(root, byRing);
  updateStatusBar();
}

function showBlastReport(root, byRing) {
  const ringLabel = ['root', '① direct impact', '② transitive', '③ distant'];
  const ringColor = ['#fb923c', '#fb923c', '#fcd053', '#c8b7ab'];
  let sections = '';
  Object.keys(byRing).map(Number).sort().filter(r => r > 0).forEach(r => {
    const items = byRing[r].map(id => {
      const n = cy.getElementById(id), d = n.data();
      const hasPath = !!d.path;
      return '<li class="edge-li blast-report-item" data-nid="' + escHtml(id) + '" style="cursor:pointer">' +
        '<span style="color:' + ringColor[Math.min(r, 3)] + '">●</span> ' +
        escHtml(d.label || id) +
        (hasPath
          ? ' <button class="blast-goto" data-path="' + escHtml(d.path) + '" data-line="' + (d.line || 1) + '" title="Go to definition">→</button>'
          : '') +
        '<br><span style="color:#5a5a5a;padding-left:12px">' + escHtml(d.path || '') + '</span>' +
        '</li>';
    }).join('');
    sections += '<div class="d-section"><div class="d-title" style="color:' + ringColor[Math.min(r, 3)] + '">' +
      (ringLabel[Math.min(r, 3)] || 'ring ' + r) + ' (' + byRing[r].length + ')</div><ul>' + items + '</ul></div>';
  });

  const detail = document.getElementById('detail');
  detail.classList.add('open');
  detail.innerHTML =
    '<div style="display:flex;gap:10px;align-items:center;margin-bottom:4px">' +
      '<div class="node-icon-lg" style="background:#fb923c14;border:2px solid #fb923c;color:#fb923c">⊗</div>' +
      '<div><div class="d-sig">' + escHtml(root.data('label') || '') + '</div>' +
        '<span class="d-kind" style="background:#fb923c14;color:#fb923c">blast radius</span></div>' +
    '</div>' +
    (sections || '<div class="d-section"><div class="d-title">No upstream impact found in loaded subgraph</div></div>') +
    '<div class="d-section"><div class="d-title">Actions</div>' +
      '<button class="btn-action" data-act="blast-export">⬇ Export blast report (PNG)</button>' +
      '<button class="btn-action hot" data-act="blast-exit">✕ Exit blast view</button>' +
    '</div>';

  const _bexp = detail.querySelector('[data-act="blast-export"]');
  if (_bexp) _bexp.addEventListener('click', exportPng);
  const _bexit = detail.querySelector('[data-act="blast-exit"]');
  if (_bexit) _bexit.addEventListener('click', exitBlast);

  // → Go-to-definition: each button jumps to the node's source file
  detail.querySelectorAll('.blast-goto').forEach(btn => {
    btn.addEventListener('click', e => {
      e.stopPropagation();
      vscode.postMessage({ command: 'goToDefinition', path: btn.dataset.path, line: Number(btn.dataset.line) || 1 });
    });
  });

  // Clicking a list item centres + selects that node in the blast graph
  detail.querySelectorAll('.blast-report-item').forEach(li => {
    li.addEventListener('click', () => {
      const nid = li.dataset.nid;
      if (!nid) return;
      const n = cy.getElementById(nid);
      if (!n.length) return;
      cy.elements().unselect();
      n.select();
      cy.animate({ center: { eles: n } }, { duration: 200 });
    });
  });
}

function exitBlast() {
  blastOn = false;
  _blastRingState = null;
  document.getElementById('blastbar').style.display = 'none';
  document.getElementById('main').classList.remove('blastmode');
  // Restore graph display
  cy.batch(() => {
    cy.elements().forEach(el => {
      el.removeStyle('display');
      el.removeClass('blast-root blast-r1 blast-r2 blast-r3 blast-path');
    });
  });
  document.querySelectorAll('.col-head').forEach(el => {
    el.style.opacity = layoutName === 'flow' ? '1' : '0';
  });
  renderGraph();
  flashBanner('blast view closed');
}

// ── P2: Definition peek panel ─────────────────────────────────────────────────
function peekNode(path, line) {
  if (!path || !line) return;
  peekDefPath = path;
  peekDefLine = line;
  document.getElementById('peekPath').textContent = path + ':' + line;
  document.getElementById('peekBody').innerHTML = '<div style="color:#6a6a6a;padding:12px 14px;font-size:11px">Loading…</div>';
  document.getElementById('peek').classList.add('open');
  vscode.postMessage({ command: 'requestPeek', path, line });

  document.getElementById('peekJumpBtn').onclick = () => {
    vscode.postMessage({ command: 'goToDefinition', path, line });
    closePeek();
  };
}

// Last peek, kept so a new diagnostics overlay can re-mark the open panel
// without asking the host to read the file again.
let _lastPeek = null;

/**
 * Worst severity and messages per line, for the file being peeked.
 * Errors outrank warnings on a line that has both.
 */
function diagByLineFor(path) {
  const out = {};
  (_diagItemsByFile[path] || []).forEach(i => {
    const cur = out[i.line];
    if (!cur) {
      out[i.line] = { severity: i.severity, messages: [i.message] };
      return;
    }
    cur.messages.push(i.message);
    if (i.severity === 'error') cur.severity = 'error';
  });
  return out;
}

/**
 * How many of a file's problems fall outside the peeked window.
 *
 * Counted against the *uncapped* total: `_diagItemsByFile` is clamped to
 * MAX_DIAGNOSTIC_ITEMS_PER_FILE by the host, with the remainder in
 * `_diagTruncated`, and leaving that remainder out here would under-report
 * exactly on the files that have the most wrong with them.
 */
function peekOutsideCount(path, byLine, shownLines) {
  const total = (_diagItemsByFile[path] || []).length + (_diagTruncated[path] || 0);
  let inside = 0;
  shownLines.forEach(no => {
    if (byLine[no]) inside += byLine[no].messages.length;
  });
  return Math.max(0, total - inside);
}

/**
 * Apply diagnostics to the rows already in the peek body.
 *
 * Marks in place rather than re-rendering: overlays land on every keystroke
 * (debounced), and rebuilding `peekBody.innerHTML` would throw away the
 * reader's scroll position each time. Every row carries a `.pk-mark` slot and
 * a `data-line` from the start, so marking one never reflows the code either.
 */
function markPeekDiagnostics(path, defLine) {
  const body = document.getElementById('peekBody');
  if (!body) return;
  const byLine = diagByLineFor(path);
  const shown = [];

  body.querySelectorAll('.pk-ln').forEach(row => {
    const no = Number(row.getAttribute('data-line'));
    shown.push(no);
    const dg = byLine[no];
    row.classList.remove('pk-err', 'pk-warn');
    const mark = row.querySelector('.pk-mark');
    if (!dg) {
      if (mark) { mark.textContent = ''; mark.removeAttribute('title'); }
      return;
    }
    row.classList.add(dg.severity === 'error' ? 'pk-err' : 'pk-warn');
    if (mark) {
      mark.textContent = dg.severity === 'error' ? '⊗' : '⚠';
      mark.title = dg.messages.join('\n');
    }
  });

  // A problem outside the peeked window would otherwise be invisible here, and
  // the panel would read as though the rest of the file were fine.
  const outside = peekOutsideCount(path, byLine, shown);
  const note = document.getElementById('peekOutside');
  if (note) {
    note.textContent = outside > 0
      ? outside + (outside === 1 ? ' more problem' : ' more problems') +
        ' in this file, outside the lines shown'
      : '';
  }
}

function renderPeekPanel(path, defLine, lines) {
  _lastPeek = { path: path, defLine: defLine };
  document.getElementById('peekPath').textContent = path + ':' + defLine;

  const pre = lines.map(({ no, text }) =>
    '<div class="pk-ln' + (no === defLine ? ' hl' : '') + '" data-line="' + no + '">' +
      '<span class="pk-mark"></span>' +
      '<span class="no">' + no + '</span>' +
      '<span class="code">' + escHtml(text) + '</span>' +
    '</div>'
  ).join('');

  const body = document.getElementById('peekBody');
  body.innerHTML = '<pre>' + pre + '</pre><div class="pk-outside" id="peekOutside"></div>';
  document.getElementById('peek').classList.add('open');

  // #688: the same diagnostics the rings and the Problems list use, on the
  // source itself. A ring says the file is broken; this says which line.
  markPeekDiagnostics(path, defLine);

  // Delegated, and bound once per render rather than per row: marking happens
  // again on every overlay, so a per-row listener would have to be rebound
  // each time and would not survive a row that becomes marked later.
  body.onclick = e => {
    const row = e.target.closest ? e.target.closest('.pk-ln') : null;
    if (!row) return;
    if (!row.classList.contains('pk-err') && !row.classList.contains('pk-warn')) return;
    vscode.postMessage({
      command: 'goToDefinition',
      path: path,
      line: Number(row.getAttribute('data-line')) || defLine,
    });
  };
}

/** Re-mark an open peek when a new overlay lands, so it tracks the editor. */
function refreshOpenPeekDiagnostics() {
  if (!_lastPeek) return;
  const peek = document.getElementById('peek');
  if (!peek || !peek.classList.contains('open')) return;
  markPeekDiagnostics(_lastPeek.path, _lastPeek.defLine);
}

function closePeek() {
  document.getElementById('peek').classList.remove('open');
}

// ── Keyboard ───────────────────────────────────────────────────────────────────
document.addEventListener('keydown', e => {
  if (e.key === '/' && document.activeElement !== document.getElementById('searchInput')) {
    e.preventDefault();
    document.getElementById('searchInput').focus();
    return;
  }
  if (e.key === 'Escape') {
    if (document.getElementById('peek').classList.contains('open')) { closePeek(); return; }
    if (blastOn) { exitBlast(); return; }
    cy.elements().removeClass('dimmed');
    document.getElementById('detail').classList.remove('open');
  }
});
document.getElementById('peekBody').addEventListener('keydown', e => {
  if (e.key === 'Enter') { vscode.postMessage({ command: 'goToDefinition', path: peekDefPath, line: peekDefLine }); closePeek(); }
});

// ── FX engine ─────────────────────────────────────────────────────────────────
const bgfx = document.getElementById('bgfx');
const bctx = bgfx.getContext('2d');

const FX = {
  on: !window.matchMedia('(prefers-reduced-motion: reduce)').matches,
  particles: [], dash: 0, frames: [], degraded: false,
};

function sizeBgfx() {
  const r = document.getElementById('main').getBoundingClientRect();
  bgfx.width  = Math.max(1, Math.round(r.width));
  bgfx.height = Math.max(1, Math.round(r.height));
  initParticles();
}

function initParticles() {
  const W = bgfx.width, H = bgfx.height;
  const n = Math.min(80, Math.round(W * H / 22000));
  FX.particles = Array.from({ length: n }, () => ({
    x: Math.random() * W, y: Math.random() * H,
    r: Math.random() * 1.6 + 0.5,
    vx: (Math.random() - 0.5) * 0.2, vy: (Math.random() - 0.5) * 0.2,
    tw: Math.random() * Math.PI * 2,
    hue: Math.random() < 0.85 ? '134,223,134' : '251,146,60',
  }));
}

function drawParticles(t) {
  const W = bgfx.width, H = bgfx.height, P = FX.particles;
  bctx.clearRect(0, 0, W, H);
  for (const p of P) {
    p.x += p.vx; p.y += p.vy;
    if (p.x < -12) p.x = W + 12; if (p.x > W + 12) p.x = -12;
    if (p.y < -12) p.y = H + 12; if (p.y > H + 12) p.y = -12;
    bctx.beginPath();
    bctx.arc(p.x, p.y, p.r, 0, 6.2832);
    bctx.fillStyle = 'rgba(' + p.hue + ',' + (0.10 + 0.16 * (Math.sin(t / 900 + p.tw) + 1) / 2).toFixed(3) + ')';
    bctx.fill();
  }
  // O(n²) links — skip when degraded
  if (!FX.degraded) {
    bctx.lineWidth = 0.5;
    for (let i = 0; i < P.length; i++) {
      for (let j = i + 1; j < P.length; j++) {
        const dx = P[i].x - P[j].x, dy = P[i].y - P[j].y, d2 = dx * dx + dy * dy;
        if (d2 < 8100) {
          bctx.beginPath(); bctx.moveTo(P[i].x, P[i].y); bctx.lineTo(P[j].x, P[j].y);
          bctx.strokeStyle = 'rgba(134,223,134,' + (0.05 * (1 - d2 / 8100)).toFixed(3) + ')';
          bctx.stroke();
        }
      }
    }
  }
}

function updateHalo() {
  const h = document.getElementById('halo');
  const root = cy.nodes('[?root]:visible');
  if (!FX.on || blastOn || !root.length) { h.style.display = 'none'; return; }
  const p = root[0].renderedPosition();
  const z = Math.max(0.5, Math.min(2.2, cy.zoom()));
  h.style.display = 'block';
  h.style.transform = 'translate(' + (p.x - 60) + 'px,' + (p.y - 60) + 'px) scale(' + z + ')';
}

let _fxLast = performance.now();
function fxLoop(t) {
  requestAnimationFrame(fxLoop);
  const dt = t - _fxLast; _fxLast = t;

  // Minimap (throttled to rAF)
  drawMinimap();

  if (!FX.on) return;

  // FPS watchdog: 90-frame rolling avg > 26ms → auto-degrade
  if (!FX.degraded && dt < 120 && document.hasFocus()) {
    FX.frames.push(dt);
    if (FX.frames.length > 90) FX.frames.shift();
    if (FX.frames.length === 90 && FX.frames.reduce((a, b) => a + b, 0) / 90 > 26) {
      return degradeFx();
    }
  }

  drawParticles(t);
  FX.dash -= dt * 0.012;
  // Gate animated marching to visible structural edges only (expensive style write)
  cy.batch(() => {
    cy.edges('[kind="calls"]:visible, .blast-path:visible')
      .forEach(e => e.style('line-dash-offset', FX.dash));
  });
  updateHalo();
}

function bloom(originId) {
  if (!FX.on) return;
  const nodes = cy.nodes().not(':parent');
  if (!nodes.length) return;
  const oEl = originId ? cy.getElementById(originId) : null;
  let o;
  if (oEl && oEl.length && !oEl.isParent()) {
    o = { ...oEl.position() };
  } else {
    const bb = nodes.boundingBox();
    o = { x: (bb.x1 + bb.x2) / 2, y: (bb.y1 + bb.y2) / 2 };
  }
  // Opacity-only ripple — nodes stay at full size throughout so drag/tap
  // is immediately available on the first frame after render.
  nodes.forEach(n => {
    const pos = n.position();
    const d = Math.hypot(pos.x - o.x, pos.y - o.y);
    n.style({ opacity: 0 });
    n.delay(Math.min(280, d * 0.22))
      .animate({ style: { opacity: 1 } }, {
        duration: 320, easing: 'ease-out-cubic',
        complete: () => n.removeStyle('opacity'),
      });
  });
  cy.edges().style('opacity', 0);
  setTimeout(() => cy.edges().animate(
    { style: { opacity: 0.8 } },
    { duration: 270, complete: () => cy.edges().removeStyle('opacity') }
  ), 300);
}

function shock(x, y, cls) {
  if (!FX.on) return;
  const s = document.createElement('div');
  s.className = 'shockwave' + (cls ? ' ' + cls : '');
  s.style.left = x + 'px'; s.style.top = y + 'px';
  document.getElementById('main').appendChild(s);
  s.addEventListener('animationend', () => s.remove(), { once: true });
}

// Pulse = replay bloom animation without fabricated stats
function pulseGraph() {
  if (!cy.nodes().length) return;
  bloom((allNodes.find(n => n.root) || {}).id || null);
  flashBanner('⟳ pulse');
}

function applyFx() {
  document.body.classList.toggle('fx-off', !FX.on);
  document.getElementById('btn-fx').classList.toggle('active', FX.on);
  cy.style().update(); // re-evaluate line-style mappers (dashed vs solid)
  if (!FX.on) {
    bctx.clearRect(0, 0, bgfx.width, bgfx.height);
    cy.edges().removeStyle('line-dash-offset');
    document.getElementById('halo').style.display = 'none';
  } else {
    initParticles();
  }
}

function toggleFx() {
  FX.on = !FX.on; FX.degraded = false; FX.frames = [];
  applyFx();
  flashBanner(FX.on ? '⚡ effects on' : 'effects off — minimal mode');
}

function degradeFx() {
  FX.degraded = true; FX.on = false;
  applyFx();
  flashBanner('⚡ fx auto-disabled — below 60fps');
}

// Cursor spotlight
document.getElementById('main').addEventListener('pointermove', e => {
  if (!FX.on) return;
  const r = document.getElementById('main').getBoundingClientRect();
  document.getElementById('spotlight').style.transform =
    'translate(' + (e.clientX - r.left) + 'px,' + (e.clientY - r.top) + 'px) translate(-50%,-50%)';
});

// Resize observer — must call cy.resize() so Cytoscape's canvas matches the container.
// VS Code creates webviews at 0×0 initially; without cy.resize() on first expand,
// Cytoscape's event canvas stays zero-size and drag/pan never registers.
new ResizeObserver(() => {
  sizeBgfx();
  cy.resize();
  if (cy.nodes().length) progZoom(() => cy.fit(cy.elements(), 55));
}).observe(document.getElementById('main'));

// ── Utility ───────────────────────────────────────────────────────────────────
function escHtml(s) {
  return String(s == null ? '' : s)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}
function escAttr(s) {
  return String(s == null ? '' : s).replace(/'/g, "\\'").replace(/"/g, '&quot;');
}

// ── Bootstrap ─────────────────────────────────────────────────────────────────
sizeBgfx();
// Double-rAF: VS Code webview panels often start at zero height because the flex
// layout hasn't resolved at script-load time.  A single rAF lands before style
// recalculation; the nested one lands after the first paint when #cy has its
// real dimensions, so cy.resize() correctly primes Cytoscape's bounding-rect cache.
requestAnimationFrame(() => requestAnimationFrame(() => {
  cy.resize();
  sizeBgfx();
}));
applyFx();
requestAnimationFrame(fxLoop);

// ── Custom pan / drag / tap ───────────────────────────────────────────────────
// Cytoscape's E(t) bounding-rect check fails in VS Code webviews (the panel can
// start at zero-height; various focus/role mechanics suppress mousedown before it
// reaches Cytoscape). We bypass it entirely: preventDefault on pointerdown stops
// the browser generating a synthetic mousedown, so our handler has sole ownership
// of pan, node-drag, and tap. Cytoscape's mousemove-based hover/tooltip still fires.
;(function () {
  const el = document.getElementById('cy');
  let drag = null;

  function graphXY(cx, cy_c) {
    const bb = el.getBoundingClientRect();
    const p = cy.pan(), z = cy.zoom();
    return { x: (cx - bb.left - p.x) / z, y: (cy_c - bb.top - p.y) / z };
  }

  function nodeAt(gx, gy) {
    let hit = null, bestArea = Infinity;
    cy.nodes(':visible').not(':parent').forEach(n => {
      const pos = n.position();
      const hw = (n.data('w') || 26) / 2, hh = (n.data('h') || 26) / 2;
      if (Math.abs(gx - pos.x) <= hw && Math.abs(gy - pos.y) <= hh) {
        const a = hw * hh;
        if (a < bestArea) { bestArea = a; hit = n; }
      }
    });
    return hit;
  }

  el.addEventListener('pointerdown', e => {
    if (e.button !== 0) return;
    e.preventDefault(); // suppresses synthetic mousedown → Cytoscape E(t) never runs
    el.setPointerCapture(e.pointerId);
    const g = graphXY(e.clientX, e.clientY);
    const node = nodeAt(g.x, g.y);
    drag = node
      ? { kind: 'node', node, gx0: g.x, gy0: g.y, px0: node.position().x, py0: node.position().y, moved: false }
      : { kind: 'pan',  cx0: e.clientX, cy0: e.clientY, pan0: { ...cy.pan() }, moved: false };
  }, { passive: false });

  el.addEventListener('pointermove', e => {
    if (!drag) return;
    if (drag.kind === 'pan') {
      const dx = e.clientX - drag.cx0, dy = e.clientY - drag.cy0;
      if (!drag.moved && Math.hypot(dx, dy) > 3) drag.moved = true;
      if (drag.moved) cy.pan({ x: drag.pan0.x + dx, y: drag.pan0.y + dy });
    } else {
      const g = graphXY(e.clientX, e.clientY);
      if (!drag.moved && Math.hypot(g.x - drag.gx0, g.y - drag.gy0) > 4) drag.moved = true;
      if (drag.moved) {
        drag.node.position({ x: drag.px0 + (g.x - drag.gx0), y: drag.py0 + (g.y - drag.gy0) });
        scheduleMinimapRedraw();
      }
    }
  });

  el.addEventListener('pointerup', e => {
    if (!drag) return;
    const d = drag; drag = null;
    try { el.releasePointerCapture(e.pointerId); } catch (_) { /* noop */ }
    if (d.moved) return;

    // Tap / click — replicate cy.on('tap', ...) since mousedown was suppressed.
    if (d.kind === 'node' && !d.node.isParent()) {
      const n = d.node;
      cy.batch(() => {
        cy.elements().not(':parent').addClass('dimmed');
        n.closedNeighborhood().removeClass('dimmed');
      });
      showDetail(n);
      if (FX.on) {
        const bb = el.getBoundingClientRect();
        shock(e.clientX - bb.left, e.clientY - bb.top);
      }
    } else {
      cy.elements().removeClass('dimmed');
      document.getElementById('detail').classList.remove('open');
      if (FX.on) {
        const bb = el.getBoundingClientRect();
        shock(e.clientX - bb.left, e.clientY - bb.top);
      }
    }
  });

  el.addEventListener('pointercancel', () => { drag = null; });
})();

// ── Node search ───────────────────────────────────────────────────────────
(function () {
  const overlay  = document.getElementById('node-search');
  const input    = document.getElementById('node-search-input');
  const results  = document.getElementById('node-search-results');
  let activeIdx  = -1;
  let _highlighted = null;

  function openSearch() {
    overlay.style.display = 'block';
    input.value = '';
    results.innerHTML = '';
    activeIdx = -1;
    input.focus();
  }

  function closeSearch() {
    overlay.style.display = 'none';
    if (_highlighted) { _highlighted.removeClass('node-search-hit'); _highlighted = null; }
  }

  window.toggleNodeSearch = function() {
    overlay.style.display === 'none' ? openSearch() : closeSearch();
  };

  function renderResults(query) {
    results.innerHTML = '';
    activeIdx = -1;
    if (!query) return;
    const q = query.toLowerCase();
    const matches = [];
    cy.nodes(':visible').not(':parent').forEach(n => {
      const label = n.data('label') || '';
      const id    = n.id() || '';
      if (label.toLowerCase().includes(q) || id.toLowerCase().includes(q)) {
        matches.push(n);
      }
    });
    matches.slice(0, 40).forEach((n, i) => {
      const li = document.createElement('li');
      const kind = n.data('kind') || '';
      li.innerHTML = escHtml(n.data('label') || n.id()) +
        (kind ? '<span class="match-kind">' + escHtml(kind) + '</span>' : '');
      li.setAttribute('role', 'option');
      li.addEventListener('mousedown', e => { e.preventDefault(); selectResult(n); });
      results.appendChild(li);
    });
    if (matches.length > 40) {
      const li = document.createElement('li');
      li.style.color = 'var(--ch-400)';
      li.textContent = '+ ' + (matches.length - 40) + ' more…';
      results.appendChild(li);
    }
  }

  function selectResult(n) {
    closeSearch();
    // Highlight node
    if (_highlighted) _highlighted.removeClass('node-search-hit');
    _highlighted = n;
    n.addClass('node-search-hit');
    // Pan + zoom to node
    cy.animate({ zoom: Math.max(cy.zoom(), 0.8), center: { eles: n } }, { duration: 300 });
    // Show detail
    cy.batch(() => {
      cy.elements().not(':parent').addClass('dimmed');
      n.closedNeighborhood().removeClass('dimmed');
    });
    showDetail(n);
    applyLabelLod();
    setTimeout(() => {
      if (_highlighted === n) {
        n.removeClass('node-search-hit');
        _highlighted = null;
        applyLabelLod();
      }
    }, 2000);
  }

  function setActive(idx) {
    const items = results.querySelectorAll('li');
    items.forEach(li => li.classList.remove('active'));
    activeIdx = Math.max(0, Math.min(idx, items.length - 1));
    if (items[activeIdx]) items[activeIdx].classList.add('active');
  }

  input.addEventListener('input', () => renderResults(input.value.trim()));

  input.addEventListener('keydown', e => {
    const items = results.querySelectorAll('li');
    if (e.key === 'ArrowDown') { e.preventDefault(); setActive(activeIdx + 1); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); setActive(activeIdx - 1); }
    else if (e.key === 'Enter') {
      const active = results.querySelector('li.active');
      if (active) active.dispatchEvent(new MouseEvent('mousedown'));
    }
    else if (e.key === 'Escape') closeSearch();
  });

  // Cmd+F / Ctrl+F toggles, because that is what the same key does in every
  // other find UI. Opening only meant a stray Cmd+F left an overlay the user
  // had to know Escape to dismiss.
  document.addEventListener('keydown', e => {
    if ((e.metaKey || e.ctrlKey) && e.key === 'f') {
      e.preventDefault();
      overlay.style.display === 'none' ? openSearch() : closeSearch();
    }
  });

  // Start closed, explicitly. The markup ships `display:none`, but the panel
  // is created with retainContextWhenHidden, so this DOM can outlive the view
  // that opened the overlay and come back with it still up. Asserting the
  // closed state here means the overlay is only ever open because someone
  // asked for it in this session.
  closeSearch();

  // Click outside to close
  document.addEventListener('mousedown', e => {
    if (overlay.style.display !== 'none' && !overlay.contains(e.target)) closeSearch();
  });
})();

// Wire all toolbar controls — CSP ('nonce-only') blocks onclick= HTML attributes;
// every event handler must be attached via addEventListener from within this script.
document.getElementById('btn-callers').addEventListener('click', () => setDirection('callers'));
document.getElementById('btn-both').addEventListener('click', () => setDirection('both'));
document.getElementById('btn-deps').addEventListener('click', () => setDirection('deps'));
document.getElementById('depthSlider').addEventListener('input', function() { onDepthSlider(this.value); });
document.getElementById('spreadSlider').addEventListener('input', function() {
  const prev = _spreadMul;
  _spreadMul = parseFloat(this.value);
  document.getElementById('spreadVal').textContent = _spreadMul.toFixed(1) + '×';
  if (blastOn) {
    applyBlastPositions(_spreadMul);
  } else {
    (layoutName === 'flow' ? flowPositions : ringPositions)();
  }
  // Zoom out proportionally so the graph stays visible as nodes spread
  const newZoom = Math.max(cy.minZoom(), cy.zoom() * (prev / _spreadMul));
  const rootEl = blastOn
    ? cy.nodes().filter(n => n.data('ring') === 0).first()
    : cy.nodes('[?root]').first();
  cy.zoom(newZoom);
  if (rootEl.length) cy.center(rootEl); else cy.center();
  scheduleMinimapRedraw();
});
document.getElementById('btn-flow').addEventListener('click', () => setLayout('flow'));
document.getElementById('btn-rings').addEventListener('click', () => setLayout('rings'));
document.getElementById('btn-group').addEventListener('click', toggleGrouping);
document.getElementById('btn-vars').addEventListener('click', toggleVars);
document.getElementById('btn-noise').addEventListener('click', toggleNoise);
document.getElementById('chip-calls').addEventListener('click', () => toggleEdgeKind('calls'));
document.getElementById('chip-imports').addEventListener('click', () => toggleEdgeKind('imports'));
document.getElementById('btn-fx').addEventListener('click', toggleFx);
document.getElementById('btn-pulse').addEventListener('click', pulseGraph);
document.getElementById('btn-dot').addEventListener('click', exportDot);
document.getElementById('btn-json').addEventListener('click', exportJson);
document.getElementById('btn-png').addEventListener('click', exportPng);
document.getElementById('btn-fit').addEventListener('click', fitView);
document.getElementById('btn-search').addEventListener('click', toggleNodeSearch);
document.getElementById('blastExit').addEventListener('click', exitBlast);
document.getElementById('btn-zoom-in').addEventListener('click', () => zoomBy(1.35));
document.getElementById('btn-zoom-out').addEventListener('click', () => zoomBy(1 / 1.35));
document.getElementById('btn-peek-close').addEventListener('click', closePeek);

// ── P3: Repo-map LOD — overview tile map ──────────────────────────────────

// Navigation stack: [{label, mode, pathPrefix}] — top of stack = current drill level.
// Empty = at repo root overview.
let _navStack = [];
let _overviewActive = false; // true when tilemap is showing instead of Cytoscape

// Switch between Cytoscape canvas (symbol graph) and HTML tile map (overview).
function setViewMode(isOverview) {
  _overviewActive = isOverview;
  // Cytoscape handles both symbol graph and tile map — keep #cy always visible.
  // Col-heads are absolutely positioned inside #main and overlay the tile map; hide them.
  ['colCallers', 'colRoot', 'colDeps'].forEach(id => {
    const el = document.getElementById(id);
    if (el) el.style.display = isOverview ? 'none' : '';
  });
  // Graph-specific toolbar groups hidden in overview.
  const graphOnlyGroups = ['grp-direction', 'grp-depth', 'grp-spread', 'grp-layout', 'grp-edges', 'grp-fx'];
  graphOnlyGroups.forEach(id => {
    const el = document.getElementById(id);
    if (el) el.style.visibility = isOverview ? 'hidden' : '';
  });
}

// Shelf-pack tiles into centred rows (ported from the mockup).
// Sizes tiles proportionally to sqrt(file_count), normalised so the largest = maxW.
function packTiles(nodes) {
  const maxFc = Math.max(1, ...nodes.map(n => n.file_count || 1));
  const MAX_W = 220, MIN_W = 76;
  const scale = fc => Math.max(MIN_W, Math.round(MAX_W * Math.sqrt(fc / maxFc)));
  const sized = nodes.map(n => {
    const w = scale(n.file_count || 1);
    return { ...n, w, h: Math.round(w * 0.62) };
  });
  sized.sort((a, b) => b.w - a.w); // largest first → better row utilisation
  const MAXW = 640, GAP = 50;
  const rows = [[]], rws = [0];
  sized.forEach(t => {
    const last = rows.length - 1;
    if (rws[last] + t.w + GAP > MAXW && rows[last].length) {
      rows.push([]); rws.push(0);
    }
    rows[rows.length - 1].push(t);
    rws[rws.length - 1] += t.w + GAP;
  });
  let y = 0; const out = [];
  rows.forEach(row => {
    const rowW = row.reduce((s, t) => s + t.w, 0) + GAP * (row.length - 1);
    const rowH = Math.max(...row.map(t => t.h));
    let x = -rowW / 2;
    row.forEach(t => { out.push({ ...t, x: x + t.w / 2, y: y + rowH / 2 }); x += t.w + GAP; });
    y += rowH + GAP + 14;
  });
  const totalH = y;
  out.forEach(t => t.y -= totalH / 2);
  return out;
}

// Render the repo-map or package-drill tile map using Cytoscape nodes (matches mockup design).
function renderOverview(data, serverMode, pathPrefix) {
  setViewMode(true);
  const nodes = data.nodes || [];
  const edges = data.edges || [];
  const realNodes = nodes.filter(n => !n.ghost);
  const ghostNodes = nodes.filter(n => n.ghost);

  // Cytoscape uses element ids inside selectors, so a package-path id with
  // whitespace / # / ()[] breaks the canvas the same way query mode did before
  // sanitizeGraphIds. Map every tile/ghost id to a safe canvas handle; the real
  // path stays in each tile's `_raw` payload, which drill-in navigation reads,
  // so navigation is unaffected. No-op when all ids are already selector-safe.
  const _ovMap = new Map();
  let _ovC = 0;
  const _ovUnsafe = /[\s`#()[\]]/;
  const _ovNeedsMap = nodes.some(n => n && n.id != null && _ovUnsafe.test(String(n.id)));
  const sid = (raw) => {
    if (!_ovNeedsMap) return String(raw);
    const key = String(raw);
    let m = _ovMap.get(key);
    if (m === undefined) { m = 'o' + _ovC++; _ovMap.set(key, m); }
    return m;
  };

  cy.elements().remove();
  cy.off('dbltap', 'node');
  cy.off('zoom.overview');

  if (!realNodes.length) {
    cy.add([{ data: { id: '__empty', label: 'No packages found.\nRun travsr init to index this repo.', kind: 'pkg', w: 240, h: 80 }, position: { x: 0, y: 0 } }]);
    cy.layout({ name: 'preset' }).run(); cy.fit();
    renderBreadcrumb();
    return;
  }

  // Pack real tiles and add as Cytoscape nodes
  const packed = packTiles(realNodes);
  packed.forEach(t => {
    cy.add({ data: {
      id: sid(t.id),
      label: (t.label || t.id) + (t.file_count ? '\n' + t.file_count + ' files' : ''),
      kind: t.kind || 'pkg',
      w: t.w, h: t.h, tile: 1, file_count: t.file_count || 0,
      // Real path preserved for drill-in (tilemapDrillIn reads this, not the id).
      _raw: JSON.stringify({ id: t.id, label: t.label, file_count: t.file_count }),
    }, position: { x: t.x, y: t.y } });
  });

  // Ghost port nodes positioned to the right
  const span = packed.length ? Math.max(...packed.map(t => t.x + t.w / 2)) : 300;
  ghostNodes.forEach((g, i) => {
    cy.add({ data: { id: sid(g.id), label: g.label || g.id, kind: 'ghost', w: 110, h: 46 },
             position: { x: span + 200, y: -60 + i * 96 } });
  });

  // Edges between tiles — width scales with import count
  const edgeSeen = new Set();
  edges.forEach(e => {
    if (!e.source || !e.target) return;
    const src = sid(e.source);
    const tgt = sid(e.target);
    const key = src + '->' + tgt;
    if (edgeSeen.has(key)) return;
    edgeSeen.add(key);
    cy.add({ data: { id: key, source: src, target: tgt, kind: 'imports', wgt: e.count || 1 } });
  });

  cy.layout({ name: 'preset' }).run();
  cy.fit(cy.elements(), 60);

  // Double-tap (Cytoscape event) → drill into a tile; ghost nodes are read-only ports
  cy.off('dbltap', 'node');
  cy.on('dbltap', 'node', evt => {
    const n = evt.target;
    if (n.data('kind') === 'ghost') return; // ghost ports don't drill
    try { tilemapDrillIn(JSON.parse(n.data('_raw') || '{}'), serverMode, pathPrefix); }
    catch { /* ignore */ }
  });

  // Zoom-to-drill: zoom in past threshold → drill into closest tile;
  // zoom out past threshold → go up one level (matches mockup lines 1002–1020)
  cy.off('zoom.overview');
  let _zoomTimer = null;
  cy.on('zoom.overview', () => {
    if (!_overviewActive) return;
    clearTimeout(_zoomTimer);
    _zoomTimer = setTimeout(() => {
      const z = cy.zoom();
      if (z > 1.9) {
        // Find node closest to viewport centre and drill into it
        const ext = cy.extent();
        const cx = (ext.x1 + ext.x2) / 2, cy2 = (ext.y1 + ext.y2) / 2;
        let closest = null, bestDist = Infinity;
        cy.nodes().filter(n => n.data('kind') !== 'ghost').forEach(n => {
          const p = n.position();
          const d = Math.hypot(p.x - cx, p.y - cy2);
          if (d < bestDist) { bestDist = d; closest = n; }
        });
        if (closest) {
          try { tilemapDrillIn(JSON.parse(closest.data('_raw') || '{}'), serverMode, pathPrefix); }
          catch { /* ignore */ }
        }
      } else if (z < 0.42 && _navStack.length > 0) {
        // Go up one level in the breadcrumb stack
        navigateTo(_navStack.length - 2); // -1 = root when stack has 1 entry
      }
    }, 200);
  });

  renderBreadcrumb();
  updateStatusBar();
  document.getElementById('hint').style.display = 'none';
  // Trigger bloom on the largest tile for visual FX parity with the mockup
  if (FX.on && packed.length) {
    setTimeout(() => bloom(packed[0].id), 120);
  }
}

// Drill into a tile (from overview → package drill, or from package drill → file graph).
function tilemapDrillIn(tile, currentServerMode, currentPathPrefix) {
  if (currentServerMode === 'overview') {
    // Repo overview → package drill: show files inside this package
    const prefix = (tile.label || tile.id.replace(/^pkg:/, '')) + '/';
    _navStack.push({ label: tile.label || tile.id, mode: 'overview', pathPrefix: currentPathPrefix });
    const reqId = ++currentReqId;
    vscode.postMessage({ command: 'query', query: '', direction, depth, kind_filter: '', mode: 'overview', path_prefix: prefix, reqId });
  } else if (currentServerMode === 'prefix') {
    // Package drill → file: open file-level import graph for this file
    const filePath = tile.id.replace(/^file:/, '');
    _navStack.push({ label: tile.label || filePath.split('/').pop() || filePath, mode: 'overview', pathPrefix: currentPathPrefix });
    const reqId = ++currentReqId;
    // Show file-level import graph
    vscode.postMessage({ command: 'query', query: filePath, direction: 'both', depth: 2, kind_filter: 'file', reqId });
  }
  renderBreadcrumb();
}

// Render the breadcrumb nav from _navStack + the current view's label.
function renderBreadcrumb() {
  const bc = document.getElementById('breadcrumb');
  if (!_navStack.length && !_overviewActive) {
    bc.classList.remove('open');
    return;
  }
  bc.classList.add('open');

  const items = [
    { label: '⌂ repo', idx: -1 },
    ..._navStack.map((s, i) => ({ label: s.label, idx: i })),
  ];

  bc.innerHTML = items.map((item, i) => {
    const isLast = i === items.length - 1 && !_overviewActive;
    return (
      (i > 0 ? '<span class="bc-sep" aria-hidden="true">›</span>' : '') +
      '<span class="bc-crumb' + (isLast ? ' active' : '') + '"' +
      ' role="link" tabindex="0" data-bc-idx="' + item.idx + '"' +
      ' aria-current="' + (isLast ? 'page' : 'false') + '">' +
      escHtml(item.label) + '</span>'
    );
  }).join('');

  // Wire breadcrumb clicks
  bc.querySelectorAll('.bc-crumb:not(.active)').forEach(el => {
    el.addEventListener('click', () => {
      const idx = Number(el.getAttribute('data-bc-idx'));
      navigateTo(idx);
    });
    el.addEventListener('keydown', e => {
      if (e.key === 'Enter') {
        const idx = Number(el.getAttribute('data-bc-idx'));
        navigateTo(idx);
      }
    });
  });
}

// Navigate to breadcrumb entry at idx (-1 = repo root, >=0 = navStack entry).
function navigateTo(idx) {
  if (idx === -1) {
    // Back to repo overview
    _navStack = [];
    const reqId = ++currentReqId;
    vscode.postMessage({ command: 'query', query: '', direction, depth, kind_filter: '', mode: 'overview', path_prefix: '', reqId });
  } else if (idx < _navStack.length) {
    const target = _navStack[idx];
    _navStack = _navStack.slice(0, idx);
    const reqId = ++currentReqId;
    vscode.postMessage({ command: 'query', query: '', direction, depth, kind_filter: '', mode: target.mode, path_prefix: target.pathPrefix, reqId });
  }
}

// Show initial hint
document.getElementById('hint').innerHTML =
  'Type a symbol name and press <kbd>Enter</kbd> to explore the graph';

// Graph panel starts empty — the sidebar Repo Files tree is the entry point now.
