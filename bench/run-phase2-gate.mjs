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

import { spawn } from "node:child_process";
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
      const m = /^(.+?)\s+§\s+(.+)$/.exec(line);
      if (m) sections.docs.push({ path: m[1].trim(), heading: m[2].trim() });
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

// ── gate verdicts ───────────────────────────────────────────────────────────
const gate1 = docsSummary["hit@1"] >= 0.6 && docsSummary["hit@3"] >= 0.9;
const gate2 = onSummary["hit@1"] === offSummary["hit@1"] && onSummary["hit@10"] === offSummary["hit@10"];
const gate3 = onSummary.abstainRate === offSummary.abstainRate;
const inference = summarizeInference(docsRows, probeRows);
const gate4 = inference.pass;

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
  `\n[not a gate] cross-encoder cost: ratio=${latencyRatio.toFixed(3)} ` +
    `(off=${offSummary.medianMs}ms on=${onSummary.medianMs}ms) — accepted, §12 selective reranking is the follow-up`
);
if (perQueryRegressions.length) {
  console.error(`\nPer-query rank/abstain diffs (off vs on):`);
  for (const r of perQueryRegressions) console.error(`  ${r.id}: rank ${r.offRank}->${r.onRank}, abstain ${r.offAbstain}->${r.onAbstain}`);
}
console.error(`\nwrote bench/report-phase2-gate-${LABEL}.json`);
