import * as assert from "assert";
import * as crypto from "crypto";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import {
  resolveTargetTriple,
  resolveInstallDir,
  resolveInstallPath,
  buildDownloadUrl,
  buildSumsUrl,
  verifyChecksum,
  checkOnPath,
  pickPathCandidate,
  resolveOnPath,
  hasCmdShimOnPath,
  findCmdShimPath,
  resolveNpmShimExe,
  assertExecutableBinary,
} from "../../installer";

// ── resolveTargetTriple ────────────────────────────────────────────────────

suite("VSCODE-205: installer — resolveTargetTriple", () => {
  test("linux/x64 → x86_64-unknown-linux-gnu", () => {
    assert.strictEqual(resolveTargetTriple("linux", "x64"), "x86_64-unknown-linux-gnu");
  });

  test("linux/arm64 → aarch64-unknown-linux-gnu", () => {
    assert.strictEqual(resolveTargetTriple("linux", "arm64"), "aarch64-unknown-linux-gnu");
  });

  test("darwin/x64 → x86_64-apple-darwin", () => {
    assert.strictEqual(resolveTargetTriple("darwin", "x64"), "x86_64-apple-darwin");
  });

  test("darwin/arm64 → aarch64-apple-darwin", () => {
    assert.strictEqual(resolveTargetTriple("darwin", "arm64"), "aarch64-apple-darwin");
  });

  test("win32/x64 → x86_64-pc-windows-msvc", () => {
    assert.strictEqual(resolveTargetTriple("win32", "x64"), "x86_64-pc-windows-msvc");
  });

  test("win32/arm64 → aarch64-pc-windows-msvc", () => {
    assert.strictEqual(resolveTargetTriple("win32", "arm64"), "aarch64-pc-windows-msvc");
  });

  test("unknown platform throws with platform and arch in message", () => {
    assert.throws(
      () => resolveTargetTriple("freebsd", "x64"),
      (e: Error) => e.message.includes("freebsd") && e.message.includes("x64")
    );
  });

  test("known platform with unknown arch throws", () => {
    assert.throws(
      () => resolveTargetTriple("linux", "mips"),
      (e: Error) => e.message.includes("linux") && e.message.includes("mips")
    );
  });
});

// ── resolveInstallDir ──────────────────────────────────────────────────────

suite("VSCODE-205: installer — resolveInstallDir", () => {
  test("returns path ending in .travsr/bin", () => {
    const dir = resolveInstallDir();
    assert.ok(
      dir.endsWith(path.join(".travsr", "bin")),
      `expected to end with .travsr/bin, got: ${dir}`
    );
  });

  test("is rooted at os.homedir()", () => {
    const dir = resolveInstallDir();
    assert.ok(dir.startsWith(os.homedir()), `expected to start with homedir, got: ${dir}`);
  });
});

// ── resolveInstallPath ─────────────────────────────────────────────────────

suite("VSCODE-205: installer — resolveInstallPath", () => {
  test("unix (linux): filename is 'travsr'", () => {
    const p = resolveInstallPath("/home/user/.travsr/bin", "linux");
    assert.ok(p.endsWith(`${path.sep}travsr`), `unexpected: ${p}`);
    assert.ok(!p.endsWith(".exe"), "must not end with .exe on linux");
  });

  test("unix (darwin): filename is 'travsr'", () => {
    const p = resolveInstallPath("/Users/user/.travsr/bin", "darwin");
    assert.ok(p.endsWith(`${path.sep}travsr`), `unexpected: ${p}`);
  });

  test("win32: filename is 'travsr.exe'", () => {
    const p = resolveInstallPath("C:\\Users\\user\\.travsr\\bin", "win32");
    assert.ok(p.endsWith("travsr.exe"), `expected travsr.exe, got: ${p}`);
  });
});

// ── buildDownloadUrl ───────────────────────────────────────────────────────

suite("VSCODE-205: installer — buildDownloadUrl", () => {
  test("url contains version and triple", () => {
    const url = buildDownloadUrl("0.5.0", "x86_64-unknown-linux-gnu");
    assert.ok(url.includes("v0.5.0"), `version missing: ${url}`);
    assert.ok(url.includes("x86_64-unknown-linux-gnu"), `triple missing: ${url}`);
  });

  test("url starts with GitHub releases base", () => {
    const url = buildDownloadUrl("0.5.0", "aarch64-apple-darwin");
    assert.ok(
      url.startsWith("https://github.com/Travsr-com/travsr/releases/download/"),
      `unexpected: ${url}`
    );
  });

  test("url ends with .tar.gz", () => {
    const url = buildDownloadUrl("0.5.0", "x86_64-pc-windows-msvc");
    assert.ok(url.endsWith(".tar.gz"), `expected .tar.gz suffix, got: ${url}`);
  });

  test("tarball name embeds both version and triple", () => {
    const url = buildDownloadUrl("1.2.3", "x86_64-apple-darwin");
    assert.ok(url.includes("travsr-v1.2.3-x86_64-apple-darwin.tar.gz"), `unexpected: ${url}`);
  });
});

