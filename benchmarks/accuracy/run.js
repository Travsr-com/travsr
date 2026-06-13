#!/usr/bin/env node
'use strict';

// Nightly accuracy benchmark runner (#318 O8).
//
// For every enabled corpus in manifest.json:
//   1. copy the fixture into a temp git repo and run `travsr init` (timed —
//      this doubles as the #295-T6 init-at-scale perf section),
//   2. run each ground-truth query case through the release binary,
//   3. score recall / precision / seed resolution per class, plus latency
//      p50/p95 and output token cost per query,
//   4. write report.json and a console summary; exit 1 on any threshold
//      violation so CI fails on regression.
//
// Plain Node >= 18, no dependencies. Usage:
//   node benchmarks/accuracy/run.js [--out report.json] [--bin path/to/travsr]

const { execFileSync, spawnSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const REPO_ROOT = path.resolve(__dirname, '..', '..');
const MANIFEST = JSON.parse(fs.readFileSync(path.join(__dirname, 'manifest.json'), 'utf8'));

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

/** Run the travsr binary, returning { stdout, ms, status }. */
function travsr(args, cwd) {
  const t0 = process.hrtime.bigint();
  const res = spawnSync(BIN, args, { cwd, env: ENV, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 });
  const ms = Number(process.hrtime.bigint() - t0) / 1e6;
  return { stdout: res.stdout || '', stderr: res.stderr || '', ms, status: res.status };
}

function percentile(sorted, p) {
  if (sorted.length === 0) return 0;
  const idx = Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1);
  return sorted[Math.max(0, idx)];
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
    expected,
    got: [...got].sort(),
  };
}

function prepareCorpus(corpus) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `travsr-acc-${corpus.name}-`));
  fs.cpSync(path.join(REPO_ROOT, corpus.path), dir, { recursive: true });
  sh('git', ['init', '-q'], dir);
  sh('git', ['add', '-A'], dir);
  sh('git', ['-c', 'user.email=ci@travsr.com', '-c', 'user.name=travsr-ci', 'commit', '-qm', 'fixture'], dir);
  return dir;
}

const report = {
  generated_at: new Date().toISOString(),
  binary: BIN,
  thresholds: MANIFEST.thresholds,
  corpora: [],
};
const failures = [];

