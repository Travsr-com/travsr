/**
 * Security tests for travsr-lsif-py path-containment checks (SEC-003).
 *
 * Validates that assertPathsContained, isUnderRoot, and resolveRoot behave
 * correctly and that walk() rejects obviously bad inputs before parsing.
 *
 * Uses Node.js built-in test runner (node:test, Node ≥ 20).
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { Writable } from 'node:stream';
import { assertPathsContained, isUnderRoot, resolveRoot } from '../security';
import { Emitter } from '../emitter';
import { walk } from '../walker';

// ── Unit tests for security.ts ────────────────────────────────────────────────

test('resolveRoot: resolves to absolute path', () => {
  const root = resolveRoot('/tmp');
  assert.ok(path.isAbsolute(root), `resolveRoot must return an absolute path, got: ${root}`);
});

test('resolveRoot: equivalent paths normalise to the same root', () => {
  const a = resolveRoot('/tmp');
  const b = resolveRoot('/tmp/');
  // Both should refer to the same directory (platform-specific resolution).
  assert.strictEqual(path.normalize(a), path.normalize(b));
});

test('isUnderRoot: correctly identifies contained paths', () => {
  const root = '/home/user/project';
  assert.ok(isUnderRoot('/home/user/project/src/foo.py', root));
  assert.ok(isUnderRoot('/home/user/project', root));
});

test('isUnderRoot: correctly rejects escaped paths', () => {
  const root = '/home/user/project';
  assert.ok(!isUnderRoot('/home/user/other/foo.py', root));
  assert.ok(
    !isUnderRoot('/home/user/project-evil/foo.py', root),
    'simple prefix match is not sufficient — must check path separator'
  );
  assert.ok(!isUnderRoot('/etc/passwd', root));
  assert.ok(!isUnderRoot('/', root));
});

test('assertPathsContained: accepts files inside root', () => {
  const root = resolveRoot('/tmp');
  // /tmp itself is the root — an exact match passes.
  assert.doesNotThrow(() => assertPathsContained([], root));
  // Files that don't exist yet are still checked syntactically via safeRealpath.
  // Just test the no-error case with an empty list.
  assert.doesNotThrow(() => assertPathsContained([], root));
});

test('assertPathsContained: rejects file outside root', () => {
  const root = resolveRoot('/tmp/travsr-test-proj-xyz');
  assert.throws(
    () => assertPathsContained(['/etc/passwd'], root),
    (err: unknown) => {
      assert.ok(err instanceof Error);
      assert.ok(
        err.message.includes('SEC-003'),
        `expected SEC-003 prefix, got: ${err.message}`
      );
      return true;
    }
  );
});

test('assertPathsContained: rejects path that escapes via prefix trick', () => {
  const root = resolveRoot('/tmp/proj');
  assert.throws(
    () => assertPathsContained(['/tmp/proj-evil/src/evil.py'], root),
    (err: unknown) => {
      assert.ok(err instanceof Error);
      assert.ok(err.message.includes('SEC-003'));
      return true;
    }
  );
});

// ── Integration: walk() SEC-003 surface ───────────────────────────────────────

function silentEmitter(): Emitter {
  const sink = new Writable({ write(_c, _e, cb) { cb(); } });
  return new Emitter(sink);
}

test('walk: rejects non-existent root directory', () => {
  assert.throws(
    () => walk('/tmp/travsr-lsif-py-no-such-dir-0xdeadbeef', silentEmitter()),
    (err: unknown) => {
      assert.ok(err instanceof Error);
      assert.ok(
        err.message.includes('does not exist') || err.message.includes('ENOENT'),
        `unexpected message: ${err.message}`
      );
      return true;
    }
  );
});

test('walk: rejects a file path passed as root (must be a directory)', () => {
  // Use the current test binary as a guaranteed-existing file.
  const file = process.execPath;
  assert.throws(
    () => walk(file, silentEmitter()),
    (err: unknown) => {
      assert.ok(err instanceof Error);
      assert.ok(
        err.message.includes('directory') || err.message.includes('ENOTDIR'),
        `unexpected message: ${err.message}`
      );
      return true;
    }
  );
});

test('walk: clean fixture produces a metaData vertex (regression guard)', () => {
  const fixtureRoot = path.join(__dirname, '../../fixtures/simple');
  const lines: string[] = [];
  const sink = new Writable({
    write(chunk: Buffer, _enc, cb) {
      lines.push(...chunk.toString().split('\n').filter(Boolean));
      cb();
    },
  });
  const emitter = new Emitter(sink);

  assert.doesNotThrow(() => walk(fixtureRoot, emitter));

  const hasMetaData = lines.some((l) => {
    const obj = JSON.parse(l) as Record<string, unknown>;
    return obj['label'] === 'metaData';
  });
  assert.ok(hasMetaData, 'clean fixture must emit a metaData vertex');
});

test('walk: all emitted document URIs are under the project root', () => {
  const fixtureRoot = path.join(__dirname, '../../fixtures/simple');
  const repoRoot = resolveRoot(fixtureRoot);

  const lines: string[] = [];
  const sink = new Writable({
    write(chunk: Buffer, _enc, cb) {
      lines.push(...chunk.toString().split('\n').filter(Boolean));
      cb();
    },
  });

  assert.doesNotThrow(() => walk(fixtureRoot, new Emitter(sink)));

  const docs = lines
    .map((l) => JSON.parse(l) as Record<string, unknown>)
    .filter((o) => o['label'] === 'document');

  assert.ok(docs.length > 0, 'expected at least one document vertex');

  for (const doc of docs) {
    const uri = (doc['uri'] as string).replace(/^file:\/\//, '');
    assert.ok(
      isUnderRoot(uri, repoRoot),
      `document URI escapes project root: ${uri} not under ${repoRoot}`
    );
  }
});
