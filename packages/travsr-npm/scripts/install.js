#!/usr/bin/env node
'use strict';

const { ensureBinary } = require('./ensure-binary');

ensureBinary().catch(err => {
  // Non-fatal: if the binary download fails (e.g. release not yet published,
  // or npm skipped this postinstall script), warn and exit 0 so npm install
  // completes. bin/travsr.js downloads on demand on first invocation if the
  // binary is still missing then.
  console.warn(
    `\nTravsr: binary download failed: ${err.message}\n` +
    `The binary was not installed now, but will be downloaded automatically ` +
    `the first time you run \`travsr\`. To install it now instead, re-run:\n` +
    `  npm install -g travsr\n` +
    `Or set TRAVSR_BINARY=/path/to/travsr to use a local build.\n`
  );
});
