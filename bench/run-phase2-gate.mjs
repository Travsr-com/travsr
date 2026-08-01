#!/usr/bin/env node
// #376 Phase 2 gate — plan §7's four gates, run against the live daemon-facing
// MCP process (not the offline flat-search prototype in docs-bench.mjs).
//
// Gate 1: doc hit@1 >= 0.60, hit@3 >= 0.90 on bench/queries-docs-{repo}.json
// Gate 2: zero regression in code hit@1/hit@10 on bench/queries-seeded-{repo}.json
//         (TRAVSR_DOCS_ENABLED=1 vs unset, same indexed corpus, no reindex between)
// Gate 3: zero regression on the abstain arm (nonsense/salad/degenerate categories)
// Gate 4: enabling the docs lane adds no second query-embedding inference
//         (plan §4.4's "one inference, two searches" contract)
//
// Gate 5: the `ask` surface renders the docs lane, at parity with get_context
//         (#516 added a second surface that gates 1-4 never touch)
//
// Gate 5 exists because gates 1-4 all call get_context over MCP, while #516
// added `travsr ask` as a second surface that renders docs through a different
// path: CLI -> daemon control socket -> ask_query -> docs_section, with its own
// hook arming, its own renderer and its own abstain return. #516 itself was
// found only by running that surface by hand — the daemon never called
// set_embed_doc_knn_hook, so `ask` saw None, which is indistinguishable from
// "sidecar too old" or "no doc-chunk nodes" and renders as no section, with no
// warning and no error. A gate that scores one surface cannot see that class of
// bug at all, so Gate 5 scores `ask` directly and compares it against
// get_context query by query.
//
// Gate 4 was previously "combined KNN latency within 10% of the code-only
// baseline", whose stated purpose in plan §7 was precisely to prove §4.4's
// one-inference claim. Wall-clock cannot prove that: it is dominated by the
// cross-encoder passes downstream, so it both (a) failed for an unrelated
// reason — a second full rerank pass, §12 — and (b) missed the actual
// violation, a memo-cache miss caused by the two lanes normalizing the query
// differently. Gate 4 now asserts the contract directly, by counting
// inferences in the sidecar. The cross-encoder cost is still measured and
// reported, as an explicitly accepted non-gating line item (see §15).
//
// Usage:
//   BENCH_REPO=/Users/ak/Documents/k8/kubernetes BENCH_LABEL=k8s node bench/run-phase2-gate.mjs
//   BENCH_LABEL=travsr node bench/run-phase2-gate.mjs   (defaults BENCH_REPO to this repo)
//
// Exit code is 0 only when every gate passes — this is a gate, not a report.
//
// !! Gate 5 restarts the daemon for the repo under test. `docs_enabled()` is
// read by whichever process performs retrieval, and for `ask` that is the
// daemon, not the CLI: `TRAVSR_DOCS_ENABLED=1 travsr ask ...` sets it on the
// wrong process and silently does nothing. Any daemon running when this script
// starts is stopped and a plain one is started again at the end (its original
// environment cannot be recovered — the script says so when it happens).

import { spawn, execFile } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, "..");
const BIN = join(ROOT, "target/release/travsr");
const LABEL = process.env.BENCH_LABEL || "travsr";
const REPO = process.env.BENCH_REPO || ROOT;
const BUDGET = process.env.BENCH_BUDGET || "4000";
const seededPath = join(HERE, `queries-seeded-${LABEL}.json`);
const docsPath = join(HERE, `queries-docs-${LABEL}.json`);

// Sidecar query-embedding trace. Written by travsr-embed to this file (not to
// stderr: the plugin host captures sidecar stderr into a bounded ring buffer
// for error surfacing, so it never reaches this process).
const EMBED_TRACE_PATH = join(HERE, `_query-embed-trace-${LABEL}.tsv`);

// "QUERY_EMBED_CACHE\t<space>\t<hit|miss>\t<query>" — query is last so a tab
// inside it cannot shift the fields we read.
function readEmbedTrace() {
  let raw;
  try {
    raw = readFileSync(EMBED_TRACE_PATH, "utf8");
  } catch {
    return [];
  }
  const out = [];
  for (const line of raw.split("\n")) {
    if (!line.startsWith("QUERY_EMBED_CACHE\t")) continue;
    const f = line.split("\t");
    if (f.length < 4) continue;
    out.push({ space: f[1], outcome: f[2], query: f.slice(3).join("\t") });
  }
  return out;
}

