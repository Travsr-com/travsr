/**
 * Extension entry point — wires MCP client, status bar, code lens, hover,
 * Activity Bar tree view, first-run welcome page, binary auto-installer,
 * and actionable error toasts.
 */

import * as fs from "fs";
import * as vscode from "vscode";
import { StdioMcpClient } from "./mcp";
import { MutableMcpClientProxy } from "./clientProxy";
import { createStatusBarItem } from "./status";
import {
  BlastRadiusCodeLensProvider,
  BLAST_RADIUS_SELECTOR,
} from "./codelens";
import { CallersHoverProvider, HOVER_SELECTOR } from "./hover";
import { TravsrTreeDataProvider } from "./tree";
import { showWelcome, showWelcomeIfFirstRun } from "./welcome";
import {
  installBinary,
  checkOnPath,
  resolveInstallDir,
  resolveInstallPath,
  DOWNLOAD_VERSION,
} from "./installer";
import {
  createTelemetryReporter,
  sendEvent,
  EVT_ACTIVATED,
  EVT_MCP_INVOKED,
  EVT_DAEMON_FAILED,
} from "./telemetry";

export function activate(context: vscode.ExtensionContext): void {
  const channel = vscode.window.createOutputChannel("Travsr");
  context.subscriptions.push(channel);

  const cfg = vscode.workspace.getConfiguration("travsr");
  const configured = cfg.get<string>("binaryPath", "") ?? "";
  const binary = configured || "travsr";
  const statusBarPosition = cfg.get<"left" | "right">("statusBarPosition", "left");
  const cloudEndpoint = cfg.get<string>("cloudEndpoint", "") ?? "";
  const telemetryEnabled = cfg.get<boolean>("telemetry.enabled", false) ?? false;

  if (cloudEndpoint) {
    channel.appendLine(`Cloud endpoint configured: ${cloudEndpoint}`);
  }

  const reporter = createTelemetryReporter(telemetryEnabled);
  if (reporter !== null) {
    context.subscriptions.push(reporter);
  }
  sendEvent(reporter, EVT_ACTIVATED);

  const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  const version: string =
    typeof context.extension.packageJSON?.version === "string"
      ? context.extension.packageJSON.version
      : "0.0.0";

  // Create the raw MCP client and wrap it in a mutable proxy so all
  // providers can be wired once and survive a binary install + reconnect.
  const rawClient = new StdioMcpClient(binary, workspaceRoot, version);
  const proxy = new MutableMcpClientProxy(rawClient);
  context.subscriptions.push({ dispose: () => proxy.dispose() });

  proxy.setOnInvoke((name) => sendEvent(reporter, EVT_MCP_INVOKED, { tool: name }));

  wireDisconnectHandler(
    rawClient, proxy, context, workspaceRoot, version, channel,
    () => sendEvent(reporter, EVT_DAEMON_FAILED)
  );
  void rawClient.connect();

  // Status bar (VSCODE-201) — reconnect-aware
  createStatusBarItem(context, proxy, (cb) => proxy.onReconnect(cb), statusBarPosition);

  // Code lens (VSCODE-202)
  const codeLensProvider = new BlastRadiusCodeLensProvider(proxy);
  context.subscriptions.push(
    vscode.languages.registerCodeLensProvider(BLAST_RADIUS_SELECTOR, codeLensProvider)
  );

  // Hover (VSCODE-203)
  const hoverProvider = new CallersHoverProvider(proxy);
  context.subscriptions.push(
    vscode.languages.registerHoverProvider(HOVER_SELECTOR, hoverProvider)
  );

  // Activity Bar tree view (VSCODE-204)
  const treeProvider = new TravsrTreeDataProvider(proxy, context);
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

  // ── Commands ─────────────────────────────────────────────────────────────

  // Status bar click → Quick Pick (VSCODE-205)
  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.showStatus", async () => {
      type ItemId = "restart" | "settings" | "output" | "disable";
      const items: (vscode.QuickPickItem & { id: ItemId })[] = [
        { label: "$(refresh) Restart daemon",         id: "restart" },
        { label: "$(gear) Open settings",             id: "settings" },
        { label: "$(output) Show output channel",     id: "output" },
        { label: "$(circle-slash) Disable extension", id: "disable" },
      ];
      const pick = await vscode.window.showQuickPick(items, {
        placeHolder: "Travsr actions",
      });
      if (!pick) return;
      const id = pick.id;
      switch (id) {
        case "restart":
          await doRestart(proxy, context, workspaceRoot, version, channel);
          break;
        case "settings":
          await vscode.commands.executeCommand(
            "workbench.action.openSettings",
            "travsr"
          );
          break;
        case "output":
          channel.show();
          break;
        case "disable":
          await vscode.commands.executeCommand(
            "workbench.extensions.action.disableExtension",
            context.extension.id
          );
          break;
      }
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
        const raw = await proxy.callTool("get_callers", { symbol });
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

  // Manual download command — also reachable from the command palette
  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.downloadBinary", async () => {
      await runDownloadFlow(proxy, context, workspaceRoot, version, channel);
    })
  );

  // Binary check on activation — async, non-blocking; providers are already
  // wired and degrade gracefully until the daemon connects.
  void checkBinaryAndPrompt(proxy, context, workspaceRoot, version, channel, configured);
}

