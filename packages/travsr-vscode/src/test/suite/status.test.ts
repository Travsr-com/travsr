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

suite("S17-5: status — createStatusBarItem position parameter", () => {
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
