/**
 * Unit tests for src/contextCodeAction.ts (parity plan §ITEM 2 part 3).
 *
 * Proves the lightbulb offers the pull action on a symbol but not on a language
 * keyword, and that `copyContextForChat` writes the clipboard only when the
 * daemon returns content (never clobbers on empty/aborted results).
 */

import * as assert from "assert";
import * as vscode from "vscode";
import {
  ContextChatCodeActionProvider,
  copyContextForChat,
} from "../../contextCodeAction";

function mockClient(response: string): import("../../mcp").McpClient {
  return {
    callTool: async () => response,
    isConnected: () => true,
    dispose: () => {},
  };
}

async function docWith(content: string, language = "typescript"): Promise<vscode.TextDocument> {
  return vscode.workspace.openTextDocument({ content, language });
}

/** Position of the first character of `needle` in the document. */
function posOf(doc: vscode.TextDocument, needle: string): vscode.Position {
  return doc.positionAt(doc.getText().indexOf(needle));
}

suite("contextCodeAction: provideCodeActions", () => {
  const provider = new ContextChatCodeActionProvider();

  test("offers the copy action on a resolvable symbol", async () => {
    const doc = await docWith("function PaymentService() {}\n");
    const pos = posOf(doc, "PaymentService");
    const actions = provider.provideCodeActions(doc, new vscode.Range(pos, pos));
    assert.strictEqual(actions.length, 1);
    assert.ok(actions[0].title.includes("PaymentService"));
    assert.strictEqual(actions[0].command?.command, "travsr.copyContextForChat");
    assert.deepStrictEqual(actions[0].command?.arguments, ["PaymentService"]);
  });

  test("offers nothing on a language keyword", async () => {
    const doc = await docWith("function PaymentService() {}\n");
    const pos = posOf(doc, "function");
    const actions = provider.provideCodeActions(doc, new vscode.Range(pos, pos));
    assert.strictEqual(actions.length, 0);
  });

  test("offers nothing on whitespace / no word", async () => {
    const doc = await docWith("   \n");
    const pos = new vscode.Position(0, 1);
    const actions = provider.provideCodeActions(doc, new vscode.Range(pos, pos));
    assert.strictEqual(actions.length, 0);
  });
});

suite("contextCodeAction: copyContextForChat", () => {
  test("writes the returned context block to the clipboard", async () => {
    await vscode.env.clipboard.writeText("SENTINEL");
    await copyContextForChat(mockClient("node A\nnode B"), "PaymentService");
    const clip = await vscode.env.clipboard.readText();
    assert.ok(clip.includes("PaymentService"), "header should name the symbol");
    assert.ok(clip.includes("node A"), "body should carry the daemon output");
    assert.ok(!clip.includes("SENTINEL"), "clipboard should be replaced");
  });

  test("does not clobber the clipboard when the daemon returns empty", async () => {
    await vscode.env.clipboard.writeText("KEEP_ME");
    await copyContextForChat(mockClient("   "), "PaymentService");
    const clip = await vscode.env.clipboard.readText();
    assert.strictEqual(clip, "KEEP_ME", "empty result must leave clipboard untouched");
  });
});
