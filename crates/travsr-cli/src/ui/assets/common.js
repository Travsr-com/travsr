// travsr ui (#1036): what every page shares. The navigation strip, the repo
// and theme pickers, the API helpers, and the small parsers the extension's
// pages use on MCP tool text. Plain script, no build step, no inline handlers
// (the CSP allows only same-origin script files).
"use strict";

window.TravsrUi = (function () {
  const PAGES = [
    ["/health", "Health"],
    ["/architecture", "Architecture"],
    ["/graph", "Graph"],
    ["/callers", "Callers"],
    ["/dependencies", "Dependencies"],
    ["/blast", "Blast radius"],
    ["/path", "Path"],
    ["/context", "Context"],
    ["/terminal", "Terminal"],
    ["/repos", "Repos"],
  ];

  function esc(s) {
    return String(s == null ? "" : s)
      .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;").replace(/'/g, "&#39;");
  }

  // ── Preferences (best effort: a private window may refuse storage) ──────
  function pref(key, value) {
    try {
      if (value === undefined) return localStorage.getItem("travsr.ui." + key);
      localStorage.setItem("travsr.ui." + key, value);
    } catch (_) { /* storage unavailable */ }
    return null;
  }

  // ── Theme: the extension keys light mode on the class VS Code stamps on the
  // webview body, so the page sets that same class. Auto follows the OS. ───
  const darkQuery = window.matchMedia ? window.matchMedia("(prefers-color-scheme: dark)") : null;
  function applyTheme() {
    const t = pref("theme") || "auto";
    const dark = t === "dark" || (t === "auto" && (!darkQuery || darkQuery.matches));
    document.body.classList.toggle("vscode-dark", dark);
    document.body.classList.toggle("vscode-light", !dark);
  }
  if (darkQuery && darkQuery.addEventListener) darkQuery.addEventListener("change", applyTheme);

  // ── Repo selection: ?repo= wins, then the last one picked, then the one
  // `travsr ui` was started in. ───────────────────────────────────────────
  let repos = [];
  let repo = null;
  function repoRow() { return repos.find((r) => r.name === repo) || null; }
  function withRepo(path, extra) {
    const p = new URLSearchParams(extra || {});
    if (repo) p.set("repo", repo);
    const qs = p.toString();
    return path + (qs ? "?" + qs : "");
  }

  // ── API ────────────────────────────────────────────────────────────────
  class ApiError extends Error {
    constructor(message, offline) { super(message); this.offline = offline; }
  }
  let offlineBar = null;
  function setOffline(on) {
    if (on && !offlineBar) {
      offlineBar = document.createElement("div");
      offlineBar.className = "tsr-offline";
      offlineBar.setAttribute("role", "alert");
      offlineBar.textContent = "Cannot reach the travsr ui server. Is `travsr ui` still running?";
      const nav = document.querySelector(".tsr-nav");
      if (nav) nav.after(offlineBar); else document.body.prepend(offlineBar);
    } else if (!on && offlineBar) {
      offlineBar.remove();
      offlineBar = null;
    }
  }
  async function api(path, params) {
    const qs = new URLSearchParams(params || {}).toString();
    let resp;
    try {
      resp = await fetch(path + (qs ? "?" + qs : ""), { headers: { Accept: "application/json" } });
    } catch (_) {
      setOffline(true);
      throw new ApiError("Cannot reach the travsr ui server.", true);
    }
    setOffline(false);
    let body = null;
    try { body = await resp.json(); } catch (_) { /* not JSON */ }
    if (!resp.ok) throw new ApiError((body && body.error) || resp.status + " " + resp.statusText, false);
    return body;
  }
  /** An MCP tool's raw text, exactly as an MCP client receives it. */
  async function tool(name, args) {
    const r = await api("/api/tool/" + name, Object.assign({ repo }, args || {}));
    return r.text;
  }
  /** One of the read-only CLI commands the extension spawns: {out, stdout, code}.
   *  At most two run at once. Each holds a connection for as long as the command
   *  runs (`lang list` can take most of a minute), and a browser opens only six
   *  per host, so unthrottled they starved the quick tool calls behind them. */
  const CLI_SLOTS = 2;
  let cliRunning = 0;
  const cliWaiting = [];
  let cliGate = Promise.resolve();
  /** Hold every CLI command until `p` settles. Each command starts processes
   *  of its own (`lang list` sweeps PATH), and on a slow or heavily scanned
   *  machine they crowded out the index diagnostics started beside them,
   *  which then missed their deadline. Settled either way: a failed
   *  diagnostic must not hold the commands back for good. */
  function holdCliUntil(p) { cliGate = Promise.resolve(p).then(() => {}, () => {}); }
  // Answered inside the server from the CLI's own code, so they start no
  // process and need neither the hold nor a slot.
  const CLI_IN_PROCESS = ["version", "daemon-status", "config-list", "embed-list"];
  async function cli(name) {
    if (CLI_IN_PROCESS.includes(name)) return api("/api/cli/" + name, { repo });
    await cliGate;
    if (cliRunning >= CLI_SLOTS) await new Promise((r) => cliWaiting.push(r));
    cliRunning++;
    try {
      return await api("/api/cli/" + name, { repo });
    } finally {
      cliRunning--;
      const next = cliWaiting.shift();
      if (next) next();
    }
  }

  // ── Envelope helpers, as the extension's commands.ts has them ──────────
  function stripEnvelope(raw) {
    const m = /^<travsr-data>\n?([\s\S]*?)\n?<\/travsr-data>$/.exec(String(raw || "").trim());
    return unescapeTags(m ? m[1] : String(raw || ""));
  }
  function envelopeBody(raw) {
    raw = String(raw || "");
    const end = raw.lastIndexOf("</travsr-data>");
    return stripEnvelope(end < 0 ? raw : raw.slice(0, end + "</travsr-data>".length));
  }
  // The server escapes angle brackets for the agent's benefit; the page shows
  // the source text.
  function unescapeTags(s) { return s.replace(/&lt;/g, "<").replace(/&gt;/g, ">"); }
  function parseEnvelope(raw) {
    return envelopeBody(raw).split("\n").map((l) => l.trim())
      .filter((l) => l && !l.startsWith("~ = ") && !/^\[?note:/.test(l));
  }
  function timeAgo(ms) {
    const secs = Math.floor(ms / 1000);
    if (secs < 60) return secs + "s ago";
    const mins = Math.floor(secs / 60);
    if (mins < 60) return mins + "m ago";
    const hrs = Math.floor(mins / 60);
    if (hrs < 24) return hrs + "h ago";
    return Math.floor(hrs / 24) + "d ago";
  }
  function formatLogSize(bytes) {
    if (bytes < 1024) return bytes + " B";
    if (bytes < 1024 * 1024) return Math.round(bytes / 1024) + " KB";
    return (bytes / (1024 * 1024)).toFixed(1) + " MB";
  }
  /** A `vscode://file` link, so "open in editor" opens VS Code when it is installed. */
  function editorLink(relPath, line) {
    const row = repoRow();
    if (!row || !relPath) return null;
    const sep = row.root.includes("\\") ? "\\" : "/";
    const abs = /^([A-Za-z]:[\\/]|\/)/.test(relPath)
      ? relPath : row.root.replace(/[\\/]$/, "") + sep + relPath.replace(/[\\/]/g, sep);
    return "vscode://file/" + abs.replace(/\\/g, "/").replace(/^\/+/, "") + (line ? ":" + line : "");
  }

  // ── Navigation strip ───────────────────────────────────────────────────
  function el(tag, attrs, children) {
    const n = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v == null || v === false) continue;
      if (k === "class") n.className = v;
      else if (k === "text") n.textContent = v;
      else n.setAttribute(k, v === true ? "" : v);
    }
    for (const c of children || []) if (c) n.append(c);
    return n;
  }

  function renderNav(active) {
    const tabs = el("nav", { class: "tsr-tabs", "aria-label": "Pages" },
      PAGES.map(([href, label]) => el("a", {
        class: "tsr-tab", href: withRepo(href), text: label,
        "aria-current": href === active ? "page" : null,
      })));
    const repoSel = el("select", { id: "tsr-repo", "aria-label": "Repository" },
      repos.map((r) => el("option", {
        value: r.name, title: r.root, text: r.name + (r.indexed ? "" : " (not indexed)"),
        selected: r.name === repo,
      })));
    repoSel.disabled = repos.length === 0;
    repoSel.addEventListener("change", () => {
      pref("repo", repoSel.value);
      const u = new URL(location.href);
      u.searchParams.set("repo", repoSel.value);
      location.href = u.toString();
    });
    const themeSel = el("select", { id: "tsr-theme", "aria-label": "Theme" },
      [["auto", "Auto"], ["dark", "Dark"], ["light", "Light"]].map(([v, t]) =>
        el("option", { value: v, text: t, selected: (pref("theme") || "auto") === v })));
    themeSel.addEventListener("change", () => { pref("theme", themeSel.value); applyTheme(); });
    const conn = el("span", { class: "tsr-conn", id: "tsr-conn" }, [el("span", { class: "tsr-dot" })]);
    const nav = el("header", { class: "tsr-nav" }, [
      el("a", { class: "tsr-brand", href: withRepo("/health") },
        [el("img", { src: "/assets/icon.png", alt: "" }), el("span", { text: "travsr" })]),
      tabs,
      el("span", { class: "tsr-spacer" }),
      conn,
      el("label", null, [document.createTextNode("Repo"), repoSel]),
      el("label", null, [document.createTextNode("Theme"), themeSel]),
    ]);
    document.body.prepend(nav);
  }

  /** Reflect where answers come from: the repo's daemon, or the index file. */
  function setConnection(route) {
    const c = document.getElementById("tsr-conn");
    if (!c) return;
    const dot = el("span", { class: "tsr-dot " + (route === "daemon" ? "ok" : route ? "" : "bad") });
    c.replaceChildren(dot, document.createTextNode(
      route === "daemon" ? "daemon" : route === "direct" ? "index file (no daemon)" : "offline"));
    c.title = route === "daemon"
      ? "Answers come from this repository's running daemon."
      : "No daemon is running for this repository, so answers are read from the index on disk.";
  }

  // ── Live refresh: the browser's version of the extension's graph.db file
  // watcher (extension.ts dbWatcher -> refreshOpenPanels). Polls a cheap
  // fingerprint of the index and calls `redraw` once a change has settled. ──
  const WATCH_MS = 3000;
  /**
   * Redraw the open page when the index changes. `redraw` may be async; a
   * change that lands while one is running is coalesced into one more run.
   *
   * A change is acted on only when the next poll reads the same fingerprint:
   * a reindex writes for a while, and redrawing mid-write would show a graph
   * that is neither the old one nor the new one. Polling stops while the tab is
   * hidden and catches up when it is shown again.
   */
  function watch(redraw) {
    let seen = null;     // fingerprint the page is showing
    let candidate = null; // changed fingerprint waiting to settle
    let running = false;
    let again = false;
    async function fire() {
      if (running) { again = true; return; }
      running = true;
      setLive("updating");
      try { await redraw(); } catch (_) { /* the page reports its own errors */ }
      running = false;
      setLive("live");
      if (again) { again = false; fire(); }
    }
    async function tick() {
      if (!repo || document.hidden) return;
      let v;
      try { v = await api("/api/version", { repo }); } catch (_) { setLive("offline"); return; }
      if (seen === null) { seen = v.fingerprint; setLive("live"); return; }
      if (v.fingerprint === seen) { candidate = null; return; }
      if (v.fingerprint !== candidate) { candidate = v.fingerprint; setLive("changing"); return; }
      seen = candidate;
      candidate = null;
      fire();
    }
    tick();
    setInterval(tick, WATCH_MS);
    document.addEventListener("visibilitychange", () => { if (!document.hidden) tick(); });
  }

  /** The strip's live indicator, so a reader can tell the page is watching. */
  function setLive(stateName) {
    let el = document.getElementById("tsr-live");
    const conn = document.getElementById("tsr-conn");
    if (!el && conn) {
      el = document.createElement("span");
      el.id = "tsr-live";
      el.className = "tsr-live";
      conn.after(el);
    }
    if (!el) return;
    const TEXT = {
      live: ["live", "This page redraws when the index changes."],
      changing: ["index changing…", "The index is being updated; the page redraws once it settles."],
      updating: ["updating…", "Redrawing with the new index."],
      offline: ["not watching", "Cannot reach the travsr ui server."],
    };
    const [text, title] = TEXT[stateName] || TEXT.live;
    el.textContent = text;
    el.title = title;
    el.dataset.state = stateName;
  }

  /** Load the repo list, pick the repo, draw the strip. Resolves to the repo row. */
  async function init(active) {
    applyTheme();
    try {
      repos = (await api("/api/repos")).repos;
    } catch (_) {
      repos = [];
    }
    const urlRepo = new URLSearchParams(location.search).get("repo");
    const has = (n) => n && repos.some((r) => r.name === n);
    const saved = pref("repo");
    const current = repos.find((r) => r.current);
    repo = has(urlRepo) ? urlRepo
      : has(saved) ? saved
      : current ? current.name
      : (repos.find((r) => r.indexed) || repos[0] || {}).name || null;
    renderNav(active);
    // A link naming a repo this server does not know (renamed, pruned, or a
    // typo) must not quietly show a different one under the old name.
    if (urlRepo && !has(urlRepo)) {
      const u = new URL(location.href);
      if (repo) u.searchParams.set("repo", repo); else u.searchParams.delete("repo");
      history.replaceState(null, "", u.toString());
      const note = document.createElement("div");
      note.className = "tsr-offline";
      note.setAttribute("role", "status");
      note.textContent = "No indexed repository named “" + urlRepo + "”" +
        (repo ? "; showing " + repo + " instead." : ".");
      document.querySelector(".tsr-nav").after(note);
    }
    return repoRow();
  }

  // ── Actions: run a button's command (actions.rs) ─────────────────────────
  // The commands the buttons show, mapped to the server's action names. Only
  // these run; anything else stays a copy-to-clipboard button.
  const ACTIONS = [
    [/^travsr daemon start$/, "daemon-start", "Start the daemon for this repository? It keeps the graph fresh as you commit and save."],
    [/^travsr daemon stop$/, "daemon-stop", "Stop the daemon for this repository? The graph stops refreshing until it is started again."],
    [/^travsr daemon restart$/, "daemon-restart", "Restart the daemon for this repository?"],
    [/^travsr init$/, "init", "Index this repository now? It updates the graph and may download language tools; on a large repository it can take several minutes."],
    [/^travsr init --force$/, "init-force", "Rebuild the whole index from scratch? Every file is parsed again; on a large repository this can take a long time."],
    [/^travsr fsck$/, "fsck", "Check the graph's integrity? Report only."],
    [/^travsr fsck --fix$/, "fsck-fix", "Repair the graph? This deletes ghost nodes and sweeps orphan edges."],
    [/^travsr connect$/, "connect", "Register Travsr with the AI coding tools found on this machine? This edits their configuration files."],
    [/^travsr embed init$/, "embed-init", "Set up semantic search? This downloads the embedding sidecar and model, which can take several minutes."],
    [/^travsr embed list$/, "embed-list", "List the embedding models available?"],
    [/^travsr lang remove (\S+)$/, "lang-remove", "Stop full analysis for this language in this repository?", "language"],
    [/^travsr repos --prune$/, "repos-prune", "Remove every registry entry whose index no longer exists?"],
    [/^travsr repos --remove (\S+)$/, "repos-remove", "Remove this repository from the registry? Its index files are left alone.", "name"],
  ];
  function actionFor(cmd) {
    for (const [re, name, ask, key] of ACTIONS) {
      const m = re.exec(String(cmd || "").trim());
      if (m) return { name, ask, params: key ? { [key]: m[1] } : {} };
    }
    return null;
  }
  let sessionP = null;
  /** Whether the buttons may run commands here, and the token for it. */
  function session() {
    if (!sessionP) sessionP = api("/api/session").catch(() => ({ actions: false }));
    return sessionP;
  }
  /** True when `cmd` is one a button can run on this server. */
  async function canRun(cmd) {
    return !!actionFor(cmd) && !!(await session()).actions;
  }

  /**
   * Ask in a dialog, run `cmd` through /api/action in repository `repoName`,
   * and show its output in the same dialog. Resolves to the response, or null
   * if cancelled. In-page rather than window.confirm, so the question, the
   * command and the folder read like the rest of the UI.
   */
  async function runCommand(cmd, repoName, root) {
    const a = actionFor(cmd);
    const s = await session();
    if (!a || !s.actions) return null;
    // "Question? Detail." -> heading and body.
    const q = a.ask.indexOf("? ");
    const title = q > 0 ? a.ask.slice(0, q + 1) : a.ask;
    const detail = q > 0 ? a.ask.slice(q + 2) : "";

    const dlg = document.createElement("div");
    dlg.className = "tsr-modal";
    dlg.setAttribute("role", "dialog");
    dlg.setAttribute("aria-modal", "true");
    dlg.innerHTML =
      '<div class="tsr-modal-box">' +
      '<div class="tsr-modal-head"><span class="tsr-modal-title"></span><span class="tsr-modal-state"></span></div>' +
      '<div class="tsr-modal-ask"><p class="tsr-modal-detail"></p>' +
      '<div class="tsr-modal-kv"><span>Runs</span><code class="tsr-modal-cmd"></code></div>' +
      '<div class="tsr-modal-kv"><span>In</span><code class="tsr-modal-root"></code></div></div>' +
      '<pre class="tsr-modal-out" hidden></pre>' +
      '<div class="tsr-modal-foot">' +
      '<button class="btn" data-m-cancel>Cancel</button>' +
      '<button class="btn primary" data-m-run>Run</button>' +
      '<button class="btn" data-m-copy hidden disabled>Copy output</button>' +
      '<button class="btn primary" data-m-close hidden>Close</button></div></div>';
    const $ = (sel) => dlg.querySelector(sel);
    $(".tsr-modal-title").textContent = title;
    $(".tsr-modal-detail").textContent = detail;
    $(".tsr-modal-detail").hidden = !detail;
    $(".tsr-modal-cmd").textContent = cmd;
    $(".tsr-modal-root").textContent = root || repoName;
    document.body.append(dlg);
    $("[data-m-run]").focus();

    // Ask: Run, or Cancel / Escape / a click outside.
    const go = await new Promise((resolve) => {
      const key = (e) => {
        if (e.key === "Escape") { e.preventDefault(); finish(false); }
        else if (e.key === "Enter" && !e.target.closest("[data-m-cancel]")) { e.preventDefault(); finish(true); }
      };
      const click = (e) => {
        if (e.target.closest("[data-m-run]")) finish(true);
        else if (e.target.closest("[data-m-cancel]") || e.target === dlg) finish(false);
      };
      function finish(v) {
        dlg.removeEventListener("keydown", key);
        dlg.removeEventListener("click", click);
        resolve(v);
      }
      dlg.addEventListener("keydown", key);
      dlg.addEventListener("click", click);
    });
    if (!go) { dlg.remove(); return null; }

    // Run: the same dialog becomes the output view.
    $("[data-m-cancel]").hidden = true;
    $("[data-m-run]").hidden = true;
    $(".tsr-modal-detail").hidden = true;
    // The heading now shows the command, so its row would repeat it.
    $(".tsr-modal-cmd").parentElement.hidden = true;
    $(".tsr-modal-title").textContent = "$ " + cmd;
    $(".tsr-modal-title").classList.add("mono");
    $(".tsr-modal-state").innerHTML = 'running <span class="tsr-modal-t">0 s</span>';
    $(".tsr-modal-out").hidden = false;
    $(".tsr-modal-out").textContent = "Running…";
    const started = Date.now();
    const tick = setInterval(() => {
      const t = $(".tsr-modal-t");
      if (t) t.textContent = Math.round((Date.now() - started) / 1000) + " s";
    }, 1000);
    let text = "";
    let result = null;
    try {
      const resp = await fetch("/api/action/" + a.name + "?" + new URLSearchParams(Object.assign({ repo: repoName }, a.params)), {
        method: "POST",
        headers: { "X-Travsr-Token": s.token },
      });
      const body = await resp.json().catch(() => ({}));
      if (!resp.ok) throw new Error(body.error || resp.status + " " + resp.statusText);
      result = body;
      text = (body.stderr ? body.stderr.replace(/\s+$/, "") + "\n" : "") + (body.stdout || "").replace(/\s+$/, "");
      $(".tsr-modal-state").textContent =
        (body.code == null ? "still running" : body.code === 0 ? "done, exit 0" : "failed, exit " + body.code) + " · " + Math.round(body.millis / 1000) + " s";
      $(".tsr-modal-state").className = "tsr-modal-state " + (body.code === 0 ? "ok" : "bad");
      $(".tsr-modal-out").textContent = text || "(no output)";
    } catch (e) {
      $(".tsr-modal-state").textContent = "not run";
      $(".tsr-modal-state").className = "tsr-modal-state bad";
      $(".tsr-modal-out").textContent = e.message;
    } finally {
      clearInterval(tick);
    }
    $("[data-m-copy]").hidden = false;
    $("[data-m-copy]").disabled = !text;
    $("[data-m-close]").hidden = false;
    $("[data-m-close]").focus();

    // Close (button, Escape, a click outside) or copy the output.
    await new Promise((resolve) => {
      dlg.addEventListener("keydown", (e) => { if (e.key === "Escape") { dlg.remove(); resolve(); } });
      dlg.addEventListener("click", async (e) => {
        if (e.target.closest("[data-m-close]") || e.target === dlg) { dlg.remove(); resolve(); }
        else if (e.target.closest("[data-m-copy]")) {
          try { await navigator.clipboard.writeText(text); e.target.textContent = "Copied"; } catch (_) { /* clipboard refused */ }
        }
      });
    });
    return result;
  }

  // ── Execution path ───────────────────────────────────────────────────────
  // get_execution_path answers one of:
  //   path (N steps, source to sink):        then one `<sig> (<kind>) <dash> <path>` per step,
  //                                           and maybe `nearby context (..., NOT on it):` + lines
  //   source|sink '<x>' is ambiguous, ...:    then `  <sig> (<kind>) at <path>` per candidate
  //   no path found: ... / could not resolve ...
  // The step line is the extension's parseExecutionPath shape (commands.ts).
  const PATH_SEP = " " + String.fromCharCode(0x2014) + " ";
  function parseExecutionPath(raw) {
    const text = String(raw || "");
    const body = envelopeBody(text);
    const notes = text.slice(text.lastIndexOf("</travsr-data>") + 1).split("\n")
      .map((l) => l.trim()).filter((l) => /^\[note:/.test(l)).map((l) => l.replace(/^\[note:\s*/, "").replace(/\]$/, ""));
    const out = { steps: [], nearby: [], candidates: [], ambiguous: null, message: "", header: "", notes };
    let section = "head";
    const step = (line) => {
      const sep = line.lastIndexOf(PATH_SEP);
      if (sep <= 0) return null;
      const m = /^(.+?)\s+\(([\w ]+)\)$/.exec(line.slice(0, sep).replace(/^\[[^\]]+\]\s*/, ""));
      return m ? { sig: m[1], kind: m[2], path: line.slice(sep + PATH_SEP.length).trim() } : null;
    };
    for (const raw of body.split("\n")) {
      const line = raw.trim();
      if (!line) continue;
      const amb = /^(source|sink) '(.+?)' is ambiguous/.exec(line);
      if (amb) { out.ambiguous = amb[1]; out.message = line; section = "candidates"; continue; }
      if (/^path \(/.test(line)) { out.header = line.replace(/:$/, ""); section = "steps"; continue; }
      if (/^nearby context/.test(line)) { section = "nearby"; continue; }
      if (section === "candidates") {
        const c = /^(.+?)\s+\(([\w ]+)\)\s+at\s+(.+)$/.exec(line);
        if (c) out.candidates.push({ sig: c[1], kind: c[2], path: c[3] });
        continue;
      }
      const s = section === "steps" || section === "nearby" ? step(line) : null;
      if (s) out[section === "steps" ? "steps" : "nearby"].push(s);
      else if (section === "head") out.message += (out.message ? " " : "") + line;
    }
    return out;
  }

  // ── Suggestions under a lookup box ──────────────────────────────────────
  // search_symbol answers one line per symbol:
  //   fn:browse_tool_call (function) <dash> crates/travsr-mcp/src/server.rs:370
  const SYMBOL_LINE = /^\s*\w+:(.+?) \(([\w ]+)\) \S+ (.+?)(?::(\d+))?\s*$/;
  const SUGGEST_MAX = 12;

  /** Ranks names that equal, then start with, then contain what was typed. */
  function byMatch(typed, nameOf) {
    const low = typed.toLowerCase();
    const rank = (x) => {
      const n = nameOf(x).toLowerCase();
      return n === low ? 0 : n.startsWith(low) ? 1 : n.includes(low) ? 2 : 3;
    };
    return (list) => list.map((x, i) => ({ x, r: rank(x), i }))
      .sort((a, b) => a.r - b.r || a.i - b.i).map((e) => e.x);
  }

  /** Symbols matching `typed`, as suggestion items. Imports are dropped:
   *  they are not something to show callers or a graph of. */
  async function suggestSymbols(typed) {
    const seen = new Set();
    const out = [];
    for (const line of stripEnvelope(await tool("search_symbol", { name: typed })).split("\n")) {
      const m = SYMBOL_LINE.exec(line);
      if (!m || m[2] === "import") continue;
      const name = m[1].split("::").pop();
      const where = m[3] + (m[4] ? ":" + m[4] : "");
      if (seen.has(name + "|" + where)) continue;
      seen.add(name + "|" + where);
      out.push({ value: name, label: name, kind: m[2], detail: where });
    }
    return byMatch(typed, (s) => s.label)(out).slice(0, SUGGEST_MAX);
  }

  /** Indexed files whose path contains `typed`, file name matches first. */
  async function suggestFiles(typed) {
    const r = await api("/api/files", { repo, q: typed });
    const base = (p) => p.slice(p.lastIndexOf("/") + 1);
    return byMatch(typed, base)(r.files || []).slice(0, SUGGEST_MAX)
      .map((p) => ({ value: p, label: base(p), kind: "file", detail: p }));
  }

  /**
   * A dropdown of suggestions under `input`, so a symbol or file can be
   * picked rather than typed in full. `search(typed)` resolves to items
   * `{value, label, kind, detail}`; picking one puts `value` in the box and
   * calls `onPick(item)`. Arrow keys and Enter or a click pick; Escape closes.
   * `anchor` is the positioned box the list hangs under (the input's parent
   * by default). Register this before any other Enter handler on the input,
   * so a highlighted pick takes Enter instead of the half-typed text.
   */
  function suggest(input, opts) {
    const anchor = opts.anchor || input.parentElement;
    anchor.classList.add("suggest-anchor");
    const list = document.createElement("ul");
    list.className = "suggest";
    list.id = (input.id || "q") + "-suggest";
    list.setAttribute("role", "listbox");
    list.hidden = true;
    anchor.append(list);
    input.setAttribute("aria-controls", list.id);
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
    function pick(item) {
      close();
      input.value = item.value;
      opts.onPick(item);
    }
    function note(text) {
      const li = document.createElement("li");
      li.className = "empty";
      li.textContent = text;
      list.replaceChildren(li);
      items = [];
      active = -1;
      list.hidden = false;
    }
    function show(found, typed) {
      if (!found.length) { note("Nothing matches “" + typed + "”"); return; }
      list.replaceChildren();
      items = found;
      active = -1;
      found.forEach((s, i) => {
        const li = document.createElement("li");
        li.setAttribute("role", "option");
        const name = document.createElement("span");
        name.className = "s-name";
        name.textContent = s.label;
        const kind = document.createElement("span");
        kind.className = "s-kind";
        kind.textContent = s.kind;
        const where = document.createElement("span");
        where.className = "s-path";
        // Right-aligned and clipped from the left (CSS rtl) so the file name
        // stays; the marks keep a trailing ":line" from being reordered.
        where.textContent = "‎" + s.detail + "‎";
        where.title = s.detail;
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
      if (typed.length < 2) { close(); return; }
      timer = setTimeout(async () => {
        const mine = ++seq;
        // Without the daemon a lookup opens the index (about 2 s on a big
        // repo); say it is working rather than leave the box looking dead.
        if (list.hidden || !items.length) note("Searching for “" + typed + "”…");
        let found;
        try { found = await opts.search(typed); } catch (_) { if (mine === seq) close(); return; }
        // Drop a slow answer to an earlier keystroke.
        if (mine === seq && input.value.trim() === typed) show(found, typed);
      }, 180);
    });
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

  return {
    esc, api, tool, cli, holdCliUntil, init, setConnection, withRepo, editorLink, watch,
    stripEnvelope, envelopeBody, parseEnvelope, timeAgo, formatLogSize, pref,
    suggest, suggestSymbols, suggestFiles, parseExecutionPath, canRun, runCommand,
    get repo() { return repo; }, get repoRow() { return repoRow(); }, get repos() { return repos; },
  };
})();
