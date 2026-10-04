#!/usr/bin/env node
/**
 * travsr-lsif-ts — CLI entry point
 *
 * Usage: travsr-lsif-ts --project <path-to-tsconfig.json> [--root <repo-root>]
 *
 * Emits an LSIF dump (JSON Lines) to stdout. Pipe to a file:
 *   travsr-lsif-ts --project ./tsconfig.json > dump.lsif
 *
 * `--root` overrides the directory that emitted VName paths and the SEC-003
 * containment root are computed against. It is passed when the tsconfig is a
 * synthesized ephemeral file living outside the repo (JavaScript coverage,
 * #833) so the repo-relative paths still match the tree-sitter node ids.
 * Without it the tsconfig's own directory is used, exactly as before.
 */

import path from 'path';
import { Emitter } from './emitter';
import { walk } from './walker';

function main(): void {
  const args = process.argv.slice(2);
  const projIdx = args.indexOf('--project');

  if (projIdx === -1 || args[projIdx + 1] === undefined) {
    process.stderr.write(
      'Usage: travsr-lsif-ts --project <path-to-tsconfig.json> [--root <repo-root>]\n'
    );
    process.exit(1);
  }

  const tsconfigPath = path.resolve(args[projIdx + 1]!);

  const rootIdx = args.indexOf('--root');
  const rootDir =
    rootIdx !== -1 && args[rootIdx + 1] !== undefined ? path.resolve(args[rootIdx + 1]!) : undefined;

  const emitter = new Emitter(process.stdout);

  try {
    walk(tsconfigPath, emitter, rootDir);
  } catch (err) {
    process.stderr.write(`travsr-lsif-ts: ${(err as Error).message}\n`);
    process.exit(1);
  }
}

main();
