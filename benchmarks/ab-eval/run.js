#!/usr/bin/env node
'use strict';

// A/B eval runner (#318 O9) — the receipt for the headline claim.
//
// For every task in tasks.json we answer the SAME structural question two ways
// and compare correctness and context token cost:
//
//   GRAPH arm  — run one `travsr graph` query and read only the answer
//                subgraph (the file nodes Travsr returns). This is what an
//                agent wired to the Travsr MCP server would feed its LLM.
//
//   FILES arm  — model an agent WITHOUT Travsr: `git grep` the symbol to find
//                candidate files, then read every candidate in full to decide
//                the answer (you cannot tell a real call site from a comment or
//                a same-named token without reading the file). The answer is the
//                set of grepped files; the context cost is the sum of their
//                sizes.
//
// We report, per task: answer recall/precision for each arm, the context tokens
// each arm costs, and the token reduction the graph arm achieves. The GRAPH arm
// is gated to be exact (recall == precision == 1 — zero structural
// hallucination) and to never cost more tokens than reading files. The mean
// token-reduction figure is reported as the publishable headline.
//
// Plain Node >= 18, no dependencies. Usage:
//   node benchmarks/ab-eval/run.js [--out report.json] [--bin path/to/travsr]

const { execFileSync, spawnSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const REPO_ROOT = path.resolve(__dirname, '..', '..');
const MANIFEST = JSON.parse(fs.readFileSync(path.join(__dirname, 'tasks.json'), 'utf8'));

function arg(flag, fallback) {
  const i = process.argv.indexOf(flag);
  return i !== -1 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
}

const BIN = path.resolve(
  arg('--bin', process.env.TRAVSR_BIN || path.join(REPO_ROOT, 'target', 'release', 'travsr'))
);
const OUT = path.resolve(arg('--out', path.join(__dirname, 'report.json')));

if (!fs.existsSync(BIN)) {
  console.error(`travsr binary not found at ${BIN} — build with: cargo build --release -p travsr-cli`);
  process.exit(1);
}

const ENV = { ...process.env, TRAVSR_DISABLE_REGISTRY: '1', RUST_LOG: 'error' };

function sh(cmd, args, cwd) {
  execFileSync(cmd, args, { cwd, env: ENV, stdio: 'pipe' });
}

/** Run the travsr binary, returning { stdout, status }. */
function travsr(args, cwd) {
  const res = spawnSync(BIN, args, { cwd, env: ENV, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 });
  return { stdout: res.stdout || '', stderr: res.stderr || '', status: res.status };
}

/** ~4 chars per token — same calibration as travsr-retrieval::token_cost. */
function tokens(s) {
  return Math.ceil(s.length / 4);
}

function setMetrics(expected, got) {
  const inter = expected.filter(e => got.has(e));
  return {
    recall: expected.length === 0 ? 1 : inter.length / expected.length,
    precision: got.size === 0 ? (expected.length === 0 ? 1 : 0) : inter.length / got.size,
    got: [...got].sort(),
  };
}

function prepareFixture(fixtureRelPath, name) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `travsr-ab-${name}-`));
  fs.cpSync(path.join(REPO_ROOT, fixtureRelPath), dir, { recursive: true });
  sh('git', ['init', '-q'], dir);
  sh('git', ['add', '-A'], dir);
  sh('git', ['-c', 'user.email=ci@travsr.com', '-c', 'user.name=travsr-ci', 'commit', '-qm', 'fixture'], dir);
  return dir;
}

// ── GRAPH arm: one Travsr query; the answer IS the context ────────────────────
function graphArm(task, dir) {
  const args = ['graph', task.symbol, '--direction', task.relation, '--format', 'json', '--budget', '0'];
  let res = null;
  for (let i = 0; i < MANIFEST.iterations; i++) res = travsr(args, dir);
  let parsed = null;
  try { parsed = JSON.parse(res.stdout); } catch (_) { /* scored as miss */ }
  const answerNodes = ((parsed && parsed.nodes) || [])
    .filter(n => n.kind === 'file' && n.depth_from_seed > 0 && n.path !== task.self_path);
  const got = new Set(answerNodes.map(n => n.path));
  const m = setMetrics(task.expect_files, got);
  // The graph hands the agent the answer directly — the resolved file paths and
  // their signatures. That compact answer, NOT any source file, is what enters
  // the LLM context. (We deliberately do not count the `--format json` debug
  // envelope: an MCP-wired agent forwards the answer, not the raw CLI framing.)
  const answerPayload = answerNodes.map(n => ({ path: n.path, signature: n.signature }));
  return { ...m, context_tokens: tokens(JSON.stringify(answerPayload)), command: `travsr ${args.join(' ')}` };
}

