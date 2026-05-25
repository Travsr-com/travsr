/**
 * Extension entry point — wires MCP client, status bar, code lens, hover,
 * Activity Bar tree view, and first-run welcome page.
 */

import * as vscode from "vscode";
import { StdioMcpClient } from "./mcp";
import { createStatusBarItem } from "./status";
import {
  BlastRadiusCodeLensProvider,
  BLAST_RADIUS_SELECTOR,
} from "./codelens";
import { CallersHoverProvider, HOVER_SELECTOR } from "./hover";
import { TravsrTreeDataProvider } from "./tree";
import { showWelcome, showWelcomeIfFirstRun } from "./welcome";

export function activate(context: vscode.ExtensionContext): void {
  const binary =
    vscode.workspace
      .getConfiguration("travsr")
      .get<string>("binaryPath", "travsr");

  const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  const version: string =
    typeof context.extension.packageJSON?.version === "string"
      ? context.extension.packageJSON.version
      : "0.0.0";
  const mcp = new StdioMcpClient(binary, workspaceRoot, version);
  context.subscriptions.push({ dispose: () => mcp.dispose() });

  void mcp.connect();

  // Status bar (VSCODE-201)
  createStatusBarItem(context, mcp);

  // Code lens (VSCODE-202)
  const codeLensProvider = new BlastRadiusCodeLensProvider(mcp);
  context.subscriptions.push(
    vscode.languages.registerCodeLensProvider(
      BLAST_RADIUS_SELECTOR,
      codeLensProvider
    )
  );

  // Hover (VSCODE-203)
  const hoverProvider = new CallersHoverProvider(mcp);
  context.subscriptions.push(
    vscode.languages.registerHoverProvider(HOVER_SELECTOR, hoverProvider)
  );

  // Activity Bar tree view (VSCODE-204)
  const treeProvider = new TravsrTreeDataProvider(mcp, context);
  context.subscriptions.push(
    vscode.window.createTreeView("travsrGraph", {
      treeDataProvider: treeProvider,
      showCollapseAll: true,
    })
  );

  // Clear caches on save so providers re-query fresh data
  context.subscriptions.push(
    vscode.workspace.onDidSaveTextDocument(() => {
      codeLensProvider.clearCache();
      hoverProvider.clearCache();
      treeProvider.refresh();
    })
  );

  // First-run welcome page (VSCODE-204)
  showWelcomeIfFirstRun(context);

  // Commands
  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.showStatus", () => {
      const status = mcp.isConnected() ? "connected" : "disconnected";
      void vscode.window.showInformationMessage(`Travsr daemon: ${status}`);
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(
      "travsr.showBlastRadius",
      (file: string, files: string[]) => {
        const panel = vscode.window.createWebviewPanel(
          "travsrBlastRadius",
          `Blast radius — ${file}`,
          vscode.ViewColumn.Beside,
          { localResourceRoots: [] }
        );
        panel.webview.html = buildFileListHtml(
          `Blast radius for <code>${escHtml(file)}</code>`,
          files
        );
      }
    )
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(
      "travsr.showCallers",
      async (symbol: string) => {
        const raw = await mcp.callTool("get_callers", { symbol });
        const lines = raw
          .split("\n")
          .map((l) => l.trim())
          .filter(Boolean);
        const panel = vscode.window.createWebviewPanel(
          "travsrCallers",
          `Callers — ${symbol}`,
          vscode.ViewColumn.Beside,
          { localResourceRoots: [] }
        );
        panel.webview.html = buildFileListHtml(
          `Callers of <code>${escHtml(symbol)}</code>`,
          lines
        );
      }
    )
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.showWelcome", () => showWelcome())
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.refreshGraph", () =>
      treeProvider.refresh()
    )
  );
}

export function deactivate(): void {
  return;
}

function buildFileListHtml(title: string, items: string[]): string {
  const rows = items
    .map((f) => `<li style="font-family:monospace;padding:2px 0">${escHtml(f)}</li>`)
    .join("\n");
  return `<!DOCTYPE html><html><body style="padding:16px">
<h3>${title}</h3>
<ul style="margin:0;padding-left:20px">${rows || "<li><em>none</em></li>"}</ul>
</body></html>`;
}

function escHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}
