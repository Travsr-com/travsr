/**
 * VSCODE-205: Binary auto-installer.
 *
 * Downloads the travsr binary from GitHub Releases, verifies its SHA256
 * checksum, and writes it to ~/.travsr/bin. All pure functions accept
 * explicit platform/arch parameters so they can be unit-tested without
 * mocking process globals.
 */

import * as cp from "child_process";
import * as crypto from "crypto";
import * as fs from "fs";
import * as https from "https";
import * as os from "os";
import * as path from "path";

export const DOWNLOAD_VERSION = "0.11.0";

// #497: must stay in lockstep with the release workflow matrix
// (.github/workflows/release.yml) and the npm TARGETS map
// (packages/travsr-npm/scripts/install.js). Releases ship no
// aarch64-pc-windows-msvc artifact, so claiming win32/arm64 here sent
// Windows-on-ARM users into a guaranteed-404 download.
// #576: this drift is checked in CI by .github/scripts/check-target-maps.mjs.
const TARGET_MAP: Partial<Record<string, Partial<Record<string, string>>>> = {
  linux:  { x64: "x86_64-unknown-linux-gnu",  arm64: "aarch64-unknown-linux-gnu" },
  darwin: { x64: "x86_64-apple-darwin",        arm64: "aarch64-apple-darwin" },
  win32:  { x64: "x86_64-pc-windows-msvc" },
};

export function resolveTargetTriple(
  platform: string = process.platform,
  arch: string = process.arch
): string {
  const triple = TARGET_MAP[platform]?.[arch];
  if (!triple) {
    throw new Error(
      `Unsupported platform/arch: ${platform}/${arch} — no prebuilt travsr ` +
      `binary is published for this target`
    );
  }
  return triple;
}

/**
 * #497: whether a prebuilt binary is published for this platform/arch.
 * Lets callers skip the download prompt entirely instead of offering a
 * download that resolveTargetTriple (or GitHub, with a 404) will reject.
 */
export function isDownloadSupported(
  platform: string = process.platform,
  arch: string = process.arch
): boolean {
  return TARGET_MAP[platform]?.[arch] !== undefined;
}

export function resolveInstallDir(): string {
  return path.join(os.homedir(), ".travsr", "bin");
}

export function resolveInstallPath(
  installDir: string,
  platform: string = process.platform
): string {
  const name = platform === "win32" ? "travsr.exe" : "travsr";
  return path.join(installDir, name);
}

export function buildDownloadUrl(version: string, triple: string): string {
  const tarName = `travsr-v${version}-${triple}.tar.gz`;
  return `https://github.com/Travsr-com/travsr/releases/download/v${version}/${tarName}`;
}

export function buildSumsUrl(version: string): string {
  return `https://github.com/Travsr-com/travsr/releases/download/v${version}/SHA256SUMS`;
}

async function fetchBuffer(url: string, maxRedirects = 5): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const get = (currentUrl: string, left: number): void => {
      https
        .get(currentUrl, { headers: { "User-Agent": "travsr-vscode-installer" } }, (res) => {
          if (
            (res.statusCode === 301 || res.statusCode === 302) &&
            res.headers.location &&
            left > 0
          ) {
            res.resume();
            get(res.headers.location, left - 1);
            return;
          }
          if (res.statusCode !== 200) {
            res.resume();
            reject(new Error(`HTTP ${res.statusCode ?? "?"} from ${currentUrl}`));
            return;
          }
          const chunks: Buffer[] = [];
          res.on("data", (c: Buffer) => chunks.push(c));
          res.on("end", () => resolve(Buffer.concat(chunks)));
          res.on("error", reject);
        })
        .on("error", reject);
    };
    get(url, maxRedirects);
  });
}

/**
 * Strips the sha256sum binary-mode marker (`*`) and any directory prefix
 * (published SHA256SUMS lists files as `dist/travsr-...tar.gz`) so entries
 * can be compared against a bare tarball name.
 */
function basename(field: string): string {
  return field.replace(/^\*/, "").split("/").pop() ?? field;
}

export function verifyChecksum(
  tarball: Buffer,
  tarName: string,
  sumsBuffer: Buffer
): void {
  const sumsText = sumsBuffer.toString("utf8");
  const entry = sumsText
    .split("\n")
    .map((l) => l.trim())
    .find((l) => {
      const parts = l.split(/\s+/);
      return parts.length >= 2 && basename(parts[parts.length - 1]) === tarName;
    });
  if (!entry) throw new Error(`SHA256SUMS entry not found for ${tarName}`);
  const expectedHash = entry.split(/\s+/)[0];
  const actualHash = crypto.createHash("sha256").update(tarball).digest("hex");
  if (actualHash !== expectedHash) {
    throw new Error(
      `SHA256 mismatch for ${tarName}: expected ${expectedHash}, got ${actualHash}`
    );
  }
}

