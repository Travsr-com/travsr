#!/usr/bin/env node
'use strict';

const { spawnSync } = require('child_process');
const path = require('path');
const fs = require('fs');

const binName = process.platform === 'win32' ? 'travsr.exe' : 'travsr';
const binPath = path.join(__dirname, binName);

if (!fs.existsSync(binPath)) {
  console.error(
    'travsr binary not found. Run `npm install` to download it, or set TRAVSR_BINARY.'
  );
  process.exit(1);
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