export function deactivate(): void {
  return;
}

// ── Helpers ───────────────────────────────────────────────────────────────

async function checkBinaryAndPrompt(
  proxy: MutableMcpClientProxy,
  context: vscode.ExtensionContext,
  workspaceRoot: string | undefined,
  version: string,
  channel: vscode.OutputChannel,
  configured: string
): Promise<void> {
  // 1. Explicit path configured and exists on disk → nothing to do.
  if (configured && configured !== "travsr" && fs.existsSync(configured)) return;

  // 2. Check ~/.travsr/bin (default install location).
  const installPath = resolveInstallPath(resolveInstallDir());
  if (fs.existsSync(installPath)) {
    // Found at default location — persist the path and reconnect so the
    // status bar turns green without requiring a window reload.
    await vscode.workspace
      .getConfiguration("travsr")
      .update("binaryPath", installPath, vscode.ConfigurationTarget.Global);
    const newRaw = new StdioMcpClient(installPath, workspaceRoot, version);
    wireDisconnectHandler(newRaw, proxy, context, workspaceRoot, version, channel);
    await newRaw.connect();
    proxy.swapAndDispose(newRaw);
    return;
  }

  // 3. Check PATH — daemon may have been installed via npm or Homebrew.
  if (checkOnPath("travsr")) return;

  // 4. Binary not found anywhere — prompt once.
  const choice = await vscode.window.showInformationMessage(
    `Travsr binary not found — Download v${DOWNLOAD_VERSION}?`,
    "Download",
    "Dismiss"
  );
  if (choice === "Download") {
    await runDownloadFlow(proxy, context, workspaceRoot, version, channel);
  }
}

async function runDownloadFlow(
  proxy: MutableMcpClientProxy,
  context: vscode.ExtensionContext,
  workspaceRoot: string | undefined,
  version: string,
  channel: vscode.OutputChannel
): Promise<void> {
  try {
    const binPath = await vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: `Installing Travsr v${DOWNLOAD_VERSION}…`,
        cancellable: false,
      },
      (progress) =>
        installBinary(DOWNLOAD_VERSION, (msg) => {
          channel.appendLine(msg);
          progress.report({ message: msg });
        })
    );

    await vscode.workspace
      .getConfiguration("travsr")
      .update("binaryPath", binPath, vscode.ConfigurationTarget.Global);

    // Auto-reconnect: spin up a new client with the installed binary.
    const newRaw = new StdioMcpClient(binPath, workspaceRoot, version);
    wireDisconnectHandler(newRaw, proxy, context, workspaceRoot, version, channel);
    await newRaw.connect();
    proxy.swapAndDispose(newRaw); // fires onReconnect → status bar re-polls

    void vscode.window.showInformationMessage(
      `Travsr v${DOWNLOAD_VERSION} installed successfully.`
    );
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    channel.appendLine(`[ERROR] Install failed: ${msg}`);
    void vscode.window
      .showErrorMessage(
        `Travsr download failed: ${msg}`,
        "Show logs"
      )
      .then((action) => {
        if (action === "Show logs") channel.show();
      });
  }
}

async function doRestart(
  proxy: MutableMcpClientProxy,
  context: vscode.ExtensionContext,
  workspaceRoot: string | undefined,
  version: string,
  channel: vscode.OutputChannel
): Promise<void> {
  const configured =
    vscode.workspace.getConfiguration("travsr").get<string>("binaryPath", "") ?? "";
  const binary = configured || "travsr";
  channel.appendLine("Restarting Travsr daemon…");
  const newRaw = new StdioMcpClient(binary, workspaceRoot, version);
  wireDisconnectHandler(newRaw, proxy, context, workspaceRoot, version, channel);
  await newRaw.connect();
  proxy.swapAndDispose(newRaw);
  channel.appendLine("Travsr daemon restarted.");
}

function wireDisconnectHandler(
  client: StdioMcpClient,
  proxy: MutableMcpClientProxy,
  context: vscode.ExtensionContext,
  workspaceRoot: string | undefined,
  version: string,
  channel: vscode.OutputChannel,
  onDisconnect?: () => void
): void {
  const sub = client.onDisconnect(async () => {
    sub.dispose(); // one-shot — prevents double-firing on explicit restart
    onDisconnect?.();
    const action = await vscode.window.showWarningMessage(
      "Travsr daemon offline",
      "Restart",
      "Configure",
      "Show logs"
    );
    if (action === "Restart") {
      await doRestart(proxy, context, workspaceRoot, version, channel);
    } else if (action === "Configure") {
      await vscode.commands.executeCommand(
        "workbench.action.openSettings",
        "travsr.binaryPath"
      );
    } else if (action === "Show logs") {
      channel.show();
    }
  });
}

function buildFileListHtml(title: string, items: string[]): string {
  const rows = items
    .map(
      (f) =>
        `<li style="font-family:monospace;padding:2px 0">${escHtml(f)}</li>`
    )
    .join("\n");
  return `<!DOCTYPE html><html><body style="padding:16px">
<h3>${title}</h3>
<ul style="margin:0;padding-left:20px">${rows || "<li><em>none</em></li>"}</ul>
</body></html>`;
}

function escHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}