class Mcp {
  constructor(envExtra) {
    this.proc = spawn(BIN, ["mcp"], {
      cwd: REPO,
      stdio: ["pipe", "pipe", "ignore"],
      env: { ...process.env, ...envExtra },
    });
    this.buf = "";
    this.pending = new Map();
    this.nextId = 1;
    this.proc.stdout.on("data", (d) => this._onData(d));
  }
  _onData(d) {
    this.buf += d.toString();
    let nl;
    while ((nl = this.buf.indexOf("\n")) >= 0) {
      const line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + 1);
      if (!line.trim()) continue;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        continue;
      }
      if (msg.id != null && this.pending.has(msg.id)) {
        const { resolve } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        resolve(msg);
      }
    }
  }
  _send(obj) {
    this.proc.stdin.write(JSON.stringify(obj) + "\n");
  }
  request(method, params) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this._send({ jsonrpc: "2.0", id, method, params });
      setTimeout(() => {
        if (this.pending.has(id)) {
          this.pending.delete(id);
          reject(new Error("timeout " + method));
        }
      }, 120000);
    });
  }
  async init() {
    await this.request("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "phase2-gate", version: "1" },
    });
    this._send({ jsonrpc: "2.0", method: "notifications/initialized" });
  }
  async call(name, args) {
    const t0 = performance.now();
    const res = await this.request("tools/call", { name, arguments: args });
    const ms = performance.now() - t0;
    const text = res?.result?.content?.[0]?.text ?? "";
    return { text, ms };
  }
  close() {
    try {
      this.proc.stdin.end();
      this.proc.kill();
    } catch {}
  }
}

const strip = (t) =>
  t.replace(/^<travsr-data>\n?/, "").replace(/\n?<\/travsr-data>$/, "");
const median = (a) => {
  const s = [...a].sort((x, y) => x - y);
  return s[Math.floor(s.length / 2)];
};

// Parses "## exact/semantic/docs/relevant" sections, tolerating the [via:]
// badge hoist (RFC-022 §14). Code-node lines look like
//   "sig (kind) — path:line [via: role]"
// Doc-section lines (build_docs_section, tools.rs) look like
//   "path § Heading > Trail:line-line"  (no "(kind)", no em-dash)
// One doc-entry parser for both surfaces. get_context prints these lines inside
// its "## docs" section and `ask` returns the same strings in AskPayload.docs
// (both come from build_docs_section / docs_section, same renderer). Gate 5
// compares the two surfaces entry by entry, so they must be parsed by identical
// code — a parity gate whose sides use different parsers manufactures its own
// disagreements.
function parseDocLine(line) {
  const m = /^(.+?)\s+§\s+(.+)$/.exec(line);
  return m ? { path: m[1].trim(), heading: m[2].trim() } : null;
}

function parseSections(text) {
  const sections = { exact: [], semantic: [], docs: [], relevant: [] };
  let cur = null;
  for (const line of strip(text).split("\n")) {
    const sec = /^##\s+(exact|semantic|docs|relevant)\b/.exec(line);
    if (sec) {
      cur = sec[1];
      continue;
    }
    if (!cur || !line.trim()) continue;
    if (cur === "docs") {
      const entry = parseDocLine(line);
      if (entry) sections.docs.push(entry);
      continue;
    }
    if (!line.includes(" — ")) continue;
    const m = /^(\S+)\s+\(([^)]+)\)\s+—\s+(.+?)(?:\s+\[|$)/.exec(line);
    if (!m) continue;
    sections[cur].push({ sig: m[1], kind: m[2], path: m[3].replace(/:\d+.*$/, "") });
  }
  return sections;
}
function codeNodes(sections) {
  return [...sections.exact, ...sections.semantic, ...sections.relevant];
}
function confidence(text) {
  const m = /confidence:\s*(\w+)/i.exec(strip(text));
  return m ? m[1].toLowerCase() : "unknown";
}
function isAbstain(text, nodes) {
  const s = strip(text).toLowerCase();
  return (
    nodes.length === 0 ||
    s.includes("no grounded match") ||
    s.includes("no confident match") ||
    confidence(text) === "none"
  );
}
function hitRank(nodes, expect) {
  for (let i = 0; i < nodes.length; i++) {
    const hay = (nodes[i].sig + " " + nodes[i].path).toLowerCase();
    if (expect.some((e) => hay.includes(e.toLowerCase()))) return i + 1;
  }
  return 0;
}
// Headings are rendered by humanize_doc_anchor (tools.rs) as space-separated
// Title Case words — a query file's expectHeading may spell a multi-word
// anchor segment with a hyphen (e.g. "server-side apply", from the slug
// "server-side-apply"). Normalize both sides to spaces so hyphen/underscore
// vs space is never a false miss.
const normHeading = (s) => s.toLowerCase().replace(/[-_]/g, " ");
function docHitRank(docEntries, q) {
  for (let i = 0; i < docEntries.length; i++) {
    const e = docEntries[i];
    const pathHit = e.path.toLowerCase().includes(q.expectPath.toLowerCase());
    const headingHit =
      !q.expectHeading || normHeading(e.heading).includes(normHeading(q.expectHeading));
    if (pathHit && headingHit) return i + 1;
  }
  return 0;
}

