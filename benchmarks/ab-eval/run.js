#!/usr/bin/env node
'use strict';

// A/B eval runner (#318 O9) — the receipt for the headline claim.
//
// Three task classes:
//
//   callers  — GRAPH arm runs `travsr graph --direction callers --format json`
//              and checks which file nodes are returned. FILES arm runs `git
//              grep -l` and reads every candidate. Gated: exact (recall=1,
//              precision=1) and graph tokens ≤ file tokens.
//
//   context  — GRAPH arm runs `travsr ask <query> --format json` (the full
//              PPR+knapsack pipeline) and checks that expected symbols and
//              files appear in the ranked rows. FILES arm is the same grep
//              baseline. Gated: recall=1, precision=1, graph tokens ≤ file
//              tokens. Measures end-to-end retrieval quality.
//
//   ask      — Seed resolution quality. GRAPH arm runs `travsr ask <query>
//              --format json` and asserts that the top-ranked row's signature
//              contains expect_top_symbol and that the expected file is present.
//              No A/B token comparison — this class is purely about seed
//              accuracy (does the right symbol surface at rank 1?).
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

// ── FILES arm (shared by callers + context classes) ───────────────────────────
// Model an agent without Travsr: git grep the symbol/query to find candidate
// files, then read every candidate in full to decide the answer.
function filesArm(task, dir) {
  const term = task.symbol || task.query;
  let candidates = [];
  try {
    const out = execFileSync('git', ['grep', '-I', '-l', '--fixed-strings', term], {
      cwd: dir, env: ENV, encoding: 'utf8',
    });
    candidates = out.split('\n').map(s => s.trim()).filter(Boolean);
  } catch (_) { /* no matches → empty */ }

  let contextChars = 0;
  for (const rel of candidates) {
    try { contextChars += fs.readFileSync(path.join(dir, rel), 'utf8').length; } catch (_) { /* skip */ }
  }
  const selfPath = task.self_path || null;
  const got = new Set(candidates.filter(p => p !== selfPath));
  const m = setMetrics(task.expect_files || [], got);
  return { ...m, context_tokens: Math.ceil(contextChars / 4), candidates };
}

// ── GRAPH arm: callers class ──────────────────────────────────────────────────
function callersArm(task, dir) {
  const args = ['graph', task.symbol, '--direction', 'callers', '--format', 'json', '--budget', '0'];
  let res = null;
  for (let i = 0; i < MANIFEST.iterations; i++) res = travsr(args, dir);
  let parsed = null;
  try { parsed = JSON.parse(res.stdout); } catch (_) { /* scored as miss */ }
  const answerNodes = ((parsed && parsed.nodes) || [])
    .filter(n => n.kind === 'file' && n.depth_from_seed > 0 && n.path !== task.self_path);
  const got = new Set(answerNodes.map(n => n.path));
  const m = setMetrics(task.expect_files || [], got);
  const answerPayload = answerNodes.map(n => ({ path: n.path, signature: n.signature }));
  return { ...m, context_tokens: tokens(JSON.stringify(answerPayload)), command: `travsr ${args.join(' ')}` };
}

// ── GRAPH arm: context class ──────────────────────────────────────────────────
// Runs the full PPR+knapsack pipeline via `travsr ask --format json`.
// Checks that expected symbols appear in the ranked rows and expected files
// are represented. Context cost = AskPayload.total_tokens (what an MCP-wired
// agent would forward — not raw file bytes).
function contextArm(task, dir) {
  const args = ['ask', task.query, '--format', 'json'];
  let res = null;
  for (let i = 0; i < MANIFEST.iterations; i++) res = travsr(args, dir);
  let payload = null;
  try { payload = JSON.parse(res.stdout); } catch (_) { /* scored as miss */ }

  const rows = (payload && payload.rows) || [];
  const returnedSymbols = new Set(rows.map(r => r.signature));
  const returnedFiles = new Set(rows.map(r => r.path.replace(/:\d+$/, '')));

  // Precision/recall over expected files (primary gate — same as callers class).
  const fileMet = setMetrics(task.expect_files || [], returnedFiles);
  // Symbol recall is reported but not gated (signatures may be qualified).
  const expectSymbols = task.expect_symbols || [];
  const symbolRecall = expectSymbols.length === 0 ? 1
    : expectSymbols.filter(s => [...returnedSymbols].some(sig => sig.includes(s))).length / expectSymbols.length;

  const contextTokens = (payload && payload.total_tokens) || tokens(res.stdout);
  return {
    recall: fileMet.recall,
    precision: fileMet.precision,
    got: fileMet.got,
    symbol_recall: symbolRecall,
    embed_used: (payload && payload.embed_used) || false,
    context_tokens: contextTokens,
    command: `travsr ${args.join(' ')}`,
  };
}

