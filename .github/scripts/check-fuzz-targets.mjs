#!/usr/bin/env node
/**
 * Guard for #345 / ADR-017 Rule 4.3: every in-process Tree-sitter grammar must
 * have a cargo-fuzz target that actually RUNS in the nightly fuzz workflow.
 *
 * Four sources describe the same set and nothing tied them together, so the
 * language targets added during the v0.6.x multi-language work sat on disk for
 * ten releases while the nightly matrix ran six of them:
 *
 *   1. fuzz/fuzz_targets/*.rs                        the target sources
 *   2. fuzz/Cargo.toml [[bin]]                       what cargo-fuzz can build
 *   3. .github/workflows/fuzz.yml matrix.target      what CI actually executes
 *   4. registry.rs FUZZ_TARGETS                      what ADR-017 Rule 4 demands
 *
 * 1 and 2 must match exactly (a source with no [[bin]] is invisible to
 * `cargo fuzz list`; a [[bin]] with no source fails the build). 3 must match
 * them exactly (a matrix entry with no target fails the nightly run; a target
 * with no matrix entry is the bug this guard exists to prevent). 4 is checked
 * one-directionally: every grammar registered in-process must name a target
 * that exists in 1, and the equalities carry it the rest of the way to 3.
 * Targets that are not language grammars (fuzz_mcp_parser, fuzz_lsif_ingest,
 * fuzz_pcst_session, fuzz_markdown_chunker) are legitimately absent from 4.
 *
 * Zero dependencies, no npm install required. Run from anywhere:
 *   node .github/scripts/check-fuzz-targets.mjs
 */

import { fileURLToPath } from "node:url";
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = join(__dirname, "..", "..");

const TARGETS_DIR = join(REPO_ROOT, "fuzz/fuzz_targets");
const FUZZ_CARGO_TOML = join(REPO_ROOT, "fuzz/Cargo.toml");
const FUZZ_YML = join(REPO_ROOT, ".github/workflows/fuzz.yml");
const REGISTRY_RS = join(REPO_ROOT, "crates/travsr-plugin-host/src/registry.rs");

/** Relative path for error messages, so they read the same regardless of cwd. */
function rel(absPath) {
  return absPath.slice(REPO_ROOT.length + 1);
}

function readFileOrFail(absPath) {
  if (!existsSync(absPath)) {
    console.error(`ERROR: ${rel(absPath)} not found`);
    process.exit(1);
  }
  return readFileSync(absPath, "utf8");
}

function failEmpty(values, what, absPath) {
  if (values.length === 0) {
    console.error(
      `ERROR: extracted zero ${what} from ${rel(absPath)} (parser needs updating)`
    );
    process.exit(1);
  }
  return values;
}

/** Target names from the source files on disk. // O(n) in directory entries */
function extractTargetSources() {
  if (!existsSync(TARGETS_DIR)) {
    console.error(`ERROR: ${rel(TARGETS_DIR)} not found`);
    process.exit(1);
  }
  const names = readdirSync(TARGETS_DIR)
    .filter((f) => f.endsWith(".rs"))
    .map((f) => f.slice(0, -".rs".length));
  return failEmpty(names, "fuzz target sources", TARGETS_DIR);
}

/**
 * `name =` values of every `[[bin]]` table in fuzz/Cargo.toml, plus a check
 * that each one's `path` points at the source file that shares its name. A
 * mismatched path builds the wrong grammar under the right target name, which
 * no set comparison below would notice.
 * // O(n) in file line count
 */
function extractCargoBins(text) {
  const lines = text.split("\n");
  const bins = [];
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].trim() !== "[[bin]]") continue;
    let name = null;
    let path = null;
    for (let j = i + 1; j < lines.length && !/^\s*\[/.test(lines[j]); j++) {
      const nameMatch = lines[j].match(/^\s*name\s*=\s*"([^"]+)"/);
      if (nameMatch) name = nameMatch[1];
      const pathMatch = lines[j].match(/^\s*path\s*=\s*"([^"]+)"/);
      if (pathMatch) path = pathMatch[1];
    }
    if (name === null) {
      console.error(
        `ERROR: ${rel(FUZZ_CARGO_TOML)} line ${i + 1}: [[bin]] table has no name`
      );
      process.exit(1);
    }
    const expectedPath = `fuzz_targets/${name}.rs`;
    if (path !== expectedPath) {
      console.error(
        `ERROR: ${rel(FUZZ_CARGO_TOML)} bin '${name}' has path '${path}',\n` +
          `       expected '${expectedPath}'. A target must build the source that shares its name.`
      );
      process.exit(1);
    }
    bins.push(name);
  }
  return failEmpty(bins, "[[bin]] names", FUZZ_CARGO_TOML);
}

/**
 * The `matrix.target:` list in fuzz.yml. Region runs from the `target:` key to
 * the first line that is neither a list item nor a comment nor blank, so the
 * rationale comments interleaved in the list do not end it early.
 * // O(n) in file line count
 */
