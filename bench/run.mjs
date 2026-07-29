#!/usr/bin/env node
// Travsr tool benchmark harness — perf + correctness + usefulness (LLM-judge packets).
//
// Drives the local MCP server over stdio (single persistent process) and measures
// get_context and get_snippets (auto|full|skeleton). Model-agnostic: re-run under
// a different embedding backend with
//   ./target/release/travsr embed switch <model> && ./target/release/travsr embed reindex
// then re-run this script; results include the active model tag.
//
// Outputs (in bench/):
//   results.json      — raw perf + correctness numbers
//   report.md         — human-readable tables
//   judge-packets.json — {query, contextText} packets for an LLM-judge usefulness pass
//
// Usage: node bench/run.mjs   (from repo root; release binary must be built)

import { spawn, execSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, "..");
const BIN = join(ROOT, "target/release/travsr");

// Parametrization (defaults = travsr self-benchmark; override for other repos):
//   BENCH_REPO      absolute path to the indexed repo the MCP server should serve (cwd)
//   BENCH_QUERIES   path to a labeled queries.json
//   BENCH_TARGETS   JSON array of get_snippets targets [{label,sig},…]
//   BENCH_LABEL     suffix for output files + report repo name (e.g. "k8s")
const REPO = process.env.BENCH_REPO || ROOT;
const QUERIES_PATH = process.env.BENCH_QUERIES || join(HERE, "queries.json");
const LABEL = process.env.BENCH_LABEL || "travsr";
const SNIPPET_TARGETS = process.env.BENCH_TARGETS
  ? JSON.parse(readFileSync(process.env.BENCH_TARGETS, "utf8"))
  : [
      { label: "small_fn", sig: "fn:kind_boost" },
      { label: "large_fn", sig: "fn:get_context_body" },
      { label: "class", sig: "class:ContextExplorerPanel" },
    ];
const suffix = LABEL === "travsr" ? "" : `-${LABEL}`;

// ── minimal persistent MCP stdio client ─────────────────────────────────────
class Mcp {
  constructor() {
    this.proc = spawn(BIN, ["mcp"], { cwd: REPO, stdio: ["pipe", "pipe", "ignore"] });
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
      try { msg = JSON.parse(line); } catch { continue; }
      if (msg.id != null && this.pending.has(msg.id)) {
        const { resolve } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        resolve(msg);
      }
    }
  }
  _send(obj) { this.proc.stdin.write(JSON.stringify(obj) + "\n"); }
  request(method, params) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this._send({ jsonrpc: "2.0", id, method, params });
      setTimeout(() => {
        if (this.pending.has(id)) { this.pending.delete(id); reject(new Error("timeout " + method)); }
      }, 120000);
    });
  }
  async init() {
    await this.request("initialize", {
      protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "bench", version: "1" },
    });
    this._send({ jsonrpc: "2.0", method: "notifications/initialized" });
  }
  // Returns { text, ms }
  async call(name, args) {
    const t0 = performance.now();
    const res = await this.request("tools/call", { name, arguments: args });
    const ms = performance.now() - t0;
    const text = res?.result?.content?.[0]?.text ?? "";
    return { text, ms };
  }
  close() { try { this.proc.stdin.end(); this.proc.kill(); } catch {} }
}

// ── helpers ──────────────────────────────────────────────────────────────────
const strip = (t) => t.replace(/^<travsr-data>\n?/, "").replace(/\n?<\/travsr-data>$/, "");
const estTokens = (t) => Math.round(strip(t).length / 4);
const median = (a) => { const s = [...a].sort((x, y) => x - y); return s[Math.floor(s.length / 2)]; };
const p95 = (a) => { const s = [...a].sort((x, y) => x - y); return s[Math.min(s.length - 1, Math.floor(s.length * 0.95))]; };

