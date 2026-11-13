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
        '<div class="toolbar"><button class="btn ' + (stale > 0 ? "primary" : "") + '" data-run data-copy="travsr repos --prune" title="travsr repos --prune">Prune stale (' + stale + ")</button>" +
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
            '<td><button class="x-btn" data-run data-copy="travsr repos --remove ' + esc(r.name) + '" title="Remove from the registry: travsr repos --remove ' + esc(r.name) + '">✕</button></td></tr>';
        }).join("\n") || '<tr><td colspan="4" class="empty">No repos registered.</td></tr>') + "</tbody></table>";
    }
    page.addEventListener("click", (e) => { if (e.target.id === "refreshBtn") render(); });
    refreshView = render;
    render();
  }

  // Shared: every command button. One marked data-run runs its command here
  // when the server allows it (loopback, no --allow-remote); the rest copy.
  page.addEventListener("click", async (e) => {
    const b = e.target.closest("[data-copy]");
    if (!b) return;
    e.preventDefault();
    const cmd = b.getAttribute("data-copy");
    if (b.hasAttribute("data-run") && ui.repo && await ui.canRun(cmd)) {
      const row = ui.repoRow;
      if (await ui.runCommand(cmd, ui.repo, row && row.root)) {
        // A page that was waiting for an index has one now.
        if (refreshView && !b.classList.contains("vact")) refreshView(); else location.reload();
      }
      return;
    }
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
    // With code: `[90 nodes, 4 with snippets, ~1404 metadata-tokens + ~95
    // snippet-tokens (...)]`; without: `[142 nodes, ~2347 tokens]`.
    return {
      nodes: num(/([\d,]+) nodes/),
      snippets: num(/([\d,]+) with snippets/),
      meta: num(/~([\d,]+) metadata-tokens/) || num(/~([\d,]+) tokens\b/),
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

  // One entry per command terminal.rs runs: its fields, in form order. A
  // field is a text box (`ph`, optional `suggest` of symbols or files), a
  // select (`opts` as [value, label]) or a checkbox (`check`). Field keys are
  // the names /api/run/<cmd> reads.
  const range = (a, b, dflt) => Array.from({ length: b - a + 1 }, (_, i) => [String(a + i), String(a + i), a + i === dflt]);
  const TCMDS = {
    ask: {
      help: "Ask a question about the code, or name a symbol: the answer agents get, as ranked tables.",
      fields: [
        { k: "query", ph: "a question, or a symbol name", suggest: "sym", main: true },
        { k: "format", label: "Format", opts: [["table", "table"], ["json", "json"]] },
      ],
      extra: '<button class="btn" type="button" data-examples="1" title="travsr ask --examples">Examples</button>',
    },
    graph: {
      help: "The dependency graph around a symbol or file, as a tree, DOT or JSON.",
      fields: [
        { k: "query", ph: "symbol or file name", suggest: "sym", main: true },
        { k: "path", ph: "--path (optional)", suggest: "file" },
        { k: "depth", label: "Depth", opts: range(1, 6, 3) },
        { k: "direction", label: "Direction", opts: [["deps", "deps"], ["callers", "callers"], ["both", "both"]] },
        { k: "format", label: "Format", opts: [["tree", "tree"], ["dot", "dot"], ["json", "json"]] },
        { k: "all", check: "whole repo (--all)" },
      ],
    },
    references: {
      help: "Every place a symbol is used, as path:line: calls, assignments and type uses, not only callers.",
      fields: [
        { k: "query", ph: "symbol name", suggest: "sym", main: true },
        { k: "path", ph: "--path (optional)", suggest: "file" },
        { k: "format", label: "Format", opts: [["text", "text"], ["json", "json"]] },
      ],
    },
    pattern: {
      help: "Text search (git grep) over the indexed files. A POSIX regex, or a literal with --fixed.",
      fields: [
        { k: "query", ph: "pattern, e.g. fn\\s+run_", main: true },
        { k: "scope", ph: "--scope: path prefix or files-importing(Symbol)", suggest: "file" },
        { k: "context", label: "Context", opts: range(0, 10, 0) },
        { k: "fixed", check: "literal (--fixed)" },
        { k: "format", label: "Format", opts: [["text", "text"], ["json", "json"]] },
      ],
    },
    explain: {
      help: "Why ask ranked or skipped a symbol for a question: which terms matched and which thresholds passed.",
      fields: [
        { k: "query", ph: "the question you gave ask", main: true },
        { k: "symbol", ph: "symbol it ranked or skipped", suggest: "sym" },
        { k: "format", label: "Format", opts: [["text", "text"], ["json", "json"]] },
      ],
    },
    status: {
      help: "Index and graph status, as travsr status prints it.",
      fields: [{ k: "verbose", check: "per-language detail (--verbose)" }],
    },
    invariants: {
      help: "Check the architecture rules in architecture-invariants.json against the graph.",
      fields: [{ k: "provenance", label: "Edges", opts: [["ratified", "ratified"], ["", "all"], ["tree-sitter", "tree-sitter"], ["lsif", "lsif"], ["scip", "scip"], ["live", "live"]] }],
    },
  };

  function terminalForm(cmd, spec) {
    const id = (k) => "t-" + cmd + "-" + k;
    const parts = spec.fields.map((f) => {
      if (f.check) return '<label class="ctx-check"><input id="' + id(f.k) + '" type="checkbox"> ' + esc(f.check) + "</label>";
      if (f.opts) {
        return '<label class="sel" for="' + id(f.k) + '">' + esc(f.label) + '</label><select id="' + id(f.k) + '">' +
          f.opts.map(([v, l, sel]) => '<option value="' + esc(v) + '"' + (sel ? " selected" : "") + ">" + esc(l) + "</option>").join("") + "</select>";
      }
      return '<span class="lookup-box ' + (f.main ? "t-main" : "t-path") + '"><input id="' + id(f.k) + '" type="search" placeholder="' + esc(f.ph) +
        '" autocomplete="off" spellcheck="false" maxlength="200"></span>';
    });
    return '<form class="lookup t-form" id="t-' + cmd + '" data-cmd="' + cmd + '">' + parts.join("") +
      '<button class="btn primary" type="submit">Run</button>' + (spec.extra || "") + "</form>";
  }

  function terminalView() {
    document.title = "Travsr: Terminal";
    const start = TCMDS[params.get("cmd")] ? params.get("cmd") : "ask";
    page.innerHTML =
      '<div class="phead"><h2>Terminal</h2></div>' +
      '<p class="sub" style="margin:-4px 0 14px">Run travsr\'s read-only commands here, exactly as in a shell, in this repository. Nothing here changes the index.</p>' +
      '<div class="pills t-tabs" role="tablist">' +
      Object.keys(TCMDS).map((c) => '<button class="pill-btn" type="button" role="tab" data-tab="' + c + '">' + esc(c) + "</button>").join("") + "</div>" +
      '<p class="sub t-help" id="t-help"></p>' +
      Object.entries(TCMDS).map(([c, s]) => terminalForm(c, s)).join("") +
      '<div id="t-log" class="t-log"></div>';

    const forms = Object.fromEntries(Object.keys(TCMDS).map((c) => [c, document.getElementById("t-" + c)]));
    const log = document.getElementById("t-log");
    const el = (cmd, k) => document.getElementById("t-" + cmd + "-" + k);

    for (const [cmd, spec] of Object.entries(TCMDS)) {
      for (const f of spec.fields.filter((x) => x.suggest)) {
        const input = el(cmd, f.k);
        ui.suggest(input, { anchor: input.parentElement, search: f.suggest === "file" ? ui.suggestFiles : ui.suggestSymbols, onPick: () => input.form.requestSubmit() });
      }
    }

    function tab(name) {
      for (const [n, f] of Object.entries(forms)) f.hidden = n !== name;
      document.querySelectorAll(".t-tabs [data-tab]").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.tab === name)));
      document.getElementById("t-help").textContent = TCMDS[name].help;
      setQuery("cmd", name === "ask" ? "" : name);
    }
    tab(start);

    // graph --all ignores the symbol, path, depth and direction.
    const allBox = el("graph", "all");
    allBox.addEventListener("change", () => ["query", "path", "depth", "direction"].forEach((k) => { el("graph", k).disabled = allBox.checked; }));

    function fields(cmd, extra) {
      const out = {};
      for (const f of TCMDS[cmd].fields) {
        const input = el(cmd, f.k);
        if (input.disabled) continue;
        if (f.check) { if (input.checked) out[f.k] = "1"; } else out[f.k] = input.value;
      }
      return Object.assign(out, extra);
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
    if (q0 && el(start, "query")) {
      el(start, "query").value = q0;
      run(start);
    }
  }

  // ── Execution path ─────────────────────────────────────────────────────
  // The extension's "Show Execution Path": the lowest-cost call chain from one
  // symbol to another (get_execution_path), as steps here and drawn on the
  // Graph page. An ambiguous name lists its candidates to pick from.
  function pathView() {
    document.title = "Travsr: Path";
    const from0 = params.get("from") || "";
    const to0 = params.get("to") || "";
    page.innerHTML =
      '<div class="phead"><h2>Execution path</h2></div>' +
      '<p class="sub" style="margin:-4px 0 14px">How control gets from one symbol to another: the lowest-cost call chain through the graph, and what sits next to it.</p>' +
      '<form class="lookup p-form" id="lookup">' +
      '<label class="sel" for="p-from">From</label><span class="lookup-box p-box"><input id="p-from" type="search" placeholder="source symbol" value="' + esc(from0) + '" autocomplete="off" spellcheck="false" maxlength="200"></span>' +
      '<button class="btn p-swap" type="button" data-swap="1" title="Swap from and to" aria-label="Swap from and to">⇄</button>' +
      '<label class="sel" for="p-to">To</label><span class="lookup-box p-box"><input id="p-to" type="search" placeholder="sink symbol" value="' + esc(to0) + '" autocomplete="off" spellcheck="false" maxlength="200"></span>' +
      '<button class="btn primary" type="submit">Find path</button></form><div id="out"></div>';
    const form = document.getElementById("lookup");
    const fromEl = document.getElementById("p-from");
    const toEl = document.getElementById("p-to");
    // A pick fills the box; the search waits until both ends are named.
    for (const input of [fromEl, toEl]) {
      ui.suggest(input, { anchor: input.parentElement, search: ui.suggestSymbols, onPick: () => { if (fromEl.value.trim() && toEl.value.trim()) form.requestSubmit(); else (input === fromEl ? toEl : fromEl).focus(); } });
    }
    form.addEventListener("submit", (e) => {
      e.preventDefault();
      setQuery("from", fromEl.value.trim());
      setQuery("to", toEl.value.trim());
      run();
    });

    function stepRow(s, i, last) {
      const name = s.sig.includes(":") ? s.sig.slice(s.sig.indexOf(":") + 1) : s.sig;
      return '<li class="p-step' + (i === 0 ? " p-first" : "") + (last ? " p-last" : "") + '">' +
        '<span class="p-num">' + (i + 1) + "</span>" +
        '<span class="p-body"><span class="s-kind">' + esc(s.kind) + "</span> " +
        '<a class="p-name" href="' + esc(ui.withRepo("/graph", { q: s.sig })) + '" title="Show in the graph">' + esc(name) + "</a>" +
        '<span class="p-where">' + editorAnchor(s.path, null, s.path) + "</span></span></li>";
    }

    async function run() {
      const from = fromEl.value.trim();
      const to = toEl.value.trim();
      const out = document.getElementById("out");
      refreshView = from && to ? () => run() : null;
      if (!from || !to) {
        out.innerHTML = '<p class="sub">Name a source and a sink symbol, for example a request handler and the function that writes to the store. Bare names work; a full signature such as <code>fn:charge</code> picks one when a name is shared.</p>';
        return;
      }
      document.title = "Path, " + from + " to " + to;
      out.innerHTML = '<p class="sub"><span class="spinner">⟳</span> Finding a path…</p>';
      let r;
      try {
        r = ui.parseExecutionPath(await ui.tool("get_execution_path", { source: from, sink: to }));
      } catch (e) {
        out.innerHTML = '<div class="diag error"><div class="diag-t">' + esc(e.message) + "</div></div>";
        return;
      }
      let html = "";
      if (r.steps.length) {
        const graphLink = ui.withRepo("/graph", { from, to });
        html += '<div class="p-head"><h3>' + esc(r.steps.length - 1) + " step" + (r.steps.length === 2 ? "" : "s") + " from <code>" + esc(from) + "</code> to <code>" + esc(to) + "</code></h3>" +
          '<a class="btn primary" href="' + esc(graphLink) + '">Show on graph</a></div>' +
          '<ol class="p-steps">' + r.steps.map((s, i) => stepRow(s, i, i === r.steps.length - 1)).join("") + "</ol>";
        if (r.nearby.length) {
          html += '<details class="hsec p-near"><summary><span class="nm">Nearby, not on the path</span> <span class="badge dim">' + r.nearby.length + "</span></summary>" +
            '<ul class="results">' + r.nearby.map((s) => "<li>" + '<span class="s-kind">' + esc(s.kind) + "</span> " +
              '<a href="' + esc(ui.withRepo("/graph", { q: s.sig })) + '">' + esc(s.sig) + "</a> " + '<span class="muted">' + editorAnchor(s.path, null, s.path) + "</span></li>").join("") + "</ul></details>";
        }
      } else if (r.ambiguous) {
        // A signature listed twice cannot be picked: the path search takes no
        // file hint, so re-running with it returns this same list.
        const seen = {};
        for (const c of r.candidates) seen[c.sig] = (seen[c.sig] || 0) + 1;
        const shared = r.candidates.some((c) => seen[c.sig] > 1);
        html += '<div class="note-box">The ' + esc(r.ambiguous) + " name matches more than one symbol." + (r.candidates.some((c) => seen[c.sig] === 1) ? " Pick one:" : "") + "</div>" +
          '<ul class="p-cands">' + r.candidates.map((c) => "<li>" +
            (seen[c.sig] === 1
              ? '<button class="link-btn" type="button" data-pick="' + esc(r.ambiguous) + '" data-sig="' + esc(c.sig) + '">' + esc(c.sig) + "</button>"
              : '<span class="p-shared" title="Shared by several files; cannot be told apart here">' + esc(c.sig) + "</span> <span class=\"muted\">(shared name)</span>") +
            ' <span class="s-kind">' + esc(c.kind) + '</span> <span class="muted">' + editorAnchor(c.path, null, c.path) + "</span></li>").join("") + "</ul>" +
          (shared ? '<p class="sub" style="font-size:11.5px">A shared name is defined the same way in several files, and the path search takes no file hint. Start or end at a symbol next to it instead, such as one of its callers (Callers page) or something it calls.</p>' : "");
      } else {
        html += '<div class="diag ' + (/^no path found/.test(r.message) ? "warn" : "error") + '"><div class="diag-t">' + esc(r.message || "No path found.") + "</div></div>";
      }
      html += r.notes.map((n) => '<div class="note-box">' + esc(n) + "</div>").join("");
      out.innerHTML = html;
    }

    page.addEventListener("click", (e) => {
      const t = e.target.closest("[data-swap],[data-pick]");
      if (!t) return;
      if (t.dataset.swap) {
        const f = fromEl.value;
        fromEl.value = toEl.value;
        toEl.value = f;
        if (fromEl.value.trim() && toEl.value.trim()) form.requestSubmit();
        return;
      }
      (t.dataset.pick === "source" ? fromEl : toEl).value = t.dataset.sig;
      form.requestSubmit();
    });

    run();
  }

  // ── Architecture ───────────────────────────────────────────────────────
  // get_architecture_brief, the facts agents get for explaining how a repo is
  // put together, drawn: components in layers from foundation to entry point,
  // dependency cycles, the weighted edges, and what each component owns.
  //
  // The brief is sectioned text:
  //   ## Shape        - 21 components, 50 edges, 7 layers
  //   ## Cycles       - a <-> b   (or a -> b -> c -> a)
  //   ## Components   - name | layer N | X dependents | depends on Y | F files | S symbols
  //   ## Dependency edges   - a -> b (W, code+manifest)
  //   ## What each component owns   ### name, then - key: value
  //   ## Freshness, ## What this brief cannot tell you   - lines
  function parseArchitecture(raw) {
    const out = { header: [], shape: "", cycles: [], comps: [], edges: [], owns: {}, notes: [], caveats: [] };
    let sec = "";
    let owner = null;
    for (const line of ui.envelopeBody(raw).split("\n").map((l) => l.trim())) {
      if (!line) continue;
      const h = /^##\s+(.+)$/.exec(line);
      if (h) { sec = h[1].toLowerCase(); owner = null; continue; }
      const h3 = /^###\s+(.+)$/.exec(line);
      if (h3) { owner = h3[1]; out.owns[owner] = []; continue; }
      if (/^\[note:/.test(line)) { out.notes.push(line.replace(/^\[note:\s*/, "").replace(/\]$/, "")); continue; }
      const item = line.replace(/^-\s+/, "");
      if (!sec) { if (line !== "ARCHITECTURE BRIEF") out.header.push(line); }
      else if (sec.startsWith("shape")) out.shape += (out.shape ? "; " : "") + item;
      else if (sec.startsWith("cycles")) out.cycles.push(item.split(/\s*<?->\s*/).filter(Boolean));
      else if (sec.startsWith("components")) {
        const p = item.split("|").map((x) => x.trim());
        const num = (re, s) => { const m = re.exec(s || ""); return m ? Number(m[1]) : 0; };
        if (p.length >= 6) {
          out.comps.push({ name: p[0], layer: num(/layer (\d+)/, p[1]), dependents: num(/(\d+)/, p[2]), deps: num(/(\d+)/, p[3]), files: num(/(\d+)/, p[4]), symbols: num(/(\d+)/, p[5]) });
        }
      } else if (sec.startsWith("dependency edges")) {
        const m = /^(.+?)\s+->\s+(.+?)\s+\((\d+),\s*([^)]+)\)$/.exec(item);
        if (m) out.edges.push({ from: m[1], to: m[2], weight: Number(m[3]), source: m[4].trim() });
      } else if (sec.startsWith("what each component owns")) {
        if (owner) out.owns[owner].push(item);
      } else if (sec.startsWith("what this brief cannot")) out.caveats.push(item);
    }
    return out;
  }

  function architectureView() {
    document.title = "Travsr: Architecture";
    const prov0 = params.get("provenance") === "all" ? "all" : "ratified";
    page.innerHTML =
      '<div class="phead"><div><h2>Architecture</h2><p class="sub" style="margin:2px 0 0">How this repository is put together: components in layers from foundation to entry point, what depends on what, and any cycles.</p></div>' +
      '<div class="phead-a"><label class="sel">Edges <select id="a-prov" title="Which dependencies to trust">' +
      '<option value="ratified"' + (prov0 === "ratified" ? " selected" : "") + '>ratified</option>' +
      '<option value="all"' + (prov0 === "all" ? " selected" : "") + ">all, incl. live</option></select></label>" +
      '<button class="btn" id="a-refresh">Refresh</button></div></div><div id="out"></div>';
    const provEl = document.getElementById("a-prov");
    let brief = null;
    let selected = params.get("c") || "";

    async function load() {
      const out = document.getElementById("out");
      out.innerHTML = '<p class="sub"><span class="spinner">⟳</span> Reading the architecture…</p>';
      try {
        const raw = await ui.tool("get_architecture_brief", { provenance: provEl.value === "all" ? "" : "ratified", token_budget: "32000" });
        brief = parseArchitecture(raw);
        labels = uniqueLabels(brief.comps.map((c) => c.name));
        if (selected && !brief.comps.some((c) => c.name === selected)) selected = "";
        render();
      } catch (e) {
        out.innerHTML = '<div class="diag error"><div class="diag-t">' + esc(e.message) + "</div></div>";
      }
    }
    refreshView = () => load();

    // The shortest tail of each component's path that no other component
    // shares: a Java tree has many `model` and `service` packages, which the
    // last segment alone showed as identical cards.
    let labels = {};
    function uniqueLabels(names) {
      const parts = Object.fromEntries(names.map((n) => [n, n.split("/")]));
      const out = {};
      for (const n of names) {
        for (let k = 1; k <= parts[n].length; k++) {
          const tail = parts[n].slice(-k).join("/");
          if (!names.some((o) => o !== n && parts[o].slice(-k).join("/") === tail)) { out[n] = tail; break; }
        }
        if (!out[n]) out[n] = n;
      }
      return out;
    }
    const shortName = (n) => labels[n] || n.split("/").pop();

    function render() {
      const b = brief;
      const out = document.getElementById("out");
      if (!b.comps.length) {
        out.innerHTML = '<div class="note-box">' + esc(b.shape || "No components: this index has no indexed source files.") + "</div>";
        return;
      }
      const inCycle = new Set(b.cycles.flat());
      const depsOf = new Set(b.edges.filter((e) => e.from === selected).map((e) => e.to));
      const usersOf = new Set(b.edges.filter((e) => e.to === selected).map((e) => e.from));
      const layers = Math.max(...b.comps.map((c) => c.layer));
      const card = (k, v, cls, note) => '<div class="card' + (cls ? " " + cls : "") + '"><div class="k">' + esc(k) + '</div><div class="v">' + esc(v) + "</div>" + (note ? '<div class="s">' + esc(note) + "</div>" : "") + "</div>";

      let html = '<div class="cards a-cards">' +
        card("Components", String(b.comps.length)) +
        card("Dependencies", String(b.edges.length)) +
        card("Layers", String(layers + 1), "", "0 is the foundation") +
        card("Cycles", String(b.cycles.length), b.cycles.length ? "bad" : "", b.cycles.length ? "should be none" : "none") + "</div>";
      if (b.cycles.length) {
        html += '<div class="diag error a-cycles"><div class="diag-t">Dependency cycle' + (b.cycles.length > 1 ? "s" : "") + "</div>" +
          b.cycles.map((cy) => '<div class="a-cycle">' + cy.map((n) => '<button class="link-btn" type="button" data-comp="' + esc(n) + '">' + esc(n) + "</button>").join(' <span class="muted">⇄</span> ') + "</div>").join("") +
          '<div class="diag-d">Each component reaches the other through its dependencies, so neither can be built, tested or reasoned about alone. Select one to see the edges involved.</div></div>';
      }

      // Layers, foundation at the bottom as in a stack diagram.
      html += '<h3 class="a-h">Layers <span class="muted">(click a component)</span></h3><div class="a-layers">';
      for (let l = layers; l >= 0; l--) {
        const row = b.comps.filter((c) => c.layer === l).sort((x, y) => y.dependents - x.dependents || x.name.localeCompare(y.name));
        html += '<div class="a-layer"><div class="a-ln" title="layer ' + l + '">' + l + '</div><div class="a-row">' +
          row.map((c) => {
            const cls = ["a-comp", c.name === selected ? "sel" : "", depsOf.has(c.name) ? "dep" : "", usersOf.has(c.name) ? "user" : "", inCycle.has(c.name) ? "cyc" : ""].filter(Boolean).join(" ");
            return '<button class="' + cls + '" type="button" data-comp="' + esc(c.name) + '" title="' + esc(c.name) + '">' +
              '<span class="a-name">' + esc(shortName(c.name)) + "</span>" +
              '<span class="a-meta">' + c.dependents + " in · " + c.deps + " out · " + c.files + " files</span></button>";
          }).join("") + "</div></div>";
      }
      html += '</div><div class="a-legend"><span><i class="sel"></i>selected</span><span><i class="dep"></i>it depends on</span><span><i class="user"></i>depends on it</span><span><i class="cyc"></i>in a cycle</span></div>';

      // Detail for the selected component.
      if (selected) {
        const c = b.comps.find((x) => x.name === selected);
        const own = b.owns[selected] || [];
        html += '<div class="hsec a-detail"><div class="a-dhead"><h3>' + esc(selected) + "</h3>" +
          '<button class="link-btn" type="button" data-comp="">clear</button></div><div class="a-dbody">' +
          '<p class="sub">Layer ' + c.layer + " · " + c.files + " files · " + c.symbols + " symbols · " + c.dependents + " dependents · depends on " + c.deps + "</p>" +
          own.map((o) => {
            const kv = /^([^:]+):\s*(.+)$/.exec(o);
            return kv ? '<div class="a-own"><span class="a-k">' + esc(kv[1]) + '</span><span class="a-v">' + esc(kv[2]) + "</span></div>" : '<div class="a-own">' + esc(o) + "</div>";
          }).join("") + "</div></div>";
      }

      // Edges, heaviest first; only the selected component's when one is.
      const shown = selected ? b.edges.filter((e) => e.from === selected || e.to === selected) : b.edges;
      if (!b.edges.length) {
        html += '<h3 class="a-h">Dependencies</h3><div class="note-box">No dependencies between components were resolved, so every component sits in layer 0. ' +
          "Imports that only name packages, or a language whose full analysis has not run (see the Health page's Languages section), leave no edges here; it does not mean the components are independent.</div>";
        html += b.notes.map((n) => '<div class="note-box">' + esc(n) + "</div>").join("");
        if (b.caveats.length) html += '<details class="a-caveats"><summary>What this view cannot tell you</summary><ul>' + b.caveats.map((c) => "<li>" + esc(c) + "</li>").join("") + "</ul></details>";
        out.innerHTML = html;
        return;
      }
      const maxW = Math.max(1, ...b.edges.map((e) => e.weight));
      html += '<h3 class="a-h">Dependencies' + (selected ? " of " + esc(shortName(selected)) : "") + ' <span class="badge dim">' + shown.length + "</span></h3>" +
        '<div class="a-edges-wrap"><table class="a-edges"><thead><tr><th>From</th><th></th><th>To</th><th>Weight</th><th>Seen in</th></tr></thead><tbody>' +
        shown.map((e) => {
          const bad = inCycle.has(e.from) && inCycle.has(e.to);
          return '<tr' + (bad ? ' class="cyc"' : "") + '><td><button class="link-btn" type="button" data-comp="' + esc(e.from) + '">' + esc(e.from) + '</button></td><td class="muted">→</td>' +
            '<td><button class="link-btn" type="button" data-comp="' + esc(e.to) + '">' + esc(e.to) + "</button></td>" +
            '<td class="a-w"><span class="a-bar"><span style="width:' + Math.max(3, (e.weight / maxW) * 100).toFixed(0) + '%"></span></span> ' + e.weight + "</td>" +
            '<td class="muted">' + esc(e.source) + "</td></tr>";
        }).join("") + "</tbody></table></div>" +
        '<p class="sub" style="font-size:11.5px">Weight is the number of distinct file pairs that cross; <em>manifest</em> means declared in a package manifest, <em>code</em> means resolved in the code.</p>';

      html += b.notes.map((n) => '<div class="note-box">' + esc(n) + "</div>").join("");
      if (b.caveats.length) {
        html += '<details class="a-caveats"><summary>What this view cannot tell you</summary><ul>' + b.caveats.map((c) => "<li>" + esc(c) + "</li>").join("") + "</ul></details>";
      }
      out.innerHTML = html;
    }

    page.addEventListener("click", (e) => {
      const t = e.target.closest("[data-comp]");
      if (!t || !brief) return;
      selected = t.dataset.comp === selected ? "" : t.dataset.comp;
      setQuery("c", selected);
      render();
    });
    provEl.addEventListener("change", () => { setQuery("provenance", provEl.value === "all" ? "all" : ""); load(); });
    document.getElementById("a-refresh").addEventListener("click", load);
    load();
  }

  // ── Changes ────────────────────────────────────────────────────────────
  // What the uncommitted work touches (/api/changes, changes.rs): the files
  // changed since the commit the index describes, the indexed symbols each
  // edit overlaps, their callers, each file's blast radius on demand, and the
  // architecture rules the change would break (invariants over every edge,
  // the live overlay of uncommitted edits included).
  function changesView() {
    document.title = "Travsr: Changes";
    page.innerHTML =
      '<div class="phead"><div><h2>Changes</h2><p class="sub" style="margin:2px 0 0">What your uncommitted work touches: the symbols it edits, who calls them, what each file reaches, and which architecture rules it would break.</p></div>' +
      '<div class="phead-a"><button class="btn" id="c-refresh">Refresh</button></div></div><div id="out"></div>';
    const short = (sha) => (sha || "").slice(0, 8);
    const nameOf = (sig) => (sig.includes(":") ? sig.slice(sig.indexOf(":") + 1) : sig);
    let rules = null;
    let data = null;

    async function loadRules() {
      // Every edge, so an uncommitted edit's live edges count (RFC-027).
      try {
        const r = await ui.api("/api/run/invariants", { repo: ui.repo, provenance: "" });
        const lines = (r.stdout || "").split("\n");
        const violated = [];
        for (let i = 0; i < lines.length; i++) {
          const m = /^VIOLATED\s+(.+)$/.exec(lines[i]);
          if (!m) continue;
          const detail = [];
          for (let j = i + 1; j < lines.length && /^\s+\S/.test(lines[j]); j++) detail.push(lines[j].trim());
          violated.push({ rule: m[1].trim(), detail });
        }
        const held = lines.filter((l) => /^holds\s/.test(l)).length;
        const noFile = /no such file|not found|could not read the invariants/i.test(r.stderr + r.stdout) && !held && !violated.length;
        rules = { violated, held, noFile, error: r.code !== 0 && !violated.length && !held ? (r.stderr || r.stdout).trim().split("\n").pop() : "" };
      } catch (e) {
        rules = { violated: [], held: 0, noFile: false, error: e.message };
      }
      render();
    }

    async function load() {
      const out = document.getElementById("out");
      if (!data) out.innerHTML = '<p class="sub"><span class="spinner">⟳</span> Reading the working tree…</p>';
      try {
        data = await ui.api("/api/changes", { repo: ui.repo });
        render();
      } catch (e) {
        out.innerHTML = '<div class="diag error"><div class="diag-t">' + esc(e.message) + "</div></div>";
      }
      rules = null;
      loadRules();
    }
    refreshView = load;

    function render() {
      if (!data) return;
      const out = document.getElementById("out");
      const files = data.files;
      const syms = files.reduce((n, f) => n + f.symbols.length, 0);
      const callerSet = new Set();
      for (const f of files) for (const s of f.symbols) for (const c of s.callers) callerSet.add(c.sig + "@" + c.path);
      const card = (k, v, cls, note) => '<div class="card' + (cls ? " " + cls : "") + '"><div class="k">' + esc(k) + '</div><div class="v">' + esc(v) + "</div>" + (note ? '<div class="s">' + esc(note) + "</div>" : "") + "</div>";
      const ruleCard = rules === null ? card("Rules broken", "…", "", "checking")
        : rules.noFile ? card("Rules broken", "–", "", "no architecture-invariants.json")
        : rules.error ? card("Rules broken", "?", "warn", "could not check")
        : card("Rules broken", String(rules.violated.length), rules.violated.length ? "bad" : "", rules.held + rules.violated.length + " rules checked");
      let html = "";
      if (data.base && data.head && data.base !== data.head) {
        html += '<div class="note-box">The index describes <code>' + esc(short(data.base)) + "</code> and HEAD is <code>" + esc(short(data.head)) +
          "</code>, so commits made since the last index are included here as well.</div>";
      }
      html += '<div class="cards a-cards">' +
        card("Files changed", String(files.length) + (data.truncated ? "+" : ""), "", "since " + short(data.base)) +
        card("Symbols touched", String(syms)) +
        card("Callers affected", String(callerSet.size), "", "distinct calling symbols") +
        ruleCard + "</div>";
      if (rules && rules.violated.length) {
        html += '<div class="diag error ch-rules"><div class="diag-t">Architecture rules this state breaks</div>' +
          rules.violated.map((v) => '<div class="ch-rule"><strong>' + esc(v.rule) + "</strong>" + v.detail.map((d) => '<div class="mono muted">' + esc(d) + "</div>").join("") + "</div>").join("") +
          '<div class="diag-d">Checked over every edge, uncommitted ones included. A rule that was already broken before your change shows here too; see the Architecture page.</div></div>';
      }
      if (!files.length) {
        html += '<div class="note-box">No uncommitted changes since <code>' + esc(short(data.base)) + "</code>.</div>";
        out.innerHTML = html;
        return;
      }
      for (const f of files) {
        const callers = f.symbols.reduce((n, s) => n + s.callers_total, 0);
        html += '<details class="hsec ch-file" open data-path="' + esc(f.path) + '"><summary>' +
          '<span class="ch-status ' + esc(f.status) + '">' + esc(f.status) + "</span>" +
          '<span class="ch-path">' + editorAnchor(f.path, null, f.path) + "</span>" +
          '<span class="muted ch-meta">' + (f.indexed ? f.symbols.length + " symbol" + (f.symbols.length === 1 ? "" : "s") + " · " + callers + " caller" + (callers === 1 ? "" : "s") : "not in the index yet") + "</span>" +
          '<span class="ch-acts">' +
          (f.indexed && f.status !== "deleted" ? '<button class="link-btn" type="button" data-blast="' + esc(f.path) + '">blast radius</button>' : "") +
          (f.indexed ? '<a class="link-btn" href="' + esc(ui.withRepo("/graph", { file: f.path })) + '">graph</a>' : "") +
          "</span></summary><div class=\"ch-body\">";
        if (!f.indexed) {
          html += '<p class="sub">' + (f.status === "untracked" ? "A new file git is not tracking yet." : "A new file.") + " Its symbols appear once it is indexed (commit it, or run Reindex).</p>";
        } else if (!f.symbols.length) {
          html += '<p class="sub">' + (f.hunks ? "The edit touches no indexed symbol (comments, imports or whitespace)." : "No line changes against the indexed commit.") + "</p>";
        }
        for (const s of f.symbols) {
          html += '<div class="ch-sym"><div class="ch-sym-row"><span class="s-kind">' + esc(s.kind) + "</span>" +
            '<a class="ch-name" href="' + esc(ui.withRepo("/graph", { q: s.sig })) + '">' + esc(nameOf(s.sig)) + "</a>" +
            '<span class="muted">lines ' + s.line + (s.end_line !== s.line ? "–" + s.end_line : "") + "</span>" +
            '<span class="ch-count' + (s.callers_total ? " has" : "") + '">' + s.callers_total + " caller" + (s.callers_total === 1 ? "" : "s") + "</span></div>" +
            (s.callers.length ? '<ul class="ch-callers">' + s.callers.map((c) => "<li>" + '<a href="' + esc(ui.withRepo("/graph", { q: c.sig })) + '">' + esc(c.sig) + "</a> " +
              '<span class="muted">' + editorAnchor(c.path, c.line, c.path + ":" + c.line) + "</span></li>").join("") +
              (s.callers_total > s.callers.length ? '<li class="muted">and ' + (s.callers_total - s.callers.length) + " more</li>" : "") + "</ul>" : "") +
            "</div>";
        }
        html += '<div class="ch-blast" hidden></div></div></details>';
      }
      out.innerHTML = html;
    }

    page.addEventListener("click", async (e) => {
      if (e.target.id === "c-refresh") { load(); return; }
      const b = e.target.closest("[data-blast]");
      if (!b) return;
      e.preventDefault();
      const box = b.closest(".ch-file").querySelector(".ch-blast");
      box.hidden = false;
      box.innerHTML = '<p class="sub"><span class="spinner">⟳</span> Blast radius…</p>';
      try {
        const files = ui.parseEnvelope(await ui.tool("get_blast_radius", { file: b.dataset.blast, analysis: "tree-sitter" }));
        box.innerHTML = '<div class="ch-blast-h">Blast radius <span class="badge dim">' + files.length + " file" + (files.length === 1 ? "" : "s") + "</span></div>" +
          (files.length ? '<ul class="results">' + files.map((p) => "<li>" + editorAnchor(p, null, p) + "</li>").join("") + "</ul>" : '<p class="sub">Nothing else reaches this file.</p>');
      } catch (err) {
        box.innerHTML = '<div class="diag error"><div class="diag-t">' + esc(err.message) + "</div></div>";
      }
    });
    load();
  }

  const VIEWS = { "/architecture": architectureView, "/changes": changesView, "/callers": callers, "/dependencies": dependencies, "/blast": blast, "/path": pathView, "/context": contextView, "/terminal": terminalView, "/repos": repos };

  (async function main() {
    const path = location.pathname.replace(/\/+$/, "");
    const row = await ui.init(path);
    ui.api("/api/status", { repo: ui.repo }).then((s) => ui.setConnection(s.route), () => ui.setConnection(null));
    if (path !== "/repos" && (!row || !row.indexed)) {
      page.innerHTML = '<div class="banner idle verdict"><strong>No graph yet</strong><span>' +
        (row ? esc(row.name) + " is not indexed yet. Run travsr init in " + esc(row.root) + "." : "No indexed repositories found. Run travsr init in a git repository.") +
        '</span><button class="btn primary vact" data-run data-copy="travsr init">Index this repo</button></div>';
      return;
    }
    (VIEWS[path] || callers)();
    ui.watch(() => (refreshView ? refreshView() : undefined));
  })();
})();
