#!/usr/bin/env node
// get_execution_path benchmark harness (issue #527, P2 baseline).
//
// Drives the local MCP server over stdio and scores `get_execution_path` against
// ground-truth pairs whose true shortest-path length is known (see
// bench/gen-execpath-pairs.mjs). Two things are measured, and they answer
// different questions:
//
//   RECALL     — was the sink returned at all? This is what "did it find the
//                path" means. A tool that misses the sink has failed outright.
//
//   PRECISION  — of the nodes returned, how many can possibly be path? The
//                shortest path is `hops + 1` nodes, so anything beyond that is
//                λ-corridor padding. `path_share = (hops+1) / returned`.
//
// Precision here is an UPPER BOUND on usefulness, not a quality score. It says
// how much of the response is structurally attributable to the route; it cannot
// say whether the padding is helpful context or noise. Answering that needs
// human or LLM judgement over the returned blocks — the same shape as the
// judge-packets pass in run.mjs — and is deliberately out of scope here.
//
// This matters for #527: the open question is not whether the current algorithm
// finds paths (it does), but whether replacing the λ corridor with a solved
// prize/cost tradeoff produces a better *selection*. This harness establishes
// the baseline that claim has to beat.
//
// Usage: node bench/run-execpath.mjs   (release binary must be built)
//
//   BENCH_REPO     absolute path to the indexed repo (default: this repo)
//   BENCH_PAIRS    path to a queries-execpath-*.json
//   BENCH_LABEL    suffix for output files (default "travsr")
//
// Requires the graph to have Phase B call edges. Without them
// `get_execution_path` returns {"status":"pending"} and the run aborts with a
// clear message rather than reporting zeros — see PREFLIGHT below.

import { spawn } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, "..");
const BIN = join(ROOT, "target/release/travsr");

const REPO = process.env.BENCH_REPO || ROOT;
const LABEL = process.env.BENCH_LABEL || "travsr";
const PAIRS_PATH = process.env.BENCH_PAIRS || join(HERE, `queries-execpath-${LABEL}.json`);
const suffix = LABEL === "travsr" ? "" : `-${LABEL}`;

// ── minimal persistent MCP stdio client (mirrors run.mjs) ────────────────────
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
      if (msg.id && this.pending.has(msg.id)) {
        this.pending.get(msg.id)(msg);
        this.pending.delete(msg.id);
      }
    }
  }
  call(method, params) {
    const id = this.nextId++;
    return new Promise((res) => {
      this.pending.set(id, res);
      this.proc.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
    });
  }
  tool(name, args) { return this.call("tools/call", { name, arguments: args }); }
  kill() { this.proc.kill(); }
}

const text = (r) => r?.result?.content?.[0]?.text ?? "";

/** Signatures from a <travsr-data> block, envelope and blank lines stripped. */
function sigs(body) {
  return body
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith("<travsr-data") && !l.startsWith("</travsr-data"))
    .map((l) => l.split(" ")[0]);
}

const doc = JSON.parse(readFileSync(PAIRS_PATH, "utf8"));
const mcp = new Mcp();
await mcp.call("initialize", {
  protocolVersion: "2024-11-05",
  capabilities: {},
  clientInfo: { name: "bench-execpath", version: "0" },
});

// ── PREFLIGHT: Phase B gate ─────────────────────────────────────────────────
// get_execution_path returns {"status":"pending"} until Phase B has run once.
// Reporting 0% recall in that state would be misleading — the tool never ran.
{
  const p = doc.pairs[0];
  const probe = text(await mcp.tool("get_execution_path", { source: p.source, sink: p.sink }));
  if (probe.includes('"status":"pending"')) {
    console.error(
      "get_execution_path is gated on Phase B and returns pending.\n" +
      "Run `travsr init --semantic` in the target repo first, then re-run.\n" +
      "(`travsr status` should show phase_b: complete.)"
    );
    mcp.kill();
    process.exit(2);
  }
}

