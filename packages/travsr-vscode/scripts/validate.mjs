// One command that runs every extension check and prints a matrix.
//   npm run validate
//
// Unlike `a && b && c`, this runs all checks even when one fails, so a single
// run reports the full picture. Exit code is non-zero if any check failed.

import { execSync } from "node:child_process";
import { mkdtempSync, existsSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const results = [];

function check(name, fn) {
  process.stdout.write(`  ${name} ... `);
  try {
    const detail = fn() ?? "";
    console.log(`ok${detail ? ` (${detail})` : ""}`);
    results.push({ name, ok: true, detail });
  } catch (e) {
    const detail = (e.detail ?? e.message ?? "").split("\n")[0].slice(0, 90);
    console.log(`FAIL${detail ? ` (${detail})` : ""}`);
    results.push({ name, ok: false, detail });
  }
}

const run = (cmd) => execSync(cmd, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });

console.log("\ntravsr-vscode validate\n");

check("compile", () => {
  run("npm run compile");
});

check("lint", () => {
  run("npm run lint");
});

check("tests", () => {
  const out = run("npm test");
  const m = /(\d+) passing/.exec(out);
  return m ? `${m[1]} passing` : "";
});

check("audit (production deps)", () => {
  // `npm audit` exits non-zero when vulnerabilities are found, so read the JSON
  // rather than relying on the exit code.
  let json;
  try {
    json = run("npm audit --omit=dev --json");
  } catch (e) {
    json = e.stdout?.toString() ?? "{}";
  }
  const total = JSON.parse(json).metadata?.vulnerabilities?.total ?? 0;
  if (total > 0) {
    const err = new Error("vulnerabilities");
    err.detail = `${total} production vulnerabilities`;
    throw err;
  }
  return "0 vulnerabilities";
});

check("package: runtime deps present in .vsix", () => {
  const dir = mkdtempSync(join(tmpdir(), "travsr-vsix-"));
  try {
    const vsix = join(dir, "out.vsix");
    run(`npx vsce package --out ${vsix}`);
    run(`unzip -q ${vsix} -d ${dir}/x`);

    const deps = Object.keys(
      JSON.parse(run("cat package.json")).dependencies ?? {}
    );
    const missing = deps.filter(
      (d) => !existsSync(join(dir, "x", "extension", "node_modules", d))
    );
    if (missing.length > 0) {
      const err = new Error("missing");
      err.detail = `not bundled: ${missing.join(", ")}`;
      throw err;
    }
    return deps.length === 0 ? "no runtime deps" : `${deps.length} bundled`;
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} passed`);
if (failed.length > 0) {
  console.log(`failed: ${failed.map((r) => r.name).join(", ")}\n`);
  process.exit(1);
}
console.log("");
