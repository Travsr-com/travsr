// travsr ui (#1036): the host side of the extension's graph view.
//
// media/graph.js runs unchanged. In VS Code it talks to graph.ts through
// acquireVsCodeApi().postMessage and window "message" events; here this file
// plays graph.ts's part over HTTP, answering each message with the same
// payload graph.ts would post back. Loaded before graph.js so the API exists
// when graph.js asks for it.
"use strict";

(function () {
  const ui = window.TravsrUi;
  const pending = [];
  let ready = false;
  let saved;

  window.TRAVSR_INIT = { logoUri: "/assets/icon.png" };
  window.acquireVsCodeApi = function () {
    return {
      postMessage(msg) { if (ready) handle(msg); else pending.push(msg); },
      getState() { return saved; },
      setState(s) { saved = s; },
    };
  };

  // graph.ts -> webview. Same-window postMessage keeps graph.js's own listener
  // as the only consumer, exactly as in the webview.
  function post(msg) { window.postMessage(msg, location.origin); }

  function parseGraph(raw) {
    for (const text of [raw, ui.stripEnvelope(raw)]) {
      try { return JSON.parse(text); } catch (_) { /* try the next form */ }
    }
    return { nodes: [], edges: [] };
  }

  // The query the canvas is showing, re-run when the index changes.
  let lastQuery = null;

  // GraphPanel.query, over HTTP.
  async function query(m) {
    lastQuery = m;
    const mode = m.mode || "";
    const pathPrefix = m.path_prefix || "";
    document.title = mode === "overview"
      ? (pathPrefix ? "Travsr: " + pathPrefix : "Travsr: Repo Map")
      : m.kind_filter === "file" ? "Travsr: File Graph" : "Travsr: " + m.query;

    // Stats run beside the graph and never hold up the render, as in graph.ts.
    ui.tool("get_graph_stats").then((raw) => {
      const mm = /Nodes:\s*([\d,]+)/i.exec(ui.stripEnvelope(raw));
      const nodeCount = mm ? parseInt(mm[1].replace(/,/g, ""), 10) : 0;
      post({ command: "freshness", nodeCount, state: nodeCount > 0 ? "fresh" : "stale" });
    }, () => {});

    let data = { nodes: [], edges: [] };
    try {
      data = parseGraph(await ui.tool("get_graph_json", {
        query: m.query || "",
        direction: m.direction || "both",
        depth: String(m.depth || 2),
        kind_filter: m.kind_filter || "",
        mode,
        path_prefix: pathPrefix,
      }));
    } catch (_) {
      // An unreachable server is reported by the strip; the canvas shows empty.
    }
    const msg = { command: "render", data, query: m.query || "", mode, pathPrefix };
    if (m.reqId !== undefined) msg.reqId = m.reqId;
    post(msg);
  }

  function openNew(path, params) {
    window.open(ui.withRepo(path, params), "_blank", "noopener");
  }

  function download(name, href) {
    const a = document.createElement("a");
    a.href = href;
    a.download = name;
    document.body.append(a);
    a.click();
    a.remove();
  }

  async function handle(m) {
    switch (m && m.command) {
      case "query":
        if (m.query || m.kind_filter === "file" || m.mode === "overview") await query(m);
        break;
      case "goToDefinition": {
        // The page has no editor of its own; hand the location to VS Code.
        const link = ui.editorLink(m.path, m.line);
        if (link) location.href = link;
        break;
      }
      case "showBlastRadius":
        if (m.path) openNew("/blast", { file: m.path });
        break;
      case "showDependencies":
        if (m.path) openNew("/dependencies", { file: m.path });
        break;
      case "exportDot":
        try { await navigator.clipboard.writeText(m.dot); } catch (_) {
          download("graph.dot", URL.createObjectURL(new Blob([m.dot], { type: "text/plain" })));
        }
        break;
      case "exportJson":
        download("graph.json", URL.createObjectURL(new Blob([m.json], { type: "application/json" })));
        break;
      case "exportPng":
        download("graph.png", m.dataUrl);
        break;
      case "requestPeek": {
        let lines;
        try {
          lines = (await ui.api("/api/source", { repo: ui.repo, path: m.path, line: m.line })).lines;
        } catch (_) {
          lines = [{ no: m.line, text: "// " + m.path + ":" + m.line }];
        }
        post({ command: "renderPeek", path: m.path, line: m.line, lines });
        break;
      }
      default:
        break;
    }
  }

  // The travsr.com palette on the canvas. graph.js sets its Cytoscape styles
  // in code, so the CSS theme cannot reach them; these rules are layered on
  // top: functions and calls take the site's orange (the legend in
  // graph.html says so), and file group boxes take its warm neutral instead
  // of the extension's dark green. Other kinds keep their legend colours.
  // Re-applied when the light/dark theme flips.
  function themeCanvas() {
    if (typeof cy === "undefined" || !cy.style) return;
    const css = getComputedStyle(document.body);
    const v = (name, fallback) => css.getPropertyValue(name).trim() || fallback;
    const accent = v("--tsr-accent", "#E08C30");
    const light = document.body.classList.contains("vscode-light");
    const rgb = light ? "207,106,10" : "224,140,48";
    cy.style()
      .selector('node[kind="function"], node[kind="method"], node[kind="constructor"]')
      .style({ "background-color": accent, "border-color": accent })
      .selector('edge[kind="calls"]')
      .style({
        "line-color": "rgba(" + rgb + ",0.30)",
        "target-arrow-color": "rgba(" + rgb + ",0.30)",
        "line-gradient-stop-colors": ["rgba(" + rgb + ",0.10)", "rgba(" + rgb + ",0.60)"],
      })
      // Labels in the site's text colour, outlined in its background, so they
      // read on either theme (graph.js outlines them near-black for dark).
      .selector("node, node[?root], node.lbl-keep, node:selected")
      .style({ color: v("--tsr-text", "#F0EBE6"), "text-outline-color": v("--tsr-bg", "#1A1A1A") })
      // Repo map tiles: a filled card in the accent with a label that stays
      // on at any zoom, since the map is read by its names. graph.js's 11%
      // fill and 8px label cut-off left a wide repo as bare edges.
      .selector('node[kind="pkg"]')
      .style({
        "background-color": accent, "background-opacity": light ? 0.12 : 0.16,
        "border-color": accent, "border-width": 2,
        color: v("--tsr-text", "#F0EBE6"), "min-zoomed-font-size": 0, "font-size": "13px",
        // Folder paths have no spaces to wrap at and spilled into neighbours.
        "text-overflow-wrap": "anywhere",
      })
      // Import counts between tiles and file groups: quieter lines, and the
      // count only once zoomed in far enough to read it.
      .selector("edge[wgt]")
      .style({
        opacity: 0.55, "min-zoomed-font-size": 9,
        color: v("--tsr-muted", "#9B9590"),
        // graph.js boxes the count in near-black, which showed as grey
        // squares strewn over the light canvas.
        "text-background-color": v("--tsr-bg", "#1A1A1A"),
      })
      .selector(":parent")
      .style({
        "background-color": light ? "#EDE8E3" : "#242424",
        // Solid: Cytoscape drops the alpha of an rgba() border colour, so the
        // site's 12% white rendered as full white.
        "border-color": light ? "#D9D0C8" : "#3A3836",
        color: v("--tsr-muted", "#9B9590"),
        "text-background-color": v("--tsr-bg", "#1A1A1A"),
      })
      .update();
  }
  new MutationObserver(themeCanvas).observe(document.body, { attributes: true, attributeFilter: ["class"] });

  // Suggestions under the search box, so a symbol can be picked rather than
  // typed in full. search_symbol answers one line per symbol:
  //   fn:browse_tool_call (function) <dash> crates/travsr-mcp/src/server.rs:370
  // Imports are dropped (they are not graph roots) and names that start with
  // what was typed come first. Picking one runs graph.js's own Enter search.
  const SUGGEST_MIN = 2;
  const SUGGEST_MAX = 12;
  const SUGGEST_LINE = /^\s*\w+:(.+?) \(([\w ]+)\) \S+ (.+?)(?::(\d+))?\s*$/;

  function parseSuggestions(raw, typed) {
    const low = typed.toLowerCase();
    const seen = new Set();
    const out = [];
    for (const line of ui.stripEnvelope(raw).split("\n")) {
      const m = SUGGEST_LINE.exec(line);
      if (!m || m[2] === "import") continue;
      const name = m[1].split("::").pop();
      const key = name + "|" + m[3] + "|" + (m[4] || "");
      if (seen.has(key)) continue;
      seen.add(key);
      const n = name.toLowerCase();
      const rank = n === low ? 0 : n.startsWith(low) ? 1 : n.includes(low) ? 2 : 3;
      out.push({ name, kind: m[2], path: m[3], line: m[4] || "", rank, i: out.length });
    }
    return out.sort((a, b) => a.rank - b.rank || a.i - b.i).slice(0, SUGGEST_MAX);
  }

  function setupSuggestions() {
    const input = document.getElementById("searchInput");
    const wrap = document.getElementById("searchWrap");
    if (!input || !wrap) return;
    const list = document.createElement("ul");
    list.id = "suggest";
    list.setAttribute("role", "listbox");
    list.hidden = true;
    wrap.append(list);
    input.setAttribute("aria-controls", "suggest");
    input.setAttribute("aria-autocomplete", "list");

    let items = [];
    let active = -1;
    let timer = null;
    let seq = 0;

    function close() { list.hidden = true; items = []; active = -1; seq++; }
    function highlight(i) {
      active = i;
      [...list.children].forEach((li, j) => li.classList.toggle("active", j === i));
      if (i >= 0 && list.children[i]) list.children[i].scrollIntoView({ block: "nearest" });
    }
    function pick(s) {
      close();
      input.value = s.name;
      // graph.js submits on Enter; reuse it so direction and depth apply.
      input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    }
    // Without the daemon each lookup opens the index (about 2 s on a big
    // repo), so say it is working rather than leave the box looking dead.
    function searching(typed) {
      if (!list.hidden && items.length) return; // keep the last results up
      const li = document.createElement("li");
      li.className = "empty";
      li.textContent = "Searching for \"" + typed + "\"…";
      list.replaceChildren(li);
      items = [];
      active = -1;
      list.hidden = false;
    }
    function show(found, typed) {
      list.replaceChildren();
      items = found;
      active = -1;
      if (!found.length) {
        const li = document.createElement("li");
        li.className = "empty";
        li.textContent = "No symbol matches \"" + typed + "\"";
        list.append(li);
      }
      found.forEach((s, i) => {
        const li = document.createElement("li");
        li.setAttribute("role", "option");
        const name = document.createElement("span");
        name.className = "s-name";
        name.textContent = s.name;
        const kind = document.createElement("span");
        kind.className = "s-kind";
        kind.textContent = s.kind;
        const where = document.createElement("span");
        where.className = "s-path";
        // The path is right-aligned and clipped from the left (CSS rtl) so the
        // file name stays; the marks keep ":line" from being reordered.
        where.textContent = "‎" + s.path + (s.line ? ":" + s.line : "") + "‎";
        where.title = s.path + (s.line ? ":" + s.line : "");
        li.append(name, kind, where);
        // mousedown, not click: the input's blur would close the list first.
        li.addEventListener("mousedown", (e) => { e.preventDefault(); pick(s); });
        li.addEventListener("mousemove", () => { if (active !== i) highlight(i); });
        list.append(li);
      });
      list.hidden = false;
    }

    input.addEventListener("input", () => {
      clearTimeout(timer);
      const typed = input.value.trim();
      if (typed.length < SUGGEST_MIN) { close(); return; }
      timer = setTimeout(async () => {
        const mine = ++seq;
        searching(typed);
        let found = [];
        try { found = parseSuggestions(await ui.tool("search_symbol", { name: typed }), typed); } catch (_) { return; }
        // Drop a slow answer to an earlier keystroke.
        if (mine === seq && input.value.trim() === typed) show(found, typed);
      }, 180);
    });
    // Registered before graph.js's Enter handler, so a highlighted pick can
    // take Enter instead of searching for the half-typed text.
    input.addEventListener("keydown", (e) => {
      if (list.hidden) return;
      if (e.key === "ArrowDown") { e.preventDefault(); highlight(Math.min(items.length - 1, active + 1)); }
      else if (e.key === "ArrowUp") { e.preventDefault(); highlight(Math.max(-1, active - 1)); }
      else if (e.key === "Escape") { e.preventDefault(); close(); }
      else if (e.key === "Enter") {
        if (active >= 0 && items[active]) { e.preventDefault(); e.stopImmediatePropagation(); pick(items[active]); }
        else close();
      }
    });
    input.addEventListener("blur", () => setTimeout(close, 120));
  }
  setupSuggestions();

  // Start once the repo is known: the extension opens the graph on the active
  // file or the repo map; here the URL says which (?q=symbol, ?file=path).
  async function start() {
    const row = await ui.init("/graph");
    themeCanvas();
    ui.api("/api/status", { repo: ui.repo }).then((s) => ui.setConnection(s.route), () => ui.setConnection(null));
    ready = true;
    if (!row || !row.indexed) {
      const b = document.getElementById("banner");
      if (b) {
        b.textContent = row ? "This repository is not indexed yet. Run travsr init in it." : "No indexed repository. Run travsr init in a repository.";
        b.classList.add("show");
      }
      return;
    }
    const p = new URLSearchParams(location.search);
    const q = p.get("q");
    const file = p.get("file");
    const initial = q
      ? { command: "query", query: q, direction: "both", depth: 2, kind_filter: "" }
      : file
        ? { command: "query", query: file, direction: "both", depth: 2, kind_filter: "file" }
        : { command: "query", query: "", direction: "both", depth: 2, kind_filter: "", mode: "overview", path_prefix: "" };
    // Anything graph.js asked for while the repo was loading goes first; the
    // initial view only if it asked for nothing.
    const queued = pending.splice(0);
    if (!queued.some((m) => m.command === "query")) await handle(initial);
    for (const m of queued) await handle(m);
    // Live refresh: redraw what is on the canvas from the new index. The
    // request id is dropped so graph.js accepts the render even though the
    // user issued no new query.
    ui.watch(() => {
      if (!lastQuery) return undefined;
      const again = Object.assign({}, lastQuery);
      delete again.reqId;
      return query(again);
    });
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start);
  else start();
})();