// ── buildSumsUrl ───────────────────────────────────────────────────────────

suite("VSCODE-205: installer — buildSumsUrl", () => {
  test("url points to SHA256SUMS under the correct release tag", () => {
    const url = buildSumsUrl("0.5.0");
    assert.ok(url.includes("v0.5.0"), `version missing: ${url}`);
    assert.ok(url.endsWith("SHA256SUMS"), `expected SHA256SUMS suffix, got: ${url}`);
  });
});

// ── verifyChecksum ─────────────────────────────────────────────────────────

function makeSums(tarName: string, tarball: Buffer): Buffer {
  const hash = crypto.createHash("sha256").update(tarball).digest("hex");
  return Buffer.from(`${hash}  ${tarName}\n`, "utf8");
}

suite("VSCODE-205: installer — verifyChecksum", () => {
  test("passes when checksum matches", () => {
    const tarball = Buffer.from("fake tarball contents");
    const tarName = "travsr-v0.5.0-x86_64-unknown-linux-gnu.tar.gz";
    assert.doesNotThrow(() => verifyChecksum(tarball, tarName, makeSums(tarName, tarball)));
  });

  test("throws SHA256 mismatch when content is tampered", () => {
    const tarball = Buffer.from("correct contents");
    const tampered = Buffer.from("tampered contents");
    const tarName = "travsr-v0.5.0-x86_64-unknown-linux-gnu.tar.gz";
    assert.throws(
      () => verifyChecksum(tampered, tarName, makeSums(tarName, tarball)),
      /SHA256 mismatch/
    );
  });

  test("throws not found when tarName absent from SHA256SUMS", () => {
    const tarball = Buffer.from("contents");
    const sumsForOther = makeSums("travsr-v0.4.0-other.tar.gz", tarball);
    assert.throws(
      () => verifyChecksum(tarball, "travsr-v0.5.0-missing.tar.gz", sumsForOther),
      /not found/
    );
  });

  test("handles multiple entries in SHA256SUMS (correct entry selected)", () => {
    const tarball = Buffer.from("my tarball");
    const tarName = "travsr-v0.5.0-aarch64-apple-darwin.tar.gz";
    const hash = crypto.createHash("sha256").update(tarball).digest("hex");
    const sumsWithMultiple = Buffer.from(
      `aaaa1111  travsr-v0.5.0-x86_64-apple-darwin.tar.gz\n` +
      `${hash}  ${tarName}\n` +
      `bbbb2222  travsr-v0.5.0-x86_64-unknown-linux-gnu.tar.gz\n`,
      "utf8"
    );
    assert.doesNotThrow(() => verifyChecksum(tarball, tarName, sumsWithMultiple));
  });
});

// ── WS1: checkOnPath — Windows .cmd discrimination ────────────────────────

suite("WS1: checkOnPath — Windows .cmd discrimination", () => {
  test("returns false for nonexistent binary on all platforms", () => {
    assert.strictEqual(checkOnPath("__travsr_definitely_not_on_path_xyz__"), false);
  });

  test("hasCmdShimOnPath returns false on non-Windows platforms", () => {
    if (process.platform === "win32") return;
    assert.strictEqual(hasCmdShimOnPath("__anything__"), false);
  });

  test("hasCmdShimOnPath returns false for nonexistent binary on Windows", () => {
    if (process.platform !== "win32") return;
    assert.strictEqual(hasCmdShimOnPath("__travsr_definitely_not_on_path_xyz__"), false);
  });

  test("findCmdShimPath returns null on non-Windows platforms", () => {
    if (process.platform === "win32") return;
    assert.strictEqual(findCmdShimPath("__anything__"), null);
  });

  test("findCmdShimPath returns null for nonexistent binary on Windows", () => {
    if (process.platform !== "win32") return;
    assert.strictEqual(findCmdShimPath("__travsr_definitely_not_on_path_xyz__"), null);
  });
});

// ── #495: pickPathCandidate / resolveOnPath — PATH auto-detect ────────────

suite("#495: installer — pickPathCandidate", () => {
  test("win32: prefers the .exe hit over a preceding .cmd shim", () => {
    const lines = [
      "C:\\Users\\user\\AppData\\Roaming\\npm\\travsr.cmd",
      "C:\\Users\\user\\.cargo\\bin\\travsr.exe",
    ];
    assert.strictEqual(
      pickPathCandidate(lines, "win32"),
      "C:\\Users\\user\\.cargo\\bin\\travsr.exe"
    );
  });

  test("win32: returns null when only .cmd/.bat shims are on PATH", () => {
    const lines = [
      "C:\\Users\\user\\AppData\\Roaming\\npm\\travsr.cmd",
      "C:\\tools\\travsr.bat",
    ];
    assert.strictEqual(pickPathCandidate(lines, "win32"), null);
  });

  test("posix: returns the first absolute hit", () => {
    assert.strictEqual(
      pickPathCandidate(["/usr/local/bin/travsr"], "linux"),
      "/usr/local/bin/travsr"
    );
  });

  test("skips candidates that fail assertExecutableBinary (metacharacters)", () => {
    const lines = ["/opt/bad&dir/travsr", "/usr/local/bin/travsr"];
    assert.strictEqual(pickPathCandidate(lines, "linux"), "/usr/local/bin/travsr");
  });

  test("skips non-absolute candidates", () => {
    assert.strictEqual(pickPathCandidate(["travsr"], "linux"), null);
  });

  test("ignores blank and whitespace-only lines", () => {
    assert.strictEqual(pickPathCandidate(["", "  ", "\r"], "win32"), null);
    assert.strictEqual(
      pickPathCandidate(["", "  /usr/local/bin/travsr  "], "darwin"),
      "/usr/local/bin/travsr"
    );
  });
});

