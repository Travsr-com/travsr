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
import fs from 'node:fs';
import os from 'node:os';

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

test('a top-level generator expression stays var: (issue #755)', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--project', FIXTURE_TSCONFIG], {
    encoding: 'utf-8',
  });
  // `function*` is a FunctionExpression in the TS AST but a distinct
  // `generator_function` kind in the tree-sitter grammar, so tree-sitter writes
  // `var:`. Calling it `fn:` here would name a node that was never written.
  const sigs = travsrSignatures(result.stdout, 'arrow-helpers.ts');
  assert.ok(sigs.includes('var:genShout'), `expected var:genShout in ${JSON.stringify(sigs)}`);
  assert.ok(
    !sigs.includes('fn:genShout'),
    'a generator is not an arrow or a plain function expression to tree-sitter'
  );
});

test('an ambient declare const gets no travsr_vname at all (issue #755)', () => {
  const result = spawnSync(process.execPath, [EMITTER_BIN, '--project', FIXTURE_TSCONFIG], {
    encoding: 'utf-8',
  });
  // `declare const` is wrapped in `ambient_declaration`, so no `@topvar`
  // pattern matches and tree-sitter writes no node. Any vname for it orphans
  // every reference, exactly like a local.
  const sigs = travsrSignatures(result.stdout, 'arrow-helpers.ts');
  for (const sig of sigs) {
    assert.ok(
      !sig.endsWith(':AMBIENT_LIMIT'),
      `an ambient declaration must carry no vname; got ${sig}`
    );
  }
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

// ── issue #833: --root override for a synthesized tsconfig outside the repo ──
//
// CommonJS / plain-JS repos ship no tsconfig, so the Rust side synthesizes one
// in a temp dir with `files: [<abs js paths>]` and passes `--root <repo>`. The
// emitted VName paths and the SEC-003 root must then be the repo, not the temp
// dir, or every JS edge would point at a node id that was never written.

test('--root makes VName paths repo-relative for a synthesized out-of-repo tsconfig (#833)', () => {
  // realpath the temp dirs: on macOS os.tmpdir() is a symlink (/var → /private/var)
  // and TS's getSourceFiles() returns the resolved path, which must match the
  // canonical repo root the daemon passes in production.
  const repo = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'travsr-jsrepo-')));
  const scratch = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'travsr-jscfg-')));
  try {
    fs.writeFileSync(path.join(repo, 'math.js'), 'function add(a, b) { return a + b; }\nmodule.exports = { add };\n');
    fs.writeFileSync(
      path.join(repo, 'main.js'),
      "const { add } = require('./math');\nfunction run() { return add(1, 2); }\nmodule.exports = { run };\n"
    );

    const synthTsconfig = path.join(scratch, 'tsconfig.json');
    fs.writeFileSync(
      synthTsconfig,
      JSON.stringify({
        compilerOptions: { allowJs: true, checkJs: false, noEmit: true, module: 'commonjs', moduleResolution: 'node' },
        files: [path.join(repo, 'math.js'), path.join(repo, 'main.js')],
      })
    );

    const result = spawnSync(
      process.execPath,
      [EMITTER_BIN, '--project', synthTsconfig, '--root', repo],
      { encoding: 'utf-8' }
    );
    assert.strictEqual(result.status, 0, `emitter crashed:\n${result.stderr}`);

    const vnames = parseVertices(result.stdout)
      .map((v) => v['travsr_vname'] as { path?: string; signature?: string } | undefined)
      .filter((vn): vn is { path: string; signature: string } => vn !== undefined && typeof vn.path === 'string');

    // Paths are repo-relative — never absolute and never carrying the scratch dir.
    for (const vn of vnames) {
      assert.ok(!path.isAbsolute(vn.path), `expected repo-relative path, got absolute: ${vn.path}`);
      assert.ok(!vn.path.includes('..'), `path escaped the repo root: ${vn.path}`);
      assert.ok(!vn.path.includes(path.basename(scratch)), `path leaked the scratch dir: ${vn.path}`);
    }

    const sigs = vnames.map((vn) => `${vn.path}#${vn.signature}`);
    assert.ok(sigs.includes('math.js#fn:add'), `expected math.js#fn:add in ${JSON.stringify(sigs)}`);
    assert.ok(sigs.includes('main.js#fn:run'), `expected main.js#fn:run in ${JSON.stringify(sigs)}`);
  } finally {
    fs.rmSync(repo, { recursive: true, force: true });
    fs.rmSync(scratch, { recursive: true, force: true });
  }
});

// ── issue #833 follow-up: extensionless ESM imports must resolve cross-file ──
//
// The synthesized JS tsconfig uses `moduleResolution: "bundler"` (see
// synthesize_js_tsconfig in travsr-indexer). Under `node16`, a `.js` file in a
// `"type": "module"` package is ESM, an extensionless relative import does not
// resolve, and with `checkJs` off the cross-file reference is dropped with no
// diagnostic — the ordinary Vite/webpack/Next shape silently loses its edges.
// `bundler` resolves it. This pins the behaviour the config keys buy, which a
// config-keys assertion cannot: assert a reference occurrence in main.js links
// to the shared referenceResult for `add` defined in math.js.

test('extensionless ESM import resolves cross-file under bundler resolution (#833)', () => {
  const repo = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'travsr-esmrepo-')));
  const scratch = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'travsr-esmcfg-')));
  try {
    // `"type": "module"` makes the .js files ESM — the case node16 mishandles.
    fs.writeFileSync(path.join(repo, 'package.json'), '{ "type": "module" }\n');
    fs.writeFileSync(path.join(repo, 'math.js'), 'export function add(a, b) {\n  return a + b;\n}\n');
    fs.writeFileSync(
      path.join(repo, 'main.js'),
      // extensionless relative import — no `.js` — the bundler convention.
      "import { add } from './math';\nexport function run() {\n  return add(1, 2);\n}\n"
    );

    const synthTsconfig = path.join(scratch, 'tsconfig.json');
    fs.writeFileSync(
      synthTsconfig,
      JSON.stringify({
        // Mirror synthesize_js_tsconfig exactly.
        compilerOptions: {
          allowJs: true,
          checkJs: false,
          noEmit: true,
          module: 'preserve',
          moduleResolution: 'bundler',
          target: 'es2020',
          resolveJsonModule: true,
          skipLibCheck: true,
        },
        files: [path.join(repo, 'math.js'), path.join(repo, 'main.js')],
      })
    );

    const result = spawnSync(
      process.execPath,
      [EMITTER_BIN, '--project', synthTsconfig, '--root', repo],
      { encoding: 'utf-8' }
    );
    assert.strictEqual(result.status, 0, `emitter crashed:\n${result.stderr}`);

    // Locate the main.js document vertex; item/references edges carry the
    // document they occur in, so a references item anchored in main.js is a
    // reference site there — which exists only if `./math` resolved.
    const mainDoc = parseVertices(result.stdout).find(
      (v) => v['label'] === 'document' && typeof v['uri'] === 'string' && (v['uri'] as string).endsWith('/main.js')
    );
    assert.ok(mainDoc, 'no document vertex for main.js');

    const refItemsInMain = parseEdges(result.stdout).filter(
      (e) =>
        e['label'] === 'item' &&
        e['property'] === 'references' &&
        e['document'] === mainDoc!['id']
    );
    assert.ok(
      refItemsInMain.length > 0,
      'no cross-file reference resolved from main.js — extensionless ESM import was dropped (node16 regression)'
    );
  } finally {
    fs.rmSync(repo, { recursive: true, force: true });
    fs.rmSync(scratch, { recursive: true, force: true });
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
