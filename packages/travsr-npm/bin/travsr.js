#!/usr/bin/env node
'use strict';

const { spawnSync } = require('child_process');
const path = require('path');
const fs = require('fs');

const binName = process.platform === 'win32' ? 'travsr.exe' : 'travsr';
const binPath = path.join(__dirname, binName);

async function main() {
  if (!fs.existsSync(binPath)) {
    // The postinstall script may not have run (npm's allow-scripts
    // gate, --ignore-scripts, a flaky network mid-install), so download on
    // demand instead of dead-ending here.
    console.error('travsr: binary not found, downloading now...');
    try {
      const { ensureBinary } = require('../scripts/ensure-binary');
      await ensureBinary();
    } catch (err) {
      console.error(`travsr: on-demand download failed: ${err.message}`);
      console.error('Set TRAVSR_BINARY=/path/to/travsr to use a local build, or run `npm install -g travsr` again.');
      process.exit(1);
    }
  }

  const result = spawnSync(binPath, process.argv.slice(2), {
    stdio: 'inherit',
    argv0: 'travsr',
  });

  if (result.error) {
    console.error(`travsr: failed to launch binary: ${result.error.message}`);
    process.exit(1);
  }

  // Tech Lead requirement: always propagate exact exit code.
  process.exit(result.status ?? 1);
}

main();