suite("#495: installer — resolveOnPath", () => {
  test("returns null for a binary not on PATH", () => {
    assert.strictEqual(resolveOnPath("__travsr_definitely_not_on_path_xyz__"), null);
  });

  test("resolves a binary from PATH to its absolute location", () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "travsr-path-"));
    const name = "__travsr_resolve_test__";
    const file = path.join(dir, process.platform === "win32" ? `${name}.exe` : name);
    fs.writeFileSync(file, "MZ");
    if (process.platform !== "win32") fs.chmodSync(file, 0o755);
    const oldPath = process.env.PATH;
    process.env.PATH = dir + path.delimiter + (oldPath ?? "");
    try {
      const resolved = resolveOnPath(name);
      assert.ok(resolved !== null, "expected the binary to resolve from PATH");
      // where/which may return long-form paths while os.tmpdir() can use
      // Windows 8.3 short names; canonicalize both before comparing.
      assert.strictEqual(
        fs.realpathSync.native(resolved).toLowerCase(),
        fs.realpathSync.native(file).toLowerCase()
      );
    } finally {
      process.env.PATH = oldPath;
    }
  });
});

// ── #486: resolveNpmShimExe — npm shim → packaged native binary ───────────

suite("#486: installer — resolveNpmShimExe", () => {
  /** Build <tmp>/travsr.cmd + <tmp>/node_modules/@travsr.com/travsr/bin/<binName>. */
  function makeNpmPrefix(binName: string | null): { prefix: string; shim: string; exe: string } {
    const prefix = fs.mkdtempSync(path.join(os.tmpdir(), "travsr-shim-"));
    const shim = path.join(prefix, "travsr.cmd");
    fs.writeFileSync(shim, "@echo off\r\n");
    const binDir = path.join(prefix, "node_modules", "@travsr.com", "travsr", "bin");
    fs.mkdirSync(binDir, { recursive: true });
    const exe = path.join(binDir, binName ?? "travsr.exe");
    if (binName !== null) fs.writeFileSync(exe, "MZ");
    return { prefix, shim, exe };
  }

  test("resolves the packaged exe next to an npm .cmd shim (win32 layout)", () => {
    const { shim, exe } = makeNpmPrefix("travsr.exe");
    assert.strictEqual(resolveNpmShimExe(shim, "win32"), exe);
  });

  test("resolves the packaged binary without .exe on unix layout", () => {
    const { shim, exe } = makeNpmPrefix("travsr");
    assert.strictEqual(resolveNpmShimExe(shim, "linux"), exe);
  });

  test("returns null when the packaged binary is missing", () => {
    const { shim } = makeNpmPrefix(null); // bin dir exists but no exe inside
    assert.strictEqual(resolveNpmShimExe(shim, "win32"), null);
  });

  test("returns null when the resolved path fails assertExecutableBinary (metacharacters)", () => {
    const shimInBadDir =
      process.platform === "win32"
        ? "C:\\npm&prefix\\travsr.cmd"
        : "/npm&prefix/travsr.cmd";
    // existsFn stubbed true so the metacharacter validation is what rejects it.
    assert.strictEqual(resolveNpmShimExe(shimInBadDir, process.platform, () => true), null);
  });
});

// ── WS1: assertExecutableBinary ───────────────────────────────────────────

suite("WS1: assertExecutableBinary", () => {
  test("throws for relative (non-absolute) path", () => {
    assert.throws(
      () => assertExecutableBinary("travsr"),
      /must be an absolute path/
    );
  });

  test("throws for path containing shell metacharacters", () => {
    const malicious =
      process.platform === "win32"
        ? "C:\\Users\\user\\.travsr\\bin\\trav&sr.exe"
        : "/usr/local/bin/trav&sr";
    assert.throws(() => assertExecutableBinary(malicious), /shell metacharacters/);
  });

  test("on Windows: throws for absolute path not ending in .exe", () => {
    if (process.platform !== "win32") return;
    assert.throws(
      () => assertExecutableBinary("C:\\Users\\user\\.travsr\\bin\\travsr"),
      /must end in \.exe/
    );
  });

  test("on Windows: does not throw for valid absolute .exe path", () => {
    if (process.platform !== "win32") return;
    assert.doesNotThrow(() =>
      assertExecutableBinary("C:\\Users\\user\\.travsr\\bin\\travsr.exe")
    );
  });
});
