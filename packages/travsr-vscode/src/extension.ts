import * as vscode from "vscode";
import { createStatusBarItem } from "./status";

export function activate(context: vscode.ExtensionContext): void {
  createStatusBarItem(context);

  const showStatus = vscode.commands.registerCommand(
    "travsr.showStatus",
    () => {
      void vscode.window.showInformationMessage(
        "Travsr: scaffold - daemon wiring lands Sprint 3."
      );
    }
  );

  context.subscriptions.push(showStatus);
}

export function deactivate(): void {
  return;
}
