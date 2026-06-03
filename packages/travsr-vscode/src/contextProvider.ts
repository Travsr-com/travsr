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
// Push model: VS Code calls provideWorkspaceChatContext whenever the extension
// fires onDidChangeWorkspaceChatContext. Without the event, the provider is inert.

interface TravrsrChatContextItem {
  value: string;
  description?: string;
}

interface TravrsrChatContextProvider {
  onDidChangeWorkspaceChatContext: vscode.Event<void>;
  provideWorkspaceChatContext(): Promise<TravrsrChatContextItem[]>;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

const KEYWORDS = new Set([
  "import","export","from","const","let","var","function","class","return",
  "if","else","for","while","do","switch","case","break","continue","new",
  "this","super","extends","implements","interface","type","enum","namespace",
  "async","await","try","catch","finally","throw","typeof","instanceof","void",
  "null","undefined","true","false","default","static","public","private",
  "protected","readonly","abstract","override","declare","module","require",
  "fn","let","mut","use","mod","pub","struct","impl","trait","where","match",
  "ref","in","as","is","of","with","yield","delete","get","set",
]);

function getSymbolAtCursor(editor: vscode.TextEditor): string {
  const pos = editor.selection.active;
  const range = editor.document.getWordRangeAtPosition(pos, /[\w:.<>]+/);
  if (!range) return "";
  const word = editor.document.getText(range);
  return KEYWORDS.has(word) ? "" : word;
}

// ── Registration ──────────────────────────────────────────────────────────────

export function registerContextProvider(
  client: McpClient,
  context: vscode.ExtensionContext,
  channel: vscode.OutputChannel
): void {
  channel.appendLine(`[context] registerContextProvider called`);

  // Proposed API guard: degrade silently on VS Code < 1.99 or when Copilot Chat
  // is not installed. The cast to `any` is intentional — the stable @types/vscode
  // does not include this proposed API surface.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const chatNs = vscode.chat as any;
  if (typeof chatNs?.registerChatContextProvider !== "function") {
    channel.appendLine(`[context] registerChatContextProvider not available — proposed API missing or Copilot Chat not installed`);
    return;
  }
  channel.appendLine(`[context] registerChatContextProvider found — registering provider`);

  const emitter = new vscode.EventEmitter<void>();
  context.subscriptions.push(emitter);

  const provider: TravrsrChatContextProvider = {
    onDidChangeWorkspaceChatContext: emitter.event,

    async provideWorkspaceChatContext(): Promise<TravrsrChatContextItem[]> {
      const cfg = vscode.workspace.getConfiguration("travsr");
      if (!cfg.get<boolean>("autoContextEnabled", true)) {
        channel.appendLine(`[context] skipped — autoContextEnabled=false`);
        return [];
      }
      if (!client.isConnected()) {
        channel.appendLine(`[context] skipped — MCP client not connected`);
        return [];
      }

      const editor = vscode.window.activeTextEditor;
      if (!editor || editor.document.uri.scheme !== "file") return [];

      const symbol = getSymbolAtCursor(editor);
      const filePart = vscode.workspace.asRelativePath(editor.document.uri);
      const query = symbol || filePart;
      if (!query) return [];

      const budget = cfg.get<number>("contextTokenBudget", 2000);
      channel.appendLine(`[context] query="${query}" budget=${budget}`);

      const result = await client.callTool("get_context", {
        query,
        token_budget: String(budget),
      });

      if (!result || result.startsWith("No symbols")) {
        channel.appendLine(`[context] no results — skipped`);
        return [];
      }

      channel.appendLine(`[context] injected ${result.length} chars → Copilot Chat`);
      return [{ value: result, description: "Code graph context — Travsr" }];
    },
  };

  // API: registerChatContextProvider(extensionContext, name, provider)
  const disposable = chatNs.registerChatContextProvider(
    context.extension,
    "travsr.context",
    provider
  ) as vscode.Disposable;
  context.subscriptions.push(disposable);

  // Fire on active editor switch and file save — low-frequency, no debounce needed.
  context.subscriptions.push(
    vscode.window.onDidChangeActiveTextEditor(() => emitter.fire()),
    vscode.workspace.onDidSaveTextDocument(() => emitter.fire()),
  );
}
