// travsr ui (#1036): the extension's Health page, read-only.
//
// A port of buildStatsHtml / computeVerdict (packages/travsr-vscode/src/
// webviews.ts) and gatherHealth (commands.ts), fed by the same sources: the
// MCP tools get_index_status, get_graph_health, repos_list and repo_languages,
// and the CLI commands `daemon status`, `embed list --json`, `lang list --json`,
// `config list --json`, `status --verbose` and `--version`. The extension's
// action buttons become "copy the command" buttons, since a browser page
// cannot run anything. Keep the wording and order in step with the extension.
"use strict";

(function () {
  const ui = window.TravsrUi;
  const esc = ui.esc;

  // ── Constants, as webviews.ts has them ────────────────────────────────
  const EVENT_LABELS = {
    "daemon.session.start": "Daemon started", "daemon.ready": "Daemon ready",
    "daemon.socket.bound": "Control socket bound", "daemon.session.stop": "Daemon stopped",
    "daemon.log_pruned": "Old log files removed", "head.drift.detected": "HEAD moved, reconciling",
    "head.reconcile.complete": "Reindexed after HEAD moved", "tree.reconcile.pruned": "Pruned deleted files",
    "phase_b.start": "Semantic indexing started", "phase_b.indexed": "Semantic indexing complete",
    "phase_b.complete": "Semantic refresh complete", "kcore.updated": "Graph centrality updated",
    "embed.text.updated": "Embedding text updated", "embed.text.fts_backfill": "Search index backfilled",
    "store.fts_words.backfill": "Word index updated", "lsif.spawn": "Analyzer started",
    "lsif.complete": "Analyzer finished", "query.failed": "Query failed",
    "editor.attached": "Editor attached", "editor.detached": "Editor detached",
  };
  const EVENT_FAMILY = {
    "daemon.session.start": "daemon", "daemon.ready": "daemon", "daemon.socket.bound": "daemon",
    "daemon.session.stop": "daemon", "daemon.log_pruned": "daemon", "head.drift.detected": "git",
    "head.reconcile.complete": "git", "tree.reconcile.pruned": "git", "phase_b.start": "index",
    "phase_b.indexed": "index", "phase_b.complete": "index", "kcore.updated": "index",
    "lsif.spawn": "index", "lsif.complete": "index", "embed.text.updated": "search",
    "embed.text.fts_backfill": "search", "store.fts_words.backfill": "search",
    "query.failed": "query", "editor.attached": "editor", "editor.detached": "editor",
  };
  const LOG_RANK = { TRACE: 0, DEBUG: 1, INFO: 2, WARN: 3, ERROR: 4 };
  const rankOf = (lvl) => (lvl in LOG_RANK ? LOG_RANK[lvl] : 2);
  const LOG_LEVELS = ["error", "warn", "info", "debug", "trace"];
  const LOG_AUTO_SECONDS = [0, 5, 15, 30, 60];
  const LOG_MAX_LINES = 5000;
  const LANG_CONTRACT_FIELDS = ["language", "status", "statusLine", "repoState", "prerequisites",
    "builtin", "availableOnThisPlatform", "unavailableTarget"];
  const isTempRepo = (name) => /^\.tmp[A-Za-z0-9]{6}$/.test(name);

  const OK_ICON = '<svg class="ci" viewBox="0 0 16 16" fill="none"><path d="M3 8.4 6.4 11.8 13 5.2" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>';
  const WARN_ICON = '<svg class="ci" viewBox="0 0 16 16" fill="none"><path d="M8 2.6 14.4 13H1.6L8 2.6Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/><path d="M8 6.5v3" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/><circle cx="8" cy="11.3" r=".8" fill="currentColor"/></svg>';
  const BAD_ICON = '<svg class="ci" viewBox="0 0 16 16" fill="none"><circle cx="8" cy="8" r="5.8" stroke="currentColor" stroke-width="1.3"/><path d="M4.4 11.6 11.6 4.4" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/></svg>';
  const CHEV = '<svg class="chev" viewBox="0 0 16 16" fill="none"><path d="M6 3.5 10.5 8 6 12.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/></svg>';

  // ── Log lines (commands.ts parseLogLine / shortTarget) ─────────────────
  function shortTarget(target) {
    return target.split("::")[0].replace(/^travsr_/, "").replace(/_/g, "-");
  }
  function parseLogLine(line, day) {
    try {
      const e = JSON.parse(line);
      if (typeof e.timestamp === "string" && typeof e.level === "string") {
        const fields = Object.assign({}, e.fields || {});
        const message = fields.message, event = fields.event;
        delete fields.message; delete fields.event; delete fields.repo;
        return {
          time: new Date(e.timestamp).toTimeString().slice(0, 8),
          level: e.level, target: shortTarget(e.target || ""),
          message: typeof message === "string" ? message : "",
          event: typeof event === "string" ? event : undefined,
          detail: Object.entries(fields).map(([k, v]) => k + "=" + String(v)).join(" "),
          iso: e.timestamp, raw: line, day,
        };
      }
    } catch (_) { /* a pre-JSON rotation: show the line as itself */ }
    return { time: "", level: "", target: "", message: line, detail: "", iso: "", raw: line, day };
  }
  function looksLikeSourceRef(key, value) {
    if (key !== "path" && key !== "file") return false;
    if (value.endsWith("/") || value.includes("://")) return false;
    return /\.[A-Za-z0-9]{1,8}$/.test(value);
  }
  function renderDetail(detail) {
    if (!detail) return "";
    return detail.split(" ").map((tok) => {
      const eq = tok.indexOf("=");
      if (eq <= 0) return esc(tok);
      const k = tok.slice(0, eq), v = tok.slice(eq + 1);
      if (!looksLikeSourceRef(k, v)) return esc(tok);
      return esc(k) + '=<span class="ref" data-path="' + esc(v) + '" title="Open this file in VS Code">' + esc(v) + "</span>";
    }).join(" ");
  }
  function renderJsonValue(v, depth) {
    const pad = (n) => "  ".repeat(n);
    if (v === null) return '<span class="j-b">null</span>';
    if (typeof v === "boolean") return '<span class="j-b">' + v + "</span>";
    if (typeof v === "number") return '<span class="j-n">' + v + "</span>";
    if (typeof v === "string") return '<span class="j-s">"' + esc(v) + '"</span>';
    if (Array.isArray(v)) {
      if (v.length === 0) return '<span class="j-p">[]</span>';
      const items = v.map((x) => pad(depth + 1) + renderJsonValue(x, depth + 1)).join('<span class="j-p">,</span>\n');
      return '<span class="j-p">[</span>\n' + items + "\n" + pad(depth) + '<span class="j-p">]</span>';
    }
    const entries = Object.entries(v);
    if (entries.length === 0) return '<span class="j-p">{}</span>';
    const rows = entries.map(([k, val]) => pad(depth + 1) + '<span class="j-k">"' + esc(k) + '"</span><span class="j-p">:</span> ' + renderJsonValue(val, depth + 1)).join('<span class="j-p">,</span>\n');
    return '<span class="j-p">{</span>\n' + rows + "\n" + pad(depth) + '<span class="j-p">}</span>';
  }
  function highlightJson(line) {
    try { return renderJsonValue(JSON.parse(line), 0); } catch (_) { return '<span class="j-raw">' + esc(line) + "</span>"; }
  }
  function buildLogRowsHtml(log) {
    if (log.length === 0) {
      return '<div class="empty">No daemon log yet. Run <span class="mono">travsr daemon start</span>.</div>';
    }
    return log.slice().reverse().flatMap((e, i, rows) => {
      const row = '<div class="log-line lvl-' + esc(e.level) + '" data-rank="' + rankOf(e.level) + '"' +
        ' data-iso="' + esc(e.iso) + '" data-tg="' + esc(e.target) + '">' +
        '<span class="caret" aria-hidden="true"></span>' +
        '<span class="mono muted t" data-local="' + esc(e.time) + '">' + esc(e.time) + "</span>" +
        '<span class="pill p-' + esc(e.level) + '">' + esc(e.level || "—") + "</span>" +
        '<span class="mono muted tg">' + esc(e.target) + "</span>" +
        '<span class="msg" data-raw="' + esc(e.message) + '">' + esc(e.message) + "</span>" +
        '<span class="mono muted detail" data-raw="' + esc(e.detail) + '">' + renderDetail(e.detail) + "</span>" +
        '<span class="jsonline mono">' + highlightJson(e.raw) + "</span></div>";
      const prev = i > 0 ? rows[i - 1].day : undefined;
      return i > 0 && e.day !== undefined && e.day !== prev
        ? ['<div class="log-day" data-day="' + esc(e.day || "") + '">' + esc(e.day || "") + "</div>", row]
        : [row];
    }).join("\n");
  }
  function activeLogLevel(log) {
    for (let i = log.length - 1; i >= 0; i -= 1) {
      if (log[i].event !== "daemon.session.start") continue;
      const detail = log[i].detail || "";
      const from = (/\blog_level_from=("?)([A-Za-z_.]+)\1/.exec(detail) || [])[2] || "";
      if (from !== "log.level" && from !== "default") return "";
      const raw = (/\blog_level=("?)([A-Za-z_=,.]+)\1/.exec(detail) || [])[2] || "";
      return LOG_LEVELS.includes(raw) ? raw : "";
    }
    return "";
  }

  // ── Parsers for the gathered sources (commands.ts) ─────────────────────
  function parseIndexHealth(raw) {
    const unknown = { isStale: null, behindBy: null, indexedCommit: "", headCommit: "", phaseA: "", workingTreeDirty: null, available: false };
    const body = ui.stripEnvelope(raw || "").trim();
    if (!body) return unknown;
    try {
      const p = JSON.parse(body), s = p.staleness || {};
      const str = (v) => (typeof v === "string" ? v : "");
      return {
        isStale: typeof s.is_stale === "boolean" ? s.is_stale : null,
        behindBy: typeof s.behind_by === "number" ? s.behind_by : null,
        indexedCommit: str(p.indexed_commit), headCommit: str(p.head_commit),
        phaseA: str(p.phase_a && p.phase_a.state),
        workingTreeDirty: typeof s.working_tree_dirty === "boolean" ? s.working_tree_dirty : null,
        available: true,
      };
    } catch (_) { return unknown; }
  }
  function readDiagnostics(out) {
    const found = [];
    for (const line of (out || "").split("\n")) {
      const m = /^\s*warning:\s*(.+)$/.exec(line);
      if (!m) continue;
      const text = m[1].trim();
      const cmd = /`([^`]+)`/.exec(text);
      found.push({
        severity: /crashed|failed|not usable/i.test(text) ? "error" : "warn",
        title: text.replace(/\s*[-—,;.]?\s*(re-?run|run)\s+`[^`]+`.*$/i, "").trim(),
        hint: text,
        command: cmd ? cmd[1] : undefined,
      });
    }
    return found;
  }
  function parseReposList(raw) {
    return ui.stripEnvelope(raw).split("\n").map((l) => l.replace(/\r$/, "")).filter((l) => l.trim()).map((l) => {
      const parts = l.split("\t");
      const row = { name: parts[0] || "", path: parts[1] || "", exists: parts[2] === "1" };
      if (parts[3]) row.status = parts[3];
      return row;
    });
  }
  function parseLangList(raw) {
    let parsed;
    try { parsed = JSON.parse((raw || "").trim() || "[]"); } catch (_) { return { langs: [], missingFields: [] }; }
    if (!Array.isArray(parsed)) return { langs: [], missingFields: [] };
    const objects = parsed.filter((r) => typeof r === "object" && r !== null);
    const missingFields = parsed.length === 0 ? [] : objects.length === 0 ? LANG_CONTRACT_FIELDS.slice()
      : LANG_CONTRACT_FIELDS.filter((f) => !objects.some((r) => f in r));
    const contract = objects[0] && objects[0].contract;
    return { langs: parsed, missingFields, reportedContract: typeof contract === "number" ? contract : undefined };
  }
  function parseRepoLanguages(raw) {
    const out = new Set();
    for (const line of ui.stripEnvelope(raw || "").split("\n")) {
      const lang = (line.split("\t")[0] || "").trim().toLowerCase();
      if (lang) out.add(lang);
    }
    return out;
  }
  function buildLanguageRows(langs, diags, detected) {
    const REPO_STATES = new Set(["always_on", "enabled", "needs_analyzer", "not_enabled", "no_repo"]);
    const OS_NAMES = { windows: "Windows", macos: "macOS", linux: "Linux" };
    return langs.map((l) => {
      const statusKnown = typeof l.status === "string";
      const full = l.status === "active";
      const availableHere = l.status !== "unsupported" && l.availableOnThisPlatform !== false;
      const osName = OS_NAMES[l.unavailableTarget || ""] || "";
      const repoState = REPO_STATES.has(String(l.repoState)) ? l.repoState : "unknown";
      const installed = l.installed === true;
      const builtin = l.builtin === true;
      const inRepo = detected.size === 0 || detected.has(String(l.language).toLowerCase());
      const re = new RegExp("\\b" + String(l.language).replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + "\\b", "i");
      const flagged = diags.some((d) => re.test(d.title + " " + d.hint));
      const analysis = !statusKnown ? "unknown" : full ? "full" : "structural only";
      const statusLine = typeof l.statusLine === "string" && l.statusLine !== "" ? l.statusLine : full ? "installed and enabled" : analysis;
      const prerequisites = l.prerequisites === undefined || l.prerequisites === null ? "unknown"
        : l.prerequisites && l.prerequisites !== "none" ? l.prerequisites : "";
      const READY = { ready: 1, setting_up: 1, needs_toolchain: 1, unsupported_os: 1, failed: 1, no_calls: 1 };
      const base = { language: l.language, analysis, full, statusLine, flagged, installed, repoState,
        availableHere, osName, prerequisites, builtin, inRepo, canDisable: availableHere && full && !builtin };
      if (typeof l.state === "string" && l.state in READY) {
        const canSetUp = availableHere && inRepo && ["setting_up", "needs_toolchain", "failed"].includes(l.state);
        return Object.assign(base, { fix: canSetUp ? "setup" : "none", fixText: typeof l.fix === "string" ? l.fix : undefined });
      }
      let fix = "none";
      if (!availableHere || !inRepo) fix = "none";
      else if (l.status === "needs_consent") fix = "permission";
      else if (!installed || repoState === "needs_analyzer") fix = "install";
      else if (repoState === "not_enabled") fix = "enable";
      else if (!full || flagged) fix = "semantic";
      return Object.assign(base, { fix });
    });
  }

  // ── Verdict (webviews.ts computeVerdict) ───────────────────────────────
  function computeVerdict(mcpConnected, daemonRunning, index, diags, hasGraph) {
    if (!hasGraph) {
      return { verdict: "unindexed", headline: "No graph yet",
        detail: "This repository has not been indexed, so there is nothing for the editor or an agent to traverse.",
        action: { label: "Index this repo", cmd: "travsr init" } };
    }
    if (!mcpConnected) {
      return { verdict: "offline", headline: "Not answering",
        detail: "Travsr could not be reached, so the numbers below are read from the graph on disk rather than from a live index.",
        action: { label: "Start daemon", cmd: "travsr daemon start" } };
    }
    if (!daemonRunning) {
      return { verdict: "degraded", headline: "Not watching",
        detail: index.isStale === true
          ? "Queries are answered from the graph on disk, but the daemon is not running, so commits and saves will not refresh it. The graph describes " + (index.indexedCommit || "an earlier commit") + "; starting the daemon catches it up and keeps it up."
          : "Queries are answered from the graph on disk, but the daemon is not running, so commits and saves will not refresh it.",
        action: { label: "Start daemon", cmd: "travsr daemon start" } };
    }
    if (index.isStale === true) {
      const behind = index.behindBy !== null && index.behindBy > 0
        ? "Your checkout is " + index.behindBy + " commit" + (index.behindBy > 1 ? "s" : "") + " ahead of it."
        : "Your checkout is on a different commit.";
      return { verdict: "stale", headline: "Stale",
        detail: "The graph describes " + (index.indexedCommit || "an earlier commit") + ". " + behind,
        action: { label: "Reindex", cmd: "travsr init" } };
    }
    const errs = diags.filter((d) => d.severity === "error").length;
    if (diags.length > 0) {
      const warns = diags.length - errs;
      const parts = [errs > 0 ? errs + " error" + (errs > 1 ? "s" : "") : "", warns > 0 ? warns + " warning" + (warns > 1 ? "s" : "") : ""].filter(Boolean);
      return { verdict: "degraded", headline: "Degraded",
        detail: "The graph is fresh, but " + parts.join(" and ") + " " + (diags.length === 1 ? "affects" : "affect") + " what it can answer." };
    }
    return { verdict: "healthy", headline: "Healthy",
      detail: index.available
        ? "Graph fresh at commit " + (index.indexedCommit || "HEAD") + ", with no analyzer or index problems reported."
        : "No analyzer or index problems reported." };
  }

  // ── Rendering helpers ──────────────────────────────────────────────────
  const card = (k, v, state, note) =>
    '<div class="card' + (state ? " " + state : "") + '"><div class="k">' + esc(k) + '</div><div class="v">' + esc(v) + "</div>" +
    (note ? '<div class="n">' + esc(note) + "</div>" : "") + "</div>";
  const statusChip = (tone, word) =>
    '<span class="chip ' + tone + '">' + (tone === "ok" ? OK_ICON : tone === "bad" ? BAD_ICON : tone === "warn" ? WARN_ICON : "") + esc(word) + "</span>";
  // The extension's action buttons run a command; here they copy it.
  const act = (label, cmd, tone) =>
    '<button class="btn mini ' + (tone === "primary" ? "primary" : "ghost") + '" data-copy="' + esc(cmd) + '" title="Copy: ' + esc(cmd) + '">' + esc(label) + "</button>";
  const row = (k, v, tone, trailing, keyTitle) =>
    '<div class="hrow"><span title="' + esc(keyTitle || k) + '">' + esc(k) + "</span>" +
    '<b class="' + (tone || "") + '">' + (tone === "ok" ? OK_ICON : tone === "warn" ? WARN_ICON : tone === "bad" ? BAD_ICON : "") + esc(v) + "</b>" +
    (trailing ? '<span class="ra">' + trailing + "</span>" : "") + "</div>";
  const section = (title, chipHtml, actions, inner) =>
    '<details class="hsec" open><summary>' + CHEV + '<span class="nm">' + esc(title) + "</span>" +
    chipHtml + (actions ? '<span class="sacts">' + actions + "</span>" : "") + "</summary>" +
    '<div class="hrows">' + inner + "</div></details>";
  const unavailable = (why) => '<div class="hrow muted">' + esc(why) + "</div>";
  // Plain text, no spinner: `.hrow > span:first-child` is the 110px key column,
  // and a rotating glyph in that box orbits instead of spinning.
  const pending = (what) => '<div class="hrow muted">Reading ' + esc(what) + "…</div>";

  function renderTop(d) {
    const s = d.status && d.status.status;
    const nodes = s ? s.nodes.toLocaleString() : "—";
    const hasGraph = !!s && s.nodes > 0;
    const daemonRunning = d.daemonRunning;
    const mcpConnected = !!d.status;
    const index = d.index;
    const diags = d.diags || [];
    // No verdict until the sources it ranks have answered: a premature "Not
    // watching" over a daemon that is merely slow to report would be wrong.
    const settled = d.daemonDetail !== null && d.indexLoaded && d.diagsLoaded;
    const verdict = (settled && d.statusLoaded) || (d.statusLoaded && !hasGraph)
      ? computeVerdict(mcpConnected, daemonRunning, index, diags, hasGraph)
      : { verdict: "unindexed", headline: "Checking", detail: "Reading daemon, index and analyzer status…" };
    const VERDICT_CLASS = { offline: "bad", stale: "warn2", degraded: "warn2", unindexed: "idle", healthy: "good" };
    const verdictBanner = '<div class="banner ' + VERDICT_CLASS[verdict.verdict] + ' verdict"><strong>' + esc(verdict.headline) + "</strong><span>" + esc(verdict.detail) + "</span>" +
      (verdict.action ? '<button class="btn primary vact" data-copy="' + esc(verdict.action.cmd) + '" title="Copy: ' + esc(verdict.action.cmd) + '">' + esc(verdict.action.label) + "</button>" : "") + "</div>";

    const diagCards = diags.length ? '<div class="diags">' + diags.map((x) => {
      const extra = x.hint.startsWith(x.title) ? x.hint.slice(x.title.length).trim() : x.hint;
      const body = extra !== "" && extra !== x.title ? extra.replace(/^[-,;.\s]+/, "") : "";
      return '<div class="diag ' + x.severity + '"><div class="diag-t">' + (x.severity === "error" ? "&#10007;" : "&#9888;") + " " + esc(x.title) + "</div>" +
        (body ? '<div class="diag-h">' + esc(body) + "</div>" : "") +
        (x.command ? '<div class="diag-a"><code class="diag-cmd">' + esc(x.command) + '</code><button class="btn mini ghost" data-copy="' + esc(x.command) + '">Copy</button></div>' : "") +
        "</div>";
    }).join("\n") + "</div>" : "";

    // Daemon
    const daemonSec = section("Daemon",
      d.daemonDetail === null ? statusChip("mute", "checking") : daemonRunning ? statusChip("ok", "running") : statusChip("bad", "not running"),
      daemonRunning ? act("Restart", "travsr daemon restart") + act("Stop", "travsr daemon stop") : act("Start", "travsr daemon start", "primary"),
      d.daemonDetail === null ? pending("daemon status") :
        row("Status", d.daemonDetail || (daemonRunning ? "running" : "not running"), daemonRunning ? "ok" : "bad") +
        row("Queries", mcpConnected ? "answering from the graph on disk" : "not answering", mcpConnected ? "ok" : "bad") +
        (!daemonRunning && mcpConnected ? row("Watching", "no, commits and saves will not refresh the graph", "warn") : "") +
        (d.daemonPid && daemonRunning ? row("Process", "pid " + d.daemonPid) : "") +
        (!daemonRunning && d.daemonStopped ? row("Last stopped", d.daemonStopped) : "") +
        (d.lastEditor ? row("Last editor", d.lastEditor) : "") +
        (d.binaryVersion ? row("Version", d.binaryVersion) : "") +
        (d.logFile ? row("Log file", d.logFile.name + "  " + ui.formatLogSize(d.logFile.size), undefined, '<button class="btn mini ghost" data-show-log>View log</button>') : ""));

    // Index freshness
    const indexInner = !d.indexLoaded ? pending("index status") : index.available
      ? row("Indexed commit", index.indexedCommit || "unknown") +
        row("Checkout HEAD", index.headCommit
          ? (index.behindBy !== null && index.behindBy > 0 ? index.headCommit + ", " + index.behindBy + " commit" + (index.behindBy > 1 ? "s" : "") + " ahead" : index.headCommit)
          : "unknown", index.isStale === true ? "warn" : undefined) +
        (index.workingTreeDirty === true ? row("Working tree", "modified files not yet indexed") : "") +
        (index.phaseA ? row("Phase A", index.phaseA + ", " + nodes + " nodes") : "") +
        (d.commitHook === null || d.commitHook === undefined ? ""
          : d.commitHook ? row("Commit hook", "installed", "ok")
          : row("Commit hook", "not installed, so commits do not refresh", "warn", act("Install", "travsr init", "primary")))
      : unavailable(d.indexError ? "Index status could not be read: " + d.indexError + "."
          : !mcpConnected ? "Index status could not be read: Travsr is not answering."
          : !daemonRunning ? "Index status could not be read while the daemon is stopped."
          : "Index status is not reported by this binary.");
    const indexSec = section("Index freshness",
      !d.indexLoaded ? statusChip("mute", "checking") : index.isStale === true ? statusChip("warn", "stale") : index.available ? statusChip("ok", "fresh") : statusChip("mute", "unknown"),
      act("Reindex", "travsr init", "primary") + act("Full rebuild", "travsr init --force"), indexInner);

    // Sidecars
    const sc = d.sidecars;
    const sidecarSec = section("Sidecars",
      sc === undefined ? statusChip("mute", "checking") : sc === null ? statusChip("mute", "unknown")
        : sc.every((x) => x.ok) ? statusChip("ok", sc.length + " ready") : statusChip("warn", sc.filter((x) => !x.ok).length + " missing"),
      "",
      sc === undefined ? pending("sidecars") : sc === null ? unavailable("Could not read the sidecar list.")
        : sc.map((x) => row(x.name, x.state, x.ok ? "ok" : "warn",
            (x.name === "embed" && d.embedModels.length > 1 ? act("Change model", "travsr embed list") : "") +
            (x.action ? act(x.action === "installEmbed" ? "Install" : "Reinstall",
              x.action === "installEmbed" ? "travsr embed init" : x.backend ? "travsr embed init --reinstall --backend " + x.backend : "travsr embed init --reinstall --yes",
              x.ok ? "ghost" : "primary") : ""))).join(""));

    // Storage and integrity
    const integ = d.integrity;
    const integSec = section("Storage and integrity",
      integ === undefined ? statusChip("mute", "checking") : integ === null ? statusChip("mute", "not checked")
        : integ.ghostCount && integ.ghostCount > 0 ? statusChip("warn", integ.ghostCount + " ghost path" + (integ.ghostCount > 1 ? "s" : "")) : statusChip("ok", "consistent"),
      act("Run fsck", "travsr fsck") + act("Compact", "travsr fsck --fix"),
      integ === undefined ? pending("the integrity report") : integ === null ? unavailable("Could not read the integrity report.")
        : row("graph.db", integ.dbSize) +
          row("Logs", integ.logSize + ", " + integ.logFiles + " file" + (integ.logFiles === 1 ? "" : "s") + ", pruned by the daemon") +
          row("Ghost paths", integ.ghostCount === null ? "unknown" : integ.ghostCount === 0 ? "none, every node points at a real file" : integ.ghostCount + " entries point at files that no longer exist", integ.ghostCount ? "warn" : "ok") +
          integ.ghostSample.slice(0, 2).map((p) => '<div class="hrow mono-only">' + esc(p) + "</div>").join("") +
          row("Text index", integ.lexicalOk === null ? "unknown" : integ.lexicalOk ? "consistent with the node table" : "out of step with the node table", integ.lexicalOk === false ? "warn" : "ok"));

    // Languages
    const langs = d.languages;
    const langHere = (langs || []).filter((l) => l.availableHere && l.inRepo);
    const langPartial = langHere.filter((l) => !l.full || l.flagged).length;
    const langSec = section("Languages",
      langs === undefined ? statusChip("mute", "checking") : langs === null ? statusChip("mute", "unknown")
        : langPartial > 0 ? statusChip("warn", langPartial + " partial") : statusChip("ok", langHere.length + " full"),
      act("Set up again", "travsr init"),
      d.languagesSkew ? skewBanner(d.languagesSkew)
        : langs === undefined ? pending("the language list (this can take a while)")
        : langs === null ? unavailable("Could not read the language list.")
        : renderLanguages(langs));

    // Agent connections: the extension reads editor config files on the
    // machine it runs on. A browser page has no such view, so it names the
    // command that reports and writes them.
    const agentSec = section("Agent connections", statusChip("mute", "see CLI"),
      act("Register all", "travsr connect", "primary"),
      row("Status", "travsr connect --print shows which AI tools are wired to Travsr", undefined, act("Copy", "travsr connect --print")));

    // Repositories
    const repos = d.repos;
    const activeRepo = ui.repo;
    const repoTemps = (repos || []).filter((r) => r.name !== activeRepo && isTempRepo(r.name));
    const repoReal = (repos || []).filter((r) => !(r.name !== activeRepo && isTempRepo(r.name)));
    const repoStale = (repos || []).filter((r) => !r.exists).length;
    const repoSec = section("Repositories",
      repos === undefined ? statusChip("mute", "checking") : repos === null ? statusChip("mute", "unknown")
        : statusChip("mute", repoReal.length + " registered") + (repoTemps.length > 0 ? statusChip("mute", repoTemps.length + " from tests") : ""),
      repoStale > 0 ? act("Prune stale (" + repoStale + ")", "travsr repos --prune", "primary") : "",
      repos === undefined ? pending("the registry") : repos === null ? unavailable("Could not read the repository registry.") : renderRepos(repoReal, repoTemps, activeRepo));

    const sections = '<div class="hcols"><div class="hcol">' + daemonSec + indexSec + sidecarSec + integSec +
      '</div><div class="hcol">' + langSec + diagCards + agentSec + repoSec + "</div></div>";

    const semanticTile = (() => {
      const ls = langs ? langs.filter((l) => l.availableHere && l.inRepo) : null;
      if (!ls || ls.length === 0) return "";
      const bad = ls.filter((l) => !l.full || l.flagged);
      return bad.length > 0 ? card("Semantic", (ls.length - bad.length) + " of " + ls.length, "warn", bad.map((l) => l.language).join(", "))
        : card("Semantic", "full", undefined, ls.length + " languages");
    })();
    const dbSize = d.status && d.status.db_bytes != null ? (d.status.db_bytes / 1048576).toFixed(1) + " MB" : "—";
    const lastIndexed = d.status && d.status.db_modified_ms ? ui.timeAgo(Date.now() - d.status.db_modified_ms) : "—";

    return verdictBanner +
      '<div class="cards">' +
      card("Nodes", nodes, undefined, "on disk") +
      card("Edges", s ? s.edges.toLocaleString() : "—") +
      card("Schema", s ? String(s.schema) : "—") +
      card("DB size", dbSize) +
      card("Last indexed", lastIndexed, index.isStale === true ? "warn" : undefined, index.isStale === true ? "stale" : undefined) +
      semanticTile + "</div>" + sections;
  }

  function skewBanner(skew) {
    const fields = skew.missingFields.length ? "It does not report " + skew.missingFields.map((f) => "<code>" + esc(f) + "</code>").join(", ") + "." : "";
    const reported = skew.reportedContract === undefined ? "It reports no contract revision at all (this panel needs 1)."
      : "It reports contract revision " + skew.reportedContract + "; this panel needs 1.";
    return '<div class="banner warn" role="alert"><b>The <code>travsr</code> serving this page is older than this page expects.</b>' +
      '<p style="margin:6px 0 0">' + reported + " " + fields + " Rather than show cells it would have to guess at, the language rows are held back.</p></div>";
  }

  function renderLanguages(langs) {
    const thead = '<thead><tr><th>Language</th><th>Analysis</th><th>Global installed</th><th>This repo</th><th class="ra">Action</th></tr></thead>';
    const REPO_TEXT = { always_on: "always on", enabled: "enabled", needs_analyzer: "no analyzer", not_enabled: "not enabled", no_repo: "n/a", unknown: "unknown" };
    const body = langs.filter((l) => l.inRepo).map((l) => {
      const tone = !l.availableHere ? "mute" : !l.full || l.flagged ? "warn" : "ok";
      const icon = tone === "ok" ? OK_ICON : tone === "warn" ? WARN_ICON : "";
      const analysisText = !l.availableHere ? "not available on " + (l.osName || "this platform") : l.flagged ? l.analysis + ", see the warning below" : l.analysis;
      const globalCell = !l.availableHere ? '<td class="mute">-</td>' : l.installed ? '<td class="ok">' + OK_ICON + "installed</td>" : '<td class="warn">' + WARN_ICON + "not installed</td>";
      const repoTone = !l.availableHere || l.repoState === "no_repo" ? "mute" : l.repoState === "always_on" || l.repoState === "enabled" ? "ok" : "warn";
      const repoCell = !l.availableHere ? '<td class="mute">-</td>'
        : '<td class="' + repoTone + '">' + (repoTone === "ok" ? OK_ICON : repoTone === "warn" ? WARN_ICON : "") + esc(REPO_TEXT[l.repoState] || "unknown") + "</td>";
      const name = l.language;
      const FIX = {
        install: ["Install analyzer", "travsr lang install " + name + " --no-interactive --yes"],
        enable: ["Enable for this repo", "travsr lang install " + name + " --skip-wrapper --no-interactive --yes"],
        permission: ["Allow & enable", "travsr lang allow-unsandboxed " + name + " --yes"],
        semantic: ["Re-run semantic", "travsr init --force"],
        setup: ["Set up again", "travsr init"],
      };
      const primary = FIX[l.fix] ? act(FIX[l.fix][0], FIX[l.fix][1], "primary") : "";
      const disable = l.canDisable ? act("Disable", "travsr lang remove " + name) : "";
      const actions = primary || disable ? primary + (primary && disable ? " " : "") + disable : "-";
      const prereq = l.prerequisites !== "" ? '<div class="lprereq">needs: ' + esc(l.prerequisites) + "</div>" : "";
      return "<tr><td>" + esc(name) + prereq + '</td><td class="' + tone + '">' + icon + esc(analysisText) + "</td>" + globalCell + repoCell + '<td class="ra">' + actions + "</td></tr>";
    }).join("");
    return body ? '<table class="ltbl">' + thead + "<tbody>" + body + "</tbody></table>" : unavailable("The graph found no supported languages in this repository yet.");
  }

  function renderRepos(repoReal, repoTemps, activeRepo) {
    const REPO_ROWS = 8;
    const rank = (r) => (r.name === activeRepo ? 0 : r.exists ? 1 : 2);
    const sorted = repoReal.slice().sort((a, b) => rank(a) - rank(b));
    const shown = sorted.slice(0, REPO_ROWS), hidden = sorted.slice(REPO_ROWS);
    const hiddenStale = hidden.filter((r) => !r.exists).length;
    const repoRow = (r) => {
      const active = r.name === activeRepo;
      return row(r.name, r.exists ? (active ? "active" : "database present") : (active ? "active, no database" : "database missing"),
        r.exists ? "ok" : "warn", "", r.path ? r.name + "\nGraph database: " + r.path : r.name);
    };
    return shown.map(repoRow).join("") +
      (hidden.length > 0 ? '<div class="hrow muted">and ' + hidden.length + " more" + (hiddenStale > 0 ? ", " + hiddenStale + " with no database" : "") +
        ' <button class="link-btn" aria-expanded="false" data-toggle="repoMore">Show</button></div><div id="repoMore" hidden>' + hidden.map(repoRow).join("") + "</div>" : "") +
      (repoTemps.length > 0 ? '<div class="hrow muted">' + repoTemps.length + " test " + (repoTemps.length === 1 ? "repository" : "repositories") +
        ' left by test runs, not listed <button class="link-btn" aria-expanded="false" data-toggle="repoTemps">Show</button></div><div id="repoTemps" hidden>' + repoTemps.map(repoRow).join("") + "</div>" : "") +
      (shown.length === 0 && repoTemps.length === 0 ? '<div class="hrow muted">No repositories registered.</div>' : "");
  }

  function renderActivity(log) {
    const labelled = log.filter((e) => e.event !== undefined && EVENT_LABELS[e.event] !== undefined);
    const runs = [];
    for (const e of labelled) {
      const last = runs[runs.length - 1];
      if (last && last.entry.event === e.event) { last.entry = e; last.count += 1; } else runs.push({ entry: e, count: 1 });
    }
    const activity = runs.slice(-8).reverse();
    return activity.length ? activity.map(({ entry: e, count }) =>
      '<tr class="lvl-' + esc(e.level) + '" data-fam="' + esc(EVENT_FAMILY[e.event] || "other") + '"><td class="fam"><span class="fam-dot"></span></td>' +
      '<td class="mono muted when">' + esc(e.time) + '</td><td class="what">' + esc(EVENT_LABELS[e.event]) + (count > 1 ? ' <span class="run">&times;' + count + "</span>" : "") +
      '</td><td class="mono muted detail">' + renderDetail(e.detail) + "</td></tr>").join("\n")
      : '<tr><td colspan="4" class="empty">No lifecycle events yet. Start the daemon to see activity.</td></tr>';
  }

  // ── Page state ─────────────────────────────────────────────────────────
  const state = {
    loadedLines: 500, logFile: "", autoSeconds: 0, autoTimer: null, minRank: 0,
    data: null, log: [], logFiles: null, renderedAt: Date.now(), seq: 0,
  };

  function pageShell(row) {
    const autoOptions = LOG_AUTO_SECONDS.map((s) => '<option value="' + s + '"' + (s === state.autoSeconds ? " selected" : "") + ">" + (s === 0 ? "Off" : s + "s") + "</option>").join("");
    return '<div class="phead"><div><h2>' + esc(row.name) + '</h2><p class="sub mono">' + esc(row.root) + "</p></div>" +
      '<div class="phead-a"><span class="checked" id="checkedAt">checked just now</span>' +
      '<label class="sel">Auto <select id="logAuto" title="Refresh the page on a timer. Off by default.">' + autoOptions + "</select></label>" +
      '<button class="btn" id="refreshBtn">Refresh</button></div></div>' +
      '<div id="top"></div>' +
      '<div class="split"><section class="col-activity"><h2>Recent activity</h2><p class="sub">Lifecycle events from the daemon, newest first.</p>' +
      '<table class="activity"><tbody id="activityBody"></tbody></table></section>' +
      '<section class="col-log" id="daemonLogSection"><h2>Daemon log</h2>' +
      '<div class="log-bar"><input id="logSearch" type="search" placeholder="Filter lines…" aria-label="Filter log lines">' +
      '<div class="chips" role="group" aria-label="Minimum severity" id="logChips"></div>' +
      '<span class="count" id="logCount"></span></div>' +
      '<div class="log-bar modes"><span id="fileControl"></span>' +
      '<label class="sel">Lines <select id="logLines">' +
      [100, 200, 500, 2000, LOG_MAX_LINES].map((n) => '<option value="' + n + '"' + (n === state.loadedLines ? " selected" : "") + ">" + (n === LOG_MAX_LINES ? "All (max " + n + ")" : n) + "</option>").join("") +
      "</select></label>" +
      '<label class="sel">Since <select id="logSince"><option value="0" selected>All</option><option value="5">5m</option><option value="60">1h</option><option value="1440">24h</option></select></label>' +
      '<label class="tog"><input type="checkbox" id="logUtc"> UTC</label>' +
      '<label class="tog"><input type="checkbox" id="logJson"> JSON</label>' +
      '<span id="logLevelInfo"></span></div>' +
      '<div id="logNote"></div>' +
      '<div class="log" id="logBox"></div></section></div>';
  }

  function renderLog() {
    const log = state.log;
    const counts = { all: log.length, info: 0, warn: 0, error: 0 };
    for (const e of log) {
      const r = rankOf(e.level);
      if (r >= 2) counts.info += 1;
      if (r >= 3) counts.warn += 1;
      if (r >= 4) counts.error += 1;
    }
    const RANKS = { all: 0, info: 2, warn: 3, error: 4 };
    document.getElementById("logChips").innerHTML = [["all", "All"], ["info", "Info+"], ["warn", "Warn+"], ["error", "Error"]]
      .map(([id, label]) => '<button class="chip-btn' + (RANKS[id] === state.minRank ? " on" : "") + '" data-level="' + id + '">' + label + ' <span class="chip-n">' + counts[id] + "</span></button>").join("");
    const lf = state.logFiles;
    const today = new Date().toISOString().slice(0, 10);
    const yesterday = (() => { const d = new Date(today + "T00:00:00Z"); d.setUTCDate(d.getUTCDate() - 1); return d.toISOString().slice(0, 10); })();
    document.getElementById("fileControl").innerHTML = lf && lf.files.length > 0
      ? '<label class="sel">File <select id="logFile">' + lf.files.map((f) => {
          const day = f.name.slice("daemon.log".length).replace(/^\./, "") || f.name;
          const rel = day === today ? "today" : day === yesterday ? "yesterday" : "";
          const label = [day, rel, ui.formatLogSize(f.size)].filter(Boolean).join(" · ");
          return '<option value="' + esc(f.name) + '"' + (f.name === lf.selected ? " selected" : "") + ">" + esc(label) + "</option>";
        }).join("") + '</select></label> <span class="hint" title="The daemon rotates on the UTC date, so one file covers one UTC day. Turn on UTC to read the times on the same clock.">UTC days</span>' +
        (lf.on_disk > lf.files.length ? ' <span class="hint">' + lf.files.length + " of " + lf.on_disk + " files</span>" : "")
      : "";
    const d = state.data;
    document.getElementById("logLevelInfo").innerHTML = d && d.logLevel
      ? '<span class="hint" title="Change with: travsr config set log.level <level>. Takes effect when the daemon next starts.">Write: ' + esc(d.logLevel) + (d.logLevelSource ? " (" + esc(d.logLevelSource) + ")" : "") + "</span>"
      : "";
    const activeLevel = activeLogLevel(log);
    document.getElementById("logNote").innerHTML = d && d.daemonRunning && d.logLevel && activeLevel && activeLevel !== d.logLevel
      ? '<div class="log-note">The running daemon is still writing at <b>' + esc(activeLevel) + "</b>. <b>" + esc(d.logLevel) + "</b> applies when it next starts. " + act("Restart daemon", "travsr daemon restart") + "</div>"
      : "";
    const box = document.getElementById("logBox");
    const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 4;
    const was = box.scrollTop;
    box.innerHTML = '<div class="empty" id="logEmpty" style="display:none">No lines match this filter.</div>' + buildLogRowsHtml(log);
    document.getElementById("activityBody").innerHTML = renderActivity(log);
    filterLog();
    box.scrollTop = atBottom ? box.scrollHeight : was;
  }

  function mark(el, q) {
    if (!el) return;
    const raw = el.getAttribute("data-raw") || "";
    el.textContent = "";
    if (!q) { el.textContent = raw; return; }
    const lower = raw.toLowerCase();
    let i = 0, at;
    while ((at = lower.indexOf(q, i)) !== -1) {
      if (at > i) el.appendChild(document.createTextNode(raw.slice(i, at)));
      const m = document.createElement("mark");
      m.textContent = raw.slice(at, at + q.length);
      el.appendChild(m);
      i = at + q.length;
    }
    el.appendChild(document.createTextNode(raw.slice(i)));
  }

  function filterLog() {
    const box = document.getElementById("logBox");
    if (!box) return;
    const q = (document.getElementById("logSearch").value || "").toLowerCase();
    const maxLines = Number(document.getElementById("logLines").value);
    const sinceMin = Number(document.getElementById("logSince").value);
    const utc = document.getElementById("logUtc").checked;
    const asJson = document.getElementById("logJson").checked;
    const cutoff = sinceMin ? Date.now() - sinceMin * 60000 : 0;
    box.classList.toggle("json-mode", asJson);
    const lines = box.querySelectorAll(".log-line");
    let shown = 0;
    for (const line of lines) {
      const iso = line.getAttribute("data-iso");
      const tEl = line.querySelector(".t");
      if (iso && tEl) tEl.textContent = utc ? new Date(iso).toISOString().slice(11, 19) : (tEl.getAttribute("data-local") || "");
      const vis = Number(line.getAttribute("data-rank")) >= state.minRank &&
        (!cutoff || (iso && new Date(iso).getTime() >= cutoff)) &&
        (!q || line.textContent.toLowerCase().indexOf(q) !== -1) && shown < maxLines;
      line.style.display = vis ? "" : "none";
      if (vis) { shown++; mark(line.querySelector(".msg"), q); mark(line.querySelector(".detail"), q); }
    }
    let pendingDay = null;
    for (const el of box.children) {
      if (el.classList.contains("log-day")) { el.style.display = "none"; pendingDay = el; }
      else if (pendingDay && el.classList.contains("log-line") && el.style.display !== "none") { pendingDay.style.display = ""; pendingDay = null; }
    }
    const total = lines.length;
    const label = document.getElementById("logCount");
    label.textContent = shown === total ? total + " lines" : shown + " of " + total + " lines";
    label.classList.toggle("filtered", shown !== total);
    const empty = document.getElementById("logEmpty");
    if (empty) empty.style.display = shown === 0 && total > 0 ? "" : "none";
  }

  // ── Gathering (commands.ts gatherHealth) ───────────────────────────────
  const settle = (p, fallback) => p.then((v) => v, () => fallback);

  async function loadLog() {
    const r = await settle(ui.api("/api/logs", { repo: ui.repo, lines: state.loadedLines, file: state.logFile }), null);
    if (!r) { state.log = []; state.logFiles = null; return; }
    const day = r.selected ? r.selected.slice("daemon.log".length).replace(/^\./, "") || r.selected : undefined;
    state.log = r.lines.map((l) => parseLogLine(l, day));
    state.logFiles = r;
    state.logFile = r.selected || "";
  }

  async function gather(row) {
    const seq = ++state.seq;
    const d = {
      status: null, index: parseIndexHealth(""), diags: [], daemonRunning: false, daemonDetail: null,
      sidecars: undefined, embedModels: [], integrity: undefined, languages: undefined, repos: undefined,
      commitHook: null, binaryVersion: "", logLevel: "", logLevelSource: "",
      daemonPid: "", daemonStopped: "", lastEditor: "", logFile: null,
    };
    state.data = d;
    const draw = () => {
      if (seq !== state.seq) return;
      document.getElementById("top").innerHTML = renderTop(d);
    };
    draw();

    // Queued first: `lang list` is the slowest source by far, so it gets a CLI
    // slot before the quick ones rather than starting after them.
    const langCliP = settle(ui.cli("lang-list"), null);
    const statusP = settle(ui.api("/api/status", { repo: ui.repo }), null).then((s) => {
      d.status = s;
      d.statusLoaded = true;
      d.commitHook = s ? s.commit_hook : null;
      ui.setConnection(s ? s.route : null);
    });
    const logP = loadLog().then(() => {
      const log = state.log;
      const findEvent = (re) => { for (const e of log) { const m = re.exec(e.message || ""); if (m) return m[1] || ""; } return ""; };
      d.daemonPid = findEvent(/\bpid[= ](\d+)/);
      const stop = log.find((e) => /daemon stopped|shutting down/i.test(e.message || ""));
      if (stop) {
        const t = new Date(stop.iso);
        const p = (n) => String(n).padStart(2, "0");
        d.daemonStopped = isNaN(t.getTime()) ? stop.time
          : t.getFullYear() + "-" + p(t.getMonth() + 1) + "-" + p(t.getDate()) + " " + p(t.getHours()) + ":" + p(t.getMinutes()) + ":" + p(t.getSeconds());
      }
      const ed = log.find((e) => /editor (attached|detached)/i.test(e.message || ""));
      if (ed) {
        const sess = /session=([\w-]+)/.exec(ed.message || "");
        d.lastEditor = ((sess ? sess[1] : "editor") + " " + (/detached/i.test(ed.message || "") ? "detached" : "attached") + " " + (ed.time || "")).trim();
      }
      const newest = state.logFiles && state.logFiles.files[0];
      d.logFile = newest || null;
      if (seq === state.seq) renderLog();
    });
    const indexP = ui.tool("get_index_status").then(
      (raw) => { d.index = parseIndexHealth(raw); },
      (e) => { d.indexError = e.message; }
    ).then(() => { d.indexLoaded = true; });
    const diagsP = settle(ui.cli("status-verbose"), null).then((r) => { d.diags = r ? readDiagnostics(r.out) : []; d.diagsLoaded = true; });
    const daemonP = Promise.all([settle(ui.cli("daemon-status"), null), statusP]).then(([r]) => {
      const line = r ? ((/^daemon:\s*(.+)$/m.exec(r.out) || [])[1] || "").trim() : "";
      if (line) {
        d.daemonRunning = /^running|^starting/.test(line);
        d.daemonDetail = line;
        return;
      }
      // `daemon status` gave no answer (timed out, or could not start). That
      // is not evidence the daemon is down, so fall back to whether it just
      // answered this page's own status query.
      const answered = !!d.status && d.status.route === "daemon";
      d.daemonRunning = answered;
      d.daemonDetail = answered
        ? "running (answered a query; `travsr daemon status` did not reply in time)"
        : "unknown (`travsr daemon status` did not reply in time)";
    });
    const versionP = settle(ui.cli("version"), null).then((r) => { d.binaryVersion = r ? ((/(\d+\.\d+\.\d+[^\s]*)/.exec(r.out) || [])[1] || "") : ""; });
    const configP = settle(ui.cli("config-list"), null).then((r) => {
      if (!r || !r.stdout.trim()) return;
      try {
        const row = JSON.parse(r.stdout).find((x) => x.key === "log.level");
        const value = row && (typeof row.value === "string" && row.value !== "" ? row.value : row.default);
        if (LOG_LEVELS.includes(value)) { d.logLevel = value; d.logLevelSource = row.source || ""; }
      } catch (_) { /* hide the control */ }
    });
    const embedP = settle(ui.cli("embed-list"), null).then((r) => {
      if (!r || !r.stdout.trim()) { d.sidecars = null; return; }
      try {
        const models = JSON.parse(r.stdout);
        d.embedModels = models.filter((m) => typeof m.id === "string");
        const active = models.find((m) => m.active === true);
        const anyInstalled = models.some((m) => m.installed === true);
        const ok = !!active && active.installed === true;
        d.sidecars = [{ name: "embed", ok, backend: active && active.id,
          state: ok ? (active.id || "model") + ", ready" : anyInstalled ? "installed but not active" : "not installed, semantic search off",
          action: ok ? "reinstallEmbed" : "installEmbed" }];
      } catch (_) { d.sidecars = null; }
    });
    const healthP = Promise.all([settle(ui.tool("get_graph_health"), ""), statusP, logP]).then(([raw]) => {
      const body = ui.stripEnvelope(raw).trim();
      if (!body) { d.integrity = null; return; }
      try {
        const h = JSON.parse(body);
        const files = (state.logFiles && state.logFiles.files) || [];
        d.integrity = {
          ghostCount: h.ghost_paths && typeof h.ghost_paths.count === "number" ? h.ghost_paths.count : null,
          ghostSample: h.ghost_paths && Array.isArray(h.ghost_paths.sample) ? h.ghost_paths.sample.slice(0, 3) : [],
          lexicalOk: h.lexical_index_parity && typeof h.lexical_index_parity.ok === "boolean" ? h.lexical_index_parity.ok : null,
          dbSize: d.status && d.status.db_bytes != null ? (d.status.db_bytes / 1048576).toFixed(1) + " MB" : "—",
          logSize: ui.formatLogSize(files.reduce((a, f) => a + f.size, 0)),
          logFiles: files.length,
        };
      } catch (_) { d.integrity = null; }
    });
    const reposP = settle(ui.tool("repos_list"), "").then((raw) => { d.repos = raw.trim() ? parseReposList(raw) : null; });
    const repoLangsP = settle(ui.tool("repo_languages"), "");
    // Slowest last: `lang list` can take most of a minute on a cold Windows cache.
    const langsP = Promise.all([langCliP, repoLangsP, diagsP]).then(([r, repoLangs]) => {
      if (!r || !r.stdout.trim()) { d.languages = null; return; }
      const parsed = parseLangList(r.stdout);
      if (parsed.missingFields.length > 0) { d.languages = null; d.languagesSkew = parsed; }
      else d.languages = parsed.langs.length > 0 ? buildLanguageRows(parsed.langs, d.diags, parseRepoLanguages(repoLangs)) : null;
    });

    for (const p of [statusP, logP, indexP, diagsP, daemonP, versionP, configP, embedP, healthP, reposP, langsP]) p.then(draw);
    await Promise.all([statusP, logP, indexP, diagsP, daemonP, versionP, configP, embedP, healthP, reposP, langsP]);
    if (seq === state.seq) { state.renderedAt = Date.now(); renderLog(); }
  }

  // ── Events (delegated: the CSP allows no inline handlers) ──────────────
  function wire(row) {
    const page = document.getElementById("page");
    page.addEventListener("click", async (ev) => {
      const copy = ev.target.closest("[data-copy]");
      if (copy) {
        // A button in a section header sits inside <summary>; copying must not
        // also fold the section.
        ev.preventDefault();
        const cmd = copy.getAttribute("data-copy");
        const label = copy.textContent;
        try { await navigator.clipboard.writeText(cmd); copy.textContent = "Copied"; }
        catch (_) { window.prompt("Run this in a terminal in " + row.root, cmd); }
        setTimeout(() => { copy.textContent = label; }, 1200);
        return;
      }
      const tog = ev.target.closest("[data-toggle]");
      if (tog) {
        const box = document.getElementById(tog.getAttribute("data-toggle"));
        if (box) { box.hidden = !box.hidden; tog.textContent = box.hidden ? "Show" : "Hide"; tog.setAttribute("aria-expanded", String(!box.hidden)); }
        return;
      }
      if (ev.target.closest("[data-show-log]")) {
        document.getElementById("daemonLogSection").scrollIntoView({ behavior: "smooth", block: "start" });
        document.getElementById("logSearch").focus({ preventScroll: true });
        return;
      }
      const chip = ev.target.closest(".chip-btn[data-level]");
      if (chip) {
        state.minRank = { all: 0, info: 2, warn: 3, error: 4 }[chip.getAttribute("data-level")];
        for (const c of document.querySelectorAll(".chip-btn")) c.classList.toggle("on", c === chip);
        filterLog();
        return;
      }
      const ref = ev.target.closest(".ref");
      if (ref) {
        ev.stopPropagation();
        const link = ui.editorLink(ref.getAttribute("data-path"));
        if (link) location.href = link;
        return;
      }
      const line = ev.target.closest(".log-line");
      if (line && document.getElementById("logBox").classList.contains("json-mode")) line.classList.toggle("open");
      if (ev.target.id === "refreshBtn") gather(row);
    });
    page.addEventListener("input", (ev) => { if (ev.target.id === "logSearch") filterLog(); });
    page.addEventListener("change", async (ev) => {
      const id = ev.target.id;
      if (id === "logSince" || id === "logUtc" || id === "logJson") filterLog();
      else if (id === "logLines") {
        const want = Number(ev.target.value);
        if (want > state.loadedLines) { state.loadedLines = want; await loadLog(); renderLog(); } else filterLog();
      } else if (id === "logFile") { state.logFile = ev.target.value; await loadLog(); renderLog(); }
      else if (id === "logAuto") {
        state.autoSeconds = Number(ev.target.value);
        clearInterval(state.autoTimer);
        // The log tail refreshes in place, as the extension's auto tick does;
        // the search box, chips and toggles keep their state.
        if (state.autoSeconds > 0) state.autoTimer = setInterval(async () => { await loadLog(); renderLog(); }, state.autoSeconds * 1000);
      }
    });
    setInterval(() => {
      const el = document.getElementById("checkedAt");
      if (!el) return;
      const s = Math.max(0, Math.round((Date.now() - state.renderedAt) / 1000));
      el.textContent = s < 5 ? "checked just now" : s < 60 ? "checked " + s + "s ago"
        : Math.floor(s / 60) < 60 ? "checked " + Math.floor(s / 60) + "m ago"
        : "checked " + Math.floor(s / 3600) + "h " + (Math.floor(s / 60) % 60) + "m ago";
    }, 1000);
  }

  async function main() {
    const row = await ui.init("/health");
    const page = document.getElementById("page");
    if (!row) {
      page.innerHTML = '<h2>Health</h2><div class="banner idle verdict"><strong>No graph yet</strong><span>No indexed repositories found. Run travsr init in a git repository, then reload this page.</span>' +
        '<button class="btn primary vact" data-copy="travsr init">Index a repo</button></div>';
      wire({ root: "" });
      return;
    }
    page.innerHTML = pageShell(row);
    wire(row);
    if (!row.indexed) {
      document.getElementById("top").innerHTML = renderTop({ status: null, statusLoaded: true, index: parseIndexHealth(""), indexLoaded: true, diags: [], diagsLoaded: true, daemonDetail: "", sidecars: null, embedModels: [], integrity: null, languages: null, repos: null });
      renderLog();
      return;
    }
    await gather(row);
  }

  main();
})();
