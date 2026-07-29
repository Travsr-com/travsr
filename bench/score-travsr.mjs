#!/usr/bin/env node
// Score bench/queries-seeded-travsr.json against the local release binary via
// `travsr ask --format json` (cold path; stop the daemon first for determinism).
//   node bench/score-travsr.mjs
import { readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";

const BIN = "./target/release/travsr";
const cases = JSON.parse(
  readFileSync(new URL("./queries-seeded-travsr.json", import.meta.url)),
).queries;

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
    // empty query / CLI bail / no payload => treat as abstain
    return { matched: false, rows: [], confidence: "" };
  }
};

const rank = 3; // "hit" if an expected token appears in the top-3 rows
const hitOf = (p, expect) => {
  const hay = (p.rows || [])
    .slice(0, rank)
    .flatMap((r) => [r.signature || "", r.path || ""])
    .join(" ")
    .toLowerCase();
  const i = expect.findIndex((e) => hay.includes(e.toLowerCase()));
  return i; // -1 = miss
};

const cOK = (min, got) => {
  if (!min) return true;
  const ord = { weak: 1, strong: 2, exact: 3 };
  return (ord[got] || 0) >= (ord[min] || 0);
};

const byCat = {};
let pass = 0;
const rows = [];
for (const c of cases) {
  const p = ask(c.query);
  const abstained = p.matched === false;
  let ok, detail;
  if (c.expect_abstain) {
    ok = abstained;
    detail = abstained ? "abstained" : `LEAK conf=${p.confidence}`;
  } else {
    const i = abstained ? -1 : hitOf(p, c.expect);
    const confOk = cOK(c.min_confidence, p.confidence);
    ok = i >= 0 && confOk;
    detail = abstained
      ? "ABSTAINED (wanted hit)"
      : i >= 0
        ? `hit[${c.expect[i]}] conf=${p.confidence}${confOk ? "" : ` <${c.min_confidence}!`}`
        : `MISS conf=${p.confidence}`;
  }
  pass += ok ? 1 : 0;
  byCat[c.category] ||= { p: 0, n: 0 };
  byCat[c.category].n++;
  byCat[c.category].p += ok ? 1 : 0;
  rows.push(`${ok ? "PASS" : "FAIL"} ${c.id.padEnd(4)} ${c.category.padEnd(11)} ${c.query.slice(0, 44).padEnd(45)} ${detail}`);
}

console.log(rows.join("\n"));
console.log("\n── by category ──");
for (const [k, v] of Object.entries(byCat))
  console.log(`  ${k.padEnd(12)} ${v.p}/${v.n}`);
console.log(`\nTOTAL ${pass}/${cases.length}`);
