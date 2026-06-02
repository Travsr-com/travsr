/**
 * RFC-012 A2 F4: ambient context provider for Copilot Chat.
 *
 * Fires before every Copilot Chat turn. Builds a combined query from the
 * symbol under the cursor + the last user message, calls get_context via
 * the existing MCP client, and silently injects the result as chat context.
 * Zero explicit invocation required from the developer.
 *
 * Guard: isConnected()===false → returns [] immediately (zero degradation).
 */

import * as vscode from "vscode";
import type { McpClient } from "./mcp";

function getSymbolAtCursor(editor: vscode.TextEditor): string {
  const pos = editor.selection.active;
  const range = editor.document.getWordRangeAtPosition(pos, /[\w:.<>]+/);
  return range ? editor.document.getText(range) : "";
}

function lastUserMessage(
  messages: readonly vscode.LanguageModelChatMessage[]
): string {
  for (let i = messages.length - 1; i >= 0; i--) {
    const msg = messages[i];
    if (msg.role === vscode.LanguageModelChatMessageRole.User) {
      const c = msg.content;
      if (typeof c === "string") return c;
      if (Array.isArray(c)) {
        return (c as Array<{ value?: string }>)
          .filter((p) => typeof p.value === "string")
          .map((p) => p.value as string)
          .join(" ");
      }
    }
  }
  return "";
}

export function registerContextProvider(
  client: McpClient,
  context: vscode.ExtensionContext
): void {
  if (typeof vscode.chat?.registerChatContextProvider !== "function") {
    // VS Code < 1.99 — API not available; degrade silently.
    return;
  }

  const provider = vscode.chat.registerChatContextProvider(
    "travsr.context",
    {
      async provideContext(
        ctx: vscode.ChatContextProviderContext,
        _token: vscode.CancellationToken
      ): Promise<vscode.ChatContext[]> {
        const cfg = vscode.workspace.getConfiguration("travsr");
        if (!cfg.get<boolean>("autoContextEnabled", true)) return [];
        if (!client.isConnected()) return [];

        const userMsg = lastUserMessage(ctx.messages);
        if (!userMsg.trim()) return [];

        const editor = vscode.window.activeTextEditor;
        const symbol = editor ? getSymbolAtCursor(editor) : "";
        const filePart = editor
          ? vscode.workspace.asRelativePath(editor.document.uri)
          : "";
        const anchor = symbol || filePart;
        const query = anchor ? `${anchor} ${userMsg}` : userMsg;

        const budget = cfg.get<number>("contextTokenBudget", 2000);
        const result = await client.callTool("get_context", {
          query,
          token_budget: String(budget),
        });

        if (!result || result.startsWith("No symbols")) return [];

        return [
          {
            value: result,
            description: "Code graph context — Travsr",
          } as vscode.ChatContext,
        ];
      },
    }
  );
  context.subscriptions.push(provider);
}
