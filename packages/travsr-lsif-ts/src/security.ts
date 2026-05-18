/**
 * Containment checks for the TypeScript compiler shell-out (SEC-003).
 *
 * ts.createProgram honours tsconfig fields that can read files from anywhere
 * on the filesystem or execute arbitrary npm packages at index time:
 *
 *   compilerOptions.plugins  → require()s arbitrary npm modules (RCE)
 *   extends / references     → pulls config — and transitively source — from
 *                              anywhere on disk
 *   files / include globs    → can escape the project root via ../../
 *
 * Every public function throws a "SEC-003:" prefixed Error on violation so
 * that `grep SEC-003` in incident logs surfaces the exact event and tsconfig
 * path. Errors are hard failures — there is no "warn and continue" path,
 * because a warning a user ignores IS the vulnerability.
 *
 * Call order in walker.ts:
 *   1. sanitizeTsconfig(configFile.config, basePath)  — raw JSON, pre-parse
 *   2. const repoRoot = resolveRoot(basePath)          — compute once
 *   3. assertFilesContained(parsed.fileNames, repoRoot) — post-parse
 *   4. isUnderRoot(sf.fileName, repoRoot)              — belt-and-suspenders
 *                                                        filter on getSourceFiles()
 */

import * as fs from 'fs';
import * as path from 'path';

// ── Public API ────────────────────────────────────────────────────────────────

/**
 * Compute the canonical absolute repo root from the tsconfig directory.
 * Uses fs.realpathSync so symlinks are resolved — a symlink inside the repo
 * pointing outside counts as an escape. Call ONCE per walk; pass the result
 * to assertFilesContained and isUnderRoot.
 */
export function resolveRoot(basePath: string): string {
  try {
    return fs.realpathSync(path.resolve(basePath));
  } catch {
    return path.resolve(basePath);
  }
}

/**
 * Validate raw tsconfig JSON before handing it to the TS compiler.
 *
 * Rejects:
 *   - compilerOptions.plugins   (arbitrary npm module execution)
 *   - extends pointing outside basePath (filesystem escape)
 *   - references[].path pointing outside basePath (filesystem escape)
 *   - extends with http:// or https:// URLs
 *
 * Throws a SEC-003 error on the first violation; never returns a warning.
 */
export function sanitizeTsconfig(config: unknown, basePath: string): void {
  if (typeof config !== 'object' || config === null) return;
  const cfg = config as Record<string, unknown>;
  const repoRoot = resolveRoot(basePath);

  // ── 1. Reject compilerOptions.plugins ─────────────────────────────────────
  // plugins causes the TS compiler to require() arbitrary npm packages at
  // compile time. There is no safe subset — reject unconditionally.
  const opts = cfg['compilerOptions'];
  if (typeof opts === 'object' && opts !== null) {
    if ((opts as Record<string, unknown>)['plugins'] !== undefined) {
      throw new Error(
        'SEC-003: compilerOptions.plugins is not permitted. ' +
          'Plugins execute arbitrary npm modules at TS compile time. ' +
          'Remove compilerOptions.plugins from tsconfig.json.'
      );
    }
  }

  // ── 2. Reject extends that escapes the repo root ───────────────────────────
  // TS 5+ allows extends to be an array; handle both forms.
  const extendsVal = cfg['extends'];
  if (extendsVal !== undefined) {
    const entries = Array.isArray(extendsVal) ? extendsVal : [extendsVal];
    for (const ext of entries) {
      if (typeof ext !== 'string') continue;
      if (ext.startsWith('http://') || ext.startsWith('https://')) {
        throw new Error(
          `SEC-003: tsconfig.extends "${ext}" is a URL. ` +
            'Only local paths within the project root are permitted.'
        );
      }
      assertUnderRoot(path.resolve(basePath, ext), repoRoot, 'tsconfig.extends');
    }
  }

  // ── 3. Reject project references that escape the repo root ─────────────────
  const refs = cfg['references'];
  if (Array.isArray(refs)) {
    for (const ref of refs) {
      if (typeof ref !== 'object' || ref === null) continue;
      const refPath = (ref as Record<string, unknown>)['path'];
      if (typeof refPath !== 'string') continue;
      assertUnderRoot(
        path.resolve(basePath, refPath),
        repoRoot,
        'tsconfig.references[].path'
      );
    }
  }
}

/**
 * Assert that every resolved file name from ts.parseJsonConfigFileContent
 * is contained within repoRoot. Uses fs.realpathSync to follow symlinks —
 * a symlink inside the repo pointing outside is also an escape.
 *
 * Call this AFTER ts.parseJsonConfigFileContent, BEFORE ts.createProgram.
 * Catches malicious include globs (e.g. "../../**") and explicit files[]
 * entries that escape the project root.
 */
export function assertFilesContained(
  fileNames: readonly string[],
  repoRoot: string
): void {
  for (const fileName of fileNames) {
    assertUnderRoot(safeRealpath(fileName), repoRoot, 'source file');
  }
}

/**
 * Pure in-memory containment check — no I/O, safe to call in hot filter loops.
 * Does NOT follow symlinks; use assertFilesContained for the authoritative check.
 * Used as a belt-and-suspenders filter on program.getSourceFiles().
 */
export function isUnderRoot(filePath: string, repoRoot: string): boolean {
  const normalized = path.normalize(path.resolve(filePath));
  const root = path.normalize(repoRoot);
  return normalized.startsWith(root + path.sep) || normalized === root;
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/**
 * Follow symlinks as far as possible. If the full path doesn't exist yet
 * (e.g. a generated .d.ts), walk up the directory tree until we hit a real
 * path, then re-append the remaining segments. This ensures that /tmp on
 * macOS (a symlink to /private/tmp) is expanded consistently even for
 * non-existent children, so containment checks don't false-positive.
 */
function safeRealpath(filePath: string): string {
  try {
    return fs.realpathSync(filePath);
  } catch {
    const dir = path.dirname(filePath);
    if (dir === filePath) {
      return path.normalize(filePath);
    }
    return path.join(safeRealpath(dir), path.basename(filePath));
  }
}

function assertUnderRoot(resolvedPath: string, repoRoot: string, context: string): void {
  const normalized = path.normalize(resolvedPath);
  const root = path.normalize(repoRoot);
  if (!normalized.startsWith(root + path.sep) && normalized !== root) {
    throw new Error(
      `SEC-003: ${context} "${resolvedPath}" resolves outside ` +
        `the project root "${repoRoot}". ` +
        'Refusing to process — this tsconfig may be attempting to read ' +
        'files outside the project.'
    );
  }
}
