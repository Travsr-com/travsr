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
  // What the open view re-runs when the index changes; each view sets it to
  // repeat its current query.
  let refreshView = null;

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

  // Suggestions under the lookup box (common.js); a pick submits the form,
  // the same as typing the name and pressing Show.
  function suggestInto(id, search) {
    const input = document.getElementById(id);
    const form = document.getElementById("lookup");
    // A wrapper so the list hangs under the box, not under the label.
    const box = document.createElement("span");
    box.className = "lookup-box";
    input.replaceWith(box);
    box.append(input);
    ui.suggest(input, { anchor: box, search, onPick: () => form.requestSubmit() });
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
    // ?q= as well, the parameter the Graph page takes.
    const symbol = params.get("symbol") || params.get("q") || "";
    page.innerHTML = "<h2>Callers</h2>" + lookup("sym", "Symbol", "symbol name, e.g. VecIndex.knn", symbol) + '<div id="out"></div>';
    suggestInto("sym", ui.suggestSymbols);
    document.getElementById("lookup").addEventListener("submit", (e) => {
      e.preventDefault();
      const v = document.getElementById("sym").value.trim();
      setQuery("symbol", v);
      params.set("symbol", v);
      run(v);
    });
    async function run(sym, quiet) {
      refreshView = () => run(sym, true);
      const out = document.getElementById("out");
      if (!sym) { out.innerHTML = '<p class="sub">Type a few letters of a symbol and pick one, or type its name and press Show.</p>'; return; }
      document.title = "Callers, " + sym;
      if (!quiet) out.innerHTML = '<h3 style="margin:0 0 12px">Callers of <code>' + esc(sym) + '</code></h3><p class="sub"><span class="spinner">⟳</span> Loading…</p>';
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
    suggestInto("file", ui.suggestFiles);
    document.getElementById("lookup").addEventListener("submit", (e) => {
      e.preventDefault();
      const v = document.getElementById("file").value.trim();
      setQuery("file", v);
      run(v);
    });
    async function run(target, quiet) {
      refreshView = () => run(target, true);
      const out = document.getElementById("out");
      if (!target) { out.innerHTML = '<p class="sub">Type part of a file name and pick one, or type its path and press Show.</p>'; return; }
      document.title = "Dependencies, " + target;
      if (!quiet) out.innerHTML = '<p class="sub"><span class="spinner">⟳</span> Loading…</p>';
      try {
        const raw = ui.stripEnvelope(await ui.tool("get_dependencies", { file: target, transitive: "true", depth: "3" }));
        let lines = raw.split("\n").map((l) => l.replace(/\s+$/, "")).filter((l) => l.trim());
        // Every dependency line is a `kind:specifier`; a line with spaces and
        // no such prefix is the tool talking ("no file matched ..."), so it
        // is shown as a note rather than listed as a dependency.
        const isSpec = (l) => /^\s*(↳\s*)?[\w-]+:\S/.test(l) && !/\s\S+\s/.test(l.trim());
        const notes = lines.filter((l) => !isSpec(l));
        lines = lines.filter(isSpec);
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
          notes.map((n) => '<div class="note-box">' + esc(n.trim()) + "</div>").join("") +
          '<ul class="results">' + (direct.map(li).join("\n") || (notes.length ? "" : "<li><em>none</em></li>")) + "</ul>" +
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
    suggestInto("file", ui.suggestFiles);
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
    async function render(quiet) {
      const out = document.getElementById("out");
      if (!quiet) out.innerHTML = '<h3 style="margin:0 0 12px">Blast radius for <code>' + esc(target) + "</code></h3>" + pills() + '<p class="sub"><span class="spinner">⟳</span> Loading…</p>';
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
      // Keeps the chosen analysis mode; only the answer is re-read.
      refreshView = () => (target ? render(true) : undefined);
      const out = document.getElementById("out");
      if (!target) { out.innerHTML = '<p class="sub">Type part of a file name and pick one, or type its path and press Show.</p>'; return; }
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

  // Same comparison the server's `root_key` makes: no `\\?\` prefix, one kind
  // of separator, no trailing one, and no case on Windows paths.
  function rootKey(p) {
    const s = String(p || "").replace(/^\\\\\?\\/, "").replace(/\\/g, "/").replace(/\/+$/, "");
    return /^[A-Za-z]:\//.test(s) ? s.toLowerCase() : s;
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
          // Matched on the repo root, not the name: `repos_list` names a repo
          // by its folder, and two folders can share a name that this server
          // disambiguates. The link carries the server's own name for it.
          const open = r.exists ? ui.repos.find((x) => rootKey(x.root) === rootKey(r.path.replace(/[\\/]\.travsr[\\/]graph\.db$/i, ""))) : null;
          return '<tr><td class="mono">' + (open ? '<a href="' + esc("/health?repo=" + encodeURIComponent(open.name)) + '">' + esc(r.name) + "</a>" : esc(r.name)) + "</td>" +
            '<td class="mono muted" style="max-width:260px;overflow:hidden;text-overflow:ellipsis" title="' + esc(r.path) + '">' + esc(r.path) + "</td>" +
            "<td>" + (r.exists ? '<span class="badge ok">active</span>' : '<span class="badge stale">' + esc(LABEL[r.status] || "stale") + "</span>") + "</td>" +
            '<td><button class="x-btn" data-copy="travsr repos --remove ' + esc(r.name) + '" title="Copy: travsr repos --remove ' + esc(r.name) + '">✕</button></td></tr>';
        }).join("\n") || '<tr><td colspan="4" class="empty">No repos registered.</td></tr>') + "</tbody></table>";
    }
    page.addEventListener("click", (e) => { if (e.target.id === "refreshBtn") render(); });
    refreshView = render;
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

  // ── Context explorer ───────────────────────────────────────────────────
  // What get_context hands an AI agent for a question (the extension's
  // Context Explorer): the same tool and arguments an MCP client sends, laid
  // out so a reader can see why each symbol was picked and what it cost.
  //
  // get_context answers sanitized text:
  //   [index commit: <sha>, embeddings: off]
  //   [retrieval: exact+lexical | coverage 1/3 | confidence: weak ]
  //   ## <section>
  //   <sig> (<kind>) <dash> <path>:<line> [via: caller of x] [score: 0.19]
  //   <rule>                        (then the snippet, when include_snippets)
  //   [90 nodes, 4 with snippets, ~1404 metadata-tokens + ~95 snippet-tokens (...)]
  //   [note: ...]
  const DASH_SEP = " " + String.fromCharCode(0x2014) + " ";
  const SNIPPET_RULE = String.fromCharCode(0x2500).repeat(3);
  const BUDGETS = [1000, 2000, 4096, 8000, 16000];

  /** The sanitizer's entities back to characters (textarea parses, never runs). */
  function decodeEntities(s) {
    const t = document.createElement("textarea");
    t.innerHTML = s;
    return t.value;
  }

  /** One `<sig> (<kind>) <dash> <path>:<line> [k: v]...` line, or null. */
  function parseEntry(line) {
    const sep = line.lastIndexOf(DASH_SEP);
    if (sep <= 0 || /^\s/.test(line)) return null;
    const left = /^(\S.*) \(([^()]+)\)$/.exec(line.slice(0, sep));
    if (!left) return null;
    const right = line.slice(sep + DASH_SEP.length);
    const firstTag = right.indexOf(" [");
    const loc = (firstTag >= 0 ? right.slice(0, firstTag) : right).trim();
    const tags = {};
    right.replace(/\[(\w+): ([^\]]*)\]/g, (_, k, v) => { tags[k] = v.trim(); return ""; });
    const m = /^(.*?)(?::(\d+))?$/.exec(loc);
    return { sig: left[1], kind: left[2], path: m[1], line: m[2] ? Number(m[2]) : null,
      via: tags.via || "", score: tags.score ? Number(tags.score) : null, snippet: "" };
  }

  function parseContext(raw) {
    const text = decodeEntities(ui.envelopeBody(raw));
    const out = { facts: {}, sections: [], notes: [], totals: null, text };
    let section = null;
    let entry = null;
    let inSnippet = false;
    const endSnippet = () => {
      if (entry && entry.snippet) entry.snippet = entry.snippet.replace(/\s+$/, "");
      inSnippet = false;
    };
    for (const line of text.split("\n")) {
      const t = line.trim();
      // Headings, entries and bracket lines start in column 0; snippet code
      // that happens to look like one is indented.
      const head = /^##\s+(.+)$/.exec(line);
      const bracket = /^\[(.*)\]$/.exec(line);
      const parsed = section ? parseEntry(line) : null;
      if (head) {
        endSnippet();
        section = { title: head[1].trim(), entries: [] };
        out.sections.push(section);
        entry = null;
      } else if (parsed) {
        // A new entry also ends the snippet above it.
        endSnippet();
        entry = parsed;
        section.entries.push(entry);
      } else if (t === SNIPPET_RULE && entry) {
        inSnippet = true;
      } else if (inSnippet) {
        if (bracket && !line.startsWith(" ")) { endSnippet(); }
        else { entry.snippet += line + "\n"; continue; }
      }
      if (bracket && !inSnippet && !head) {
        const body = bracket[1];
        if (/^note:/i.test(body)) out.notes.push(body.replace(/^note:\s*/i, ""));
        else if (/\bnodes\b/.test(body) && /tokens/.test(body)) out.totals = body;
        else {
          for (const part of body.split(/\s*[|,]\s*/)) {
            // `key: value`, or the colon-less `coverage 1/3 (...)`.
            const kv = /^([\w -]+?):\s*(.+)$/.exec(part.trim()) || /^(coverage)\s+(.+)$/.exec(part.trim());
            if (kv) out.facts[kv[1].trim().toLowerCase()] = kv[2].trim();
          }
        }
      }
    }
    endSnippet();
    return out;
  }

  /** Token counts out of the totals line, for the budget meter. */
  function tokenUse(totals) {
    if (!totals) return null;
    const num = (re) => { const m = re.exec(totals); return m ? Number(m[1].replace(/,/g, "")) : 0; };
    return {
      nodes: num(/([\d,]+) nodes/),
      snippets: num(/([\d,]+) with snippets/),
      meta: num(/~([\d,]+) metadata-tokens/),
      code: num(/~([\d,]+) snippet-tokens/),
    };
  }

  function contextView() {
    document.title = "Travsr: Context";
    const q0 = params.get("q") || "";
    const b0 = Number(params.get("budget")) || 4096;
    const code0 = params.get("code") === "1";
    page.innerHTML =
      '<div class="phead"><h2>Context explorer</h2></div>' +
      '<p class="sub" style="margin:-4px 0 14px">What Travsr hands an AI agent for a question: the symbols it picks, why, and what they cost in tokens. Ask in plain words or name a symbol.</p>' +
      '<form class="lookup ctx-form" id="lookup">' +
      '<span class="lookup-box ctx-q"><input id="ctxq" type="search" placeholder="e.g. where is the session token validated?" value="' + esc(q0) + '" autocomplete="off" spellcheck="false" maxlength="200"></span>' +
      '<label class="sel" for="ctxb">Budget</label><select id="ctxb">' +
      BUDGETS.map((b) => '<option value="' + b + '"' + (b === b0 ? " selected" : "") + ">" + b.toLocaleString() + " tokens</option>").join("") +
      "</select>" +
      '<label class="ctx-check"><input id="ctxc" type="checkbox"' + (code0 ? " checked" : "") + "> Include code</label>" +
      '<button class="btn primary" type="submit">Explore</button></form>' +
      '<div id="out"></div>';
    const form = document.getElementById("lookup");
    const qEl = document.getElementById("ctxq");
    const bEl = document.getElementById("ctxb");
    const cEl = document.getElementById("ctxc");
    let last = null;      // the parsed answer on screen
    let runId = 0;

    form.addEventListener("submit", (e) => {
      e.preventDefault();
      const q = qEl.value.trim();
      setQuery("q", q);
      setQuery("budget", bEl.value === "4096" ? "" : bEl.value);
      setQuery("code", cEl.checked ? "1" : "");
      run(q);
    });

    // Tips, not canned questions: an example that suits one repo abstains
    // in the next.
    function empty() {
      return '<p class="sub">Type a question and press Explore.</p><ul class="ctx-tips">' +
        "<li>Name a function, type or file to see what is gathered around it.</li>" +
        "<li>Ask in plain words using terms that appear in code names, for example <em>how is the cache invalidated</em>.</li>" +
        "<li>One concept per question gives the most focused answer; Travsr says when a question spans several.</li>" +
        "</ul>";
    }

    async function run(q) {
      const out = document.getElementById("out");
      // A changed index offers a re-run rather than repeating a slow query.
      refreshView = () => {
        if (!last) return;
        const bar = document.getElementById("ctxStale");
        if (bar) bar.hidden = false;
      };
      if (!q) { out.innerHTML = empty(); last = null; return; }
      const mine = ++runId;
      document.title = "Context, " + q;
      const started = Date.now();
      out.innerHTML = '<p class="sub"><span class="spinner">⟳</span> Retrieving context… <span id="ctxTime">0 s</span></p>' +
        '<p class="sub muted" style="font-size:11.5px">Seconds with the daemon running; without it the index is opened cold, which can take a minute or more on a large repo.</p>';
      const tick = setInterval(() => {
        const el = document.getElementById("ctxTime");
        if (el) el.textContent = Math.round((Date.now() - started) / 1000) + " s";
      }, 1000);
      try {
        const raw = await ui.tool("get_context", { query: q, token_budget: bEl.value, include_snippets: cEl.checked ? "true" : "false" });
        if (mine !== runId) return;
        last = parseContext(raw);
        render(q, Number(bEl.value), (Date.now() - started) / 1000);
      } catch (e) {
        if (mine !== runId) return;
        out.innerHTML = '<div class="diag error"><div class="diag-t">' + esc(e.message) + "</div>" +
          (/no answer within/.test(e.message) ? '<div class="diag-d">Start the daemon (<code>travsr daemon start</code>) for fast answers, or try a smaller budget.</div>' : "") + "</div>";
      } finally {
        clearInterval(tick);
      }
    }

    function confidenceClass(c) {
      c = (c || "").toLowerCase();
      return c.startsWith("strong") ? "ok" : c.startsWith("weak") || c.startsWith("partial") ? "warn" : c ? "bad" : "";
    }

    function render(q, budget, secs) {
      const r = last;
      const out = document.getElementById("out");
      const use = tokenUse(r.totals);
      const used = use ? use.meta + use.code : 0;
      const pct = budget ? Math.min(100, Math.round((used / budget) * 100)) : 0;
      // No sections means Travsr abstained: no grounded match for the question.
      const abstained = !r.sections.length;
      const conf = abstained ? "no match" : r.facts.confidence || "";
      const count = r.sections.reduce((n, s) => n + s.entries.length, 0);
      const maxScore = Math.max(0.0001, ...r.sections.flatMap((s) => s.entries.map((e) => e.score || 0)));
      const card = (k, v, cls, sub) => '<div class="card' + (cls ? " " + cls : "") + '"><div class="k">' + esc(k) + '</div><div class="v">' + esc(v) + "</div>" + (sub ? '<div class="s">' + esc(sub) + "</div>" : "") + "</div>";

      let html = '<div id="ctxStale" class="note-box ctx-stale" hidden>The index changed since this answer. <button class="link-btn" type="button" data-rerun="1">Run again</button></div>';
      html += '<div class="ctx-head"><h3>Context for <code>' + esc(q) + "</code></h3>" +
        '<span class="ctx-actions"><button class="btn" type="button" data-copy-ctx="1" title="Copy exactly what the agent receives">Copy for chat</button>' +
        '<button class="btn" type="button" data-raw="1" aria-pressed="false">Raw text</button></span></div>';
      html += '<div class="cards ctx-cards">' +
        card("Confidence", conf ? conf.charAt(0).toUpperCase() + conf.slice(1) : "Unknown", confidenceClass(conf) === "ok" ? "" : confidenceClass(conf) === "warn" ? "warn" : conf ? "bad" : "", r.facts.retrieval || "") +
        card("Symbols", String(use ? use.nodes : count), "", use && use.snippets ? use.snippets + " with code" : "") +
        card("Tokens", "~" + used.toLocaleString(), pct >= 100 ? "warn" : "", "of " + budget.toLocaleString() + " budget") +
        card("Coverage", (r.facts.coverage || "–").replace(/\s*\(.*$/, ""), "", (r.facts.coverage || "").replace(/^[^(]*\(?/, "").replace(/\)$/, "")) +
        card("Answered in", secs < 10 ? secs.toFixed(1) + " s" : Math.round(secs) + " s", "", "embeddings " + (r.facts.embeddings || "off")) +
        "</div>";
      if (use && !abstained) {
        const metaPct = budget ? Math.min(100, (use.meta / budget) * 100) : 0;
        const codePct = budget ? Math.min(100 - metaPct, (use.code / budget) * 100) : 0;
        html += '<div class="ctx-meter" title="~' + use.meta + " metadata + ~" + use.code + " code tokens of " + budget + '"><span class="m-meta" style="width:' + metaPct.toFixed(1) + '%"></span><span class="m-code" style="width:' + codePct.toFixed(1) + '%"></span></div>' +
          '<div class="ctx-legend"><span><i class="m-meta"></i>metadata ~' + use.meta.toLocaleString() + '</span><span><i class="m-code"></i>code ~' + use.code.toLocaleString() + "</span><span>" + pct + "% of budget</span></div>";
      }
      html += r.notes.map((n) => '<div class="note-box">' + esc(n) + "</div>").join("");
      html += '<pre class="ctx-raw" id="ctxRaw" hidden>' + esc(r.text.trim()) + "</pre>";
      html += '<div id="ctxParsed">';
      if (abstained) {
        html += '<p class="sub" style="margin:12px 0 6px">Travsr found nothing it could ground this question in, so an agent would be told that rather than handed guesses. Its explanation:</p>' +
          '<pre class="ctx-raw">' + esc(r.text.trim()) + "</pre>";
      }
      for (const s of r.sections) {
        html += '<details class="hsec ctx-sec" open><summary><span class="nm">' + esc(s.title) + '</span> <span class="badge dim">' + s.entries.length + "</span></summary><ul class=\"ctx-list\">";
        for (const e of s.entries) {
          const name = e.sig.includes(":") ? e.sig.slice(e.sig.indexOf(":") + 1) : e.sig;
          const where = e.path + (e.line ? ":" + e.line : "");
          const bar = e.score != null ? '<span class="ctx-score" title="relevance ' + e.score.toFixed(2) + '"><span style="width:' + Math.max(2, (e.score / maxScore) * 100).toFixed(0) + '%"></span></span>' : "";
          html += '<li class="ctx-item"><div class="ctx-row">' + bar +
            '<span class="s-kind">' + esc(e.kind) + "</span>" +
            '<a class="ctx-name" href="' + esc(ui.withRepo("/graph", { q: e.sig })) + '" title="Show in the graph">' + esc(name) + "</a>" +
            '<span class="ctx-where">' + editorAnchor(e.path, e.line, where) + "</span>" +
            (e.via ? '<span class="ctx-via">' + esc(e.via) + "</span>" : "") + "</div>" +
            (e.snippet ? '<pre class="ctx-code">' + esc(e.snippet) + "</pre>" : "") + "</li>";
        }
        html += "</ul></details>";
      }
      html += "</div>";
      out.innerHTML = html;
    }

    page.addEventListener("click", async (e) => {
      const t = e.target.closest("[data-copy-ctx],[data-raw],[data-rerun]");
      if (!t) return;
      if (t.dataset.rerun) { form.requestSubmit(); return; }
      if (t.dataset.copyCtx && last) {
        try { await navigator.clipboard.writeText(last.text.trim()); t.textContent = "Copied"; } catch (_) { t.textContent = "Copy failed"; }
        setTimeout(() => { t.textContent = "Copy for chat"; }, 1600);
        return;
      }
      if (t.dataset.raw) {
        const raw = document.getElementById("ctxRaw");
        const parsed = document.getElementById("ctxParsed");
        const on = raw.hidden;
        raw.hidden = !on;
        parsed.hidden = on;
        t.setAttribute("aria-pressed", String(on));
      }
    });

    run(q0);
  }

  // ── Terminal ───────────────────────────────────────────────────────────
  // `travsr ask` and `travsr graph`, run as in a shell (terminal.rs): the
  // CLI's own output, with symbols and file:line made clickable. The page
  // sends named fields; the server builds the argv.
  const SIG_KINDS = "fn|method|struct|enum|trait|impl|class|interface|const|static|type|field|variant|var|test|macro|module|constructor|function";
  const LINKS = new RegExp(
    "\\b(" + SIG_KINDS + "):([A-Za-z_$][\\w$.]*)" +                    // signature: fn:name
    "|((?:[\\w.-]+/)+[\\w.-]+\\.[A-Za-z0-9]+)(?::(\\d+))?",              // path: a/b.rs:12
    "g");

  /** One line of CLI output as HTML, signatures and paths linked. */
  function linkify(line) {
    let html = "";
    let at = 0;
    for (const m of line.matchAll(LINKS)) {
      html += esc(line.slice(at, m.index));
      if (m[1]) {
        html += '<a class="t-sig" href="' + esc(ui.withRepo("/graph", { q: m[0] })) + '" title="Show in the graph">' + esc(m[0]) + "</a>";
      } else {
        const link = ui.editorLink(m[3], m[4] ? Number(m[4]) : null);
        html += link ? '<a class="t-path" href="' + esc(link) + '" title="Open in VS Code">' + esc(m[0]) + "</a>" : esc(m[0]);
      }
      at = m.index + m[0].length;
    }
    return html + esc(line.slice(at));
  }

  function terminalView() {
    document.title = "Travsr: Terminal";
    const start = params.get("cmd") === "graph" ? "graph" : "ask";
    const depthOpts = [1, 2, 3, 4, 5, 6].map((d) => '<option value="' + d + '"' + (d === 3 ? " selected" : "") + ">" + d + "</option>").join("");
    page.innerHTML =
      '<div class="phead"><h2>Terminal</h2></div>' +
      '<p class="sub" style="margin:-4px 0 14px">Run <code>travsr ask</code> and <code>travsr graph</code> here, exactly as in a shell, in this repository. Read-only: nothing here changes the index.</p>' +
      '<div class="pills t-tabs" role="tablist">' +
      '<button class="pill-btn" type="button" role="tab" data-tab="ask">travsr ask</button>' +
      '<button class="pill-btn" type="button" role="tab" data-tab="graph">travsr graph</button></div>' +
      // ask
      '<form class="lookup t-form" id="t-ask" data-cmd="ask">' +
      '<span class="lookup-box t-main"><input id="t-ask-q" type="search" placeholder="a question, or a symbol name" autocomplete="off" spellcheck="false" maxlength="200"></span>' +
      '<label class="sel" for="t-ask-f">Format</label><select id="t-ask-f"><option value="table">table</option><option value="json">json</option></select>' +
      '<button class="btn primary" type="submit">Run</button>' +
      '<button class="btn" type="button" data-examples="1" title="travsr ask --examples">Examples</button></form>' +
      // graph
      '<form class="lookup t-form" id="t-graph" data-cmd="graph">' +
      '<span class="lookup-box t-main"><input id="t-graph-q" type="search" placeholder="symbol or file name" autocomplete="off" spellcheck="false" maxlength="200"></span>' +
      '<span class="lookup-box t-path"><input id="t-graph-p" type="search" placeholder="--path (optional)" autocomplete="off" spellcheck="false" maxlength="200"></span>' +
      '<label class="sel" for="t-graph-d">Depth</label><select id="t-graph-d">' + depthOpts + "</select>" +
      '<label class="sel" for="t-graph-r">Direction</label><select id="t-graph-r"><option value="deps">deps</option><option value="callers">callers</option><option value="both">both</option></select>' +
      '<label class="sel" for="t-graph-f">Format</label><select id="t-graph-f"><option value="tree">tree</option><option value="dot">dot</option><option value="json">json</option></select>' +
      '<label class="ctx-check"><input id="t-graph-all" type="checkbox"> whole repo (--all)</label>' +
      '<button class="btn primary" type="submit">Run</button></form>' +
      '<div id="t-log" class="t-log"></div>';

    const forms = { ask: document.getElementById("t-ask"), graph: document.getElementById("t-graph") };
    const log = document.getElementById("t-log");
    const allBox = document.getElementById("t-graph-all");
    const graphOnlyOne = ["t-graph-q", "t-graph-p", "t-graph-d", "t-graph-r"].map((id) => document.getElementById(id));

    // The query box is a symbol on graph, often a symbol on ask; --path is a file.
    for (const [id, search] of [["t-ask-q", ui.suggestSymbols], ["t-graph-q", ui.suggestSymbols], ["t-graph-p", ui.suggestFiles]]) {
      const input = document.getElementById(id);
      ui.suggest(input, { anchor: input.parentElement, search, onPick: () => input.form.requestSubmit() });
    }

    function tab(name) {
      for (const [n, f] of Object.entries(forms)) f.hidden = n !== name;
      document.querySelectorAll(".t-tabs [data-tab]").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.tab === name)));
      setQuery("cmd", name === "ask" ? "" : name);
    }
    tab(start);

    allBox.addEventListener("change", () => graphOnlyOne.forEach((el) => { el.disabled = allBox.checked; }));

    function fields(cmd, extra) {
      if (cmd === "ask") {
        return Object.assign({ query: document.getElementById("t-ask-q").value, format: document.getElementById("t-ask-f").value }, extra);
      }
      if (allBox.checked) return { all: "1", format: document.getElementById("t-graph-f").value };
      return {
        query: document.getElementById("t-graph-q").value,
        path: document.getElementById("t-graph-p").value,
        depth: document.getElementById("t-graph-d").value,
        direction: document.getElementById("t-graph-r").value,
        format: document.getElementById("t-graph-f").value,
      };
    }

    async function run(cmd, extra) {
      const args = fields(cmd, extra);
      const entry = document.createElement("section");
      entry.className = "t-entry";
      entry.innerHTML = '<div class="t-head"><span class="t-cmd">$ travsr ' + esc(cmd) + ' <span class="spinner">⟳</span></span>' +
        '<span class="t-meta">running <span class="t-time">0 s</span></span></div>';
      log.prepend(entry);
      const started = Date.now();
      const tick = setInterval(() => {
        const t = entry.querySelector(".t-time");
        if (t) t.textContent = Math.round((Date.now() - started) / 1000) + " s";
      }, 1000);
      try {
        const r = await ui.api("/api/run/" + cmd, Object.assign({ repo: ui.repo }, args));
        const ok = r.code === 0;
        const secs = r.millis / 1000;
        const out = r.stdout.replace(/\s+$/, "");
        const notes = r.stderr.replace(/\s+$/, "");
        entry.innerHTML =
          '<div class="t-head"><span class="t-cmd">$ ' + esc(r.command) + "</span>" +
          '<span class="t-meta ' + (ok ? "" : "bad") + '">' + (r.code == null ? "timed out" : "exit " + r.code) + " · " + (secs < 10 ? secs.toFixed(1) : Math.round(secs)) + " s</span>" +
          '<span class="t-acts"><button class="link-btn" type="button" data-copy-cmd="1">copy command</button>' +
          '<button class="link-btn" type="button" data-copy-out="1">copy output</button>' +
          '<button class="link-btn" type="button" data-close="1" title="Remove from the log">✕</button></span></div>' +
          (notes ? '<pre class="t-out t-err">' + notes.split("\n").map(linkify).join("\n") + "</pre>" : "") +
          '<pre class="t-out">' + (out ? out.split("\n").map(linkify).join("\n") : '<span class="muted">(no output)</span>') + "</pre>" +
          (r.truncated ? '<div class="note-box">Output cut at 2 MB. Run the command in a shell for all of it.</div>' : "");
        entry.dataset.command = r.command;
        entry.dataset.output = (notes ? notes + "\n" : "") + out;
      } catch (e) {
        entry.innerHTML = '<div class="t-head"><span class="t-cmd">$ travsr ' + esc(cmd) + '</span><span class="t-meta bad">not run</span>' +
          '<span class="t-acts"><button class="link-btn" type="button" data-close="1">✕</button></span></div>' +
          '<div class="diag error"><div class="diag-t">' + esc(e.message) + "</div></div>";
      } finally {
        clearInterval(tick);
      }
    }

    for (const f of Object.values(forms)) {
      f.addEventListener("submit", (e) => { e.preventDefault(); run(f.dataset.cmd); });
    }

    page.addEventListener("click", async (e) => {
      const t = e.target.closest("[data-tab],[data-examples],[data-copy-cmd],[data-copy-out],[data-close]");
      if (!t) return;
      if (t.dataset.tab) { tab(t.dataset.tab); return; }
      if (t.dataset.examples) { run("ask", { examples: "1" }); return; }
      const entry = t.closest(".t-entry");
      if (t.dataset.close) { entry.remove(); return; }
      const text = t.dataset.copyCmd ? entry.dataset.command : entry.dataset.output;
      const label = t.textContent;
      try { await navigator.clipboard.writeText(text || ""); t.textContent = "copied"; } catch (_) { t.textContent = "copy failed"; }
      setTimeout(() => { t.textContent = label; }, 1400);
    });

    // A link like /terminal?cmd=graph&q=foo runs straight away.
    const q0 = params.get("q");
    if (q0) {
      document.getElementById(start === "graph" ? "t-graph-q" : "t-ask-q").value = q0;
      run(start);
    }
  }

  const VIEWS = { "/callers": callers, "/dependencies": dependencies, "/blast": blast, "/context": contextView, "/terminal": terminalView, "/repos": repos };

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
    ui.watch(() => (refreshView ? refreshView() : undefined));
  })();
})();