// Parse get_context node lines: "sig (kind) — path:line [via: role] [score: N]".
// RFC-022 §14 (default-on): tolerate match-source grouping — skip "## exact/
// semantic/relevant" section headers, record the section as matchSource, and
// accept rows with no `[via:]` badge (hoisted onto Exact/Semantic headers) by
// terminating the path at the first bracketed tag OR end of line.
function parseNodes(text) {
  const out = [];
  let section = null;
  for (const line of strip(text).split("\n")) {
    const sec = /^##\s+(exact|semantic|relevant)\b/.exec(line);
    if (sec) { section = sec[1]; continue; }
    if (!line.includes(" — ")) continue;
    const m = /^(\S+)\s+\(([^)]+)\)\s+—\s+(.+?)(?:\s+\[|$)/.exec(line);
    if (!m) continue;
    out.push({ sig: m[1], kind: m[2], path: m[3].replace(/:\d+$/, ""), matchSource: section });
  }
  return out;
}
function confidence(text) {
  const m = /confidence:\s*(\w+)/i.exec(strip(text));
  return m ? m[1].toLowerCase() : "unknown";
}
function isAbstain(text, nodes) {
  const s = strip(text).toLowerCase();
  return nodes.length === 0 || s.includes("no grounded match") || s.includes("no confident match") || confidence(text) === "none";
}
// hit rank (1-based) of an expected substring among node sig|path; 0 = miss
function hitRank(nodes, expect) {
  for (let i = 0; i < nodes.length; i++) {
    const hay = (nodes[i].sig + " " + nodes[i].path).toLowerCase();
    if (expect.some((e) => hay.includes(e.toLowerCase()))) return i + 1;
  }
  return 0;
}

// ── main ──────────────────────────────────────────────────────────────────────
const { queries } = JSON.parse(readFileSync(QUERIES_PATH, "utf8"));
const model = (() => {
  try {
    const s = execSync(`${BIN} embed status`, { cwd: REPO }).toString();
    const m = /Repo model\s*:\s*(\S+)/.exec(s) || /Backend\s*:\s*(\S+)/.exec(s);
    return m ? m[1] : "unknown";
  } catch { return "unknown"; }
})();

const mcp = new Mcp();
await mcp.init();

console.error("model:", model, "— warming up…");
const cold = await mcp.call("get_context", { query: "get_context", token_budget: "2000" });
console.error(`cold first get_context: ${cold.ms.toFixed(0)} ms`);

const REPEATS = 3;
const results = { model, generatedAt: new Date().toISOString(), coldMs: Math.round(cold.ms), perf: {}, correctness: [], perfSweep: {}, snippetModes: {} };
const judgePackets = [];

// ── correctness + per-query warm latency ──
let hitAt = { 1: 0, 3: 0, 5: 0, 10: 0 }, mrrSum = 0, answerable = 0, abstainCorrect = 0, nonsense = 0;
for (const q of queries) {
  const times = [];
  let last = null;
  for (let r = 0; r < REPEATS; r++) { last = await mcp.call("get_context", { query: q.query, token_budget: "2000" }); times.push(last.ms); }
  const nodes = parseNodes(last.text);
  const abstain = isAbstain(last.text, nodes);
  const rank = q.expect.length ? hitRank(nodes, q.expect) : 0;

  if (q.category === "nonsense") {
    nonsense++;
    if (abstain) abstainCorrect++;
  } else {
    answerable++;
    if (rank >= 1 && rank <= 1) hitAt[1]++;
    if (rank >= 1 && rank <= 3) hitAt[3]++;
    if (rank >= 1 && rank <= 5) hitAt[5]++;
    if (rank >= 1 && rank <= 10) hitAt[10]++;
    if (rank >= 1) mrrSum += 1 / rank;
  }

  results.correctness.push({
    id: q.id, category: q.category, query: q.query, expect: q.expect,
    medianMs: Math.round(median(times)), nodes: nodes.length, confidence: confidence(last.text),
    abstain, hitRank: rank, topSig: nodes[0]?.sig ?? null, tokens: estTokens(last.text),
  });
  judgePackets.push({ id: q.id, category: q.category, query: q.query, expect: q.expect, contextText: strip(last.text) });
  console.error(`${q.id} ${q.category.padEnd(10)} rank=${rank} conf=${confidence(last.text)} nodes=${nodes.length} ${Math.round(median(times))}ms`);
}