// ── run one arm (docs on/off) over the seeded query set ─────────────────────
async function runSeeded(docsEnabled) {
  const { queries } = JSON.parse(readFileSync(seededPath, "utf8"));
  const mcp = new Mcp(
    docsEnabled ? { TRAVSR_DOCS_ENABLED: "1" } : { TRAVSR_DOCS_ENABLED: "" }
  );
  await mcp.init();
  await mcp.call("get_context", { query: "warmup", token_budget: BUDGET });

  const rows = [];
  for (const q of queries) {
    const { text, ms } = await mcp.call("get_context", { query: q.query, token_budget: BUDGET });
    const sections = parseSections(text);
    const nodes = codeNodes(sections);
    const abstain = isAbstain(text, nodes);
    const rank = q.expect.length ? hitRank(nodes, q.expect) : 0;
    rows.push({ id: q.id, category: q.category, expect: q.expect, rank, abstain, ms: Math.round(ms), docsInBody: sections.docs.length });
  }
  mcp.close();
  return rows;
}

function summarizeCode(rows) {
  const answerable = rows.filter((r) => r.expect.length > 0);
  const abstainSet = rows.filter((r) => r.expect.length === 0);
  const hit = (n) => answerable.filter((r) => r.rank >= 1 && r.rank <= n).length;
  const n = Math.max(1, answerable.length);
  return {
    n: answerable.length,
    "hit@1": +(hit(1) / n).toFixed(3),
    "hit@3": +(hit(3) / n).toFixed(3),
    "hit@10": +(hit(10) / n).toFixed(3),
    abstainN: abstainSet.length,
    abstainCorrect: abstainSet.filter((r) => r.abstain).length,
    abstainRate: +((abstainSet.filter((r) => r.abstain).length) / Math.max(1, abstainSet.length)).toFixed(3),
    medianMs: Math.round(median(rows.map((r) => r.ms))),
    docsLeaked: abstainSet.filter((r) => r.docsInBody > 0 && !docsEnabledGlobal).length,
  };
}
let docsEnabledGlobal = false; // set per-call context below (used only in summarizeCode's leak check meaning)

// ── run the docs query set (docs lane on) ───────────────────────────────────
// Also collects the per-query query-embedding trace that Gate 4 asserts on.
// Attribution is positional (snapshot the trace length either side of each
// call), never by matching query text: the sidecar logs the *normalized* query
// while the query file holds the raw one, and re-implementing the normalizer
// here would be exactly the kind of drift this gate exists to catch.
const settle = (ms) => new Promise((r) => setTimeout(r, ms));

async function runDocs() {
  const { queries } = JSON.parse(readFileSync(docsPath, "utf8"));
  // Truncate before the run so a previous run's lines cannot be counted.
  writeFileSync(EMBED_TRACE_PATH, "");
  const mcp = new Mcp({
    TRAVSR_DOCS_ENABLED: "1",
    TRAVSR_EMBED_QUERY_CACHE_DEBUG: EMBED_TRACE_PATH,
  });
  await mcp.init();
  await mcp.call("get_context", { query: "warmup", token_budget: BUDGET });
  await settle(100);

  const rows = [];
  for (const q of queries) {
    const traceStart = readEmbedTrace().length;
    const { text, ms } = await mcp.call("get_context", { query: q.query, token_budget: BUDGET });
    // The sidecar writes the trace from its own process; give it a moment to
    // flush before slicing off this query's lines.
    await settle(50);
    const trace = readEmbedTrace().slice(traceStart);
    const sections = parseSections(text);
    const rank = docHitRank(sections.docs, q);
    rows.push({
      id: q.id,
      query: q.query,
      rank,
      ms: Math.round(ms),
      nDocs: sections.docs.length,
      // Gate 5 compares this against the `ask` surface's leading entry.
      topEntry: sections.docs[0] ?? null,
      embedInferences: trace.filter((t) => t.outcome === "miss").length,
      embedMemoHits: trace.filter((t) => t.outcome === "hit").length,
      spacesSearched: [...new Set(trace.map((t) => t.space))].sort(),
    });
  }

  // Punctuated probe — the case the scored query sets do not cover.
  //
  // Neither queries-docs-{travsr,k8s}.json contains a single query with
  // sentence punctuation, so `normalize_nl_query` is a no-op across the whole
  // scored set and the memo would hit even if the two lanes disagreed about
  // normalization. That is exactly the defect Gate 4 exists to catch, so
  // without this probe the gate is green on both the fixed and the broken
  // build. These queries are traced only — never scored for hit@k, so Gate 1
  // stays comparable with the numbers recorded in §14.3.
  const probeRows = [];
  for (const q of queries.slice(0, 3)) {
    const raw = `${q.query}?`;
    const traceStart = readEmbedTrace().length;
    await mcp.call("get_context", { query: raw, token_budget: BUDGET });
    await settle(50);
    const trace = readEmbedTrace().slice(traceStart);
    probeRows.push({
      id: `${q.id}-punct`,
      query: raw,
      embedInferences: trace.filter((t) => t.outcome === "miss").length,
      embedMemoHits: trace.filter((t) => t.outcome === "hit").length,
      spacesSearched: [...new Set(trace.map((t) => t.space))].sort(),
      // The text the sidecar actually received, for eyeballing that
      // normalization ran at all.
      normalizedSeen: [...new Set(trace.map((t) => t.query))],
    });
  }

  mcp.close();
  return { rows, probeRows };
}

