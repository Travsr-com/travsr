/**
 * Integration test — runs the LSIF emitter against the 10-file fixture and
 * validates that the output is a well-formed LSIF dump with the semantic
 * edge types required by Issue #24 [S4-1].
 *
 * Uses Node.js built-in test runner (node:test, available in Node ≥ 20).
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import path from 'node:path';

const FIXTURE_TSCONFIG = path.join(__dirname, '../../fixtures/tsconfig.json');
const EMITTER_BIN = path.join(__dirname, '../index.js');

test('emitter exits 0 and produces non-empty output for the 10-file fixture', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--project', FIXTURE_TSCONFIG], {
    encoding: 'utf-8',
  });

  assert.strictEqual(result.status, 0, `emitter crashed:\n${result.stderr}`);
  assert.ok(result.stdout.trim().length > 0, 'stdout was empty');
});

test('every LSIF line is valid JSON with id, type, and label', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--project', FIXTURE_TSCONFIG], {
    encoding: 'utf-8',
  });

  const lines = result.stdout.trim().split('\n').filter(Boolean);
  assert.ok(lines.length > 0, 'no lines emitted');

  for (const [i, line] of lines.entries()) {
    let obj: Record<string, unknown>;
    try {
      obj = JSON.parse(line) as Record<string, unknown>;
    } catch {
      assert.fail(`Line ${i + 1} is not valid JSON: ${line}`);
    }
    assert.ok(typeof obj['id'] === 'number', `Line ${i + 1} missing numeric id`);
    assert.ok(
      obj['type'] === 'vertex' || obj['type'] === 'edge',
      `Line ${i + 1} has invalid type: ${String(obj['type'])}`
    );
    assert.ok(typeof obj['label'] === 'string', `Line ${i + 1} missing label`);
  }
});

test('dump contains a metaData vertex with version 0.4.3', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--project', FIXTURE_TSCONFIG], {
    encoding: 'utf-8',
  });

  const vertices = parseVertices(result.stdout);
  const meta = vertices.find((v) => v['label'] === 'metaData');
  assert.ok(meta !== undefined, 'no metaData vertex found');
  assert.strictEqual(meta['version'], '0.4.3');
});

test('dump contains referenceResult vertices (RefCall / RefImports edges)', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--project', FIXTURE_TSCONFIG], {
    encoding: 'utf-8',
  });

  const vertices = parseVertices(result.stdout);
  const refResults = vertices.filter((v) => v['label'] === 'referenceResult');
  assert.ok(
    refResults.length > 0,
    'no referenceResult vertices — call-site and import edges are missing'
  );
});

test('dump contains implementationResult vertices (IsImplementation edges)', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--project', FIXTURE_TSCONFIG], {
    encoding: 'utf-8',
  });

  const vertices = parseVertices(result.stdout);
  const implResults = vertices.filter((v) => v['label'] === 'implementationResult');
  assert.ok(
    implResults.length > 0,
    'no implementationResult vertices — IsImplementation edges are missing'
  );
});

test('dump contains at least 10 document vertices (one per fixture file)', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--project', FIXTURE_TSCONFIG], {
    encoding: 'utf-8',
  });

  const vertices = parseVertices(result.stdout);
  const docs = vertices.filter((v) => v['label'] === 'document');
  assert.ok(docs.length >= 10, `expected ≥10 document vertices, got ${docs.length}`);
});

test('dump contains item/references edges for method overrides (Overrides)', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--project', FIXTURE_TSCONFIG], {
    encoding: 'utf-8',
  });

  // Overrides are emitted as `item` edges with property `references` that link
  // an overriding method range to the base class method's referenceResult.
  // AuthService.initialize and AuthService.getName both shadow BaseService methods.
  const edges = parseEdges(result.stdout);
  const overrideItems = edges.filter(
    (e) => e['label'] === 'item' && e['property'] === 'references'
  );
  assert.ok(
    overrideItems.length > 0,
    'no item/references edges found — override ranges may be missing'
  );
});

// ── helpers ───────────────────────────────────────────────────────────────────

function parseAll(stdout: string): Record<string, unknown>[] {
  return stdout
    .trim()
    .split('\n')
    .filter(Boolean)
    .map((l) => JSON.parse(l) as Record<string, unknown>);
}

function parseVertices(stdout: string): Record<string, unknown>[] {
  return parseAll(stdout).filter((o) => o['type'] === 'vertex');
}

function parseEdges(stdout: string): Record<string, unknown>[] {
  return parseAll(stdout).filter((o) => o['type'] === 'edge');
}
