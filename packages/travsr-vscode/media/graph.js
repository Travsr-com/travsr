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
};
function nodeColor(kind) {
  return ({ function: C.fn, class: C.cls, file: C.file, interface: C.iface, var: C.vr })[kind] || '#8f7a6c';
}
function nodeShape(kind) {
  return ({ function: 'ellipse', class: 'diamond', file: 'round-rectangle', interface: 'triangle', var: 'round-tag' })[kind] || 'ellipse';
}
function edgeColor(kind) {
  return ({ calls: 'rgba(134,223,134,0.28)', imports: 'rgba(72,72,72,0.42)', reads: 'rgba(252,208,83,0.35)' })[kind] || 'rgba(72,72,72,0.42)';
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
    }},
    { selector: 'node[kind="var"]', style: {
        width: 42, height: 18, 'font-size': '9px', 'background-opacity': 0.22,
        'border-color': '#fcd053', 'border-width': 1.5, opacity: 0.7, display: 'none',
    }},
    { selector: 'node[kind="file"]', style: {
        width: 54, height: 30, 'font-size': '9.5px', 'border-radius': '4px',
    }},
    { selector: 'node.noise', style: {
        'border-color': '#4d4d4d', color: '#6a6a6a', 'border-style': 'dotted', 'background-opacity': 0.06,
    }},
    { selector: ':parent', style: {
        'background-color': '#172017', 'background-opacity': 0.5,
        'border-width': 1, 'border-color': '#2d3a2d', 'border-style': 'solid',
        'text-valign': 'top', 'font-size': '9px', color: '#9ab89a',
        shape: 'round-rectangle', padding: '14px',
        'text-background-color': '#121212', 'text-background-opacity': 0.78,
        'text-background-padding': '3px', 'text-outline-width': 0,
    }},
    { selector: 'node.dimmed', style: { opacity: 0.10 }},
    { selector: 'node.softdim', style: { opacity: 0.22 }},
    { selector: 'node:selected', style: {
        'background-opacity': 0.42, 'border-width': 3, 'border-color': '#ffffff',
    }},
    { selector: 'node.hub', style: { 'border-width': 3.5, 'border-style': 'double' }},
    { selector: 'node.wave-1', style: { 'background-opacity': 0.44 }},
    { selector: 'node.wave-2', style: { 'background-opacity': 0.28 }},
    { selector: 'edge', style: {
        'curve-style': 'bezier',
        width: e => e.data('wgt') ? Math.min(5, 1 + Math.log2(e.data('wgt')) * 0.7) : 1.3,
        'line-color': e => edgeColor(e.data('kind')),
        'line-fill': 'linear-gradient',
        'line-gradient-stop-colors': e => ({
          calls:   ['rgba(134,223,134,0.10)', 'rgba(134,223,134,0.58)'],
          imports: ['rgba(72,72,72,0.18)', 'rgba(100,100,100,0.52)'],
          reads:   ['rgba(252,208,83,0.10)', 'rgba(252,208,83,0.58)'],
        })[e.data('kind')] || ['rgba(72,72,72,0.18)', 'rgba(100,100,100,0.52)'],
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
  wheelSensitivity: 0.3, minZoom: 0.15, maxZoom: 5,
});

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
let peekDefLine  = 0;
let peekDefPath  = '';

let _prog = 0; // programmatic zoom guard (prevents drill logic reacting to cy.fit)
function progZoom(fn) { _prog++; fn(); setTimeout(() => _prog--, 500); }

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
const STRUCTURAL = new Set(['calls', 'imports', 'defines', 'is-implementation', 'overrides', 'ffi/call']);

function assignSignedHops(nodes, edges) {
  const byId = Object.fromEntries(nodes.map(n => [n.id, n]));
  nodes.forEach(n => { n.hop = undefined; });

  const root = nodes.find(n => n.root);
  if (root) {
    root.hop = 0;
    const queue = [root.id];
    while (queue.length) {
      const curId = queue.shift();
      const cur = byId[curId];
      if (!cur) continue;
      for (const e of edges) {
        if (!STRUCTURAL.has(e.kind)) continue;
        if (e.source === curId) {
          const tgt = byId[e.target];
          if (tgt && tgt.hop === undefined) { tgt.hop = cur.hop + 1; queue.push(tgt.id); }
        }
        if (e.target === curId) {
          const src = byId[e.source];
          if (src && src.hop === undefined) { src.hop = cur.hop - 1; queue.push(src.id); }
        }
      }
    }
  }

  // Fallback for unreachable nodes: derive unsigned abs from score
  nodes.forEach(n => {
    if (n.hop === undefined) {
      n.hop = Math.round(-Math.log(Math.max(0.001, n.score || 0.3)) / Math.log(1 / 0.7));
    }
  });
}