// Gate 4 (plan §4.4): enabling the docs lane must not add a second
// query-embedding inference. The host issues one KNN round trip per space, so
// the contract is upheld by the sidecar's single-slot exact-text memo — which
// only hits when both lanes present byte-identical query text.
//
// `vacuous` guards the gate against passing for the wrong reason: if no query
// ever searched both spaces (docs lane off, no doc index, sidecar too old, or
// the debug env not reaching the sidecar) there is nothing to prove and the
// gate must not report green.
function summarizeInference(rows, probeRows) {
  const all = [...rows, ...probeRows];
  const observed = all.filter((r) => r.embedInferences + r.embedMemoHits > 0);
  const bothSpaces = all.filter((r) => r.spacesSearched.length === 2);
  const violations = observed
    .filter((r) => r.embedInferences > 1)
    .map((r) => ({ id: r.id, inferences: r.embedInferences, spaces: r.spacesSearched }));
  const vacuous = bothSpaces.length === 0;
  // The probe must have actually run and searched both spaces, or the
  // normalization-divergence case went unexercised and the gate is only
  // proving the easy path.
  const probeCovered = probeRows.some((r) => r.spacesSearched.length === 2);
  return {
    pass: !vacuous && probeCovered && violations.length === 0,
    vacuous,
    probeCovered,
    queriesTraced: observed.length,
    queriesSearchingBothSpaces: bothSpaces.length,
    totalInferences: all.reduce((a, r) => a + r.embedInferences, 0),
    totalMemoHits: all.reduce((a, r) => a + r.embedMemoHits, 0),
    violations,
    punctuatedProbe: probeRows,
    threshold:
      "<=1 query-embedding inference per query; >=1 query searching both spaces; " +
      "punctuated probe exercised (scored sets contain no sentence punctuation)",
  };
}
// ── Gate 5: the `ask` surface ───────────────────────────────────────────────
//
// `ask` is answered by the daemon whenever one is running, and by a read-only
// cold path otherwise. Only the daemon path can render docs
// (`try_inject_embed_hook_readonly` is an intentional no-op, plan §17.6), and
// the daemon reads TRAVSR_DOCS_ENABLED from *its own* environment. So the arm
// has to own the daemon: stop whatever is running, start one with the env this
// arm needs, and put a plain one back afterwards.

function run(cmd, args, opts = {}) {
  return new Promise((resolve) => {
    execFile(
      cmd,
      args,
      { cwd: REPO, timeout: opts.timeoutMs ?? 120000, maxBuffer: 32 * 1024 * 1024, env: { ...process.env, ...(opts.env ?? {}) } },
      (err, stdout, stderr) => resolve({ code: err?.code ?? 0, stdout: stdout ?? "", stderr: stderr ?? "", err })
    );
  });
}

async function daemonRunning() {
  const { code, stdout } = await run(BIN, ["daemon", "status"], { timeoutMs: 20000 });
  return code === 0 && /daemon:\s*running/i.test(stdout);
}

async function daemonStop() {
  await run(BIN, ["daemon", "stop"], { timeoutMs: 30000 });
  // `stop` returns as soon as it has signalled; wait for the socket to actually
  // go away, or the next `start` races a dying process for it.
  for (let i = 0; i < 40; i++) {
    if (!(await daemonRunning())) return true;
    await settle(250);
  }
  return false;
}

// Start a daemon with `env` and wait until it actually answers `ask`. Daemon
// readiness is not socket-up: the control socket stays unbound during the
// initial watcher scan (10-30 s on a large repo), and an `ask` issued in that
// window falls back to the cold path, which renders no docs — indistinguishable
// from a broken lane. So readiness is proven by a real `ask` round trip.
// Readiness budget. Generous by default because the bound is set by the repo's
// initial watcher scan, not by the daemon: on kubernetes (264k nodes) that scan
// is minutes, and a timeout here would report Gate 5 FAIL for a harness reason
// rather than a product one — the worst kind of gate failure, because it trains
// the reader to discount the gate.
const DAEMON_READY_MS = Number(process.env.BENCH_DAEMON_READY_MS || 600000);

