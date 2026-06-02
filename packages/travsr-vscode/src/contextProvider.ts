/**
 * RFC-012 A2 F4: ambient context provider for Copilot Chat.
 *
 * Fires before every Copilot Chat turn. Builds a combined query from the
 * symbol under the cursor + the last user message, calls get_context via
 * the existing MCP client, and silently injects the result as chat context.
 * Zero explicit invocation required from the developer.
 *
 * Guard: isConnected()===false → returns [] immediately (zero degradation).
 *
 * Note: vscode.chat.registerChatContextProvider is a VS Code proposed API
 * (chatContextProvider proposal). Types are declared locally so the extension
 * compiles against the stable @types/vscode without needing vscode.proposed.d.ts.
 * The runtime guard ensures graceful no-op on VS Code < 1.99.
 */

import * as vscode from "vscode";
import type { McpClient } from "./mcp";

// ── Minimal local types for the proposed ChatContextProvider API ──────────────

interface TravrsrChatContextItem {
  value: string;
  description?: string;
}

interface TravrsrChatContextProviderContext {
  messages: readonly vscode.LanguageModelChatMessage[];
}

interface TravrsrChatContextProvider {
  provideContext(
    ctx: TravrsrChatContextProviderContext,
    token: vscode.CancellationToken
  ): Promise<TravrsrChatContextItem[]>;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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

// ── Registration ──────────────────────────────────────────────────────────────

export function registerContextProvider(
  client: McpClient,
  context: vscode.ExtensionContext
): void {
  // Proposed API guard: degrade silently on VS Code < 1.99 or when Copilot Chat
  // is not installed. The cast to `any` is intentional — the stable @types/vscode
  // does not include this proposed API surface.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const chatNs = vscode.chat as any;
  if (typeof chatNs?.registerChatContextProvider !== "function") {
    return;
  }

  const provider: TravrsrChatContextProvider = {
    async provideContext(
      ctx: TravrsrChatContextProviderContext,
      _token: vscode.CancellationToken
    ): Promise<TravrsrChatContextItem[]> {
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

      return [{ value: result, description: "Code graph context — Travsr" }];
    },
  };

  const disposable = chatNs.registerChatContextProvider(
    "travsr.context",
    provider
  ) as vscode.Disposable;
  context.subscriptions.push(disposable);
}
