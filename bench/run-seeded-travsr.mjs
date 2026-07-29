#!/usr/bin/env node
// RFC-022 Phase 0 — CI baseline harness for the travsr self-index seeded fixture.
//
// Scores bench/queries-seeded-travsr.json against the local release binary via
// `travsr ask --format json` (cold path — stop the daemon first for determinism)
// and gates the result against a frozen baseline (bench/baseline-seeded-travsr.json):
//
//   * emits a machine-readable summary (TOTAL, per-category, invariant guards),
//   * NON-BLOCKING by default: prints ::warning and still exits 0 so it can run as
//     an informational CI job while RFC-022 phases land,
//   * set BENCH_STRICT=1 to exit non-zero on any regression vs. the baseline
//     (TOTAL below baseline, an abstain leak, or a literal/rare drop) — used once
//     a phase is promoted to default-on.
//
// Refresh the frozen baseline after an intended improvement:
//   BENCH_WRITE_BASELINE=1 node bench/run-seeded-travsr.mjs
//
// Usage:
//   node bench/run-seeded-travsr.mjs
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const BIN = join(HERE, "..", "target/release/travsr");
const FIXTURE = join(HERE, "queries-seeded-travsr.json");
const BASELINE = join(HERE, "baseline-seeded-travsr.json");
const RANK = 3; // "hit" if an expected token appears in the top-3 rows

const { queries } = JSON.parse(readFileSync(FIXTURE, "utf8"));

const ask = (q) => {
  try {
    const out = execFileSync(BIN, ["ask", q, "--format", "json"], {
      encoding: "utf8",
      timeout: 60000,
      stdio: ["ignore", "pipe", "ignore"],
    });
    const line = out.trim().split("\n").filter(Boolean).pop() || "{}";
    return JSON.parse(line);
  } catch {
    return { matched: false, rows: [], confidence: "" };
  }
};

const hitOf = (p, expect) => {
  const hay = (p.rows || [])
    .slice(0, RANK)
    .flatMap((r) => [r.signature || "", r.path || ""])
    .join(" ")
    .toLowerCase();
  return expect.findIndex((e) => hay.includes(e.toLowerCase()));
};

const cOK = (min, got) => {
  if (!min) return true;
  const ord = { weak: 1, strong: 2, exact: 3 };
  return (ord[got] || 0) >= (ord[min] || 0);
};

const byCat = {};
let pass = 0;
let abstainLeak = 0;
for (const c of queries) {
  const p = ask(c.query);
  const abstained = p.matched === false;
  let ok;
  if (c.expect_abstain) {
    ok = abstained;
    if (!abstained) abstainLeak++;
  } else {
    const i = abstained ? -1 : hitOf(p, c.expect);
    ok = i >= 0 && cOK(c.min_confidence, p.confidence);
  }
  pass += ok ? 1 : 0;
  byCat[c.category] ||= { pass: 0, n: 0 };
  byCat[c.category].n++;
  byCat[c.category].pass += ok ? 1 : 0;
}

const summary = {
  generatedAt: new Date().toISOString(),
  total: pass,
  n: queries.length,
  abstainLeak,
  byCat,
};

console.log(JSON.stringify(summary, null, 2));

// ── baseline gate ────────────────────────────────────────────────────────────
if (process.env.BENCH_WRITE_BASELINE === "1") {
  writeFileSync(BASELINE, JSON.stringify(summary, null, 2) + "\n");
  console.error(`froze baseline → ${BASELINE} (total ${pass}/${queries.length})`);
  process.exit(0);
}

if (!existsSync(BASELINE)) {
  console.error("no baseline yet — run with BENCH_WRITE_BASELINE=1 to freeze one");
  process.exit(0);
}

const base = JSON.parse(readFileSync(BASELINE, "utf8"));
const regressions = [];
if (pass < base.total) regressions.push(`TOTAL ${pass} < baseline ${base.total}`);
if (abstainLeak > (base.abstainLeak || 0))
  regressions.push(`abstain leaks ${abstainLeak} > baseline ${base.abstainLeak || 0}`);
for (const cat of ["literal", "rare"]) {
  const now = byCat[cat]?.pass ?? 0;
  const was = base.byCat?.[cat]?.pass ?? 0;
  if (now < was) regressions.push(`${cat} ${now} < baseline ${was}`);
}

if (regressions.length === 0) {
  console.error(`OK — ${pass}/${queries.length} ≥ baseline ${base.total}, invariants held`);
  process.exit(0);
}
for (const r of regressions) console.error(`::warning::seeded-travsr regression: ${r}`);
process.exit(process.env.BENCH_STRICT === "1" ? 1 : 0);
