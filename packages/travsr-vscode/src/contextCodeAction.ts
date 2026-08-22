/**
 * ITEM 2 (parity plan §ITEM 2 part 3) — "Add Travsr context to chat" CodeAction.
 *
 * The Copilot/inline-chat context-injection proposed API (`chatContextProvider`)
 * cannot ship to Marketplace users, so there is no supported way to *push* graph
 * context into another agent's chat programmatically. The honest, working
 * mechanism is a **pull**: resolve the symbol under the cursor, run `get_context`
 * (the same PPR+knapsack+KNN pipeline the Context Explorer uses), and place the
 * result on the clipboard for the user to paste into any chat. No proposed API,
 * works in every install.
 *
 * Exposed as a lightbulb CodeAction on a symbol plus a command-palette command.
 */

import * as vscode from "vscode";
import type { McpClient } from "./mcp";
import { getSymbolAt, getSymbolAtCursor } from "./contextExplorer";
import { stripEnvelope } from "./commands";
import { BLAST_RADIUS_SELECTOR } from "./codelens";

const COPY_COMMAND = "travsr.copyContextForChat";

/** Token budget for the pulled context, shared with the Context Explorer default. */
function contextTokenBudget(): number {
  const raw = vscode.workspace.getConfiguration("travsr").get<number>("contextTokenBudget", 2000);
  return Number.isFinite(raw) && raw >= 500 ? Math.floor(raw) : 2000;
}

/**
 * Offers "Travsr: copy graph context for chat" when the invocation range sits on
 * a resolvable symbol. Registered on the same language set as the blast lens.
 */
export class ContextChatCodeActionProvider implements vscode.CodeActionProvider {
  static readonly providedKinds = [vscode.CodeActionKind.QuickFix];

  provideCodeActions(
    document: vscode.TextDocument,
    range: vscode.Range | vscode.Selection
  ): vscode.CodeAction[] {
    const symbol = getSymbolAt(document, range.start);
    if (!symbol) return [];

    const action = new vscode.CodeAction(
      `Travsr: copy graph context for "${symbol}"`,
      vscode.CodeActionKind.QuickFix
    );
    action.command = {
      title: "Copy graph context for chat",
      command: COPY_COMMAND,
      arguments: [symbol],
    };
    return [action];
  }
}

/**
 * Run `get_context` for a symbol and copy the formatted block to the clipboard.
 * `symbol` is provided by the CodeAction; when invoked from the palette it falls
 * back to the symbol under the active cursor.
 *
 * The copied text is the raw `get_context` body verbatim (only the SEC-001
 * `<travsr-data>` envelope is stripped). RFC-022 §14: when match-source grouping
 * is on the body carries `## exact / ## semantic / ## relevant` section headers —
 * this is the intended agent-facing format, so it flows through unchanged and the
 * pasted context tells the downstream model how confidently each node matched.
 */
export async function copyContextForChat(client: McpClient, symbol?: string): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  const query = (symbol ?? (editor ? getSymbolAtCursor(editor) : "")).trim();
  if (!query) {
    void vscode.window.showInformationMessage(
      "Travsr: place the cursor on a symbol to pull graph context."
    );
    return;
  }

  const budget = contextTokenBudget();
  let raw: string;
  try {
    // get_context runs the full PPR+KNN pipeline (20–30 s on large repos) and the
    // Rust server is sequential, so use the same 120 s ceiling as the Explorer and
    // a cancellable progress notification.
    raw = await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: `Travsr: pulling context for "${query}"…`, cancellable: true },
      (_progress, token) => {
        const ctrl = new AbortController();
        token.onCancellationRequested(() => ctrl.abort());
        return client.callTool("get_context", { query, token_budget: String(budget) }, ctrl.signal, 120_000);
      }
    );
  } catch (err) {
    void vscode.window.showWarningMessage(
      `Travsr: could not fetch context for "${query}", ${err instanceof Error ? err.message : String(err)}`
    );
    return;
  }

  const body = stripEnvelope(raw).trim();
  if (!body) {
    // Aborted, empty, or daemon unavailable — don't clobber the clipboard silently.
    void vscode.window.showWarningMessage(`Travsr: no context returned for "${query}".`);
    return;
  }

  const block = `Travsr graph context for "${query}":\n\n${body}\n`;
  await vscode.env.clipboard.writeText(block);
  void vscode.window.showInformationMessage(
    `Travsr: graph context for "${query}" copied, paste into your chat.`
  );
}

/** Wire the CodeAction provider and its backing command. */
export function registerContextCodeAction(client: McpClient): vscode.Disposable {
  return vscode.Disposable.from(
    vscode.languages.registerCodeActionsProvider(
      BLAST_RADIUS_SELECTOR,
      new ContextChatCodeActionProvider(),
      { providedCodeActionKinds: ContextChatCodeActionProvider.providedKinds }
    ),
    vscode.commands.registerCommand(COPY_COMMAND, (symbol?: string) => copyContextForChat(client, symbol))
  );
}
