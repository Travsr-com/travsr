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

export const DOWNLOAD_VERSION = "0.9.1";

const TARGET_MAP: Partial<Record<string, Partial<Record<string, string>>>> = {
  linux:  { x64: "x86_64-unknown-linux-gnu",  arm64: "aarch64-unknown-linux-gnu" },
  darwin: { x64: "x86_64-apple-darwin",        arm64: "aarch64-apple-darwin" },
  win32:  { x64: "x86_64-pc-windows-msvc", arm64: "aarch64-pc-windows-msvc" },
};

export function resolveTargetTriple(
  platform: string = process.platform,
  arch: string = process.arch
): string {
  const triple = TARGET_MAP[platform]?.[arch];
  if (!triple) throw new Error(`Unsupported platform/arch: ${platform}/${arch}`);
  return triple;
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
      return parts.length >= 2 && parts[parts.length - 1] === tarName;
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

export function hasCmdShimOnPath(binaryName: string): boolean {
  if (process.platform !== "win32") return false;
  try {
    const out = cp.execFileSync("where", [binaryName], { encoding: "utf8" });
    return out.split(/\r?\n/).some((l) => l.trim().toLowerCase().endsWith(".cmd"));
  } catch {
    return false;
  }
}

export function assertExecutableBinary(binary: string): void {
  if (!path.isAbsolute(binary)) {
    throw new Error(`travsr binary must be an absolute path, got: ${binary}`);
  }
  if (process.platform === "win32") {
    if (!binary.toLowerCase().endsWith(".exe")) {
      throw new Error(
        `travsr binary on Windows must end in .exe, got: ${binary}. ` +
        `.cmd/.bat shims are not supported — reinstall via the extension.`
      );
    }
  }
  if (/[&|;<>`$!^%(){}[\]"']/.test(binary)) {
    throw new Error(`travsr binary path contains shell metacharacters: ${binary}`);
  }
}