async function daemonStart(env, { readyTimeoutMs = DAEMON_READY_MS } = {}) {
  const child = spawn(BIN, ["daemon", "start", "--foreground"], {
    cwd: REPO,
    stdio: ["ignore", "ignore", "ignore"],
    env: { ...process.env, ...env },
    detached: false,
  });
  const t0 = performance.now();
  let lastErr = "";
  while (performance.now() - t0 < readyTimeoutMs) {
    if (child.exitCode !== null) {
      throw new Error(`daemon exited during startup (code ${child.exitCode})`);
    }
    if (await daemonRunning()) {
      const probe = await askOnce("warmup");
      if (probe.ok) return { child, readyMs: Math.round(performance.now() - t0) };
      lastErr = probe.error;
    }
    await settle(500);
  }
  throw new Error(`daemon did not become ready within ${readyTimeoutMs}ms: ${lastErr}`);
}

async function askOnce(query) {
  const t0 = performance.now();
  const { code, stdout, stderr } = await run(BIN, ["ask", query, "--format", "json"], { timeoutMs: 120000 });
  const ms = performance.now() - t0;
  if (code !== 0) return { ok: false, ms, error: (stderr || stdout).trim().slice(0, 300) };
  try {
    return { ok: true, ms, payload: JSON.parse(stdout) };
  } catch (e) {
    return { ok: false, ms, error: `unparseable --format json output: ${stdout.slice(0, 200)}` };
  }
}

// AskPayload.rows -> the shape hitRank() already understands, so the code-side
// scoring is literally the same function on both surfaces.
function askCodeNodes(payload) {
  return (payload.rows ?? []).map((r) => ({
    sig: r.signature,
    kind: r.kind,
    path: (r.path ?? "").replace(/:\d+.*$/, ""),
  }));
}

// Mirrors isAbstain()'s meaning on the structured payload: `matched:false` is
// the abstain return, and confidence "none" is the same judgement by another
// name. An empty row set counts too, matching the MCP side.
function askIsAbstain(payload) {
  return (
    payload.matched === false ||
    (payload.rows ?? []).length === 0 ||
    (payload.confidence ?? "").toLowerCase() === "none"
  );
}

async function runAskSeeded() {
  const { queries } = JSON.parse(readFileSync(seededPath, "utf8"));
  const rows = [];
  for (const q of queries) {
    const r = await askOnce(q.query);
    if (!r.ok) {
      rows.push({ id: q.id, category: q.category, expect: q.expect, rank: 0, abstain: true, ms: Math.round(r.ms), error: r.error, docsInBody: 0 });
      continue;
    }
    const nodes = askCodeNodes(r.payload);
    rows.push({
      id: q.id,
      category: q.category,
      expect: q.expect,
      rank: q.expect.length ? hitRank(nodes, q.expect) : 0,
      abstain: askIsAbstain(r.payload),
      ms: Math.round(r.ms),
      docsInBody: (r.payload.docs ?? []).length,
    });
  }
  return rows;
}

async function runAskDocs() {
  const { queries } = JSON.parse(readFileSync(docsPath, "utf8"));
  const rows = [];
  for (const q of queries) {
    const r = await askOnce(q.query);
    if (!r.ok) {
      rows.push({ id: q.id, query: q.query, rank: 0, nDocs: 0, ms: Math.round(r.ms), error: r.error, entries: [] });
      continue;
    }
    const entries = (r.payload.docs ?? []).map(parseDocLine).filter(Boolean);
    rows.push({
      id: q.id,
      query: q.query,
      rank: docHitRank(entries, q),
      nDocs: entries.length,
      ms: Math.round(r.ms),
      matched: r.payload.matched === true,
      entries,
    });
  }
  return rows;
}

// Per-query comparison against the get_context arm. Two failure shapes, both
// real regressions and both invisible to gates 1-4:
//   presence — one surface rendered a docs section and the other did not (the
//              #516 shape: a surface whose hook was never armed just goes quiet)
//   top1     — both rendered, but disagree on the leading entry (a renderer,
//              floor or budget divergence between the surfaces)
// Ranks below 1 are deliberately not compared: `ask` is fixed at
// DEFAULT_TOKEN_BUDGET (4096) while the MCP arm runs at BENCH_BUDGET, so the
// tail of the list is legitimately budget-sensitive. Top-1 is not.
function compareSurfaces(mcpRows, askRows) {
  const byId = new Map(mcpRows.map((r) => [r.id, r]));
  const disagreements = [];
  let comparable = 0;
  for (const a of askRows) {
    const m = byId.get(a.id);
    if (!m) continue;
    comparable++;
    const mTop = m.topEntry ?? null;
    const aTop = a.entries[0] ?? null;
    if (!!mTop !== !!aTop) {
      disagreements.push({ id: a.id, kind: "presence", getContext: mTop, ask: aTop });
      continue;
    }
    if (!mTop) continue;
    const same =
      mTop.path === aTop.path && normHeading(mTop.heading) === normHeading(aTop.heading);
    if (!same) disagreements.push({ id: a.id, kind: "top1", getContext: mTop, ask: aTop });
  }
  return { comparable, disagreements };
}

