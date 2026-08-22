/**
 * Path containment checks for travsr-lsif-py (SEC-003 equivalent).
 *
 * The Python file discovery recursively walks the project root.  Without
 * containment checks, a symlink inside the project could point outside and
 * cause the emitter to read files from arbitrary filesystem locations.
 *
 * Every public function throws a "SEC-003:" prefixed Error on violation.
 * Errors are hard failures — there is no "warn and continue" path.
 *
 * Call order in walker.ts:
 *   1. resolveRoot(rootDir)                          — canonical root path
 *   2. assertPathsContained(pyFiles, repoRoot)        — post-discovery check
 *   3. isUnderRoot(absPath, repoRoot)                 — per-file belt-and-suspenders
 */

import * as fs from 'fs';
import * as path from 'path';

// ── Public API ─────────────────────────────────────────────────────────────────

/**
 * Compute the canonical absolute repo root.
 * Uses safeRealpath so symlinks pointing outside are fully resolved —
 * a rootDir that is itself a symlink escaping to /etc would be caught by
 * downstream assertPathsContained calls.
 */
export function resolveRoot(rootDir: string): string {
  return safeRealpath(path.resolve(rootDir));
}

/**
 * Assert that every discovered .py / .pyi file lies inside repoRoot.
 * Uses fs.realpathSync to follow symlinks — a symlink inside the project
 * pointing outside is an escape just as dangerous as a glob that escapes.
 *
 * Call this AFTER collecting pyFiles, BEFORE parsing any of them.
 */
export function assertPathsContained(filePaths: string[], repoRoot: string): void {
  for (const filePath of filePaths) {
    assertUnderRoot(safeRealpath(filePath), repoRoot, 'source file');
  }
}

/**
 * Pure in-memory containment check — no I/O, safe to call in hot loops.
 * Does NOT follow symlinks; use assertPathsContained for the authoritative check.
 * Used as a belt-and-suspenders filter before emitting each file's ranges.
 */
export function isUnderRoot(filePath: string, repoRoot: string): boolean {
  const normalized = path.normalize(path.resolve(filePath));
  const root = path.normalize(repoRoot);
  return normalized.startsWith(root + path.sep) || normalized === root;
}

// ── Internal helpers ───────────────────────────────────────────────────────────

/**
 * Follow symlinks as far as possible.  If the full path doesn't exist yet,
 * walk up the directory tree until we hit a real path, then re-append the
 * remaining segments.  This ensures that /tmp on macOS (a symlink to
 * /private/tmp) is expanded consistently even for non-existent children,
 * so containment checks don't false-positive.
 */
function safeRealpath(filePath: string): string {
  try {
    return fs.realpathSync(filePath);
  } catch {
    const dir = path.dirname(filePath);
    if (dir === filePath) return path.normalize(filePath);
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
        'Refusing to process; this path may be attempting to read ' +
        'files outside the project.'
    );
  }
}
