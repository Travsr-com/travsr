#!/usr/bin/env node
/**
 * Guard for #576 / #497: the release build matrix and the three installer
 * target maps must agree on the exact set of published Rust target triples.
 *
 * The comparable field in release.yml is `artifact:`, not `target:`. The
 * Linux entries build against musl (`target: x86_64-unknown-linux-musl`) but
 * ship a gnu-named asset (`artifact: x86_64-unknown-linux-gnu`); the package
 * step (release.yml, STAGE="travsr-${TAG}-${ARTIFACT}") and both installers
 * (installer.ts, install.js) only ever request the artifact name. Comparing
 * against `target:` would fail on a clean tree.
 *
 * This checks set equality of triples across the four sources, not pairing
 * correctness (platform/arch -> triple). Mis-pairing is covered separately
 * by packages/travsr-vscode/src/test/suite/installer.test.ts. install.sh is
 * POSIX-only, so it is compared against the release build matrix's non-Windows
 * subset rather than the full artifact set.
 *
 * Known limitation: comment stripping is a naive line-based `//`/`#` strip
 * within the located region. A `//` or `#` inside a string literal would be
 * mis-stripped. No such literal exists in any of the four sources today.
 *
 * Zero dependencies, no npm install required. Run from anywhere:
 *   node .github/scripts/check-target-maps.mjs
 */