function summarizeAskDocs(rows) {
  const n = rows.length;
  const hit = (k) => rows.filter((r) => r.rank >= 1 && r.rank <= k).length;
  return {
    n,
    "hit@1": +(hit(1) / Math.max(1, n)).toFixed(3),
    "hit@3": +(hit(3) / Math.max(1, n)).toFixed(3),
    medianMs: Math.round(median(rows.map((r) => r.ms))),
    queriesWithDocs: rows.filter((r) => r.nDocs > 0).length,
    errors: rows.filter((r) => r.error).length,
  };
}

function summarizeDocs(rows) {
  const n = rows.length;
  const hit = (k) => rows.filter((r) => r.rank >= 1 && r.rank <= k).length;
  return {
    n,
    "hit@1": +(hit(1) / n).toFixed(3),
    "hit@3": +(hit(3) / n).toFixed(3),
    medianMs: Math.round(median(rows.map((r) => r.ms))),
  };
}

// ── main ──────────────────────────────────────────────────────────────────
console.error(`=== #376 Phase 2 gate — ${LABEL} (repo: ${REPO}) ===`);

console.error("\n[off] running queries-seeded with TRAVSR_DOCS_ENABLED unset...");
const offRows = await runSeeded(false);
docsEnabledGlobal = false;
const offSummary = summarizeCode(offRows);
console.error(JSON.stringify(offSummary, null, 2));

console.error("\n[on] running queries-seeded with TRAVSR_DOCS_ENABLED=1...");
const onRows = await runSeeded(true);
docsEnabledGlobal = true;
const onSummary = summarizeCode(onRows);
console.error(JSON.stringify(onSummary, null, 2));

console.error("\n[docs] running queries-docs with TRAVSR_DOCS_ENABLED=1...");
const { rows: docsRows, probeRows } = await runDocs();
const docsSummary = summarizeDocs(docsRows);
console.error(JSON.stringify(docsSummary, null, 2));

// ── Gate 5: the `ask` surface ───────────────────────────────────────────────
// Owns the daemon for the duration. `daemonWasRunning` decides what to restore.
const daemonWasRunning = await daemonRunning();
let harnessDaemon = null;
let restored = false;

async function restoreDaemon() {
  if (restored) return;
  restored = true;
  try {
    if (harnessDaemon) harnessDaemon.kill();
  } catch {}
  await daemonStop();
  if (daemonWasRunning) {
    await run(BIN, ["daemon", "start"], { timeoutMs: 60000 });
    console.error(
      "[ask] restarted the daemon that was running before this run. Its original " +
        "environment could not be recovered, so any env it was started with is gone."
    );
  }
}

// A killed harness must not leave a docs-enabled daemon behind: it would change
// what every later `travsr ask` on this machine returns, with nothing on screen
// to say why.
for (const sig of ["SIGINT", "SIGTERM"]) {
  process.on(sig, () => {
    void restoreDaemon().finally(() => process.exit(130));
  });
}

let askSummary = null;
let askOffSummary = null;
let askOnSummary = null;
let askDocsRowsOut = [];
let askOffLeak = 0;
let surfaceCmp = { comparable: 0, disagreements: [] };
let askError = null;
let askDaemonReadyMs = null;

try {
  console.error("\n[ask/off] restarting daemon with TRAVSR_DOCS_ENABLED unset...");
  await daemonStop();
  harnessDaemon = (await daemonStart({ TRAVSR_DOCS_ENABLED: "" })).child;
  const askOffRows = await runAskSeeded();
  docsEnabledGlobal = false;
  askOffSummary = summarizeCode(askOffRows);
  // With the lane off, no query on any surface may render a docs section —
  // the feature ships default-OFF, so a single leaked line is a shipped-state
  // bug, not a ranking nit.
  askOffLeak = askOffRows.filter((r) => r.docsInBody > 0).length;
  console.error(JSON.stringify(askOffSummary, null, 2));

  console.error("\n[ask/on] restarting daemon with TRAVSR_DOCS_ENABLED=1...");
  harnessDaemon.kill();
  await daemonStop();
  const started = await daemonStart({ TRAVSR_DOCS_ENABLED: "1" });
  harnessDaemon = started.child;
  askDaemonReadyMs = started.readyMs;
  const askOnRows = await runAskSeeded();
  docsEnabledGlobal = true;
  askOnSummary = summarizeCode(askOnRows);
  console.error(JSON.stringify(askOnSummary, null, 2));

  console.error("\n[ask/docs] running queries-docs through `travsr ask --format json`...");
  askDocsRowsOut = await runAskDocs();
  askSummary = summarizeAskDocs(askDocsRowsOut);
  console.error(JSON.stringify(askSummary, null, 2));
  surfaceCmp = compareSurfaces(docsRows, askDocsRowsOut);
} catch (e) {
  askError = String(e?.message ?? e);
  console.error(`[ask] arm failed: ${askError}`);
} finally {
  await restoreDaemon();
}

