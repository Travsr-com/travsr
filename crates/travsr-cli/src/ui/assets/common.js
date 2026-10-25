// travsr ui (#1036): what every page shares. The navigation strip, the repo
// and theme pickers, the API helpers, and the small parsers the extension's
// pages use on MCP tool text. Plain script, no build step, no inline handlers
// (the CSP allows only same-origin script files).
"use strict";

window.TravsrUi = (function () {
  const PAGES = [
    ["/health", "Health"],
    ["/graph", "Graph"],
    ["/callers", "Callers"],
    ["/dependencies", "Dependencies"],
    ["/blast", "Blast radius"],
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
  async function cli(name) {
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

  return {
    esc, api, tool, cli, holdCliUntil, init, setConnection, withRepo, editorLink, watch,
    stripEnvelope, envelopeBody, parseEnvelope, timeAgo, formatLogSize, pref,
    get repo() { return repo; }, get repoRow() { return repoRow(); }, get repos() { return repos; },
  };
})();
