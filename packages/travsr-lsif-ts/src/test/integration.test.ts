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

// ── issue #755 item 1: travsr_vname must agree with tree-sitter ──────────────
//
// The Rust ingester builds ref/call edges to the NodeId hashed from these
// vnames, and tree-sitter owns the nodes. Any signature computed differently
// from tree-sitter's classification is an edge to a node that was never
// written — an orphan a fresh `travsr init --semantic` fails fsck on.

test('a top-level arrow const gets fn:, matching tree-sitter (issue #755)', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--project', FIXTURE_TSCONFIG], {
    encoding: 'utf-8',
  });
  const sigs = travsrSignatures(result.stdout, 'arrow-helpers.ts');
  assert.ok(sigs.includes('fn:shout'), `expected fn:shout in ${JSON.stringify(sigs)}`);
  assert.ok(
    !sigs.includes('var:shout'),
    'var:shout is the orphan-producing vname tree-sitter never indexes'
  );
});

test('a top-level function-expression const gets fn: (issue #755)', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--project', FIXTURE_TSCONFIG], {
    encoding: 'utf-8',
  });
  const sigs = travsrSignatures(result.stdout, 'arrow-helpers.ts');
  assert.ok(sigs.includes('fn:legacyShout'), `expected fn:legacyShout in ${JSON.stringify(sigs)}`);
  assert.ok(!sigs.includes('var:legacyShout'));
});

test('a top-level plain const keeps var: (issue #755)', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--project', FIXTURE_TSCONFIG], {
    encoding: 'utf-8',
  });
  const sigs = travsrSignatures(result.stdout, 'arrow-helpers.ts');
  assert.ok(sigs.includes('var:MAX_VOLUME'), `expected var:MAX_VOLUME in ${JSON.stringify(sigs)}`);
  assert.ok(
    !sigs.includes('fn:MAX_VOLUME'),
    'a non-function const must stay a variable, or a real var: reference orphans instead'
  );
});

test('a local variable gets no travsr_vname at all (issue #755)', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--project', FIXTURE_TSCONFIG], {
    encoding: 'utf-8',
  });
  // Tree-sitter drops non-top-level declarators entirely (typescript.rs N4a),
  // so any vname for a local guarantees an orphan for every reference to it.
  // The resultSet must be emitted without a vname — opaque, not misnamed.
  const sigs = travsrSignatures(result.stdout, 'arrow-helpers.ts');
  for (const sig of sigs) {
    assert.ok(
      !sig.endsWith(':localEcho'),
      `local declarator must carry no vname; got ${sig}`
    );
  }
});

// ── helpers ───────────────────────────────────────────────────────────────────

/** All travsr_vname signatures in the dump whose path ends with `fileSuffix`. */
function travsrSignatures(stdout: string, fileSuffix: string): string[] {
  return parseVertices(stdout)
    .map((v) => v['travsr_vname'] as { path?: string; signature?: string } | undefined)
    .filter(
      (vn): vn is { path: string; signature: string } =>
        vn !== undefined &&
        typeof vn.path === 'string' &&
        typeof vn.signature === 'string' &&
        vn.path.endsWith(fileSuffix)
    )
    .map((vn) => vn.signature);
}

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