// ── gate verdicts ───────────────────────────────────────────────────────────
const gate1 = docsSummary["hit@1"] >= 0.6 && docsSummary["hit@3"] >= 0.9;
const gate2 = onSummary["hit@1"] === offSummary["hit@1"] && onSummary["hit@10"] === offSummary["hit@10"];
const gate3 = onSummary.abstainRate === offSummary.abstainRate;
const inference = summarizeInference(docsRows, probeRows);
const gate4 = inference.pass;

// Gate 5 has four conditions, and every one of them is a bug this project has
// actually shipped at least once:
//   5a  doc hit@1/hit@3 on `ask`, same thresholds as Gate 1 — the surface has
//       to be useful, not merely non-empty.
//   5b  vacuity — if no query produced a docs section on `ask`, the arm proves
//       nothing and must not report green. This is #516's exact shape: silent
//       None, no error, no section.
//   5c  cross-surface parity — presence and top-1 agreement with get_context.
//   5d  no code regression on `ask` between docs off and on, mirroring gates
//       2 and 3 on the second surface.
const askVacuous = !askSummary || askSummary.queriesWithDocs === 0;
const askCodeStable =
  !!askOffSummary &&
  !!askOnSummary &&
  askOnSummary["hit@1"] === askOffSummary["hit@1"] &&
  askOnSummary["hit@10"] === askOffSummary["hit@10"] &&
  askOnSummary.abstainRate === askOffSummary.abstainRate;
const askHit = !!askSummary && askSummary["hit@1"] >= 0.6 && askSummary["hit@3"] >= 0.9;
const gate5 =
  !askError &&
  !askVacuous &&
  askHit &&
  askCodeStable &&
  askOffLeak === 0 &&
  surfaceCmp.disagreements.length === 0;

// Not a gate. The docs lane runs a second cross-encoder pass per query (§12),
// which dominates the wall-clock delta and is an accepted cost pending the
// selective/hybrid reranking follow-up (§12's untried lever). Reported so the
// number stays visible and any *change* in it is noticed.
const latencyRatio = onSummary.medianMs / Math.max(1, offSummary.medianMs);

const perQueryRegressions = offRows
  .map((o, i) => ({ o, n: onRows[i] }))
  .filter(({ o, n }) => o.rank !== n.rank || o.abstain !== n.abstain)
  .map(({ o, n }) => ({ id: o.id, offRank: o.rank, onRank: n.rank, offAbstain: o.abstain, onAbstain: n.abstain }));

const report = {
  generatedAt: new Date().toISOString(),
  repo: LABEL,
  gate1_docHit: { pass: gate1, ...docsSummary, threshold: "hit@1>=0.60, hit@3>=0.90" },
  gate2_codeRegression: { pass: gate2, off: offSummary, on: onSummary, perQueryRegressions },
  gate3_abstainRegression: { pass: gate3, offAbstainRate: offSummary.abstainRate, onAbstainRate: onSummary.abstainRate },
  gate4_singleInference: inference,
  gate5_askSurface: {
    pass: gate5,
    error: askError,
    vacuous: askVacuous,
    docs: askSummary,
    codeOff: askOffSummary,
    codeOn: askOnSummary,
    codeStable: askCodeStable,
    docsLeakedWhileOff: askOffLeak,
    surfaceParity: surfaceCmp,
    daemonReadyMs: askDaemonReadyMs,
    daemonWasRunningBefore: daemonWasRunning,
    threshold:
      "ask hit@1>=0.60, hit@3>=0.90; >=1 query rendering docs; zero presence/top-1 " +
      "disagreement with get_context; zero code hit/abstain movement docs off vs on",
    rows: askDocsRowsOut,
  },
  crossEncoderCost: {
    gating: false,
    accepted: true,
    offMedianMs: offSummary.medianMs,
    onMedianMs: onSummary.medianMs,
    ratio: +latencyRatio.toFixed(3),
    cause: "second full cross-encoder pass per query (docs lane), plan §12",
    followUp: "selective/hybrid reranking — only invoke the cross-encoder when raw cosine is ambiguous (§12)",
  },
  docsRows,
};