export async function installBinary(
  version: string = DOWNLOAD_VERSION,
  onProgress?: (msg: string) => void
): Promise<string> {
  const triple = resolveTargetTriple();
  const tarName = `travsr-v${version}-${triple}.tar.gz`;
  const tarUrl = buildDownloadUrl(version, triple);
  const sumsUrl = buildSumsUrl(version);
  const installDir = resolveInstallDir();
  const installPath = resolveInstallPath(installDir);

  onProgress?.(`Downloading ${tarName}…`);
  const [tarball, sumsBuffer] = await Promise.all([
    fetchBuffer(tarUrl),
    fetchBuffer(sumsUrl),
  ]);

  onProgress?.("Verifying checksum…");
  verifyChecksum(tarball, tarName, sumsBuffer);

  onProgress?.("Extracting binary…");
  fs.mkdirSync(installDir, { recursive: true });
  const tmpTar = path.join(os.tmpdir(), tarName);
  fs.writeFileSync(tmpTar, tarball);

  const binName = process.platform === "win32" ? "travsr.exe" : "travsr";
  cp.execFileSync("tar", ["-xzf", tmpTar, "-C", installDir, binName], {
    stdio: "ignore",
  });
  try { fs.unlinkSync(tmpTar); } catch { /* ignore */ }

  if (process.platform !== "win32") fs.chmodSync(installPath, 0o755);

  onProgress?.(`Installed to ${installPath}`);
  return installPath;
}

export function checkOnPath(binaryName: string): boolean {
  try {
    if (process.platform === "win32") {
      const out = cp.execFileSync("where", [binaryName], { encoding: "utf8" });
      return out.split(/\r?\n/).some((l) => l.trim().toLowerCase().endsWith(".exe"));
    }
    cp.execFileSync("which", [binaryName], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

/**
 * #495: pick the first spawnable hit from `where`/`which` output. On Windows
 * only `.exe` hits qualify; `.cmd`/`.bat` shims are skipped (the npm-shim
 * resolution path, #486, handles those). Candidates that fail
 * assertExecutableBinary (relative, metacharacters) are skipped too.
 */
export function pickPathCandidate(
  lines: string[],
  platform: string = process.platform
): string | null {
  for (const raw of lines) {
    const line = raw.trim();
    if (!line) continue;
    if (platform === "win32" && !line.toLowerCase().endsWith(".exe")) continue;
    try {
      assertExecutableBinary(line, platform);
      return line;
    } catch {
      // unusable candidate; try the next hit
    }
  }
  return null;
}

/**
 * #495: resolve a binary on PATH to an absolute path the spawn gate accepts.
 * checkOnPath only answers "is it there?" and discards the location, but the
 * client rejects the bare binary name, so callers need the resolved path to
 * persist into travsr.binaryPath and reconnect with.
 */
export function resolveOnPath(binaryName: string): string | null {
  try {
    const lookup = process.platform === "win32" ? "where" : "which";
    const out = cp.execFileSync(lookup, [binaryName], { encoding: "utf8" });
    return pickPathCandidate(out.split(/\r?\n/));
  } catch {
    return null;
  }
}

export function findCmdShimPath(binaryName: string): string | null {
  if (process.platform !== "win32") return null;
  try {
    const out = cp.execFileSync("where", [binaryName], { encoding: "utf8" });
    const shim = out
      .split(/\r?\n/)
      .map((l) => l.trim())
      .find((l) => /\.(cmd|bat)$/i.test(l));
    return shim ?? null;
  } catch {
    return null;
  }
}

export function hasCmdShimOnPath(binaryName: string): boolean {
  return findCmdShimPath(binaryName) !== null;
}

/**
 * #486: an npm install is a complete install — the real native binary ships
 * inside the package, right next to the shim npm puts on PATH:
 *
 *   <npm prefix>\travsr.cmd                                       ← shim
 *   <npm prefix>\node_modules\@travsr.com\travsr\bin\travsr.exe   ← real binary
 *
 * Given a shim path, return the packaged binary when it exists and is
 * spawnable, else null.
 */
export function resolveNpmShimExe(
  shimPath: string,
  platform: string = process.platform,
  existsFn: (p: string) => boolean = fs.existsSync
): string | null {
  const exe = path.join(
    path.dirname(shimPath),
    "node_modules",
    "@travsr.com",
    "travsr",
    "bin",
    platform === "win32" ? "travsr.exe" : "travsr"
  );
  if (!existsFn(exe)) return null;
  try {
    assertExecutableBinary(exe, platform);
  } catch {
    return null;
  }
  return exe;
}

export function assertExecutableBinary(
  binary: string,
  platform: string = process.platform
): void {
  if (!path.isAbsolute(binary)) {
    throw new Error(`travsr binary must be an absolute path, got: ${binary}`);
  }
  if (platform === "win32") {
    if (!binary.toLowerCase().endsWith(".exe")) {
      throw new Error(
        `travsr binary on Windows must end in .exe, got: ${binary}. ` +
        `.cmd/.bat shims are not supported — point travsr.binaryPath at the ` +
        `packaged travsr.exe under node_modules\\@travsr.com\\travsr\\bin, ` +
        `or reinstall via the extension.`
      );
    }
  }
  if (/[&|;<>`$!^%(){}[\]"']/.test(binary)) {
    throw new Error(`travsr binary path contains shell metacharacters: ${binary}`);
  }
}
