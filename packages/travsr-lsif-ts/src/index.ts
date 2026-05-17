#!/usr/bin/env node
/**
 * travsr-lsif-ts — CLI entry point
 *
 * Usage: travsr-lsif-ts --project <path-to-tsconfig.json>
 *
 * Emits an LSIF dump (JSON Lines) to stdout. Pipe to a file:
 *   travsr-lsif-ts --project ./tsconfig.json > dump.lsif
 */

import path from 'path';
import { Emitter } from './emitter';
import { walk } from './walker';

function main(): void {
  const args = process.argv.slice(2);
  const projIdx = args.indexOf('--project');

  if (projIdx === -1 || args[projIdx + 1] === undefined) {
    process.stderr.write('Usage: travsr-lsif-ts --project <path-to-tsconfig.json>\n');
    process.exit(1);
  }

  const tsconfigPath = path.resolve(args[projIdx + 1]!);
  const emitter = new Emitter(process.stdout);

  try {
    walk(tsconfigPath, emitter);
  } catch (err) {
    process.stderr.write(`travsr-lsif-ts: ${(err as Error).message}\n`);
    process.exit(1);
  }
}

main();
