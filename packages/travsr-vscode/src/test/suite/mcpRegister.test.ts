/**
 * Unit tests for src/mcpRegister.ts.
 *
 * Validates config-merge logic in isolation — no filesystem I/O.
 */

import * as assert from "assert";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { mergeServerEntry, resolveExportBinaryPath } from "../../mcpRegister";
import { resolveInstallDir, resolveInstallPath } from "../../installer";

const BINARY = "/usr/local/bin/travsr";

suite("mcpRegister: mergeServerEntry", () => {
  test("claude_desktop: creates mcpServers when absent", () => {
    const merged = mergeServerEntry({}, "claude_desktop", BINARY);
    assert.deepStrictEqual(merged["mcpServers"], {
      travsr: { command: BINARY, args: ["mcp", "--stdio"] },
    });
  });

  test("claude_desktop: preserves existing servers, adds travsr", () => {
    const existing = {
      mcpServers: { otherTool: { command: "/bin/other", args: [] } },
    };
    const merged = mergeServerEntry(existing, "claude_desktop", BINARY);
    const servers = merged["mcpServers"] as Record<string, unknown>;
    assert.ok("otherTool" in servers, "existing server must be preserved");
    assert.ok("travsr" in servers, "travsr entry must be added");
  });

  test("claude_desktop: idempotent, overwrites existing travsr entry", () => {
    const existing = {
      mcpServers: { travsr: { command: "/old/travsr", args: ["mcp"] } },
    };
    const merged = mergeServerEntry(existing, "claude_desktop", BINARY);
    const servers = merged["mcpServers"] as Record<string, unknown>;
    assert.deepStrictEqual(servers["travsr"], { command: BINARY, args: ["mcp", "--stdio"] });
  });

  test("cursor_mcp: creates mcpServers when absent", () => {
    const merged = mergeServerEntry({}, "cursor_mcp", BINARY);
    assert.deepStrictEqual(merged["mcpServers"], {
      travsr: { command: BINARY, args: ["mcp", "--stdio"] },
    });
  });

  test("continue: appends to list, removes old travsr entry", () => {
    const existing = {
      mcpServers: [
        { name: "travsr", command: "/old/travsr", args: [] },
        { name: "other", command: "/bin/other", args: [] },
      ],
    };
    const merged = mergeServerEntry(existing, "continue", BINARY);
    const list = merged["mcpServers"] as Array<Record<string, unknown>>;
    const travsrEntries = list.filter((e) => e["name"] === "travsr");
    assert.strictEqual(travsrEntries.length, 1, "exactly one travsr entry");
    assert.strictEqual(travsrEntries[0]["command"], BINARY);
    assert.ok(list.some((e) => e["name"] === "other"), "other entry preserved");
  });

  test("does not mutate the original config object", () => {
    const original = { mcpServers: { other: { command: "/bin/other" } } };
    const originalCopy = JSON.stringify(original);
    mergeServerEntry(original, "claude_desktop", BINARY);
    assert.strictEqual(JSON.stringify(original), originalCopy, "original must be unchanged");
  });
});

// ── #498: resolveExportBinaryPath — export boundary validation ─────────────

suite("#498: mcpRegister, resolveExportBinaryPath", () => {
  // Host-semantics-safe absolute paths (assertExecutableBinary checks
  // absoluteness with the host's path.isAbsolute).
  const abs = (...segs: string[]): string => path.resolve(os.tmpdir(), ...segs);
  const exeName = process.platform === "win32" ? "travsr.exe" : "travsr";
  const never = (): null => null;
  const noneExist = (): boolean => false;
  const installPath = resolveInstallPath(resolveInstallDir());

  test("valid configured absolute path is exported as-is", () => {
    const configured = abs("custom", exeName);
    assert.strictEqual(
      resolveExportBinaryPath(configured, process.platform, noneExist, never, never),
      configured
    );
  });

  test("configured npm .cmd shim is substituted with the packaged binary (win32)", () => {
    // npm .cmd shims are a Windows-only artifact; on POSIX platform rules a
    // .cmd path is spawnable as-is, so pin win32 semantics explicitly.
    const shim = abs("npm-prefix", "travsr.cmd");
    const packaged = path.join(
      path.dirname(shim), "node_modules", "@travsr.com", "travsr", "bin", "travsr.exe"
    );
    const existsFn = (p: string): boolean => p === packaged;
    assert.strictEqual(
      resolveExportBinaryPath(shim, "win32", existsFn, never, never),
      packaged
    );
  });

  test("bare 'travsr' configured value is never exported, falls to install dir", () => {
    const existsFn = (p: string): boolean => p === installPath;
    assert.strictEqual(
      resolveExportBinaryPath("travsr", process.platform, existsFn, never, never),
      installPath
    );
  });

  test("empty configured value falls to the default install location", () => {
    const existsFn = (p: string): boolean => p === installPath;
    assert.strictEqual(
      resolveExportBinaryPath("", process.platform, existsFn, never, never),
      installPath
    );
  });

  test("falls back to PATH resolution when nothing is installed", () => {
    const onPath = abs("on-path", exeName);
    assert.strictEqual(
      resolveExportBinaryPath("", process.platform, noneExist, () => onPath, never),
      onPath
    );
  });

  test("npm shim on PATH is substituted with the packaged binary", () => {
    const shim = abs("npm-on-path", "travsr.cmd");
    const packaged = path.join(
      path.dirname(shim), "node_modules", "@travsr.com", "travsr", "bin", exeName
    );
    const existsFn = (p: string): boolean => p === packaged;
    assert.strictEqual(
      resolveExportBinaryPath("", process.platform, existsFn, never, () => shim),
      packaged
    );
  });

  test("returns null when no spawnable binary exists anywhere", () => {
    assert.strictEqual(
      resolveExportBinaryPath("", process.platform, noneExist, never, never),
      null
    );
    assert.strictEqual(
      resolveExportBinaryPath("travsr", process.platform, noneExist, never, never),
      null
    );
  });

  test("defaults exercise the real fs path without throwing", () => {
    // Smoke test with real fs.existsSync and PATH lookups — must not throw
    // regardless of what is installed on the machine running the suite.
    assert.doesNotThrow(() => resolveExportBinaryPath(""));
  });

  test("uses injected existsFn, never the real filesystem, for the shim substitute", () => {
    // win32 semantics so the .cmd shim is rejected and substitution is
    // attempted; packaged binary "missing" per existsFn → null, no fs access.
    const shim = abs("npm-prefix-2", "travsr.cmd");
    assert.strictEqual(
      resolveExportBinaryPath(shim, "win32", noneExist, never, never),
      null
    );
    assert.ok(!fs.existsSync(shim), "test precondition: shim does not really exist");
  });
});
