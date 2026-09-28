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

test('sanitizeTsconfig: rejects references[].path escaping the project root', () => {
  const config = { references: [{ path: '../../other-project' }] };
  assert.throws(
    () => sanitizeTsconfig(config, '/tmp/proj/nested'),
    (err: unknown) => {
      assert.ok(err instanceof Error);
      assert.ok(err.message.includes('SEC-003'), `missing SEC-003 prefix: ${err.message}`);
      assert.ok(
        err.message.includes('references'),
        `missing 'references' in message: ${err.message}`
      );
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
// #806: isUnderRoot resolved filePath but not repoRoot. On Windows resolve()
// prepends the current drive letter and normalize() does not, so the two sides
// compared "C:\...\foo.ts" against "\home\user\project" and every
// contained file was rejected. The platform-native test above only catches that
// when run ON Windows; these two pin both platforms' semantics from any host by
// injecting the flavour explicitly. Inputs are fully qualified — a
// drive-relative path would pick up the host's cwd and stop being
// deterministic.
//
// The drive-letter asymmetry itself is NOT reproducible off Windows, and it is
// worth being precise about why, because it is the reason the earlier version
// of these tests passed against the unfixed body. path.win32.resolve() takes
// the drive from process.cwd(), and a POSIX cwd has none, so on macOS/Linux it
// yields "\home\user\project\src\foo.ts" — the same prefix normalize() alone
// produces, which is exactly what the bug needed to differ.
//
// What does discriminate on every host is a repoRoot that resolve() changes and
// normalize() does not: a trailing separator. normalize("C:\\p\\") keeps it,
// normalize(resolve("C:\\p\\")) strips it, so the unfixed body compares against
// "C:\\p\\" + sep and rejects a contained file. That assertion is the actual
// regression guard here; the rest pin surrounding behaviour.
test('isUnderRoot: Windows semantics, verified from any host (#806)', () => {
  const root = 'C:\\home\\user\\project';
  assert.ok(isUnderRoot('C:\\home\\user\\project\\src\\foo.ts', root, path.win32));
  assert.ok(isUnderRoot('C:\\home\\user\\project', root, path.win32));
  assert.ok(
    isUnderRoot('C:/home/user/project/src/foo.ts', root, path.win32),
    'forward slashes normalize to backslashes'
  );
  assert.ok(!isUnderRoot('C:\\home\\user\\other\\foo.ts', root, path.win32));
  assert.ok(
    !isUnderRoot('C:\\home\\user\\project-evil\\foo.ts', root, path.win32),
    'prefix match is not enough'
  );
  assert.ok(
    !isUnderRoot('D:\\home\\user\\project\\src\\foo.ts', root, path.win32),
    'same path on another drive is outside'
  );
  assert.ok(
    !isUnderRoot('C:\\home\\user\\project\\src\\..\\..\\..\\etc\\passwd', root, path.win32),
    'traversal collapses before comparing'
  );
  assert.ok(
    isUnderRoot('C:\\home\\user\\project\\src\\foo.ts', root + '\\', path.win32),
    'an unresolved root (trailing separator) must still contain its files'
  );
});

test('isUnderRoot: POSIX semantics, verified from any host (#806)', () => {
  const root = '/home/user/project';
  assert.ok(isUnderRoot('/home/user/project/src/foo.ts', root, path.posix));
  assert.ok(isUnderRoot('/home/user/project', root, path.posix));
  assert.ok(!isUnderRoot('/home/user/other/foo.ts', root, path.posix));
  assert.ok(
    !isUnderRoot('/home/user/project-evil/foo.ts', root, path.posix),
    'prefix match is not enough'
  );
  assert.ok(
    !isUnderRoot('/home/user/project/../project-evil/foo.ts', root, path.posix),
    'traversal collapses before comparing'
  );
  assert.ok(
    isUnderRoot('/home/user/project/src/foo.ts', root + '/', path.posix),
    'an unresolved root (trailing separator) must still contain its files'
  );
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