for (const corpus of MANIFEST.corpora) {
  if (!corpus.enabled) {
    report.corpora.push({ name: corpus.name, skipped: true });
    continue;
  }
  console.log(`\n=== corpus: ${corpus.name} ===`);
  const dir = prepareCorpus(corpus);

  // ── Perf section (#295-T6): cold init wall time + graph size ──────────────
  const init = travsr(['init', '--quiet'], dir);
  if (init.status !== 0) {
    failures.push(`${corpus.name}: travsr init failed (exit ${init.status}): ${init.stderr.slice(0, 500)}`);
    report.corpora.push({ name: corpus.name, init_failed: true });
    continue;
  }
  const initSeconds = init.ms / 1000;
  const status = travsr(['status'], dir);
  const counts = /nodes: (\d+) \| edges: (\d+)/.exec(status.stdout) || [null, '0', '0'];
  const perf = {
    init_seconds: Number(initSeconds.toFixed(2)),
    nodes: Number(counts[1]),
    edges: Number(counts[2]),
  };
  console.log(`init: ${perf.init_seconds}s · ${perf.nodes} nodes · ${perf.edges} edges`);
  if (initSeconds > MANIFEST.thresholds.init_seconds) {
    failures.push(`${corpus.name}: init took ${perf.init_seconds}s > ${MANIFEST.thresholds.init_seconds}s`);
  }

  const corpusReport = { name: corpus.name, perf, classes: {}, queries: [] };
  const latencies = [];

  function runCase(args) {
    let last = null;
    for (let i = 0; i < MANIFEST.iterations; i++) {
      last = travsr(args, dir);
      latencies.push(last.ms);
    }
    return last;
  }

  function record(klass, name, args, metrics, pass) {
    const entry = { class: klass, name, args: args.join(' '), pass, ...metrics };
    corpusReport.queries.push(entry);
    const cls = (corpusReport.classes[klass] ||= { pass: true, cases: 0 });
    cls.cases += 1;
    if (!pass) cls.pass = false;
    console.log(`${pass ? '✓' : '✗'} [${klass}] ${name}`);
    if (!pass) failures.push(`${corpus.name}/${name}: ${JSON.stringify(metrics)}`);
  }

  // ── Class: method recall vs source ground truth ────────────────────────────
  {
    const res = runCase(['graph', '--all', '--format', 'json', '--budget', '0']);
    let got = new Set();
    try {
      got = new Set(JSON.parse(res.stdout).nodes.map(n => n.signature));
    } catch (_) { /* scored as zero recall below */ }
    const m = setMetrics(corpus.expected_definitions, got);
    const pass = m.recall >= MANIFEST.thresholds.method_recall;
    record('method-recall', 'all definitions indexed', ['graph', '--all'], {
      recall: m.recall,
      missing: m.expected.filter(e => !got.has(e)),
      output_tokens: tokens(res.stdout),
    }, pass);
  }

  // ── Ground-truth query cases ───────────────────────────────────────────────
  for (const c of corpus.cases || []) {
    const res = runCase(c.args);
    let parsed = null;
    try { parsed = JSON.parse(res.stdout); } catch (_) { /* handled per class */ }
    const outTokens = tokens(res.stdout);

    if (c.class === 'seed-resolution') {
      const root = parsed && parsed.summary ? parsed.summary.root : null;
      record(c.class, c.name, c.args, { expected: c.expect_root, got: root, output_tokens: outTokens },
        root === c.expect_root);
    } else if (c.class === 'caller-set' || c.class === 'import-set') {
      const got = new Set(
        ((parsed && parsed.nodes) || [])
          .filter(n => n.kind === 'file' && n.depth_from_seed > 0 && n.path !== c.self_path)
          .map(n => n.path)
      );
      const m = setMetrics(c.expect_files, got);
      const tKey = c.class === 'caller-set' ? 'caller' : 'import';
      const pass =
        m.recall >= MANIFEST.thresholds[`${tKey}_recall`] &&
        m.precision >= MANIFEST.thresholds[`${tKey}_precision`];
      record(c.class, c.name, c.args,
        { recall: m.recall, precision: m.precision, expected: m.expected, got: m.got, output_tokens: outTokens },
        pass);
    } else if (c.class === 'token-budget') {
      // The capped run must be smaller than the uncapped run, and either fit
      // entirely or report its truncation in-band.
      const uncapped = travsr(c.args.map(a => (a === String(c.expect_budget_respected) ? '0' : a)), dir);
      const truncated = parsed && parsed.summary && parsed.summary.truncated_nodes > 0;
      const fits = res.stdout.length <= uncapped.stdout.length;
      record(c.class, c.name, c.args,
        { capped_tokens: outTokens, uncapped_tokens: tokens(uncapped.stdout), truncated: !!truncated },
        fits && (truncated || res.stdout.length === uncapped.stdout.length));
    } else {
      record(c.class, c.name, c.args, { error: 'unknown case class' }, false);
    }
  }

  // ── Latency over all timed invocations in this corpus ─────────────────────
  latencies.sort((a, b) => a - b);
  corpusReport.latency_ms = {
    p50: Number(percentile(latencies, 50).toFixed(1)),
    p95: Number(percentile(latencies, 95).toFixed(1)),
    samples: latencies.length,
  };
  console.log(`latency: p50 ${corpusReport.latency_ms.p50}ms · p95 ${corpusReport.latency_ms.p95}ms (${latencies.length} runs)`);
  if (corpusReport.latency_ms.p95 > MANIFEST.thresholds.latency_p95_ms) {
    failures.push(`${corpus.name}: query p95 ${corpusReport.latency_ms.p95}ms > ${MANIFEST.thresholds.latency_p95_ms}ms`);
  }

  report.corpora.push(corpusReport);
  fs.rmSync(dir, { recursive: true, force: true });
}

report.pass = failures.length === 0;
report.failures = failures;
fs.writeFileSync(OUT, JSON.stringify(report, null, 2));
console.log(`\nreport written to ${OUT}`);

if (failures.length > 0) {
  console.error(`\nFAIL — ${failures.length} regression(s):`);
  for (const f of failures) console.error(`  - ${f}`);
  process.exit(1);
}
console.log('\nPASS — all accuracy and performance gates met.');
