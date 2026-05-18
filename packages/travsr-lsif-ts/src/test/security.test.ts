/**
 * Security tests for SEC-003 — TS compiler shell-out containment.
 *
 * Each test presents a malicious tsconfig to walk() and asserts that it
 * throws a SEC-003 error before reaching ts.createProgram.
 * No LSIF output must be produced for any of these inputs.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { sanitizeTsconfig, assertFilesContained, isUnderRoot, resolveRoot } from '../security';
import { Emitter } from '../emitter';
import { walk } from '../walker';
import { Writable } from 'node:stream';

const MALICIOUS_DIR = path.join(__dirname, '../../fixtures/malicious');

// ── Unit tests for security.ts ────────────────────────────────────────────────

test('sanitizeTsconfig: rejects compilerOptions.plugins', () => {
  const config = {
    compilerOptions: { plugins: [{ name: 'evil' }] },
  };
  assert.throws(
    () => sanitizeTsconfig(config, '/tmp/proj'),
    (err: unknown) => {
      assert.ok(err instanceof Error);
      assert.ok(err.message.includes('SEC-003'), `missing SEC-003 prefix: ${err.message}`);
      assert.ok(err.message.includes('plugins'), `missing 'plugins' in message: ${err.message}`);
      return true;
    }
  );
});

test('sanitizeTsconfig: rejects extends escaping the project root', () => {
  const config = { extends: '../../other-config.json' };
  assert.throws(
    () => sanitizeTsconfig(config, '/tmp/proj/nested'),
    (err: unknown) => {
      assert.ok(err instanceof Error);
      assert.ok(err.message.includes('SEC-003'), `missing SEC-003 prefix: ${err.message}`);
      assert.ok(err.message.includes('extends'), `missing 'extends' in message: ${err.message}`);
      return true;
    }
  );
});

test('sanitizeTsconfig: rejects extends with http URL', () => {
  const config = { extends: 'https://attacker.example/malicious.json' };
  assert.throws(
    () => sanitizeTsconfig(config, '/tmp/proj'),
    (err: unknown) => {
      assert.ok(err instanceof Error);
      assert.ok(err.message.includes('SEC-003'));
      assert.ok(err.message.includes('URL'));
      return true;
    }
  );
});

test('sanitizeTsconfig: accepts extends within the project root', () => {
  const config = { extends: './tsconfig.base.json' };
  // Must NOT throw — relative path stays inside /tmp/proj
  assert.doesNotThrow(() => sanitizeTsconfig(config, '/tmp/proj'));
});

test('sanitizeTsconfig: accepts config with no dangerous fields', () => {
  const config = {
    compilerOptions: { target: 'ES2020', strict: true },
    include: ['./**/*.ts'],
  };
  assert.doesNotThrow(() => sanitizeTsconfig(config, '/tmp/proj'));
});

test('assertFilesContained: throws for file outside repo root', () => {
  const repoRoot = resolveRoot('/tmp/proj');
  assert.throws(
    () => assertFilesContained(['/tmp/other-project/src/evil.ts'], repoRoot),
    (err: unknown) => {
      assert.ok(err instanceof Error);
      assert.ok(err.message.includes('SEC-003'));
      return true;
    }
  );
});

test('assertFilesContained: accepts files inside repo root', () => {
  const repoRoot = resolveRoot('/tmp');
  assert.doesNotThrow(() =>
    assertFilesContained(['/tmp/proj/src/index.ts', '/tmp/proj/src/utils.ts'], repoRoot)
  );
});

test('isUnderRoot: correctly identifies contained and escaped paths', () => {
  const root = '/home/user/project';
  assert.ok(isUnderRoot('/home/user/project/src/foo.ts', root));
  assert.ok(isUnderRoot('/home/user/project', root));
  assert.ok(!isUnderRoot('/home/user/other/foo.ts', root));
  assert.ok(!isUnderRoot('/home/user/project-evil/foo.ts', root), 'prefix match is not enough');
});

// ── Integration tests against malicious tsconfig fixtures ─────────────────────

function silentEmitter(): Emitter {
  const sink = new Writable({ write(_c, _e, cb) { cb(); } });
  return new Emitter(sink);
}

test('walk: rejects tsconfig with compilerOptions.plugins (SEC-003)', () => {
  const tsconfig = path.join(MALICIOUS_DIR, 'tsconfig-plugins.json');
  assert.throws(
    () => walk(tsconfig, silentEmitter()),
    (err: unknown) => {
      assert.ok(err instanceof Error);
      assert.ok(
        err.message.includes('SEC-003'),
        `expected SEC-003 prefix, got: ${err.message}`
      );
      assert.ok(err.message.includes('plugins'));
      return true;
    }
  );
});

test('walk: rejects tsconfig with extends pointing outside project root (SEC-003)', () => {
  const tsconfig = path.join(MALICIOUS_DIR, 'tsconfig-extends-escape.json');
  assert.throws(
    () => walk(tsconfig, silentEmitter()),
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

test('walk: rejects tsconfig with files[] referencing a path outside project root (SEC-003)', () => {
  const tsconfig = path.join(MALICIOUS_DIR, 'tsconfig-file-escape.json');
  assert.throws(
    () => walk(tsconfig, silentEmitter()),
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

test('walk: clean fixture still produces valid LSIF output (regression guard)', () => {
  const tsconfig = path.join(__dirname, '../../fixtures/tsconfig.json');
  const lines: string[] = [];
  const sink = new Writable({
    write(chunk: Buffer, _enc, cb) {
      lines.push(...chunk.toString().split('\n').filter(Boolean));
      cb();
    },
  });
  assert.doesNotThrow(() => walk(tsconfig, new Emitter(sink)));
  const hasMetaData = lines.some((l) => {
    const obj = JSON.parse(l) as Record<string, unknown>;
    return obj['label'] === 'metaData';
  });
  assert.ok(hasMetaData, 'clean fixture must still emit a metaData vertex');
});
