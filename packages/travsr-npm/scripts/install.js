#!/usr/bin/env node
'use strict';

const https = require('https');
const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const { execFileSync } = require('child_process');

const pkg = require('../package.json');
const VERSION = pkg.version;
const BIN_DIR = path.join(__dirname, '..', 'bin');

const TARGETS = {
  'linux-x64':   'x86_64-unknown-linux-gnu',
  'linux-arm64': 'aarch64-unknown-linux-gnu',
  'darwin-x64':  'x86_64-apple-darwin',
  'darwin-arm64':'aarch64-apple-darwin',
  'win32-x64':   'x86_64-pc-windows-msvc',
};

function detect() {
  // Honor explicit override — useful in corporate environments or CI.
  if (process.env.TRAVSR_BINARY) {
    return { override: process.env.TRAVSR_BINARY };
  }
  const key = `${process.platform}-${process.arch}`;
  const target = TARGETS[key];
  if (!target) {
    console.error(
      `\nTravsr does not yet ship a prebuilt binary for ${key}.\n` +
      `Build from source: https://github.com/raj-rkv/travsr\n`
    );
    process.exit(1);
  }
  return { target };
}

function fetch(url) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    const req = https.get(url, { headers: { 'User-Agent': 'travsr-installer' } }, res => {
      if (res.statusCode === 301 || res.statusCode === 302) {
        resolve(fetch(res.headers.location));
        return;
      }
      if (res.statusCode !== 200) {
        reject(new Error(`HTTP ${res.statusCode} fetching ${url}`));
        return;
      }
      res.on('data', c => chunks.push(c));
      res.on('end', () => resolve(Buffer.concat(chunks)));
    });
    req.on('error', reject);
  });
}

async function install() {
  const { target, override } = detect();

  fs.mkdirSync(BIN_DIR, { recursive: true });
  const destBin = path.join(BIN_DIR, process.platform === 'win32' ? 'travsr.exe' : 'travsr');

  if (override) {
    fs.copyFileSync(override, destBin);
    if (process.platform !== 'win32') fs.chmodSync(destBin, 0o755);
    console.log(`travsr: using binary from TRAVSR_BINARY=${override}`);
    return;
  }

  const base = `https://github.com/raj-rkv/travsr/releases/download/v${VERSION}`;
  const tarName = `travsr-v${VERSION}-${target}.tar.gz`;
  const tarUrl = `${base}/${tarName}`;
  const sumsUrl = `${base}/SHA256SUMS`;

  console.log(`travsr: downloading ${tarName}...`);
  const [tarball, sumsRaw] = await Promise.all([fetch(tarUrl), fetch(sumsUrl)]);

  // Verify SHA256
  const expected = sumsRaw
    .toString('utf8')
    .split('\n')
    .map(l => l.trim())
    .find(l => l.endsWith(tarName));

  if (!expected) {
    console.error(`travsr: SHA256SUMS entry not found for ${tarName}`);
    process.exit(1);
  }
  const expectedHash = expected.split(/\s+/)[0];
  const actualHash = crypto.createHash('sha256').update(tarball).digest('hex');
  if (actualHash !== expectedHash) {
    console.error(`travsr: SHA256 mismatch!\n  expected: ${expectedHash}\n  actual:   ${actualHash}`);
    process.exit(1);
  }

  // Extract binary from tarball.
  // On Windows the archive contains travsr.exe; on Unix it contains travsr.
  // tar ships with macOS, Linux, and Windows 10+ (build 17063+).
  const binName = process.platform === 'win32' ? 'travsr.exe' : 'travsr';
  const tmpTar = path.join(BIN_DIR, tarName);
  fs.writeFileSync(tmpTar, tarball);
  execFileSync('tar', ['-xzf', tmpTar, '-C', BIN_DIR, binName], { stdio: 'inherit' });
  fs.unlinkSync(tmpTar);

  if (process.platform !== 'win32') fs.chmodSync(destBin, 0o755);
  console.log(`travsr: installed to ${destBin}`);
}

install().catch(err => {
  // Non-fatal: if the binary download fails (e.g. release not yet published),
  // warn and exit 0 so npm install completes.  The bin/travsr.js wrapper will
  // show a clear error on first use.
  console.warn(
    `\nTravsr: binary download failed — ${err.message}\n` +
    `The binary was not installed. Once v${VERSION} release artifacts are ` +
    `available at https://github.com/raj-rkv/travsr/releases you can re-run:\n` +
    `  npm install -g travsr\n` +
    `Or set TRAVSR_BINARY=/path/to/travsr to use a local build.\n`
  );
});