// ── FILES arm: git grep the symbol, then read every candidate file ────────────
function filesArm(task, dir) {
  // Candidate files an agent would have to open: anything textually mentioning
  // the symbol. `git grep -l` is the cheapest discovery an LLM agent has
  // without a code graph.
  let candidates = [];
  try {
    const out = execFileSync('git', ['grep', '-I', '-l', '--fixed-strings', task.symbol], {
      cwd: dir, env: ENV, encoding: 'utf8',
    });
    candidates = out.split('\n').map(s => s.trim()).filter(Boolean);
  } catch (_) { /* no matches → empty */ }

  // The agent must READ each candidate to decide the answer — that is the
  // context cost. Sum the bytes of every candidate file.
  let contextChars = 0;
  for (const rel of candidates) {
    try { contextChars += fs.readFileSync(path.join(dir, rel), 'utf8').length; } catch (_) { /* skip */ }
  }
  // The answer grep+read yields is "every file that mentions the symbol" minus
  // the file the symbol is defined in — a textual superset of the true answer.
  const got = new Set(candidates.filter(p => p !== task.self_path));
  const m = setMetrics(task.expect_files, got);
  return { ...m, context_tokens: Math.ceil(contextChars / 4), candidates };
}

const report = {
  generated_at: new Date().toISOString(),
  binary: BIN,
  thresholds: MANIFEST.thresholds,
  tasks: [],
};
const failures = [];
const reductions = [];
let graphSuccesses = 0;
let filesSuccesses = 0;

for (const task of MANIFEST.tasks) {
  console.log(`\n=== task: ${task.name} — "${task.question}" ===`);
  const dir = prepareFixture(task.fixture, task.name);
  const init = travsr(['init', '--quiet'], dir);
  if (init.status !== 0) {
    failures.push(`${task.name}: travsr init failed (exit ${init.status})`);
    fs.rmSync(dir, { recursive: true, force: true });
    continue;
  }

  const graph = graphArm(task, dir);
  const files = filesArm(task, dir);
  fs.rmSync(dir, { recursive: true, force: true });

  const graphExact = graph.recall >= MANIFEST.thresholds.graph_recall &&
    graph.precision >= MANIFEST.thresholds.graph_precision;
  const filesExact = files.recall === 1 && files.precision === 1;
  if (graphExact) graphSuccesses += 1;
  if (filesExact) filesSuccesses += 1;

  // Token reduction the graph arm achieves over reading files.
  const reduction = files.context_tokens === 0
    ? 0
    : 1 - graph.context_tokens / files.context_tokens;
  reductions.push(reduction);

  const cheaper = graph.context_tokens <= files.context_tokens;
  const pass = graphExact && cheaper && reduction >= MANIFEST.thresholds.min_token_reduction;

  report.tasks.push({
    name: task.name,
    question: task.question,
    graph: { recall: graph.recall, precision: graph.precision, context_tokens: graph.context_tokens, answer: graph.got },
    files_only: { recall: files.recall, precision: files.precision, context_tokens: files.context_tokens, answer: files.got, files_read: files.candidates.length },
    token_reduction: Number(reduction.toFixed(3)),
    pass,
  });

  console.log(`  graph : recall ${graph.recall} precision ${graph.precision} · ${graph.context_tokens} ctx tokens → ${JSON.stringify(graph.got)}`);
  console.log(`  files : recall ${files.recall} precision ${files.precision} · ${files.context_tokens} ctx tokens (read ${files.candidates.length} files) → ${JSON.stringify(files.got)}`);
  console.log(`  ${pass ? '✓' : '✗'} token reduction ${(reduction * 100).toFixed(1)}%  (graph exact: ${graphExact}, files exact: ${filesExact})`);
  if (!pass) failures.push(`${task.name}: graphExact=${graphExact} cheaper=${cheaper} reduction=${reduction.toFixed(3)}`);
}

const meanReduction = reductions.length ? reductions.reduce((a, b) => a + b, 0) / reductions.length : 0;
report.summary = {
  tasks: MANIFEST.tasks.length,
  graph_success_rate: Number((graphSuccesses / MANIFEST.tasks.length).toFixed(3)),
  files_only_success_rate: Number((filesSuccesses / MANIFEST.tasks.length).toFixed(3)),
  mean_token_reduction: Number(meanReduction.toFixed(3)),
};
report.pass = failures.length === 0;
report.failures = failures;
fs.writeFileSync(OUT, JSON.stringify(report, null, 2));

console.log('\n──────────── A/B summary ────────────');
console.log(`graph arm success:      ${(report.summary.graph_success_rate * 100).toFixed(0)}%  (exact, no structural hallucination)`);
console.log(`files-only arm success: ${(report.summary.files_only_success_rate * 100).toFixed(0)}%`);
console.log(`mean context-token reduction (graph vs files): ${(meanReduction * 100).toFixed(1)}%`);
console.log(`report written to ${OUT}`);

if (failures.length > 0) {
  console.error(`\nFAIL — ${failures.length} task(s) below threshold:`);
  for (const f of failures) console.error(`  - ${f}`);
  process.exit(1);
}
console.log('\nPASS — graph arm exact and cheaper on every task.');