// ── GRAPH arm: ask class ──────────────────────────────────────────────────────
// Seed resolution: does the top-ranked row's signature match expect_top_symbol
// and does expect_files[0] appear in the results?
function askArm(task, dir) {
  const args = ['ask', task.query, '--format', 'json'];
  let res = null;
  for (let i = 0; i < MANIFEST.iterations; i++) res = travsr(args, dir);
  let payload = null;
  try { payload = JSON.parse(res.stdout); } catch (_) { /* scored as miss */ }

  const rows = (payload && payload.rows) || [];
  const topSig = rows.length > 0 ? rows[0].signature : '';
  const topMatch = topSig.includes(task.expect_top_symbol || '');
  const returnedFiles = new Set(rows.map(r => r.path.replace(/:\d+$/, '')));
  const filePresent = (task.expect_files || []).every(f => returnedFiles.has(f));

  return {
    top_match: topMatch,
    top_signature: topSig,
    file_present: filePresent,
    returned_files: [...returnedFiles].sort(),
    embed_used: (payload && payload.embed_used) || false,
    context_tokens: (payload && payload.total_tokens) || 0,
    command: `travsr ${args.join(' ')}`,
  };
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
  const cls = task.class || 'callers';
  console.log(`\n=== [${cls}] ${task.name} — "${task.question}" ===`);
  const dir = prepareFixture(task.fixture, task.name);
  const init = travsr(['init', '--quiet'], dir);
  if (init.status !== 0) {
    failures.push(`${task.name}: travsr init failed (exit ${init.status})`);
    fs.rmSync(dir, { recursive: true, force: true });
    continue;
  }

  if (cls === 'ask') {
    // Seed resolution — no A/B token comparison.
    const arm = askArm(task, dir);
    fs.rmSync(dir, { recursive: true, force: true });

    const pass = arm.top_match && arm.file_present;
    if (pass) graphSuccesses += 1;

    report.tasks.push({
      name: task.name,
      class: cls,
      question: task.question,
      ask: {
        top_match: arm.top_match,
        top_signature: arm.top_signature,
        file_present: arm.file_present,
        returned_files: arm.returned_files,
        embed_used: arm.embed_used,
      },
      pass,
    });

    console.log(`  ask   : top="${arm.top_signature}" match=${arm.top_match} · files=${JSON.stringify(arm.returned_files)}`);
    console.log(`  ${pass ? '✓' : '✗'} seed resolution (top_match=${arm.top_match}, file_present=${arm.file_present})`);
    if (!pass) failures.push(`${task.name}: top_match=${arm.top_match} file_present=${arm.file_present}`);
    continue;
  }

  // callers and context classes share the A/B structure.
  const graph = cls === 'callers' ? callersArm(task, dir) : contextArm(task, dir);
  const files = filesArm(task, dir);
  fs.rmSync(dir, { recursive: true, force: true });

  const graphExact = graph.recall >= MANIFEST.thresholds.graph_recall &&
    graph.precision >= MANIFEST.thresholds.graph_precision;
  const filesExact = files.recall === 1 && files.precision === 1;
  if (graphExact) graphSuccesses += 1;
  if (filesExact) filesSuccesses += 1;

  const reduction = files.context_tokens === 0
    ? 0
    : 1 - graph.context_tokens / files.context_tokens;
  reductions.push(reduction);

  const cheaper = graph.context_tokens <= files.context_tokens;
  const pass = graphExact && cheaper && reduction >= MANIFEST.thresholds.min_token_reduction;

  const taskEntry = {
    name: task.name,
    class: cls,
    question: task.question,
    graph: { recall: graph.recall, precision: graph.precision, context_tokens: graph.context_tokens, answer: graph.got },
    files_only: { recall: files.recall, precision: files.precision, context_tokens: files.context_tokens, answer: files.got, files_read: files.candidates.length },
    token_reduction: Number(reduction.toFixed(3)),
    pass,
  };
  if (cls === 'context') {
    taskEntry.graph.symbol_recall = graph.symbol_recall;
    taskEntry.graph.embed_used = graph.embed_used;
  }
  report.tasks.push(taskEntry);

  const extra = cls === 'context' ? ` · symbol_recall=${graph.symbol_recall.toFixed(2)} embed=${graph.embed_used}` : '';
  console.log(`  graph : recall ${graph.recall} precision ${graph.precision} · ${graph.context_tokens} ctx tokens → ${JSON.stringify(graph.got)}${extra}`);
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
