import * as assert from "assert";
import * as vscode from "vscode";
import { createStatusBarItem } from "../../status";
import type { McpClient } from "../../mcp";

function makeContext(): vscode.ExtensionContext {
  return { subscriptions: [] } as unknown as vscode.ExtensionContext;
}

function makeMcp(): McpClient {
  return {
    callTool: async () => "",
    isConnected: () => false,
    dispose: () => undefined,
  };
}

// Drain subscriptions to clear the setInterval started by createStatusBarItem's poll().
function drain(ctx: vscode.ExtensionContext): void {
  for (const s of (ctx as unknown as { subscriptions: { dispose(): void }[] }).subscriptions) {
    s.dispose();
  }
}

// ── Status bar position parameter ─────────────────────────────────────────

suite("S17-5: status, createStatusBarItem position parameter", () => {
  test("position='right' produces StatusBarAlignment.Right", () => {
    const ctx = makeContext();
    const item = createStatusBarItem(ctx, makeMcp(), undefined, "right");
    assert.strictEqual(item.alignment, vscode.StatusBarAlignment.Right);
    item.dispose();
    drain(ctx);
  });

  test("position='left' produces StatusBarAlignment.Left", () => {
    const ctx = makeContext();
    const item = createStatusBarItem(ctx, makeMcp(), undefined, "left");
    assert.strictEqual(item.alignment, vscode.StatusBarAlignment.Left);
    item.dispose();
    drain(ctx);
  });

  test("omitting position defaults to StatusBarAlignment.Left", () => {
    const ctx = makeContext();
    const item = createStatusBarItem(ctx, makeMcp());
    assert.strictEqual(item.alignment, vscode.StatusBarAlignment.Left);
    item.dispose();
    drain(ctx);
  });

  // For Left alignment: higher priority = further left (100 = near left edge).
  // For Right alignment: higher priority = further left of the right group (10 = near right edge).
  test("position='left' has priority 100", () => {
    const ctx = makeContext();
    const item = createStatusBarItem(ctx, makeMcp(), undefined, "left");
    assert.strictEqual(item.priority, 100);
    item.dispose();
    drain(ctx);
  });

  test("position='right' has priority 10 (near right edge)", () => {
    const ctx = makeContext();
    const item = createStatusBarItem(ctx, makeMcp(), undefined, "right");
    assert.strictEqual(item.priority, 10);
    item.dispose();
    drain(ctx);
  });

  test("command is 'travsr.showStatus' regardless of position", () => {
    const ctx = makeContext();
    const item = createStatusBarItem(ctx, makeMcp(), undefined, "right");
    assert.strictEqual(item.command, "travsr.showStatus");
    item.dispose();
    drain(ctx);
  });
});

// ── parseGraphStats / get_graph_stats integration ─────────────────────────

suite("S224: status, parseGraphStats reads node count from get_graph_stats", () => {
  test("node count is parsed from 'nodes: N' line", async () => {
    let called = "";
    const mcp: import("../../mcp").McpClient = {
      callTool: async (name: string) => {
        called = name;
        return "nodes: 2121\nedges: 9847\n";
      },
      isConnected: () => true,
      dispose: () => undefined,
    };
    const ctx = makeContext();
    const item = createStatusBarItem(ctx, mcp);
    // Allow the initial poll() to run.
    await new Promise((r) => setTimeout(r, 50));
    assert.strictEqual(called, "get_graph_stats");
    // Status bar formats large numbers with locale separators (e.g. "2,121").
    const text = (item.text as string).replace(/,/g, "");
    assert.ok(text.includes("2121"), `expected 2121 in "${item.text}"`);
    item.dispose();
    drain(ctx);
  });

  test("node count is 0 when response is empty", async () => {
    const mcp: import("../../mcp").McpClient = {
      callTool: async () => "",
      isConnected: () => true,
      dispose: () => undefined,
    };
    const ctx = makeContext();
    const item = createStatusBarItem(ctx, mcp);
    await new Promise((r) => setTimeout(r, 50));
    // Empty response → lastStats stays null → status bar shows connecting/error text, not a stale 2121.
    assert.ok(!(item.text as string).includes("2121"));
    item.dispose();
    drain(ctx);
  });
});

// ── saveDebounce — status bar uses get_graph_stats, not get_repo_map ───────

suite("S17-6: status, poll calls get_graph_stats after save", () => {
  test("callTool is invoked with 'get_graph_stats', never 'get_repo_map'", async () => {
    const calls: string[] = [];
    const mcp: import("../../mcp").McpClient = {
      callTool: async (name: string) => { calls.push(name); return "nodes: 42\n"; },
      isConnected: () => true,
      dispose: () => undefined,
    };
    const ctx = makeContext();
    const item = createStatusBarItem(ctx, mcp);
    await new Promise((r) => setTimeout(r, 50));
    assert.ok(calls.every((c) => c !== "get_repo_map"), "get_repo_map must never be called");
    assert.ok(calls.includes("get_graph_stats"), "get_graph_stats must be called");
    item.dispose();
    drain(ctx);
  });
});
