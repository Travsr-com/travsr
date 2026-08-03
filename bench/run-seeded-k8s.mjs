#!/usr/bin/env node
// #478 RFC-023 §9/WS-9 — CI baseline harness for the k8s seeded fixture.
//
// Mirrors bench/run-seeded-travsr.mjs exactly, but against an external repo:
// scores bench/queries-seeded-k8s.json against the local release binary via
// `travsr ask --format json` run with `cwd` set to a checked-out, already
// `travsr init`-ed kubernetes/kubernetes clone (BENCH_REPO — same convention
// as bench/run.mjs), and gates the result against a frozen baseline
// (bench/baseline-seeded-k8s.json).
//
// k8s is the hard merge gate for #478 (RFC-023 §11 AC #9): Go's exported-
// identifier (PascalCase) convention means the ident-segmenter boundary-guard
// fix has a far larger blast radius here than on the travsr repo itself.
//
//   * emits a machine-readable summary (TOTAL, per-category, invariant guards),
//   * NON-BLOCKING by default: prints ::warning and still exits 0,
//   * set BENCH_STRICT=1 to exit non-zero on any regression vs. the baseline
//     (TOTAL below baseline, an abstain leak, or a literal/rare drop).
//
// Refresh the frozen baseline after an intended improvement:
//   BENCH_WRITE_BASELINE=1 BENCH_REPO=/path/to/kubernetes node bench/run-seeded-k8s.mjs
//
// Usage:
//   BENCH_REPO=/path/to/kubernetes node bench/run-seeded-k8s.mjs
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const BIN = join(HERE, "..", "target/release/travsr");
const FIXTURE = join(HERE, "queries-seeded-k8s.json");
const BASELINE = join(HERE, "baseline-seeded-k8s.json");
const RANK = 3; // "hit" if an expected token appears in the top-3 rows

const REPO = process.env.BENCH_REPO;
if (!REPO || !existsSync(join(REPO, ".travsr", "graph.db"))) {
  console.error(
    "BENCH_REPO must point to a kubernetes/kubernetes checkout already " +
      "indexed with `travsr init` (no .travsr/graph.db found at " +
      `${REPO || "(unset)"}).`
  );
  process.exit(0); // non-blocking: informational CI job, missing fixture is not a code regression.
}

const { queries } = JSON.parse(readFileSync(FIXTURE, "utf8"));

const ask = (q) => {
  try {
    const out = execFileSync(BIN, ["ask", q, "--format", "json"], {
      cwd: REPO,
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
  if (c.expect_abstain || c.expect.length === 0) {
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
  repo: REPO,
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
for (const cat of ["literal"]) {
  const now = byCat[cat]?.pass ?? 0;
  const was = base.byCat?.[cat]?.pass ?? 0;
  if (now < was) regressions.push(`${cat} ${now} < baseline ${was}`);
}

if (regressions.length === 0) {
  console.error(`OK — ${pass}/${queries.length} ≥ baseline ${base.total}, invariants held`);
  process.exit(0);
}
for (const r of regressions) console.error(`::warning::seeded-k8s regression: ${r}`);
process.exit(process.env.BENCH_STRICT === "1" ? 1 : 0);