// ── Build Cytoscape elements from filtered data ────────────────────────────────
function buildElements() {
  const visNodes = allNodes.filter(n => {
    if (!varsOn && n.kind === 'var') return false;
    if (noiseOn && isNoise(n.path)) return false;
    if (Math.abs(n.hop || 0) > depth) return false;
    if (n.root) return true;
    if (direction === 'callers' && (n.hop || 0) > 0) return false;
    if (direction === 'deps'    && (n.hop || 0) < 0) return false;
    return true;
  });
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

  // Nodes
  visNodes.forEach(n => {
    const d = deg[n.id] || 0;
    const sz = n.root ? 48 : Math.min(44, 20 + d * 4);
    const isHidden = n.kind === 'var' && !varsOn;
    els.push({
      data: {
        id: n.id, label: n.label, kind: n.kind, path: n.path || '',
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

// ── Flow layout: callers left, root center, deps right (O(n), no solver) ──────
function flowPositions() {
  const cols = {};
  cy.nodes().not(':parent').forEach(n => {
    const h = n.data('hop') || 0;
    (cols[h] = cols[h] || []).push(n);
  });
  Object.entries(cols).forEach(([h, nodes]) => {
    nodes.sort((a, b) => (a.data('path') || '').localeCompare(b.data('path') || ''));
    const span = (nodes.length - 1) * 95;
    nodes.forEach((n, i) => n.position({ x: 380 + Number(h) * 275, y: 290 - span / 2 + i * 95 }));
  });
}

// ── Rings layout: concentric by |hop|, callers left hemisphere, deps right ────
function ringPositions() {
  const g = {};
  cy.nodes().not(':parent').forEach(n => {
    const h = n.data('hop') || 0;
    (g[h] = g[h] || []).push(n);
  });
  Object.entries(g).forEach(([h, nodes]) => {
    const hop = Number(h);
    if (hop === 0) { nodes.forEach(n => n.position({ x: 400, y: 300 })); return; }
    const r = Math.abs(hop) * 200;
    const base = hop < 0 ? Math.PI : 0;
    const arc = Math.PI * 0.82;
    nodes.forEach((n, i) => {
      const a = base - arc / 2 + (nodes.length === 1 ? arc / 2 : arc * i / (nodes.length - 1));
      n.position({ x: 400 + Math.cos(a) * r, y: 300 + Math.sin(a) * r * 0.78 });
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

  (layoutName === 'flow' ? flowPositions : ringPositions)();

  // Column header visibility
  const showCols = layoutName === 'flow';
  document.querySelectorAll('.col-head').forEach(el => el.style.opacity = showCols ? '1' : '0');

  progZoom(() => {
    if (FX.on && cy.nodes().length > 0) {
      cy.fit(cy.elements(), 55);
      bloom(bloomOriginId || (allNodes.find(n => n.root) || {}).id || null);
    } else {
      if (cy.nodes().length > 0) cy.layout({ name: 'preset', fit: true, padding: 55,
        animate: false }).run();
    }
  });

  // Noise badge
  const noiseBadge = document.getElementById('noiseBadge');
  if (noiseCount > 0) {
    noiseBadge.textContent = '⊘ ' + noiseCount + ' noise nodes hidden';
    noiseBadge.style.display = 'inline';
  } else {
    noiseBadge.style.display = 'none';
  }

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
    allNodes = (data.nodes || []);
    allEdges = (data.edges || []);
    loadedDepth = depth;
    loadedDirection = direction;

    assignSignedHops(allNodes, allEdges);
    renderGraph(allNodes.find(n => n.root)?.id);

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
});

// ── Search ─────────────────────────────────────────────────────────────────────
function submitQuery(forceFetch) {
  const query = document.getElementById('searchInput').value.trim();
  if (!query) return;

  // Client-side re-filter when within loaded depth/direction
  const depthOk = depth <= loadedDepth;
  const dirOk = direction === loadedDirection || loadedDirection === 'both';
  if (!forceFetch && allNodes.length > 0 && depthOk && dirOk) {
    renderGraph();
    return;
  }

  const reqId = ++currentReqId;
  document.getElementById('statusDepth').textContent = 'depth ' + depth;
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
  renderGraph();
}

function setLayout(l) {
  layoutName = l;
  ['flow', 'rings'].forEach(x => {
    document.getElementById('btn-' + x).classList.toggle('active', x === l);
  });
  renderGraph();
}

function onDepthSlider(v) {
  depth = Number(v);
  document.getElementById('depthVal').textContent = v;
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
  renderGraph();
}

function toggleVars() {
  varsOn = !varsOn;
  document.getElementById('btn-vars').classList.toggle('active-gold', varsOn);
  flashBanner(varsOn ? 'showing exported variable nodes' : 'variable nodes hidden');
  renderGraph();
}

function toggleNoise() {
  noiseOn = !noiseOn;
  document.getElementById('btn-noise').classList.toggle('active-orange', noiseOn);
  flashBanner(noiseOn ? 'noise filter ON — tests & vendor hidden' : 'noise filter OFF');
  renderGraph();
}

function toggleEdgeKind(k) {
  edgeKinds[k] = !edgeKinds[k];
  document.getElementById('chip-' + k).classList.toggle('on', edgeKinds[k]);
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

  if (FX.on && !blastOn) {
    const h1 = evt.target.closedNeighborhood().nodes().difference(evt.target);
    const h2 = h1.closedNeighborhood().nodes().difference(h1).difference(evt.target);
    clearTimeout(tip._wt1); clearTimeout(tip._wt2);
    tip._wt1 = setTimeout(() => h1.addClass('wave-1'), 60);
    tip._wt2 = setTimeout(() => h2.addClass('wave-2'), 175);
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
});

cy.on('tap', 'node', evt => {
  if (evt.target.isParent()) return;
  const n = evt.target;
  cy.batch(() => {
    cy.elements().addClass('dimmed');
    n.closedNeighborhood().removeClass('dimmed');
  });
  showDetail(n);
  if (FX.on && evt.renderedPosition) shock(evt.renderedPosition.x, evt.renderedPosition.y);
});
cy.on('tap', evt => {
  if (evt.target === cy) {
    cy.elements().removeClass('dimmed');
    document.getElementById('detail').classList.remove('open');
  }
  if (FX.on && evt.renderedPosition) shock(evt.renderedPosition.x, evt.renderedPosition.y);
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
    const other = isOut ? e.data('target') : e.data('source');
    const wgt   = e.data('wgt') ? ' <span style="color:#686868">×' + e.data('wgt') + '</span>' : '';
    return '<li class="edge-li"><span class="edge-arrow">' + (isOut ? '→' : '←') + '</span> ' +
      escHtml(other.replace(/^(f::|grp::)/, '')) + ' <span class="edge-type">' + e.data('kind') + '</span>' + wgt + '</li>';
  }).join('');

  const detailEl = document.getElementById('detail');
  detailEl.classList.add('open');
  detailEl.innerHTML =
    '<div style="display:flex;gap:10px;align-items:center;margin-bottom:4px">' +
      '<div class="node-icon-lg" style="background:' + color + '14;border:2px solid ' + color + ';color:' + color + '">' +
        (iconMap[d.kind] || '●') +
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
    '<div class="d-section"><div class="d-title">Actions</div>' +
      (d.path && d.line ? '<button class="btn-action" onclick="peekNode(' + JSON.stringify(d.path) + ',' + (d.line || 0) + ')">↗ Definition peek</button>' : '') +
      (d.path ? '<button class="btn-action" onclick="vscode.postMessage({command:\'goToDefinition\',path:' + JSON.stringify(d.path) + ',line:' + (d.line || 0) + '})">↗ Go to definition</button>' : '') +
      '<button class="btn-action hot" onclick="enterBlast(\'' + escAttr(d.id) + '\')">⊗ Show blast radius</button>' +
      (d.kind === 'file' && d.path ? '<button class="btn-action" onclick="vscode.postMessage({command:\'showDependencies\',path:' + JSON.stringify(d.path) + '})">⊟ Show dependencies</button>' : '') +
      '<button class="btn-action" onclick="copyVName(\'' + escAttr(d.id) + '\')">⧉ Copy VName</button>' +
    '</div>';
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
    nodes: cy.nodes(':visible').not(':parent').map(n => ({
      id: n.id(), label: n.data('label'), kind: n.data('kind'),
      path: n.data('path'), package: n.data('pkg'), score: n.data('score'), line: n.data('line'),
    })),
    edges: cy.edges(':visible').map(e => ({
      source: e.data('source'), target: e.data('target'), kind: e.data('kind'),
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

cy.on('layoutstop pan zoom', () => { scheduleMinimapRedraw(); updateStatusBar(); });

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
  while (frontier.length && k < 6) {
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

  // Concentric ring positions
  const byRing = {};
  Object.entries(ring).forEach(([id, r]) => (byRing[r] = byRing[r] || []).push(id));
  Object.entries(byRing).forEach(([r, ids]) => {
    const R = Number(r) * 210;
    ids.forEach((id, i) => {
      const n = cy.getElementById(id);
      if (!n.length) return;
      if (Number(r) === 0) return n.position({ x: 0, y: 0 });
      const a = -Math.PI / 2 + 2 * Math.PI * i / ids.length;
      n.position({ x: Math.cos(a) * R, y: Math.sin(a) * R * 0.78 });
    });
  });

  progZoom(() => cy.layout({ name: 'preset', fit: true, padding: 80,
    animate: !window.matchMedia('(prefers-reduced-motion: reduce)').matches,
    animationDuration: 350 }).run());

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
      return '<li class="edge-li"><span style="color:' + ringColor[Math.min(r, 3)] + '">●</span> ' +
        escHtml((d.label || id)) + '<br><span style="color:#5a5a5a;padding-left:12px">' + escHtml(d.path || '') + '</span></li>';
    }).join('');
    sections += '<div class="d-section"><div class="d-title" style="color:' + ringColor[Math.min(r, 3)] + '">' +
      (ringLabel[Math.min(r, 3)] || 'ring ' + r) + ' (' + byRing[r].length + ')</div><ul>' + items + '</ul></div>';
  });

  const color = '#fb923c';
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
      '<button class="btn-action" onclick="exportPng()">⬇ Export blast report (PNG)</button>' +
      '<button class="btn-action hot" onclick="exitBlast()">✕ Exit blast view</button>' +
    '</div>';
}

function exitBlast() {
  blastOn = false;
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

function renderPeekPanel(path, defLine, lines) {
  document.getElementById('peekPath').textContent = path + ':' + defLine;
  const pre = lines.map(({ no, text }) => {
    const hl = no === defLine;
    const escaped = escHtml(text);
    return '<div class="pk-ln' + (hl ? ' hl' : '') + '"><span class="no">' + no + '</span><span class="code">' + escaped + '</span></div>';
  }).join('');
  document.getElementById('peekBody').innerHTML = '<pre>' + pre + '</pre>';
  document.getElementById('peek').classList.add('open');
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
  nodes.forEach(n => {
    const tgt = { ...n.position() }, d = Math.hypot(tgt.x - o.x, tgt.y - o.y);
    n.position(o); n.style('opacity', 0.12);
    n.delay(Math.min(480, d * 0.5))
      .animate({ position: tgt, style: { opacity: 1 } }, {
        duration: 430, easing: 'ease-out-cubic',
        complete: () => n.removeStyle('opacity'),
      });
  });
  cy.edges().style('opacity', 0);
  setTimeout(() => cy.edges().animate(
    { style: { opacity: 0.8 } },
    { duration: 300, complete: () => cy.edges().removeStyle('opacity') }
  ), 420);
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

// Resize observer
new ResizeObserver(sizeBgfx).observe(document.getElementById('main'));

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
applyFx();
requestAnimationFrame(fxLoop);

// Show initial hint
document.getElementById('hint').innerHTML =
  'Type a symbol name and press <kbd>Enter</kbd> to explore the graph';
