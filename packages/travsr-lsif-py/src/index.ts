#!/usr/bin/env node
/**
 * travsr-lsif-py — CLI entry point
 *
 * Usage: travsr-lsif-py --root <project-root>
 *
 * Emits an LSIF dump (JSON Lines) to stdout covering every .py / .pyi file
 * found recursively under <project-root>.  Pipe to a file:
 *   travsr-lsif-py --root ./src > dump.lsif
 */

import path from 'path';
import { Emitter } from './emitter';
import { walk } from './walker';

function main(): void {
  const args = process.argv.slice(2);
  const rootIdx = args.indexOf('--root');

  if (rootIdx === -1 || args[rootIdx + 1] === undefined) {
    process.stderr.write('Usage: travsr-lsif-py --root <project-root>\n');
    process.exit(1);
  }

  const projectRoot = path.resolve(args[rootIdx + 1]!);
  const emitter = new Emitter(process.stdout);

  try {
    walk(projectRoot, emitter);
  } catch (err) {
    process.stderr.write(`travsr-lsif-py: ${(err as Error).message}\n`);
    process.exit(1);
  }
}

main();