writeFileSync(join(HERE, `report-phase2-gate-${LABEL}.json`), JSON.stringify(report, null, 2));
console.error(`\n=== VERDICT (${LABEL}) ===`);
console.error(`Gate 1 (doc hit@1/hit@3):   ${gate1 ? "PASS" : "FAIL"}  (hit@1=${docsSummary["hit@1"]}, hit@3=${docsSummary["hit@3"]})`);
console.error(`Gate 2 (code regression):   ${gate2 ? "PASS" : "FAIL"}  (off hit@1=${offSummary["hit@1"]}/hit@10=${offSummary["hit@10"]}, on hit@1=${onSummary["hit@1"]}/hit@10=${onSummary["hit@10"]})`);
console.error(`Gate 3 (abstain regression):${gate3 ? "PASS" : "FAIL"}  (off=${offSummary.abstainRate}, on=${onSummary.abstainRate})`);
console.error(
  `Gate 4 (single inference):  ${gate4 ? "PASS" : "FAIL"}  ` +
    `(${inference.totalInferences} inferences / ${inference.totalMemoHits} memo hits over ` +
    `${inference.queriesTraced} traced queries, ${inference.queriesSearchingBothSpaces} searching both spaces)`
);
if (inference.vacuous) {
  console.error(
    `  !! VACUOUS: no query searched both spaces — the docs lane never ran, or the sidecar ` +
      `predates TRAVSR_EMBED_QUERY_CACHE_DEBUG. Gate 4 proves nothing in this state.`
  );
}
if (!inference.vacuous && !inference.probeCovered) {
  console.error(
    `  !! punctuated probe never searched both spaces — the normalization-divergence case ` +
      `went unexercised, so Gate 4 only proves the no-punctuation path.`
  );
}
for (const v of inference.violations) {
  console.error(`  !! ${v.id}: ${v.inferences} query-embedding inferences (spaces: ${v.spaces.join(",")})`);
}
console.error(
  `Gate 5 (ask surface):       ${gate5 ? "PASS" : "FAIL"}  ` +
    (askSummary
      ? `(hit@1=${askSummary["hit@1"]}, hit@3=${askSummary["hit@3"]}, ` +
        `${askSummary.queriesWithDocs}/${askSummary.n} queries rendered docs, ` +
        `${surfaceCmp.disagreements.length} surface disagreement(s))`
      : `(arm did not complete)`)
);
if (askError) {
  console.error(`  !! ask arm failed: ${askError}`);
}
if (!askError && askVacuous) {
  console.error(
    `  !! VACUOUS: no query rendered a docs section on the ask surface. Either the ` +
      `daemon-side hook is not armed (#516's shape), the sidecar predates the doc space, ` +
      `or the repo has no doc-chunk nodes. Gate 5 proves nothing in this state.`
  );
}
if (askOffLeak > 0) {
  console.error(
    `  !! ${askOffLeak} quer(ies) rendered a docs section on \`ask\` with the lane OFF — ` +
      `the feature ships default-off, so this is a shipped-state leak.`
  );
}
if (!askCodeStable && askOffSummary && askOnSummary) {
  console.error(
    `  !! ask code results moved when the docs lane was enabled: ` +
      `hit@1 ${askOffSummary["hit@1"]}->${askOnSummary["hit@1"]}, ` +
      `hit@10 ${askOffSummary["hit@10"]}->${askOnSummary["hit@10"]}, ` +
      `abstain ${askOffSummary.abstainRate}->${askOnSummary.abstainRate}`
  );
}
for (const d of surfaceCmp.disagreements) {
  const fmt = (e) => (e ? `${e.path} § ${e.heading}` : "(no docs section)");
  console.error(`  !! ${d.id} [${d.kind}]: get_context=${fmt(d.getContext)} | ask=${fmt(d.ask)}`);
}
console.error(
  `\n[not a gate] cross-encoder cost: ratio=${latencyRatio.toFixed(3)} ` +
    `(off=${offSummary.medianMs}ms on=${onSummary.medianMs}ms) — accepted, §12 selective reranking is the follow-up`
);
if (perQueryRegressions.length) {
  console.error(`\nPer-query rank/abstain diffs (off vs on):`);
  for (const r of perQueryRegressions) console.error(`  ${r.id}: rank ${r.offRank}->${r.onRank}, abstain ${r.offAbstain}->${r.onAbstain}`);
}
console.error(`\nwrote bench/report-phase2-gate-${LABEL}.json`);

// This is a gate: a non-zero exit is the whole point of running it in CI or
// from a wrapper. Reporting FAIL on stderr and exiting 0 is how a gate rots.
const allPass = gate1 && gate2 && gate3 && gate4 && gate5;
console.error(`\nOVERALL: ${allPass ? "PASS" : "FAIL"}`);
process.exitCode = allPass ? 0 : 1;
