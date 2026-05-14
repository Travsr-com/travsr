import * as vscode from "vscode";

export function createStatusBarItem(
  context: vscode.ExtensionContext
): vscode.StatusBarItem {
  const item = vscode.window.createStatusBarItem(
    vscode.StatusBarAlignment.Right,
    100
  );
  item.text = "Travsr: idle";
  item.tooltip = "Travsr daemon status";
  item.command = "travsr.showStatus";
  item.show();
  context.subscriptions.push(item);
  return item;
}