function extractMatrixTargets(text) {
  const lines = text.split("\n");
  const startIdx = lines.findIndex((l) => /^\s+target:\s*$/.test(l));
  if (startIdx === -1) {
    console.error(
      `ERROR: could not locate the "target:" matrix key in ${rel(FUZZ_YML)} (parser needs updating)`
    );
    process.exit(1);
  }
  const targets = [];
  for (let i = startIdx + 1; i < lines.length; i++) {
    const line = lines[i];
    if (line.trim() === "" || /^\s*#/.test(line)) continue;
    const m = line.match(/^\s*-\s*(\S+)\s*$/);
    if (!m) break;
    targets.push(m[1]);
  }
  return failEmpty(targets, "matrix targets", FUZZ_YML);
}

/**
 * The `(language, filename)` pairs of registry.rs's FUZZ_TARGETS table, which
 * is the list of grammars parsed in the daemon's own address space.
 * // O(n) in file line count
 */
function extractRegistryTargets(text) {
  const lines = text.split("\n");
  const startIdx = lines.findIndex((l) => /\bconst FUZZ_TARGETS\b/.test(l));
  if (startIdx === -1) {
    console.error(
      `ERROR: could not locate FUZZ_TARGETS in ${rel(REGISTRY_RS)} (parser needs updating)`
    );
    process.exit(1);
  }
  let endIdx = -1;
  for (let i = startIdx + 1; i < lines.length; i++) {
    if (lines[i].trim() === "];") {
      endIdx = i;
      break;
    }
  }
  if (endIdx === -1) {
    console.error(
      `ERROR: could not locate the closing "];" for FUZZ_TARGETS in ${rel(REGISTRY_RS)} (parser needs updating)`
    );
    process.exit(1);
  }
  const region = lines
    .slice(startIdx, endIdx + 1)
    .map((l) => l.replace(/\/\/.*$/, ""))
    .join("\n");

  const pairs = [];
  const pairRe = /\(\s*"([^"]+)"\s*,\s*"([^"]+)\.rs"\s*\)/g;
  let m;
  while ((m = pairRe.exec(region)) !== null) {
    pairs.push({ language: m[1], target: m[2] });
  }
  return failEmpty(pairs, "FUZZ_TARGETS entries", REGISTRY_RS);
}

const sources = new Set(extractTargetSources());
const bins = new Set(extractCargoBins(readFileOrFail(FUZZ_CARGO_TOML)));
const matrix = new Set(extractMatrixTargets(readFileOrFail(FUZZ_YML)));
const registry = extractRegistryTargets(readFileOrFail(REGISTRY_RS));

console.log(`OK: ${rel(TARGETS_DIR)} holds ${sources.size} target source(s)`);
console.log(`OK: ${rel(FUZZ_CARGO_TOML)} declares ${bins.size} [[bin]] target(s)`);
console.log(`OK: ${rel(FUZZ_YML)} matrix runs ${matrix.size} target(s)`);
console.log(`OK: ${rel(REGISTRY_RS)} registers ${registry.length} in-process grammar(s)`);

const setDiff = (a, b) => [...a].filter((x) => !b.has(x));

let failed = false;
const fail = (message) => {
  failed = true;
  console.error(`ERROR: ${message}`);
};

for (const t of setDiff(sources, bins)) {
  fail(
    `fuzz/fuzz_targets/${t}.rs has no [[bin]] entry in ${rel(FUZZ_CARGO_TOML)},\n` +
      `       so cargo-fuzz cannot build it and it can never run.`
  );
}
for (const t of setDiff(bins, sources)) {
  fail(
    `${rel(FUZZ_CARGO_TOML)} declares bin '${t}' but fuzz/fuzz_targets/${t}.rs does not exist.`
  );
}
for (const t of setDiff(bins, matrix)) {
  fail(
    `fuzz target '${t}' builds but is absent from the ${rel(FUZZ_YML)} matrix,\n` +
      `       so nothing ever executes it (ADR-017 Rule 4.3).`
  );
}
for (const t of setDiff(matrix, bins)) {
  fail(
    `${rel(FUZZ_YML)} matrix runs '${t}' but no such [[bin]] exists in ${rel(FUZZ_CARGO_TOML)},\n` +
      `       so the nightly job fails before it fuzzes anything.`
  );
}
// Only existence is checked here: the set equalities above already carry an
// existing source through to a matrix entry.
for (const { language, target } of registry) {
  if (!sources.has(target)) {
    fail(
      `${rel(REGISTRY_RS)} parses '${language}' in-process but fuzz/fuzz_targets/${target}.rs\n` +
        `       does not exist (ADR-017 Rule 4.3 requires a fuzz target for every in-process grammar).`
    );
  }
}

if (failed) {
  console.error(
    "\nFix: these four must describe the same set of fuzz targets:\n" +
      `  - ${rel(TARGETS_DIR)}/*.rs (the target sources)\n` +
      `  - ${rel(FUZZ_CARGO_TOML)} ([[bin]] name/path)\n` +
      `  - ${rel(FUZZ_YML)} (matrix.target)\n` +
      `  - ${rel(REGISTRY_RS)} (FUZZ_TARGETS, in-process grammars only)`
  );
  process.exit(1);
}

console.log(`OK: every fuzz target builds and runs nightly (${matrix.size} targets)`);
process.exit(0);