// ── run ─────────────────────────────────────────────────────────────────────
const rows = [];
for (const p of doc.pairs) {
  const t0 = Date.now();
  const body = text(await mcp.tool("get_execution_path", { source: p.source, sink: p.sink }));
  const ms = Date.now() - t0;
  const out = sigs(body);
  const minPath = p.hops + 1;          // nodes on the shortest route, inclusive
  rows.push({
    ...p,
    ms,
    returned: out.length,
    sink_found: out.includes(p.sink),
    source_found: out.includes(p.source),
    path_share: out.length ? Math.min(1, minPath / out.length) : 0,
    padding: Math.max(0, out.length - minPath),
  });
}
mcp.kill();

// ── aggregate ───────────────────────────────────────────────────────────────
const byHop = {};
for (const r of rows) (byHop[r.hops] ??= []).push(r);
const mean = (a, f) => (a.length ? a.reduce((s, x) => s + f(x), 0) / a.length : 0);
const pct = (a, f) => (a.length ? (100 * a.filter(f).length) / a.length : 0);

const summary = {
  label: LABEL,
  generated_at: new Date().toISOString(),
  pairs: rows.length,
  recall_sink_found_pct: +pct(rows, (r) => r.sink_found).toFixed(1),
  source_present_pct: +pct(rows, (r) => r.source_found).toFixed(1),
  mean_returned: +mean(rows, (r) => r.returned).toFixed(1),
  mean_padding: +mean(rows, (r) => r.padding).toFixed(1),
  mean_path_share: +mean(rows, (r) => r.path_share).toFixed(3),
  p50_ms: rows.map((r) => r.ms).sort((a, b) => a - b)[Math.floor(rows.length / 2)],
  by_hop: Object.fromEntries(
    Object.entries(byHop).map(([h, rs]) => [
      h,
      {
        pairs: rs.length,
        recall_pct: +pct(rs, (r) => r.sink_found).toFixed(1),
        mean_returned: +mean(rs, (r) => r.returned).toFixed(1),
        mean_path_share: +mean(rs, (r) => r.path_share).toFixed(3),
      },
    ])
  ),
};

writeFileSync(
  join(HERE, `results-execpath${suffix}.json`),
  JSON.stringify({ summary, rows }, null, 2) + "\n"
);

const md = [
  `# get_execution_path benchmark — \`${LABEL}\``,
  ``,
  `_${summary.generated_at}_ · ${summary.pairs} ground-truth pairs · p50 ${summary.p50_ms} ms`,
  ``,
  `## Summary`,
  ``,
  `| metric | value |`,
  `|---|---|`,
  `| recall (sink returned) | **${summary.recall_sink_found_pct}%** |`,
  `| source returned | ${summary.source_present_pct}% |`,
  `| mean nodes returned | ${summary.mean_returned} |`,
  `| mean padding beyond shortest path | ${summary.mean_padding} |`,
  `| mean path share (upper bound) | ${summary.mean_path_share} |`,
  ``,
  `\`path_share\` = (hops + 1) / nodes returned. It bounds how much of the`,
  `response can be route; the remainder is λ-corridor padding. It does **not**`,
  `say whether that padding is useful — that needs a judgement pass.`,
  ``,
  `## By hop distance`,
  ``,
  `| hops | pairs | recall | mean returned | path share |`,
  `|---|---|---|---|---|`,
  ...Object.entries(summary.by_hop).map(
    ([h, s]) => `| ${h} | ${s.pairs} | ${s.recall_pct}% | ${s.mean_returned} | ${s.mean_path_share} |`
  ),
  ``,
  `## Per pair`,
  ``,
  `| id | hops | source → sink | returned | sink? | ms |`,
  `|---|---|---|---|---|---|`,
  ...rows.map(
    (r) =>
      `| ${r.id} | ${r.hops} | \`${r.source}\` → \`${r.sink}\` | ${r.returned} | ${r.sink_found ? "yes" : "**NO**"} | ${r.ms} |`
  ),
  ``,
].join("\n");

writeFileSync(join(HERE, `report-execpath${suffix}.md`), md);

console.log(`pairs ${summary.pairs} · recall ${summary.recall_sink_found_pct}% · mean returned ${summary.mean_returned} · path share ${summary.mean_path_share} · p50 ${summary.p50_ms}ms`);
console.log(`→ bench/results-execpath${suffix}.json`);
console.log(`→ bench/report-execpath${suffix}.md`);