import { fileURLToPath } from "node:url";
import { readFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = join(__dirname, "..", "..");

const RELEASE_YML = join(REPO_ROOT, ".github/workflows/release.yml");
const INSTALLER_TS = join(REPO_ROOT, "packages/travsr-vscode/src/installer.ts");
const INSTALL_JS = join(REPO_ROOT, "packages/travsr-npm/scripts/install.js");
const INSTALL_SH = join(REPO_ROOT, "install.sh");

/** Relative path for error messages, so they read the same regardless of cwd. */
function rel(absPath) {
  return absPath.slice(REPO_ROOT.length + 1);
}

/** Reads a file's contents or hard-fails with a clear message. // O(1) */
function readFileOrFail(absPath) {
  if (!existsSync(absPath)) {
    console.error(`ERROR: ${rel(absPath)} not found`);
    process.exit(1);
  }
  return readFileSync(absPath, "utf8");
}

/**
 * Extracts the release.yml build matrix's `artifact:` values.
 *
 * Region: from the `  build:` job key to the next two-space-indented job
 * key. Sub-checks that the number of `- target:` matrix entries equals the
 * number of `artifact:` lines, so a future entry that omits `artifact:`
 * fails here instead of 404ing at release time, and that no two entries
 * share the same `artifact:` value, so a duplicate fails here instead of
 * making actions/upload-artifact@v4 reject the second upload and fail the
 * release build.
 * // O(n) in file line count
 */
function extractReleaseArtifacts(text) {
  const lines = text.split("\n");
  const startIdx = lines.findIndex((l) => /^ {2}build:/.test(l));
  if (startIdx === -1) {
    console.error(
      `ERROR: could not locate the "build:" job in ${rel(RELEASE_YML)} (parser needs updating)`
    );
    process.exit(1);
  }
  let endIdx = lines.length;
  for (let i = startIdx + 1; i < lines.length; i++) {
    if (/^ {2}\S+:/.test(lines[i])) {
      endIdx = i;
      break;
    }
  }
  const regionLines = lines.slice(startIdx, endIdx);
  const stripped = regionLines.map((l) => l.replace(/#.*$/, ""));

  const targetCount = stripped.filter((l) => /^\s*-\s*target:/.test(l)).length;
  const artifacts = [];
  for (const l of stripped) {
    const m = l.match(/^\s*artifact:\s*(\S+)\s*$/);
    if (m) artifacts.push(m[1].replace(/^["']|["']$/g, ""));
  }

  if (artifacts.length === 0) {
    console.error(
      `ERROR: could not locate any "artifact:" entries in the build matrix of ${rel(RELEASE_YML)} (parser needs updating)`
    );
    process.exit(1);
  }
  if (targetCount !== artifacts.length) {
    console.error(
      `ERROR: ${rel(RELEASE_YML)} build matrix has ${targetCount} "target:" entries ` +
        `but ${artifacts.length} "artifact:" entries. Every matrix entry must set both.`
    );
    process.exit(1);
  }

  const dupes = [...new Set(artifacts.filter((a, i) => artifacts.indexOf(a) !== i))];
  if (dupes.length > 0) {
    console.error(
      `ERROR: ${rel(RELEASE_YML)} build matrix ships duplicate artifact name(s): ` +
        `${dupes.join(", ")}. Two matrix entries writing the same artifact name ` +
        `make actions/upload-artifact@v4 reject the second upload, so the release ` +
        `build fails and nothing publishes.`
    );
    process.exit(1);
  }
  return artifacts;
}

/**
 * Extracts values from a brace-delimited object literal region bounded by
 * a `const <objectName>` declaration (identifier-boundary regex match, so
 * a rename like `TARGET_MAP` -> `TARGET_MAPX` cannot satisfy it via
 * substring containment) and a line whose trimmed content is `};`. Strips
 * `//` comments before extracting `: "value"` / `: 'value'` pairs.
 * // O(n) in file line count
 */
function extractObjectValues(text, absPath, objectName) {
  const lines = text.split("\n");
  const escapedName = objectName.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const startRe = new RegExp(String.raw`\bconst ${escapedName}\b`);
  const startIdx = lines.findIndex((l) => startRe.test(l));
  if (startIdx === -1) {
    console.error(
      `ERROR: could not locate ${objectName} in ${rel(absPath)} (parser needs updating)`
    );
    process.exit(1);
  }
  let endIdx = -1;
  for (let i = startIdx + 1; i < lines.length; i++) {
    if (lines[i].trim() === "};") {
      endIdx = i;
      break;
    }
  }
  if (endIdx === -1) {
    console.error(
      `ERROR: could not locate the closing "};" for ${objectName} in ${rel(absPath)} (parser needs updating)`
    );
    process.exit(1);
  }

  const regionLines = lines.slice(startIdx, endIdx + 1);
  const stripped = regionLines.map((l) => l.replace(/\/\/.*$/, ""));
  const region = stripped.join("\n");

  const values = [];
  const valueRe = /:\s*(['"])([^'"]+)\1/g;
  let m;
  while ((m = valueRe.exec(region)) !== null) {
    values.push(m[2]);
  }

  if (values.length === 0) {
    console.error(
      `ERROR: located ${objectName} in ${rel(absPath)} but extracted zero values (parser needs updating)`
    );
    process.exit(1);
  }
  return values;
}

function extractVscodeTargets(text) {
  return extractObjectValues(text, INSTALLER_TS, "TARGET_MAP");
}

function extractNpmTargets(text) {
  return extractObjectValues(text, INSTALL_JS, "TARGETS");
}

/**
 * Extracts install.sh's POSIX target triples from the region between the
 * "# BEGIN TARGET_MAP" and "# END TARGET_MAP" marker comments. Unlike
 * extractObjectValues, this does NOT strip # comments first, since the
 * markers themselves are comments; anchoring on `target=` already excludes
 * surrounding prose.
 * // O(n) in file line count
 */
function extractShellTargets(text) {
  const lines = text.split("\n");
  const startIdx = lines.findIndex((l) => l.trim().startsWith("# BEGIN TARGET_MAP"));
  if (startIdx === -1) {
    console.error(
      `ERROR: could not locate the "# BEGIN TARGET_MAP" marker in ${rel(INSTALL_SH)} (parser needs updating)`
    );
    process.exit(1);
  }
  let endIdx = -1;
  for (let i = startIdx + 1; i < lines.length; i++) {
    if (lines[i].trim().startsWith("# END TARGET_MAP")) {
      endIdx = i;
      break;
    }
  }
  if (endIdx === -1) {
    console.error(
      `ERROR: could not locate the closing "# END TARGET_MAP" marker in ${rel(INSTALL_SH)} (parser needs updating)`
    );
    process.exit(1);
  }

  const region = lines.slice(startIdx, endIdx + 1).join("\n");
  const values = [];
  const valueRe = /\btarget=(['"])([^'"]+)\1/g;
  let m;
  while ((m = valueRe.exec(region)) !== null) {
    values.push(m[2]);
  }

  // valueRe only sees quoted values, and sh does not require the quotes. An
  // unquoted `target=x86_64-pc-windows-msvc` would contribute nothing to the
  // extracted set and so trip none of the comparisons below (a partially
  // blind parser rather than a totally broken one). Reject it outright.
  const unquoted = lines
    .slice(startIdx, endIdx + 1)
    .filter((l) => /\btarget=[^'"\s]/.test(l));
  if (unquoted.length > 0) {
    console.error(
      `ERROR: unquoted target= assignment(s) in the ${rel(INSTALL_SH)} TARGET_MAP block:\n` +
        unquoted.map((l) => `       ${l.trim()}`).join("\n") +
        `\n       Quote every value ('x86_64-unknown-linux-gnu') so this check can see it.`
    );
    process.exit(1);
  }

  if (values.length === 0) {
    console.error(
      `ERROR: located the TARGET_MAP block in ${rel(INSTALL_SH)} but extracted zero values (parser needs updating)`
    );
    process.exit(1);
  }
  return values;
}

const releaseArtifacts = extractReleaseArtifacts(readFileOrFail(RELEASE_YML));
const vscodeTargets = extractVscodeTargets(readFileOrFail(INSTALLER_TS));
const npmTargets = extractNpmTargets(readFileOrFail(INSTALL_JS));
const installShText = readFileOrFail(INSTALL_SH);
const installShTargets = extractShellTargets(installShText);

const R = new Set(releaseArtifacts);
const V = new Set(vscodeTargets);
const N = new Set(npmTargets);
const S = new Set(installShTargets);
const P = new Set([...R].filter((t) => !/windows/.test(t)));

console.log(`OK: ${rel(RELEASE_YML)} ships ${R.size} artifact(s)`);
console.log(`OK: ${rel(INSTALLER_TS)} TARGET_MAP claims ${V.size} target(s)`);
console.log(`OK: ${rel(INSTALL_JS)} TARGETS claims ${N.size} target(s)`);
console.log(`OK: install.sh TARGET_MAP claims ${S.size} POSIX target(s)`);

const setDiff = (a, b) => [...a].filter((x) => !b.has(x));

const vscodeExtra = setDiff(V, R);
const npmExtra = setDiff(N, R);
const vscodeMissing = setDiff(R, V);
const npmMissing = setDiff(R, N);
// Scanned over the whole file, not just the extracted set: a Windows triple
// added unquoted, or in a case arm outside the TARGET_MAP markers, never
// reaches S and would slip past the set comparisons below entirely.
const shellWindows = installShText
  .split("\n")
  .map((line, i) => ({ line, no: i + 1 }))
  // Match the triple's infix rather than the bare word, so prose such as an
  // error message saying "no Windows build" does not hard-fail CI while every
  // real *-windows-* triple still does.
  .filter(({ line }) => !line.trim().startsWith("#") && /-windows-/i.test(line));
const shellExtra = setDiff(S, P);
const shellMissing = setDiff(P, S);

let failed = false;

for (const v of vscodeExtra) {
  failed = true;
  console.error(
    `ERROR: ${rel(INSTALLER_TS)} TARGET_MAP claims '${v}',\n` +
      `       but ${rel(RELEASE_YML)} build matrix ships no such artifact (guaranteed 404).`
  );
}
for (const v of npmExtra) {
  failed = true;
  console.error(
    `ERROR: ${rel(INSTALL_JS)} TARGETS claims '${v}',\n` +
      `       but ${rel(RELEASE_YML)} build matrix ships no such artifact (guaranteed 404).`
  );
}
for (const v of vscodeMissing) {
  failed = true;
  console.error(
    `ERROR: ${rel(RELEASE_YML)} ships '${v}',\n` +
      `       but ${rel(INSTALLER_TS)} TARGET_MAP cannot consume it.`
  );
}
for (const v of npmMissing) {
  failed = true;
  console.error(
    `ERROR: ${rel(RELEASE_YML)} ships '${v}',\n` +
      `       but ${rel(INSTALL_JS)} TARGETS cannot consume it.`
  );
}
for (const { line, no } of shellWindows) {
  failed = true;
  console.error(
    `ERROR: ${rel(INSTALL_SH)} line ${no} mentions windows: ${line.trim()}\n` +
      `       but install.sh is POSIX-only and must never claim a Windows triple.`
  );
}
for (const v of shellExtra) {
  failed = true;
  console.error(
    `ERROR: ${rel(INSTALL_SH)} TARGET_MAP claims '${v}',\n` +
      `       but ${rel(RELEASE_YML)} build matrix ships no such artifact (guaranteed 404).`
  );
}
for (const v of shellMissing) {
  failed = true;
  console.error(
    `ERROR: ${rel(RELEASE_YML)} ships POSIX artifact '${v}',\n` +
      `       but ${rel(INSTALL_SH)} TARGET_MAP cannot consume it.`
  );
}

if (failed) {
  console.error(
    "\nFix: these four must agree on the published platform set:\n" +
      `  - ${rel(RELEASE_YML)} (build matrix, artifact:)\n` +
      `  - ${rel(INSTALLER_TS)} (TARGET_MAP)\n` +
      `  - ${rel(INSTALL_JS)} (TARGETS)\n` +
      `  - install.sh (TARGET_MAP block, POSIX targets only, no windows)`
  );
  process.exit(1);
}

console.log(`OK: all four target maps agree (${R.size} published targets, ${P.size} POSIX for install.sh)`);
process.exit(0);
