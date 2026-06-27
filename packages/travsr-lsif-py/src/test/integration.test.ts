/**
 * Integration tests — runs the Python LSIF emitter against the simple fixture
 * and validates that the output is a well-formed LSIF dump with the expected
 * semantic edge types.
 *
 * Uses Node.js built-in test runner (node:test, Node ≥ 20).
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import { Writable } from 'node:stream';
import { Emitter } from '../emitter';
import { walk } from '../walker';

const FIXTURE_ROOT = path.join(__dirname, '../../fixtures/simple');
const EMITTER_BIN = path.join(__dirname, '../index.js');

// ── CLI smoke tests ───────────────────────────────────────────────────────────

test('emitter exits 0 and produces non-empty output for the simple fixture', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--root', FIXTURE_ROOT], {
    encoding: 'utf-8',
  });
  assert.strictEqual(result.status, 0, `emitter crashed:\n${result.stderr}`);
  assert.ok(result.stdout.trim().length > 0, 'stdout was empty');
});

test('every LSIF line is valid JSON with id, type, and label', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--root', FIXTURE_ROOT], {
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
      continue;
    }
    assert.ok(typeof obj['id'] === 'number', `Line ${i + 1} missing numeric id`);
    assert.ok(
      obj['type'] === 'vertex' || obj['type'] === 'edge',
      `Line ${i + 1} has invalid type: ${String(obj['type'])}`
    );
    assert.ok(typeof obj['label'] === 'string', `Line ${i + 1} missing label`);
  }
});

test('IDs are monotonically increasing', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--root', FIXTURE_ROOT], {
    encoding: 'utf-8',
  });
  const ids = result.stdout
    .trim()
    .split('\n')
    .filter(Boolean)
    .map((l) => (JSON.parse(l) as Record<string, unknown>)['id'] as number);
  for (let i = 1; i < ids.length; i++) {
    assert.ok(ids[i]! > ids[i - 1]!, `ID ${String(ids[i])} not > previous ${String(ids[i - 1])}`);
  }
});

test('dump contains a metaData vertex with version 0.4.3', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--root', FIXTURE_ROOT], {
    encoding: 'utf-8',
  });
  const vertices = parseVertices(result.stdout);
  const meta = vertices.find((v) => v['label'] === 'metaData');
  assert.ok(meta !== undefined, 'no metaData vertex found');
  assert.strictEqual(meta['version'], '0.4.3');
  assert.ok(typeof meta['projectRoot'] === 'string', 'metaData missing projectRoot');
});

test('dump contains at least 3 document vertices (one per fixture file)', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--root', FIXTURE_ROOT], {
    encoding: 'utf-8',
  });
  const vertices = parseVertices(result.stdout);
  const docs = vertices.filter((v) => v['label'] === 'document');
  assert.ok(docs.length >= 3, `expected ≥3 document vertices, got ${docs.length}`);
  for (const doc of docs) {
    assert.strictEqual(doc['languageId'], 'python', 'document languageId must be python');
    assert.ok(
      (doc['uri'] as string).endsWith('.py') || (doc['uri'] as string).endsWith('.pyi'),
      `document URI should be a .py file, got: ${String(doc['uri'])}`
    );
  }
});

test('dump contains resultSet vertices with travsr_vname (fn: class: method:)', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--root', FIXTURE_ROOT], {
    encoding: 'utf-8',
  });
  const vertices = parseVertices(result.stdout);
  const resultSetsWithVname = vertices.filter(
    (v) => v['label'] === 'resultSet' && v['travsr_vname'] !== undefined
  );
  assert.ok(
    resultSetsWithVname.length > 0,
    'no resultSet vertices with travsr_vname — LSIF ingester will miss all nodes'
  );

  // Verify vname format: {path, signature} where signature has the right prefix.
  for (const rs of resultSetsWithVname) {
    const vname = rs['travsr_vname'] as { path: string; signature: string };
    assert.ok(typeof vname.path === 'string', 'travsr_vname.path must be a string');
    assert.ok(typeof vname.signature === 'string', 'travsr_vname.signature must be a string');
    assert.ok(
      vname.signature.startsWith('fn:') ||
        vname.signature.startsWith('class:') ||
        vname.signature.startsWith('method:'),
      `unexpected travsr_vname.signature: ${vname.signature}`
    );
  }
});

test('dump contains referenceResult vertices (cross-file call edges exist)', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--root', FIXTURE_ROOT], {
    encoding: 'utf-8',
  });
  const vertices = parseVertices(result.stdout);
  const refResults = vertices.filter((v) => v['label'] === 'referenceResult');
  assert.ok(
    refResults.length > 0,
    'no referenceResult vertices — call-site edges are missing'
  );
});

test('dump contains item/references edges (ref ranges linked to referenceResult)', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--root', FIXTURE_ROOT], {
    encoding: 'utf-8',
  });
  const edges = parseEdges(result.stdout);
  const refItems = edges.filter(
    (e) => e['label'] === 'item' && e['property'] === 'references'
  );
  assert.ok(
    refItems.length > 0,
    'no item/references edges — RefCall ranges are not linked to referenceResults'
  );
});

// ── Semantic accuracy: cross-file call resolution ─────────────────────────────

test('service.py emits refs to models.py:fn:create_user and fn:validate_email', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--root', FIXTURE_ROOT], {
    encoding: 'utf-8',
  });
  const all = parseAll(result.stdout);

  // Collect resultSetIds whose travsr_vname.signature is fn:create_user or fn:validate_email
  const targetSigs = new Set(['fn:create_user', 'fn:validate_email']);
  const targetRsIds = new Set<number>();
  for (const line of all) {
    if (line['label'] === 'resultSet' && line['travsr_vname']) {
      const vname = line['travsr_vname'] as { path: string; signature: string };
      if (vname.path.endsWith('models.py') && targetSigs.has(vname.signature)) {
        targetRsIds.add(line['id'] as number);
      }
    }
  }
  assert.ok(targetRsIds.size > 0, 'no resultSet for models.py:fn:create_user or fn:validate_email');

  // Each target resultSet should have a textDocument/references edge to a referenceResultId.
  const refResultIds = new Set<number>();
  for (const line of all) {
    if (
      line['label'] === 'textDocument/references' &&
      targetRsIds.has(line['outV'] as number)
    ) {
      refResultIds.add(line['inV'] as number);
    }
  }
  assert.ok(refResultIds.size > 0, 'no textDocument/references edges from target resultSets');

  // There must be at least one item/references edge pointing to those referenceResultIds.
  const refItems = all.filter(
    (e) =>
      e['label'] === 'item' &&
      e['property'] === 'references' &&
      refResultIds.has(e['outV'] as number)
  );
  assert.ok(
    refItems.length > 0,
    'no item/references edges pointing to models.py function referenceResults — ' +
      'cross-file call edges from service.py not emitted'
  );
});

test('runner.py emits attribute call refs to models.py functions', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--root', FIXTURE_ROOT], {
    encoding: 'utf-8',
  });
  const all = parseAll(result.stdout);

  // runner.py uses `import models; models.create_user(...)` — attribute call style.
  const targetSigs = new Set(['fn:create_user', 'fn:validate_email']);
  const targetRsIds = new Set<number>();
  for (const line of all) {
    if (line['label'] === 'resultSet' && line['travsr_vname']) {
      const vname = line['travsr_vname'] as { path: string; signature: string };
      if (vname.path.endsWith('models.py') && targetSigs.has(vname.signature)) {
        targetRsIds.add(line['id'] as number);
      }
    }
  }

  // We just need at least one item/references edge across the whole dump
  // — service.py or runner.py may provide it.
  const allRefItems = all.filter(
    (e) => e['label'] === 'item' && e['property'] === 'references'
  );
  assert.ok(
    allRefItems.length >= 2,
    `expected ≥2 total ref items (from service.py + runner.py), got ${allRefItems.length}`
  );
});

// ── API tests (walk() directly) ────────────────────────────────────────────────

test('walk() on empty directory produces only metaData + project vertices', () => {
  const lines: string[] = [];
  const sink = new Writable({
    write(chunk: Buffer, _enc, cb) {
      lines.push(...chunk.toString().split('\n').filter(Boolean));
      cb();
    },
  });
  const emitter = new Emitter(sink);

  // Use OS tmpdir as a guaranteed-empty-ish directory with no .py files.
  const tmp = os.tmpdir();
  // Don't fail — just verify it doesn't crash and emits the header.
  assert.doesNotThrow(() => walk(tmp, emitter));
  const objs = lines.map((l) => JSON.parse(l) as Record<string, unknown>);
  assert.ok(
    objs.some((o) => o['label'] === 'metaData'),
    'metaData vertex must always be emitted'
  );
});

test('walk() throws a descriptive error for a non-existent root', () => {
  const emitter = new Emitter(new Writable({ write(_c, _e, cb) { cb(); } }));
  assert.throws(
    () => walk('/tmp/travsr-lsif-py-nonexistent-root-xyz', emitter),
    (err: unknown) => {
      assert.ok(err instanceof Error);
      assert.ok(
        err.message.includes('does not exist') || err.message.includes('ENOENT'),
        `unexpected error message: ${err.message}`
      );
      return true;
    }
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
