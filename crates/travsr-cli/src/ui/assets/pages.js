// travsr ui (#1036): the extension's Callers, Dependencies, Blast radius and
// Repos panels (extension.ts showCallers / showBlastRadius, commands.ts
// registerShowDependencies / buildReposHtml), one per path. Each calls the MCP
// tool the extension calls, with the same arguments. Where the extension asks
// for a symbol or file with an input box, the page has an input; where it
// opens a file, the page hands the location to VS Code with a vscode:// link.
"use strict";

(function () {
  const ui = window.TravsrUi;
  const esc = ui.esc;
  const page = document.getElementById("page");
  const params = new URLSearchParams(location.search);

  // `[edge] label (kind) — path:line ~`, the caller row shape.
  const ROW = /^(?:\[([a-z-]+)\]\s*)?(.+?)\s+\(([^()]+)\)\s+—\s+(.+?)(?::(\d+))?(\s+~)?$/;

  function setQuery(key, value) {
    const u = new URL(location.href);
    if (value) u.searchParams.set(key, value); else u.searchParams.delete(key);
    history.replaceState(null, "", u.toString());
  }

  function lookup(id, label, placeholder, value) {
    return '<form class="lookup" id="lookup"><label class="sel" for="' + id + '">' + esc(label) + "</label>" +
      '<input id="' + id + '" type="search" placeholder="' + esc(placeholder) + '" value="' + esc(value || "") + '" autocomplete="off" spellcheck="false">' +
      '<button class="btn primary" type="submit">Show</button></form>';
  }

  function editorAnchor(path, line, text) {
    const link = ui.editorLink(path, line);
    return link ? '<a href="' + esc(link) + '" title="Open in VS Code">' + esc(text) + "</a>" : esc(text);
  }

  function callerLine(l) {
    const m = ROW.exec(l);
    if (!m) return "<li>" + esc(l) + "</li>";
    const [, edge, label, kind, path, line, heuristic] = m;
    return "<li>" + (edge ? '<span class="muted">[' + esc(edge) + "]</span> " : "") +
      '<a href="' + esc(ui.withRepo("/graph", { q: label })) + '" title="Show in the graph">' + esc(label) + "</a>" +
      ' <span class="muted">(' + esc(kind) + ") —</span> " + editorAnchor(path, line ? Number(line) : null, path + (line ? ":" + line : "")) +
      (heuristic ? ' <span class="muted" title="matched by name, not resolved by type">~</span>' : "") + "</li>";
  }

  // ── Callers ────────────────────────────────────────────────────────────
  async function callers() {
    document.title = "Travsr: Callers";
    const symbol = params.get("symbol") || "";
    page.innerHTML = "<h2>Callers</h2>" + lookup("sym", "Symbol", "symbol name, e.g. VecIndex.knn", symbol) + '<div id="out"></div>';
    document.getElementById("lookup").addEventListener("submit", (e) => {
      e.preventDefault();
      const v = document.getElementById("sym").value.trim();
      setQuery("symbol", v);
      params.set("symbol", v);
      run(v);
    });
    async function run(sym) {
      const out = document.getElementById("out");
      if (!sym) { out.innerHTML = '<p class="sub">Type a symbol and press Show.</p>'; return; }
      document.title = "Callers, " + sym;
      out.innerHTML = '<h3 style="margin:0 0 12px">Callers of <code>' + esc(sym) + '</code></h3><p class="sub"><span class="spinner">⟳</span> Loading…</p>';
      try {
        const raw = await ui.tool("get_callers", { symbol: sym });
        const lines = ui.parseEnvelope(raw);
        const notes = ui.envelopeBody(raw).split("\n").map((l) => l.trim()).filter((l) => /^\[?note:/.test(l));
        out.innerHTML = '<h3 style="margin:0 0 12px">Callers of <code>' + esc(sym) + "</code></h3>" +
          notes.map((n) => '<div class="note-box">' + esc(n.replace(/^\[note:\s*/, "").replace(/\]$/, "")) + "</div>").join("") +
          '<ul class="results">' + (lines.map(callerLine).join("\n") || "<li><em>none</em></li>") + "</ul>";
      } catch (e) {
        out.innerHTML = '<div class="diag error"><div class="diag-t">' + esc(e.message) + "</div></div>";
      }
    }
    run(symbol);
  }

  // ── Dependencies ───────────────────────────────────────────────────────
  async function dependencies() {
    document.title = "Travsr: Dependencies";
    const file = params.get("file") || "";
    page.innerHTML = "<h2>Dependencies</h2>" + lookup("file", "File", "repo-relative path, e.g. src/main.rs", file) + '<div id="out"></div>';
    document.getElementById("lookup").addEventListener("submit", (e) => {
      e.preventDefault();
      const v = document.getElementById("file").value.trim();
      setQuery("file", v);
      run(v);
    });
    async function run(target) {
      const out = document.getElementById("out");
      if (!target) { out.innerHTML = '<p class="sub">Type a file path and press Show.</p>'; return; }
      document.title = "Dependencies, " + target;
      out.innerHTML = '<p class="sub"><span class="spinner">⟳</span> Loading…</p>';
      try {
        const raw = ui.stripEnvelope(await ui.tool("get_dependencies", { file: target, transitive: "true", depth: "3" }));
        const lines = raw.split("\n").map((l) => l.replace(/\s+$/, "")).filter((l) => l.trim());
        const direct = lines.filter((l) => !l.startsWith(" ") && !l.includes("↳"));
        const transitive = lines.filter((l) => l.startsWith(" ") || l.includes("↳"));
        // The extension resolves `./x` specifiers against the file system to
        // make them clickable; the page cannot see the file system, so every
        // entry is shown as the extension shows an unresolved one.
        const li = (spec) => {
          const clean = spec.replace(/^\s*↳\s*/, "").trim();
          const colon = clean.indexOf(":");
          return '<li class="dep-ext" title="' + esc(clean) + '">' + esc(colon >= 0 ? clean.slice(colon + 1) : clean) + "</li>";
        };
        out.innerHTML = '<h3 style="margin:0 0 12px">Dependencies of <code>' + esc(target) + "</code> " +
          '<a class="link-btn" href="' + esc(ui.withRepo("/graph", { file: target })) + '">show in graph</a></h3>' +
          '<ul class="results">' + (direct.map(li).join("\n") || "<li><em>none</em></li>") + "</ul>" +
          (transitive.length ? '<details open><summary style="cursor:pointer;margin:12px 0 6px;font-weight:600;font-size:12px;color:var(--fg-muted)">Transitive (' + transitive.length + ')</summary><ul class="results">' + transitive.map(li).join("\n") + "</ul></details>" : "");
      } catch (e) {
        out.innerHTML = '<div class="diag error"><div class="diag-t">' + esc(e.message) + "</div></div>";
      }
    }
    run(file);
  }

  // ── Blast radius ───────────────────────────────────────────────────────
  async function blast() {
    document.title = "Travsr: Blast radius";
    const file = params.get("file") || "";
    page.innerHTML = "<h2>Blast radius</h2>" + lookup("file", "File", "repo-relative path, e.g. src/main.rs", file) + '<div id="out"></div>';
    let mode = "tree-sitter";
    let lang = { semantic_available: false, install_hint: "", prerequisites: "" };
    let target = file;
    document.getElementById("lookup").addEventListener("submit", (e) => {
      e.preventDefault();
      target = document.getElementById("file").value.trim();
      setQuery("file", target);
      mode = "tree-sitter";
      start();
    });
    function pills() {
      const semTip = lang.semantic_available ? "Semantic analysis, precise cross-file calls and references"
        : lang.install_hint ? "Semantic analysis not yet available. " + lang.install_hint : "Semantic analysis not yet available for this language.";
      return '<div class="pills"><span class="lbl">Analysis:</span>' +
        '<button class="pill-btn" data-mode="tree-sitter" aria-pressed="' + (mode === "tree-sitter") + '" title="Structural analysis, single-file structure and best-effort calls (always available)">Structural</button>' +
        '<button class="pill-btn" data-mode="semantic" aria-pressed="' + (mode === "semantic") + '" title="' + esc(semTip) + '"' + (lang.semantic_available ? "" : " disabled") + ">Semantic" + (lang.semantic_available ? "" : " ▾") + "</button></div>" +
        (!lang.semantic_available && lang.install_hint ? '<div class="note-box mono">' + esc(lang.install_hint) +
          (lang.prerequisites && lang.prerequisites !== "none" ? "<div>needs: " + esc(lang.prerequisites) + "</div>" : "") + "</div>" : "");
    }
    async function render() {
      const out = document.getElementById("out");
      out.innerHTML = '<h3 style="margin:0 0 12px">Blast radius for <code>' + esc(target) + "</code></h3>" + pills() + '<p class="sub"><span class="spinner">⟳</span> Loading…</p>';
      try {
        const raw = await ui.tool("get_blast_radius", { file: target, analysis: mode });
        const files = ui.parseEnvelope(raw);
        const capped = /\[note: showing the \d+ files nearest the change/.test(raw);
        out.innerHTML = '<h3 style="margin:0 0 12px">Blast radius for <code>' + esc(target) + "</code> " +
          '<span class="badge dim">' + files.length + (capped ? "+" : "") + " file" + (files.length === 1 ? "" : "s") + "</span></h3>" + pills() +
          '<ul class="results">' + (files.map((f) => "<li>" + editorAnchor(f, null, f) +
            ' <a class="link-btn" href="' + esc(ui.withRepo("/dependencies", { file: f })) + '">deps</a></li>').join("\n") || "<li><em>none</em></li>") + "</ul>";
      } catch (e) {
        out.innerHTML = '<div class="diag error"><div class="diag-t">' + esc(e.message) + "</div></div>";
      }
    }
    async function start() {
      const out = document.getElementById("out");
      if (!target) { out.innerHTML = '<p class="sub">Type a file path and press Show.</p>'; return; }
      document.title = "Blast radius, " + target;
      try { lang = Object.assign(lang, JSON.parse(ui.stripEnvelope(await ui.tool("get_lang_status", { file: target })))); } catch (_) { /* safe defaults */ }
      render();
    }
    page.addEventListener("click", (e) => {
      const b = e.target.closest(".pill-btn[data-mode]");
      if (!b || b.disabled) return;
      const m = b.getAttribute("data-mode");
      if (m !== mode) { mode = m; render(); }
    });
    start();
  }

  // ── Repos (buildReposHtml) ─────────────────────────────────────────────
  async function repos() {
    document.title = "Travsr: Repos";
    async function render() {
      let rows;
      try {
        rows = ui.stripEnvelope(await ui.tool("repos_list")).split("\n").map((l) => l.replace(/\r$/, "")).filter((l) => l.trim()).map((l) => {
          const p = l.split("\t");
          return { name: p[0] || "", path: p[1] || "", exists: p[2] === "1", status: p[3] };
        });
      } catch (e) {
        page.innerHTML = '<h2>Registered repos</h2><div class="diag error"><div class="diag-t">' + esc(e.message) + "</div></div>";
        return;
      }
      const LABEL = { index_missing: "index deleted", not_indexed: "never indexed" };
      const stale = rows.filter((r) => !r.exists).length;
      page.innerHTML = '<h2>Registered repos</h2><p class="sub">' + rows.length + " repo(s) in ~/.travsr/registry.json · " + stale + " stale.</p>" +
        '<div class="toolbar"><button class="btn ' + (stale > 0 ? "primary" : "") + '" data-copy="travsr repos --prune" title="Copy: travsr repos --prune">Prune stale (' + stale + ")</button>" +
        '<button class="btn" id="refreshBtn">Refresh</button></div>' +
        "<table><thead><tr><th>Name</th><th>DB Path</th><th>Status</th><th></th></tr></thead><tbody>" +
        (rows.map((r) => {
          const openable = r.exists && ui.repos.some((x) => x.name === r.name);
          return '<tr><td class="mono">' + (openable ? '<a href="' + esc("/health?repo=" + encodeURIComponent(r.name)) + '">' + esc(r.name) + "</a>" : esc(r.name)) + "</td>" +
            '<td class="mono muted" style="max-width:260px;overflow:hidden;text-overflow:ellipsis" title="' + esc(r.path) + '">' + esc(r.path) + "</td>" +
            "<td>" + (r.exists ? '<span class="badge ok">active</span>' : '<span class="badge stale">' + esc(LABEL[r.status] || "stale") + "</span>") + "</td>" +
            '<td><button class="x-btn" data-copy="travsr repos --remove ' + esc(r.name) + '" title="Copy: travsr repos --remove ' + esc(r.name) + '">✕</button></td></tr>';
        }).join("\n") || '<tr><td colspan="4" class="empty">No repos registered.</td></tr>') + "</tbody></table>";
    }
    page.addEventListener("click", (e) => { if (e.target.id === "refreshBtn") render(); });
    render();
  }

  // Shared: every "copy the command" button.
  page.addEventListener("click", async (e) => {
    const b = e.target.closest("[data-copy]");
    if (!b) return;
    e.preventDefault();
    const cmd = b.getAttribute("data-copy");
    const label = b.textContent;
    try { await navigator.clipboard.writeText(cmd); b.textContent = "Copied"; } catch (_) { window.prompt("Run this in a terminal:", cmd); }
    setTimeout(() => { b.textContent = label; }, 1200);
  });

  const VIEWS = { "/callers": callers, "/dependencies": dependencies, "/blast": blast, "/repos": repos };

  (async function main() {
    const path = location.pathname.replace(/\/+$/, "");
    const row = await ui.init(path);
    ui.api("/api/status", { repo: ui.repo }).then((s) => ui.setConnection(s.route), () => ui.setConnection(null));
    if (path !== "/repos" && (!row || !row.indexed)) {
      page.innerHTML = '<div class="banner idle verdict"><strong>No graph yet</strong><span>' +
        (row ? esc(row.name) + " is not indexed yet. Run travsr init in " + esc(row.root) + "." : "No indexed repositories found. Run travsr init in a git repository.") +
        '</span><button class="btn primary vact" data-copy="travsr init">Index this repo</button></div>';
      return;
    }
    (VIEWS[path] || callers)();
  })();
})();
