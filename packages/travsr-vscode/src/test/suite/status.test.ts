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

// ── Status bar position parameter ─────────────────────────────────────────

suite("S17-5: status — createStatusBarItem position parameter", () => {
  test("position='right' produces StatusBarAlignment.Right", () => {
    const ctx = makeContext();
    const item = createStatusBarItem(ctx, makeMcp(), undefined, "right");
    assert.strictEqual(item.alignment, vscode.StatusBarAlignment.Right);
    item.dispose();
  });

  test("position='left' produces StatusBarAlignment.Left", () => {
    const ctx = makeContext();
    const item = createStatusBarItem(ctx, makeMcp(), undefined, "left");
    assert.strictEqual(item.alignment, vscode.StatusBarAlignment.Left);
    item.dispose();
  });

  test("omitting position defaults to StatusBarAlignment.Left", () => {
    const ctx = makeContext();
    const item = createStatusBarItem(ctx, makeMcp());
    assert.strictEqual(item.alignment, vscode.StatusBarAlignment.Left);
    item.dispose();
  });

  test("position='right' priority is 100 (unchanged)", () => {
    const ctx = makeContext();
    const item = createStatusBarItem(ctx, makeMcp(), undefined, "right");
    assert.strictEqual(item.priority, 100);
    item.dispose();
  });

  test("command is 'travsr.showStatus' regardless of position", () => {
    const ctx = makeContext();
    const item = createStatusBarItem(ctx, makeMcp(), undefined, "right");
    assert.strictEqual(item.command, "travsr.showStatus");
    item.dispose();
  });
});
