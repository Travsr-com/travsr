/**
 * Unit tests for src/mcpRegister.ts.
 *
 * Validates config-merge logic in isolation — no filesystem I/O.
 */

import * as assert from "assert";
import { mergeServerEntry } from "../../mcpRegister";

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

  test("claude_desktop: idempotent — overwrites existing travsr entry", () => {
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
