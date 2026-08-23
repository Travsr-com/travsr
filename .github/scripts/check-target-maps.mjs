#!/usr/bin/env node
/**
 * Guard for #576 / #497: the release build matrix and the three installer
 * target maps must agree on the exact set of published Rust target triples.
 *
 * The comparable field in release.yml is `artifact:`, not `target:`. The
 * Linux entries build against musl (`target: x86_64-unknown-linux-musl`) but
 * ship a gnu-named asset (`artifact: x86_64-unknown-linux-gnu`); the package
 * step (release.yml, STAGE="travsr-${TAG}-${ARTIFACT}") and both installers
 * (installer.ts, ensure-binary.js) only ever request the artifact name. Comparing
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
 *   node .github/scripts/check-target-maps.mjs --self-test
 */

import { fileURLToPath } from "node:url";
import { readFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = join(__dirname, "..", "..");

const RELEASE_YML = join(REPO_ROOT, ".github/workflows/release.yml");
const INSTALLER_TS = join(REPO_ROOT, "packages/travsr-vscode/src/installer.ts");
const INSTALL_JS = join(REPO_ROOT, "packages/travsr-npm/scripts/ensure-binary.js");
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
 * Applies one line's braces to `depth`, ignoring braces inside string
 * literals (', ", `, with backslash escapes) and after an unquoted `//`.
 * Quote state is not carried across lines: every source this parses keeps
 * its literals on one line, and resetting per line keeps one unterminated
 * literal from swallowing the rest of the file. Block comments are not
 * handled, matching the naive `//`/`#` stripping documented at the top of
 * this module. // O(n) in line length
 */
function applyBraces(line, depth) {
  let quote = null;
  for (let c = 0; c < line.length; c++) {
    const ch = line[c];
    if (quote !== null) {
      if (ch === "\\") c++;
      else if (ch === quote) quote = null;
      continue;
    }
    if (ch === '"' || ch === "'" || ch === "`") quote = ch;
    else if (ch === "/" && line[c + 1] === "/") break;
    else if (ch === "{") depth++;
    else if (ch === "}") depth--;
  }
  return depth;
}

/**
 * Finds the line index of the `}` that closes the object literal declared on
 * `lines[startIdx]`, by counting brace depth instead of stopping at the first
 * downstream line shaped like `};` (#690).
 *
 * Shape matching was the bug: with the object's own closing brace missing,
 * the scan latched onto an unrelated `});` seventy lines downstream and
 * swallowed everything between, so the guard fired pointing at the wrong
 * file. Depth counting alone does not fix it either. The rest of a real
 * source file re-balances, so the missing brace just gets absorbed and the
 * scan latches onto the last `}` in the file instead.
 *
 * So the scan is also bounded, which is what makes the failure detectable:
 * a target map's entries are all indented under its declaration, so any
 * non-blank line back at the declaration's own indentation means the literal
 * should already have closed. Lines opening with `{` or `}` are exempt, since
 * the literal's own closing `};` sits at exactly that indentation. A map
 * entry commented out at column 0 would trip this; indent it with the rest.
 *
 * Returns `{ endIdx, reason, atLine, atText }`. On success endIdx is the
 * closing brace's line and reason is null. On failure endIdx is -1 and
 * reason is one of:
 *   "unopened"  the declaration never opens an object literal
 *   "unclosed"  it opened and is still open at atLine, which is out of bounds
 *   "malformed" it balanced, but not on a line that terminates a declaration
 * // O(n) in region character count
 */
function findClosingBraceLine(lines, startIdx) {
  const indentOf = (l) => l.match(/^[ \t]*/)[0].length;
  const declIndent = indentOf(lines[startIdx]);
  const declLine = lines[startIdx].replace(/\/\/.*$/, "");
  let depth = applyBraces(declLine, 0);
  let opened = depth > 0;

  // Single-line `const X = { ... };`, which never reaches the loop below.
  if (depth === 0 && /=\s*\{.*\}[^{}]*;\s*$/.test(declLine)) {
    return { endIdx: startIdx, reason: null };
  }

  for (let i = startIdx + 1; i < lines.length; i++) {
    const line = lines[i];
    const trimmed = line.trim();
    if (trimmed !== "" && !/^[{}]/.test(trimmed) && indentOf(line) <= declIndent) {
      return { endIdx: -1, reason: opened ? "unclosed" : "unopened", atLine: i + 1, atText: trimmed };
    }
    depth = applyBraces(line, depth);
    if (depth > 0) {
      opened = true;
    } else if (opened) {
      // Balanced as of this line. It is the real closer only if the line also
      // terminates the declaration (`};`, `} as const;`).
      if (depth === 0 && /^\s*\}[^{}]*;\s*$/.test(line.replace(/\/\/.*$/, ""))) {
        return { endIdx: i, reason: null };
      }
      return { endIdx: -1, reason: "malformed", atLine: i + 1, atText: trimmed };
    }
  }
  return { endIdx: -1, reason: opened ? "unclosed" : "unopened" };
}

/**
 * Extracts values from the object literal declared by `const <objectName>`
 * (identifier-boundary regex match, so a rename like `TARGET_MAP` ->
 * `TARGET_MAPX` cannot satisfy it via substring containment), bounded by
 * that declaration's own matching closing brace. Strips `//` comments
 * before extracting `: "value"` / `: 'value'` pairs.
 *
 * Throws on every failure rather than exiting, so the self-test can assert
 * the message. The caller wrapper below turns a throw into the exit.
 * // O(n) in file line count
 */
function extractObjectValues(text, absPath, objectName) {
  const lines = text.split("\n");
  const escapedName = objectName.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const startRe = new RegExp(String.raw`\bconst ${escapedName}\b`);
  const startIdx = lines.findIndex((l) => startRe.test(l));
  if (startIdx === -1) {
    throw new Error(
      `could not locate ${objectName} in ${rel(absPath)} (parser needs updating)`
    );
  }
  const { endIdx, reason, atLine, atText } = findClosingBraceLine(lines, startIdx);
  if (endIdx === -1) {
    // Quoted, not parenthesised: atText is a raw source line and routinely
    // carries its own "(", which left the rendered message visibly unbalanced
    // on the very case this check exists to explain.
    const where = atLine ? `line ${atLine}, which reads \`${atText}\`` : "the end of the file";
    const site = `${objectName} in ${rel(absPath)} (declared on line ${startIdx + 1})`;
    throw new Error(
      reason === "unopened"
        ? `${site} does not open an object literal before ${where} ` +
            `(the declaration changed shape, so the parser needs updating).`
        : reason === "malformed"
          ? `the closing brace of ${site} is malformed: its braces balance at ` +
              `${where}, which does not end a const declaration. Expected a line ` +
              `of the form "};". Fix ${rel(absPath)}; the other target maps are not at fault.`
          : `the closing brace of ${site} is missing: the object literal is still ` +
              `open at ${where}, which is back at the declaration's own indentation ` +
              `and so cannot be part of the literal. Restore the "};" that closes ` +
              `${objectName}. Fix ${rel(absPath)}; the other target maps are not at fault.`
    );
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
    throw new Error(
      `located ${objectName} in ${rel(absPath)} but extracted zero values (parser needs updating)`
    );
  }
  return values;
}

/** Runs extractObjectValues, turning its throw into the usual hard exit. // O(n) */
function extractObjectValuesOrExit(text, absPath, objectName) {
  try {
    return extractObjectValues(text, absPath, objectName);
  } catch (err) {
    console.error(`ERROR: ${err.message}`);
    process.exit(1);
  }
}

function extractVscodeTargets(text) {
  return extractObjectValuesOrExit(text, INSTALLER_TS, "TARGET_MAP");
}

function extractNpmTargets(text) {
  return extractObjectValuesOrExit(text, INSTALL_JS, "TARGETS");
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

/**
 * `--self-test`: exercises extractObjectValues against synthetic sources, so
 * the parser itself is covered rather than only the repo's current files.
 *
 * This guard had no test harness at all, which is how #690 survived: against
 * a clean tree the broken brace scan and a correct one produce identical
 * output, so nothing on the happy path could ever notice. Same shape as
 * build-id.sh --self-test: stdlib only, no npm install, runs in a second.
 * // O(1)
 */
function selfTest() {
  let failed = 0;

  const check = (label, fn) => {
    try {
      fn();
      console.log(`ok: ${label}`);
    } catch (err) {
      console.error(`FAIL: ${label}\n      ${err.message}`);
      failed = 1;
    }
  };

  const eq = (got, want) => {
    const g = JSON.stringify(got);
    const w = JSON.stringify(want);
    if (g !== w) throw new Error(`got ${g}, want ${w}`);
  };

  // Must throw, and the message must match `re`. A returned value is itself
  // the failure: silently parsing something is exactly the #690 bug.
  const throwsMatching = (re, fn) => {
    let got;
    try {
      got = fn();
    } catch (err) {
      if (!re.test(err.message)) {
        throw new Error(`message ${JSON.stringify(err.message)} does not match ${re}`);
      }
      return;
    }
    throw new Error(`did not throw, returned ${JSON.stringify(got)}`);
  };

  const TRIPLES = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
  ];

  // The installer.ts shape: nested per-platform objects, TS type annotation.
  const MAP_HEAD = [
    "const TARGET_MAP: Partial<Record<string, Partial<Record<string, string>>>> = {",
    '  linux:  { x64: "x86_64-unknown-linux-gnu",  arm64: "aarch64-unknown-linux-gnu" },',
    '  darwin: { x64: "x86_64-apple-darwin",        arm64: "aarch64-apple-darwin" },',
    '  win32:  { x64: "x86_64-pc-windows-msvc" },',
    "",
  ].join("\n");

  // Condensed installer.ts, keeping the three things that made #690 bite:
  // the `    };` closing an arrow-function const (installer.ts line 103, what
  // the old scan latched onto), the two quoted strings ahead of it that the
  // value regex then scraped as if they were triples, and a `}` count that
  // returns to zero later on (the regex literal's stray backtick swallows an
  // opening brace), which is what a depth-only scan would latch onto.
  const TAIL = [
    "",
    "export function resolveInstallPath(installDir: string, platform: string): string {",
    '  const name = platform === "win32" ? "travsr.exe" : "travsr";',
    "  return path.join(installDir, name);",
    "}",
    "",
    "async function fetchBuffer(url: string): Promise<Buffer> {",
    "  return new Promise((resolve) => {",
    "    const get = (currentUrl: string): void => {",
    '      https.get(currentUrl, { headers: { "User-Agent": "travsr-vscode-installer" } }, resolve);',
    "    };",
    "    get(url);",
    "  });",
    "}",
    "",
    "export function assertExecutableBinary(binary: string): void {",
    '  if (/[&|;<>`$!^%(){}[\\]"\']/.test(binary)) {',
    '    throw new Error("shell metacharacters");',
    "  }",
    "}",
    "",
  ].join("\n");

  // Pins the two fixture properties the #690 case depends on. Without them
  // that case would pass against the very parsers it exists to reject.
  check("the fixture defeats a shape scan and a depth only scan alike", () => {
    const tail = TAIL.split("\n");
    if (!tail.some((l) => l.trim() === "};")) {
      throw new Error('TAIL has no line trimming to "};" for a shape scan to latch onto');
    }
    let d = 1; // as if TARGET_MAP were still open
    if (!tail.some((l) => (d = applyBraces(l, d)) === 0)) {
      throw new Error("TAIL never returns to depth 0 for a depth only scan to latch onto");
    }
  });

  check("well formed TARGET_MAP parses to its five triples", () => {
    eq(extractObjectValues(MAP_HEAD + "};\n" + TAIL, INSTALLER_TS, "TARGET_MAP"), TRIPLES);
  });

  // #690 proper. Before the fix this returned the five real triples plus
  // 'travsr' and 'travsr-vscode-installer' scraped out of TAIL, and the
  // caller then blamed the release matrix for two targets that do not exist.
  check("TARGET_MAP with its closing brace deleted reports the brace", () => {
    throwsMatching(
      /^the closing brace of TARGET_MAP in packages\/travsr-vscode\/src\/installer\.ts \(declared on line 1\) is missing/,
      () => extractObjectValues(MAP_HEAD + TAIL, INSTALLER_TS, "TARGET_MAP")
    );
  });

  check("the missing brace message names where the scan stopped and what to fix", () => {
    throwsMatching(
      /still open at line 6, which reads `export function resolveInstallPath[\s\S]*Fix packages\/travsr-vscode\/src\/installer\.ts; the other target maps are not at fault/,
      () => extractObjectValues(MAP_HEAD + TAIL, INSTALLER_TS, "TARGET_MAP")
    );
  });

  // The ensure-binary.js shape: flat map, single quotes, no type annotation.
  check("well formed TARGETS parses to its five triples", () => {
    const src =
      `const TARGETS = {\n` +
      `  'linux-x64':   'x86_64-unknown-linux-gnu',\n` +
      `  'linux-arm64': 'aarch64-unknown-linux-gnu',\n` +
      `  'darwin-x64':  'x86_64-apple-darwin',\n` +
      `  'darwin-arm64':'aarch64-apple-darwin',\n` +
      `  'win32-x64':   'x86_64-pc-windows-msvc',\n` +
      `};\n` +
      `function detect() {\n  return {};\n}\n`;
    eq(extractObjectValues(src, INSTALL_JS, "TARGETS"), TRIPLES);
  });

  // A nested closer on its own line must not be mistaken for the outer one,
  // which is the mirror image of the #690 failure: stopping too early rather
  // than too late would drop real targets and blame the release matrix again.
  check("a nested closing brace on its own line does not end the scan", () => {
    const src =
      `const TARGET_MAP = {\n` +
      `  linux: {\n    x64: "x86_64-unknown-linux-gnu",\n  },\n` +
      `  darwin: {\n    arm64: "aarch64-apple-darwin",\n  },\n` +
      `};\n`;
    eq(extractObjectValues(src, INSTALL_JS, "TARGET_MAP"), [
      "x86_64-unknown-linux-gnu",
      "aarch64-apple-darwin",
    ]);
  });

  check("braces inside strings and comments do not move the depth", () => {
    const src =
      `const TARGETS = {\n` +
      `  // a decoy };\n` +
      `  note: "unbalanced { in a string",\n` +
      `  'linux-x64': 'x86_64-unknown-linux-gnu',\n` +
      `};\n` +
      `const OTHER = 1;\n`;
    eq(extractObjectValues(src, INSTALL_JS, "TARGETS"), [
      "unbalanced { in a string",
      "x86_64-unknown-linux-gnu",
    ]);
  });

  check("a renamed map is reported as missing, not matched by substring", () => {
    throwsMatching(/could not locate TARGET_MAP/, () =>
      extractObjectValues(`const TARGET_MAPX = {\n  a: "b",\n};\n`, INSTALLER_TS, "TARGET_MAP")
    );
  });

  check("a single line map parses without needing its own closing line", () => {
    eq(extractObjectValues(`const TARGETS = { 'linux-x64': 'x86_64-apple-darwin' };\n`, INSTALL_JS, "TARGETS"), [
      "x86_64-apple-darwin",
    ]);
  });

  check("a declaration that is not an object literal is reported as such", () => {
    throwsMatching(/does not open an object literal before line 2, which reads `const X = 1;`/, () =>
      extractObjectValues(`const TARGETS = buildTargets();\nconst X = 1;\n`, INSTALL_JS, "TARGETS")
    );
  });

  // Balanced, but the brace that balances it does not end the declaration.
  check("a closer that does not terminate the declaration is reported as malformed", () => {
    throwsMatching(/is malformed: its braces balance at line 3, which reads `\}`/, () =>
      extractObjectValues(`const TARGETS = {\n  'a': 'x86_64-apple-darwin',\n}\n`, INSTALL_JS, "TARGETS")
    );
  });

  check("an object literal with no quoted values is reported as empty", () => {
    throwsMatching(/extracted zero values/, () =>
      extractObjectValues(`const TARGETS = {\n};\n`, INSTALL_JS, "TARGETS")
    );
  });

  if (failed === 0) console.log("check-target-maps.mjs self-test passed");
  process.exit(failed);
}

// Runs before the checks below and exits, so --self-test never reads the repo.
if (process.argv.includes("--self-test")) selfTest();

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
