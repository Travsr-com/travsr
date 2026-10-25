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

  // Start once the repo is known: the extension opens the graph on the active
  // file or the repo map; here the URL says which (?q=symbol, ?file=path).
  async function start() {
    const row = await ui.init("/graph");
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