results.perf.correctnessSummary = {
  answerable, nonsense,
  "hit@1": +(hitAt[1] / answerable).toFixed(3), "hit@3": +(hitAt[3] / answerable).toFixed(3),
  "hit@5": +(hitAt[5] / answerable).toFixed(3), "hit@10": +(hitAt[10] / answerable).toFixed(3),
  mrr: +(mrrSum / answerable).toFixed(3),
  abstainAccuracy: +(abstainCorrect / Math.max(1, nonsense)).toFixed(3),
};

// ── perf: get_context budget sweep (warm) ──
for (const b of [500, 2000, 4000, 8000]) {
  const ts = [];
  for (let r = 0; r < REPEATS; r++) { const c = await mcp.call("get_context", { query: "how are snippets truncated to a token budget", token_budget: String(b) }); ts.push(c.ms); }
  results.perfSweep[b] = { medianMs: Math.round(median(ts)), p95Ms: Math.round(p95(ts)) };
}

// ── perf: get_snippets modes on a small + large definition ──
const targets = SNIPPET_TARGETS;
for (const t of targets) {
  results.snippetModes[t.label] = {};
  for (const mode of ["auto", "full", "skeleton"]) {
    const ts = [];
    let last = null;
    for (let r = 0; r < REPEATS; r++) { last = await mcp.call("get_snippets", { symbols: t.sig, token_budget: "30000", mode }); ts.push(last.ms); }
    results.snippetModes[t.label][mode] = { medianMs: Math.round(median(ts)), bytes: strip(last.text).length, tokens: estTokens(last.text) };
  }
}

mcp.close();

// ── write outputs ──
writeFileSync(join(HERE, `results${suffix}.json`), JSON.stringify(results, null, 2));
writeFileSync(join(HERE, `judge-packets${suffix}.json`), JSON.stringify(judgePackets, null, 2));
writeFileSync(join(HERE, `report${suffix}.md`), renderReport(results));
console.error(`\nwrote bench/results${suffix}.json, bench/report${suffix}.md, bench/judge-packets${suffix}.json`);

function renderReport(r) {
  const cs = r.perf.correctnessSummary;
  let md = `# Travsr tool benchmark — \`${r.model}\`\n\n_${r.generatedAt}_ · repo: ${LABEL} · cold first get_context: **${r.coldMs} ms**\n\n`;
  md += `## 1. Correctness (get_context, budget 2000, ${REPEATS}× median)\n\n`;
  md += `| metric | value |\n|---|---|\n`;
  md += `| answerable queries | ${cs.answerable} |\n| hit@1 | ${cs["hit@1"]} |\n| hit@3 | ${cs["hit@3"]} |\n| hit@5 | ${cs["hit@5"]} |\n| hit@10 | ${cs["hit@10"]} |\n| MRR | ${cs.mrr} |\n| abstain accuracy (nonsense) | ${cs.abstainAccuracy} (${r.correctness.filter(c=>c.category==='nonsense'&&c.abstain).length}/${cs.nonsense}) |\n\n`;
  md += `### Per-query\n\n| id | cat | rank | conf | nodes | tok | ms | top result |\n|---|---|---|---|---|---|---|---|\n`;
  for (const c of r.correctness) md += `| ${c.id} | ${c.category} | ${c.hitRank || (c.category==='nonsense'?(c.abstain?'abstain✓':'salad✗'):'MISS')} | ${c.confidence} | ${c.nodes} | ${c.tokens} | ${c.medianMs} | ${c.topSig ?? '—'} |\n`;
  md += `\n## 2. Performance — get_context budget sweep (${REPEATS}× )\n\n| budget | median ms | p95 ms |\n|---|---|---|\n`;
  for (const [b, v] of Object.entries(r.perfSweep)) md += `| ${b} | ${v.medianMs} | ${v.p95Ms} |\n`;
  md += `\n## 3. Performance — get_snippets modes (${REPEATS}× median)\n\n| target | mode | median ms | bytes | ~tokens |\n|---|---|---|---|---|\n`;
  for (const [tl, modes] of Object.entries(r.snippetModes)) for (const [m, v] of Object.entries(modes)) md += `| ${tl} | ${m} | ${v.medianMs} | ${v.bytes} | ${v.tokens} |\n`;
  md += `\n## 4. Usefulness (LLM-judge)\n\n_Judge packets in \`judge-packets.json\`. Scored separately — see report addendum._\n`;
  return md;
}
