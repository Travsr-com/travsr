import * as assert from "assert";
import * as crypto from "crypto";
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
  hasCmdShimOnPath,
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

  // Regression coverage for #585: published SHA256SUMS entries are not bare
  // filenames. The release workflow runs `sha256sum dist/<tarball>`, so every
  // line carries a `dist/` prefix, and the Windows leg's sha256sum additionally
  // emits a `*` binary-mode marker before the filename.
  test("matches entry with a dist/ path prefix (real release format)", () => {
    const tarball = Buffer.from("real tarball");
    const tarName = "travsr-v0.10.0-aarch64-apple-darwin.tar.gz";
    const hash = crypto.createHash("sha256").update(tarball).digest("hex");
    const sums = Buffer.from(`${hash}  dist/${tarName}\n`, "utf8");
    assert.doesNotThrow(() => verifyChecksum(tarball, tarName, sums));
  });

  test("matches entry with a dist/ prefix and * binary-mode marker (Windows release format)", () => {
    const tarball = Buffer.from("windows tarball");
    const tarName = "travsr-v0.10.0-x86_64-pc-windows-msvc.tar.gz";
    const hash = crypto.createHash("sha256").update(tarball).digest("hex");
    const sums = Buffer.from(`${hash} *dist/${tarName}\n`, "utf8");
    assert.doesNotThrow(() => verifyChecksum(tarball, tarName, sums));
  });

  test("published SHA256SUMS shape (all five targets, dist/ prefix + Windows *)", () => {
    const tarName = "travsr-v0.11.0-x86_64-unknown-linux-gnu.tar.gz";
    const tarball = Buffer.from("linux tarball");
    const hash = crypto.createHash("sha256").update(tarball).digest("hex");
    const sums = Buffer.from(
      `0fbdf07864bd8e9768459a3f013b40445ae82cb0eebac04c7df494cba26f03b5  dist/travsr-v0.11.0-aarch64-apple-darwin.tar.gz\n` +
      `${hash}  dist/${tarName}\n` +
      `c6257a587122f3b5773f0d5ee901c11394df214cc8fcb92a7c569dc63e6d4da0 *dist/travsr-v0.11.0-x86_64-pc-windows-msvc.tar.gz\n`,
      "utf8"
    );
    assert.doesNotThrow(() => verifyChecksum(tarball, tarName, sums));
  });

  // release.yml's build matrix (.github/workflows/release.yml:152-173) publishes
  // exactly these five artifact triples. Each must resolve against a SHA256SUMS
  // built the way the real workflow builds it: `sha256sum dist/<tarball>` run
  // from the repo root, combined with `cat dist/*.sha256 > SHA256SUMS`. Windows
  // additionally gets the `*` binary-mode marker.
  const releaseTriples = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
  ];

  for (const triple of releaseTriples) {
    test(`resolves the real release entry for ${triple}`, () => {
      const tarName = `travsr-v0.11.0-${triple}.tar.gz`;
      const tarball = Buffer.from(`tarball for ${triple}`);
      const hash = crypto.createHash("sha256").update(tarball).digest("hex");
      const marker = triple === "x86_64-pc-windows-msvc" ? "*" : "";
      // Two spaces before a bare filename, one space + `*` before a binary-mode
      // filename: this is coreutils sha256sum's own output format, not a
      // convention this test invented.
      const separator = marker ? " " : "  ";
      const sums = Buffer.from(`${hash}${separator}${marker}dist/${tarName}\n`, "utf8");
      assert.doesNotThrow(() => verifyChecksum(tarball, tarName, sums));
    });
  }
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
