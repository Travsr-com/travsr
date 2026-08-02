#!/usr/bin/env node
// #520: sweep the doc-lane selective-reranking ambiguity threshold
// (TRAVSR_DOC_RERANK_AMBIGUITY_THRESHOLD, seed.rs::doc_rerank_ambiguity_threshold)
// against the docs query set, on both bench repos, and report per-threshold
// hit@1/hit@3/latency plus per-query hit->miss flips against the baseline
// (no threshold set == always rerank, today's shipped behavior).
//
// This is an experiment, not a patch: §12's own prior attempt at this exact
// lever measurably lost accuracy (travsr 1.0/1.0 -> 0.9/0.9) despite an
// unchanged aggregate on kubernetes, because individual queries flipped
// hit<->miss underneath the stable-looking number. The per-query flip count
// this script reports is therefore the actual gate, not the aggregate.
//
// Usage:
//   BENCH_REPO=/path/to/repo BENCH_LABEL=travsr node bench/sweep-selective-rerank.mjs
//   BENCH_REPO=/Users/ak/Documents/k8/kubernetes BENCH_LABEL=k8s node bench/sweep-selective-rerank.mjs
//
// Exit code is always 0 - this reports a sweep, it does not gate a build.
// #520's ship decision is a judgment call over the report, made once per the
// plan (a strict, explicit bar: non-regressing hit@1/hit@3 on both repos,
// zero per-query hit->miss flips), not something this script auto-decides.

import { spawn, execSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, "..");
const BIN = join(ROOT, "target/release/travsr");
const LABEL = process.env.BENCH_LABEL || "travsr";
const REPO = process.env.BENCH_REPO || ROOT;
const BUDGET = process.env.BENCH_BUDGET || "4000";
const docsPath = join(HERE, `queries-docs-${LABEL}.json`);

// Thresholds to sweep, plus the implicit baseline (no env var set at all).
//
// Raw cosine is model-relative (the codebase's own model-relative-floors
// design exists precisely because of this — a fixed absolute cutoff is not
// portable across embedding backends). Bracket the *active* model's own
// calibrated range (graph.db meta embed_cos_lo/embed_cos_hi, written by
// `travsr embed calibrate`) rather than a fixed absolute scale, so the sweep
// actually exercises the skip condition instead of measuring a threshold band
// the model's scores never reach.
function calibratedRange() {
  try {
    const out = execSync(
      `sqlite3 "${join(REPO, ".travsr/graph.db")}" "SELECT key,value FROM meta WHERE key LIKE 'embed_cos_%';"`
    ).toString();
    const kv = Object.fromEntries(out.trim().split("\n").filter(Boolean).map((l) => l.split("|")));
    if (kv.embed_cos_lo && kv.embed_cos_hi) {
      return [parseFloat(kv.embed_cos_lo), parseFloat(kv.embed_cos_hi)];
    }
  } catch {}
  return [0.3, 0.7]; // fallback if calibration meta is absent
}
const [calLo, calHi] = calibratedRange();
const THRESHOLDS = Array.from({ length: 7 }, (_, i) => +(calLo + (i / 6) * (calHi - calLo) * 1.4).toFixed(3));
console.error(`calibrated range: lo=${calLo} hi=${calHi} — sweeping ${JSON.stringify(THRESHOLDS)}`);

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
      clientInfo: { name: "sweep-selective-rerank", version: "1" },
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
function parseDocLine(line) {
  const m = /^(.+?)\s+§\s+(.+)$/.exec(line);
  return m ? { path: m[1].trim(), heading: m[2].trim() } : null;
}
function parseDocsSection(text) {
  const docs = [];
  let inDocs = false;
  for (const line of strip(text).split("\n")) {
    const sec = /^##\s+(exact|semantic|docs|relevant)\b/.exec(line);
    if (sec) {
      inDocs = sec[1] === "docs";
      continue;
    }
    if (!inDocs || !line.trim()) continue;
    const entry = parseDocLine(line);
    if (entry) docs.push(entry);
  }
  return docs;
}
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

async function runDocsArm(envExtra) {
  const { queries } = JSON.parse(readFileSync(docsPath, "utf8"));
  const mcp = new Mcp({ TRAVSR_DOCS_ENABLED: "1", ...envExtra });
  await mcp.init();
  await mcp.call("get_context", { query: "warmup", token_budget: BUDGET });

  const rows = [];
  for (const q of queries) {
    const { text, ms } = await mcp.call("get_context", { query: q.query, token_budget: BUDGET });
    const docs = parseDocsSection(text);
    const rank = docHitRank(docs, q);
    rows.push({ id: q.id, rank, ms: Math.round(ms) });
  }
  mcp.close();
  return rows;
}

function summarize(rows) {
  const n = Math.max(1, rows.length);
  const hit = (k) => rows.filter((r) => r.rank >= 1 && r.rank <= k).length;
  return {
    n: rows.length,
    "hit@1": +(hit(1) / n).toFixed(3),
    "hit@3": +(hit(3) / n).toFixed(3),
    medianMs: Math.round(median(rows.map((r) => r.ms))),
  };
}

function flips(baseline, swept) {
  const byId = new Map(baseline.map((r) => [r.id, r]));
  const out = [];
  for (const s of swept) {
    const b = byId.get(s.id);
    if (!b) continue;
    const bHit = b.rank >= 1 && b.rank <= 3;
    const sHit = s.rank >= 1 && s.rank <= 3;
    if (bHit !== sHit) out.push({ id: s.id, baselineRank: b.rank, sweptRank: s.rank, direction: bHit ? "hit->miss" : "miss->hit" });
  }
  return out;
}

async function main() {
  console.error(`[baseline] running queries-docs-${LABEL} with reranking always on...`);
  const baseline = await runDocsArm({});
  const baselineSummary = summarize(baseline);
  console.error(JSON.stringify(baselineSummary));

  const results = [];
  for (const threshold of THRESHOLDS) {
    console.error(`[threshold=${threshold}] running...`);
    const swept = await runDocsArm({ TRAVSR_DOC_RERANK_AMBIGUITY_THRESHOLD: String(threshold) });
    const summary = summarize(swept);
    const flipped = flips(baseline, swept);
    console.error(JSON.stringify({ threshold, ...summary, flips: flipped.length }));
    results.push({ threshold, ...summary, flips: flipped, latencyRatio: +(summary.medianMs / baselineSummary.medianMs).toFixed(3) });
  }

  const report = { generatedAt: new Date().toISOString(), repo: LABEL, baseline: baselineSummary, sweep: results };
  const outPath = join(HERE, `report-selective-rerank-sweep-${LABEL}.json`);
  writeFileSync(outPath, JSON.stringify(report, null, 2));
  console.log(`wrote ${outPath}`);

  console.log(`\n=== SWEEP SUMMARY (${LABEL}) ===`);
  console.log(`baseline: hit@1=${baselineSummary["hit@1"]} hit@3=${baselineSummary["hit@3"]} medianMs=${baselineSummary.medianMs}`);
  for (const r of results) {
    const clears =
      r["hit@1"] >= baselineSummary["hit@1"] &&
      r["hit@3"] >= baselineSummary["hit@3"] &&
      r.flips.length === 0;
    console.log(
      `threshold=${r.threshold}: hit@1=${r["hit@1"]} hit@3=${r["hit@3"]} ratio=${r.latencyRatio} flips=${r.flips.length} ${clears ? "CLEARS the gate" : "does not clear"}`
    );
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
