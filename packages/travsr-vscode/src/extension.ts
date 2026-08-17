/**
 * Extension entry point — wires MCP client, status bar, code lens, hover,
 * Activity Bar tree view, first-run welcome page, binary auto-installer,
 * and actionable error toasts.
 */

import * as fs from "fs";
import * as cp from "child_process";
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
import { TravsrRepoFileTreeProvider } from "./repoFileTree";
import { showWelcome, showWelcomeIfFirstRun } from "./welcome";
import { GraphPanel } from "./graph";
import {
  installBinary,
  resolveOnPath,
  findCmdShimPath,
  resolveNpmShimExe,
  assertExecutableBinary,
  isDownloadSupported,
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
import { registerParityCommands, refreshOpenPanels, stripEnvelope } from "./commands";
import { ContextExplorerPanel, getSymbolAtCursor } from "./contextExplorer";
import { registerMcpServerCommand } from "./mcpRegister";
import { registerContextCodeAction } from "./contextCodeAction";

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

  let reporter = createTelemetryReporter(telemetryEnabled);
  if (reporter !== null) {
    context.subscriptions.push(reporter);
  }
  sendEvent(reporter, EVT_ACTIVATED);

  // Re-evaluate reporter when the user toggles global VS Code telemetry mid-session.
  context.subscriptions.push(
    vscode.env.onDidChangeTelemetryEnabled((enabled) => {
      reporter?.dispose();
      reporter = enabled && telemetryEnabled ? createTelemetryReporter(true) : null;
    })
  );

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

  const onDaemonFailed = (): void => sendEvent(reporter, EVT_DAEMON_FAILED);

  wireDisconnectHandler(rawClient, proxy, context, workspaceRoot, version, channel, onDaemonFailed);
  // An invalid binaryPath (e.g. an npm .cmd shim, #486) makes connect() reject
  // before spawning — log it; checkBinaryAndPrompt below runs the recovery.
  rawClient.connect().catch((e: unknown) => {
    const msg = e instanceof Error ? e.message : String(e);
    channel.appendLine(`Initial connect failed: ${msg}`);
  });

  // Watch for graph.db creation so the daemon reconnects automatically after
  // `travsr init` runs on a fresh repo, without requiring a window reload.
  if (workspaceRoot) {
    const dbWatcher = vscode.workspace.createFileSystemWatcher(
      new vscode.RelativePattern(workspaceRoot, ".travsr/graph.db")
    );
    let restartInProgress = false;
    dbWatcher.onDidCreate(() => {
      if (restartInProgress) return;
      restartInProgress = true;
      channel.appendLine("graph.db created — reconnecting Travsr daemon…");
      void doRestart(proxy, context, workspaceRoot, version, channel, onDaemonFailed).then(() => {
        restartInProgress = false;
        void vscode.window.showInformationMessage("Travsr: graph initialized — daemon reconnected.");
        refreshOpenPanels();
      });
    });
    // External `travsr init` updates graph.db in-place — refresh panels without restarting.
    dbWatcher.onDidChange(() => {
      codeLensProvider.clearCache();
      hoverProvider.clearCache();
      treeProvider.refresh();
      refreshOpenPanels();
    });
    context.subscriptions.push(dbWatcher);
  }

  // ITEM 2: the chatContextProvider proposed API is gone. Graph context now
  // reaches agents via the Context Explorer (ITEM 1), MCP registration
  // (travsr.registerMcpServer), and the copy-to-chat CodeAction (part 3) —
  // none of which require a proposed API at runtime.

  // Status bar (VSCODE-201) — reconnect-aware
  createStatusBarItem(context, proxy, (cb) => proxy.onReconnect(cb), statusBarPosition);

  // Code lens (VSCODE-202)
  const codeLensProvider = new BlastRadiusCodeLensProvider(proxy);
  context.subscriptions.push(
    codeLensProvider,
    vscode.languages.registerCodeLensProvider(BLAST_RADIUS_SELECTOR, codeLensProvider)
  );
  // ITEM 3: re-title high-blast lenses immediately when the risk threshold changes.
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("travsr.blastRiskThreshold")) codeLensProvider.refresh();
    })
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

  // Repo Files tree view — sidebar file browser; click → open Travsr Graph
  const repoFileTreeProvider = new TravsrRepoFileTreeProvider(proxy);
  context.subscriptions.push(
    vscode.window.createTreeView("travsrRepoFiles", {
      treeDataProvider: repoFileTreeProvider,
      showCollapseAll: true,
    })
  );

  const openFileGraph = (relPath: string) => {
    const panel = GraphPanel.show(proxy, context);
    void panel.query(relPath);
  };

  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.refreshRepoFiles", () => {
      repoFileTreeProvider.refresh();
    }),

    vscode.commands.registerCommand(
      "travsr.openFileGraph",
      (relPath: string) => openFileGraph(relPath)
    ),

    vscode.commands.registerCommand("travsr.searchRepoFile", () => {
      void repoFileTreeProvider.search(openFileGraph);
    })
  );

  // Clear caches on save so providers re-query fresh data
  context.subscriptions.push(
    vscode.workspace.onDidSaveTextDocument(() => {
      codeLensProvider.clearCache();
      hoverProvider.clearCache();
      treeProvider.refresh();
      repoFileTreeProvider.refresh();
    })
  );

  // Clear caches after daemon reconnect (binary install, restart, graph.db swap)
  // so the code lens and hover counts reflect the new graph immediately.
  context.subscriptions.push(
    proxy.onReconnect(() => {
      codeLensProvider.clearCache();
      hoverProvider.clearCache();
      treeProvider.refresh();
      repoFileTreeProvider.refresh();
    })
  );

  // First-run welcome page (VSCODE-204)
  showWelcomeIfFirstRun(context);

  // ── Commands ─────────────────────────────────────────────────────────────

  // Status bar click → Quick Pick (VSCODE-205)
  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.showStatus", async () => {
      type ItemId =
        | "graphStats"
        | "repos"
        | "languages"
        | "reindex"
        | "restart"
        | "settings"
        | "output"
        | "disable"
        | "close";
      type ActionItem = vscode.QuickPickItem & { id: ItemId };
      const items: vscode.QuickPickItem[] = [
        { label: "$(graph) Graph stats",              id: "graphStats" } as ActionItem,
        { label: "$(repo) Registered repos",          id: "repos"      } as ActionItem,
        { label: "$(extensions) Languages",           id: "languages"  } as ActionItem,
        { label: "$(sync) Re-index now",              id: "reindex"    } as ActionItem,
        { label: "", kind: vscode.QuickPickItemKind.Separator },
        { label: "$(refresh) Restart daemon",         id: "restart"  } as ActionItem,
        { label: "$(gear) Open settings",             id: "settings" } as ActionItem,
        { label: "$(output) Show output channel",     id: "output"   } as ActionItem,
        { label: "$(circle-slash) Disable extension", id: "disable"  } as ActionItem,
        { label: "", kind: vscode.QuickPickItemKind.Separator },
        { label: "$(close) Close",                    id: "close"    } as ActionItem,
      ];
      const pick = await vscode.window.showQuickPick(items, {
        placeHolder: "Travsr actions",
      });
      // Separator items cannot be selected; plain QuickPickItems have no id.
      if (!pick || !("id" in pick)) return;
      switch ((pick as ActionItem).id) {
        case "graphStats":
          await vscode.commands.executeCommand("travsr.showGraphStats");
          break;
        case "repos":
          await vscode.commands.executeCommand("travsr.showRepos");
          break;
        case "languages":
          await vscode.commands.executeCommand("travsr.showLanguages");
          break;
        case "reindex":
          await vscode.commands.executeCommand("travsr.reindexNow");
          break;
        case "restart":
          await doRestart(proxy, context, workspaceRoot, version, channel, onDaemonFailed);
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
        case "disable": {
          const answer = await vscode.window.showWarningMessage(
            "Disable Travsr extension?",
            { modal: true },
            "Disable"
          );
          if (answer !== "Disable") break;
          // VS Code does not expose a reliable programmatic self-disable API.
          // Navigate to the extension page so the user can click Disable there.
          await vscode.commands.executeCommand(
            "workbench.extensions.search",
            `@id:${context.extension.id}`
          );
          void vscode.window.showInformationMessage(
            'Right-click Travsr in the Extensions panel and select "Disable".'
          );
          break;
        }
        case "close":
          break;
      }
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(
      "travsr.showBlastRadius",
      async (file: string, files?: string[]) => {
        // files is pre-fetched when called from the code lens (arguments: [file, files]).
        // When called from the hover card markdown link only [file] is encoded in the URI,
        // so re-fetch here to avoid passing undefined to buildFileListHtml.
        const panel = vscode.window.createWebviewPanel(
          "travsrBlastRadius",
          `Blast radius — ${file}`,
          vscode.ViewColumn.Beside,
          { localResourceRoots: [], enableScripts: true }
        );

        panel.webview.html = buildFileListHtml(
          `Blast radius for <code>${escHtml(file)}</code>`,
          [],
          { mode: "tree-sitter", semanticAvailable: false, installHint: "", loading: true }
        );

        // Fetch lang status and initial blast radius in parallel.
        const [langStatusRaw, initialFiles] = await Promise.all([
          proxy.callTool("get_lang_status", { file }).catch(() => null as string | null),
          files
            ? Promise.resolve(files)
            : proxy
                .callTool("get_blast_radius", { file, analysis: "tree-sitter" })
                .then((r) => parseEnvelope(r))
                .catch(() => [] as string[]),
        ]);

        if (!panel.visible && files === undefined) return;

        interface LangStatus {
          language: string;
          builtin: boolean;
          semantic_available: boolean;
          install_hint: string;
        }
        let langStatus: LangStatus = {
          language: "unknown",
          builtin: false,
          semantic_available: false,
          install_hint: "",
        };
        try {
          if (langStatusRaw) {
            langStatus = JSON.parse(langStatusRaw) as LangStatus;
          }
        } catch {
          // malformed JSON: keep safe defaults
        }

        type AnalysisMode = "tree-sitter" | "semantic";
        let currentMode: AnalysisMode = "tree-sitter";
        let currentFiles = initialFiles;

        function render(): void {
          panel.webview.html = buildFileListHtml(
            `Blast radius for <code>${escHtml(file)}</code>`,
            currentFiles,
            {
              mode: currentMode,
              semanticAvailable: langStatus.semantic_available,
              installHint: langStatus.install_hint,
              loading: false,
            }
          );
        }

        render();

        panel.webview.onDidReceiveMessage(
          async (msg: { command: string; mode?: AnalysisMode }) => {
            if (msg.command !== "setMode" || !msg.mode) return;
            if (msg.mode === currentMode) return;
            if (msg.mode === "semantic" && !langStatus.semantic_available) return;

            currentMode = msg.mode;
            panel.webview.html = buildFileListHtml(
              `Blast radius for <code>${escHtml(file)}</code>`,
              currentFiles,
              {
                mode: currentMode,
                semanticAvailable: langStatus.semantic_available,
                installHint: langStatus.install_hint,
                loading: true,
              }
            );

            try {
              const raw = await proxy.callTool("get_blast_radius", {
                file,
                analysis: currentMode,
              });
              currentFiles = parseEnvelope(raw);
            } catch {
              currentFiles = [];
            }

            if (panel.visible) render();
          },
          undefined,
          context.subscriptions
        );
      }
    )
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(
      "travsr.showCallers",
      async (symbol: string) => {
        const raw = await proxy.callTool("get_callers", { symbol });
        const lines = parseEnvelope(raw);
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

  // Graph panel (VSCODE-245)
  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.showGraph", () => {
      const panel = GraphPanel.show(proxy, context);
      const activeEditor = vscode.window.activeTextEditor;
      if (activeEditor) {
        const rel = vscode.workspace.asRelativePath(
          activeEditor.document.fileName
        );
        void panel.query(rel);
      }
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.refreshGraph", () =>
      treeProvider.refresh()
    )
  );

  // CLI↔UI parity commands (VSCODE-247): askSymbol, manageSynonyms,
  // showDependencies, showExecutionPath, showRepos, showGraphStats, showLanguages.
  registerParityCommands(proxy, context, binary, () => {
    codeLensProvider.clearCache();
    hoverProvider.clearCache();
    treeProvider.refresh();
  });

  // ITEM 1: Context Explorer — first-class surface for get_context.
  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.openContextExplorer", () => {
      const panel = ContextExplorerPanel.show(proxy, context);
      const editor = vscode.window.activeTextEditor;
      if (editor) {
        const sym = getSymbolAtCursor(editor);
        // Approximate token count of the active file (chars / 4) for the token-saved bar.
        const fileTok = Math.round(editor.document.getText().length / 4);
        if (sym) panel.seedQuery(sym, fileTok);
      }
    })
  );

  // ITEM 2: MCP server registration for external agents. #498: the command
  // resolves and validates the binary itself at invocation time — the raw
  // activation-time fallback must never reach an external agent config.
  context.subscriptions.push(registerMcpServerCommand());

  // ITEM 2 (part 3): pull graph context onto the clipboard for chat agents that
  // can't take the MCP-register route (Copilot). Lightbulb CodeAction + palette.
  context.subscriptions.push(registerContextCodeAction(proxy));

  // ITEM 3: Explicit blast-before-edit command (context menu on a symbol).
  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.blastBeforeEdit", async () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor) {
        void vscode.window.showInformationMessage("Open a file to check blast radius.");
        return;
      }
      const file = vscode.workspace.asRelativePath(editor.document.uri, false);
      const raw = await proxy.callTool("get_blast_radius", { file });
      const files = parseEnvelope(raw);
      const count = files.length;
      if (count === 0) {
        void vscode.window.showInformationMessage(`Travsr: ${file} has no dependents — safe to edit.`);
        return;
      }
      const label = `${file}: ${count} file${count !== 1 ? "s" : ""} depend on this`;
      const action = await vscode.window.showWarningMessage(label, "Show Details", "OK");
      if (action === "Show Details") {
        await vscode.commands.executeCommand("travsr.showBlastRadius", file, files);
      }
    })
  );

  // ITEM 3: File-rename safety — warn when renaming a high-dependency file.
  context.subscriptions.push(
    vscode.workspace.onWillRenameFiles(async (e) => {
      const checks = e.files.map(async ({ oldUri }) => {
        const rel = vscode.workspace.asRelativePath(oldUri, false);
        const raw = await proxy.callTool("get_blast_radius", { file: rel });
        const deps = parseEnvelope(raw);
        if (deps.length > 0) {
          void vscode.window.showWarningMessage(
            `Travsr: renaming ${rel} may break ${deps.length} dependent file${deps.length !== 1 ? "s" : ""}.`,
            "Show Details"
          ).then((action) => {
            if (action === "Show Details") {
              void vscode.commands.executeCommand("travsr.showBlastRadius", rel, deps);
            }
          });
        }
      });
      await Promise.all(checks);
    })
  );

  // Re-index command — also reachable from the status Quick Pick. Lives here
  // (not commands.ts) because it needs the output channel + workspace root.
  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.reindexNow", async () => {
      await reindexNow(workspaceRoot, channel);
      // Graph has changed — evict stale counts and refresh all open panels.
      codeLensProvider.clearCache();
      hoverProvider.clearCache();
      treeProvider.refresh();
      refreshOpenPanels();
    })
  );

  // Manual download command — also reachable from the command palette
  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.downloadBinary", async () => {
      await runDownloadFlow(proxy, context, workspaceRoot, version, channel, onDaemonFailed);
    })
  );

  // Binary check on activation — async, non-blocking; providers are already
  // wired and degrade gracefully until the daemon connects.
  void checkBinaryAndPrompt(proxy, context, workspaceRoot, version, channel, configured, onDaemonFailed);
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
  configured: string,
  onDaemonFailed?: () => void
): Promise<void> {
  // 1. Explicit path configured → validate it, not just check existence.
  //    A path that exists but can't be spawned (e.g. an npm .cmd shim, #486)
  //    must fall through to recovery instead of dead-ending at connect().
  if (configured && configured !== "travsr" && fs.existsSync(configured)) {
    try {
      assertExecutableBinary(configured);
      return; // valid — nothing to do
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      channel.appendLine(`Configured travsr.binaryPath is not usable: ${msg}`);
      // npm installs ship the real native binary next to the shim — adopt it.
      const resolved = resolveNpmShimExe(configured);
      if (resolved) {
        channel.appendLine(`Resolved npm shim to native binary: ${resolved}`);
        await adoptBinary(resolved, proxy, context, workspaceRoot, version, channel, onDaemonFailed);
        void vscode.window.showInformationMessage(
          `Travsr: travsr.binaryPath pointed at an npm shim — switched to the native binary at ${resolved}.`
        );
        return;
      }
      // No packaged binary next to it — fall through to the install flow.
    }
  }

  // 2. Check ~/.travsr/bin (default install location). Persist the path and
  //    reconnect so the status bar turns green without a window reload.
  const installPath = resolveInstallPath(resolveInstallDir());
  if (fs.existsSync(installPath)) {
    await adoptBinary(installPath, proxy, context, workspaceRoot, version, channel, onDaemonFailed);
    return;
  }

  // 3. Check PATH — daemon may have been installed via npm, Homebrew, or
  //    cargo. #495: returning early on a bare hit left the client dead — it
  //    rejects the non-absolute "travsr" fallback — so capture the resolved
  //    absolute path, persist it, and reconnect (same pattern as step 2).
  //    resolveOnPath prefers .exe hits and skips .cmd/.bat shims; shim-only
  //    installs fall through to the npm-shim resolution in step 4 (#486).
  const onPath = resolveOnPath("travsr");
  if (onPath) {
    channel.appendLine(`Resolved travsr on PATH: ${onPath}`);
    await adoptBinary(onPath, proxy, context, workspaceRoot, version, channel, onDaemonFailed);
    return;
  }

  // 4. Windows npm install: only a .cmd shim on PATH, but the package ships
  //    the real exe next to it (#486) — adopt it instead of prompting.
  const cmdShim = findCmdShimPath("travsr");
  if (cmdShim) {
    const resolved = resolveNpmShimExe(cmdShim);
    if (resolved) {
      channel.appendLine(`Resolved npm shim on PATH to native binary: ${resolved}`);
      await adoptBinary(resolved, proxy, context, workspaceRoot, version, channel, onDaemonFailed);
      void vscode.window.showInformationMessage(
        `Travsr: using the native binary from your npm install at ${resolved}.`
      );
      return;
    }
  }

  // 5. Binary not found anywhere. #497: only offer the download when a
  //    prebuilt binary is actually published for this platform/arch —
  //    e.g. Windows-on-ARM releases don't exist, so the prompt used to
  //    lead straight into an HTTP 404.
  if (!isDownloadSupported()) {
    const msg =
      `Travsr binary not found, and no prebuilt binary is published for ` +
      `${process.platform}/${process.arch}. Build travsr from source and ` +
      `point travsr.binaryPath at it.`;
    channel.appendLine(msg);
    const action = await vscode.window.showWarningMessage(msg, "Open Settings");
    if (action === "Open Settings") {
      await vscode.commands.executeCommand(
        "workbench.action.openSettings",
        "travsr.binaryPath"
      );
    }
    return;
  }

  const promptMsg = cmdShim
    ? `travsr.cmd detected on PATH but the VS Code extension requires the native binary — Download v${DOWNLOAD_VERSION}?`
    : `Travsr binary not found — Download v${DOWNLOAD_VERSION}?`;
  const choice = await vscode.window.showInformationMessage(
    promptMsg,
    "Download",
    "Dismiss"
  );
  if (choice === "Download") {
    await runDownloadFlow(proxy, context, workspaceRoot, version, channel, onDaemonFailed);
  }
}

/**
 * #486: persist a resolved binary path to settings and hot-swap the MCP
 * client so the status bar turns green without requiring a window reload.
 */
async function adoptBinary(
  binPath: string,
  proxy: MutableMcpClientProxy,
  context: vscode.ExtensionContext,
  workspaceRoot: string | undefined,
  version: string,
  channel: vscode.OutputChannel,
  onDaemonFailed?: () => void
): Promise<void> {
  await vscode.workspace
    .getConfiguration("travsr")
    .update("binaryPath", binPath, vscode.ConfigurationTarget.Global);
  const newRaw = new StdioMcpClient(binPath, workspaceRoot, version);
  wireDisconnectHandler(newRaw, proxy, context, workspaceRoot, version, channel, onDaemonFailed);
  await newRaw.connect();
  proxy.swapAndDispose(newRaw);
}

async function runDownloadFlow(
  proxy: MutableMcpClientProxy,
  context: vscode.ExtensionContext,
  workspaceRoot: string | undefined,
  version: string,
  channel: vscode.OutputChannel,
  onDaemonFailed?: () => void
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

    // Persist + auto-reconnect — fires onReconnect → status bar re-polls.
    await adoptBinary(binPath, proxy, context, workspaceRoot, version, channel, onDaemonFailed);

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
  channel: vscode.OutputChannel,
  onDaemonFailed?: () => void
): Promise<void> {
  const configured =
    vscode.workspace.getConfiguration("travsr").get<string>("binaryPath", "") ?? "";
  const binary = configured || "travsr";
  channel.appendLine("Restarting Travsr daemon…");
  const newRaw = new StdioMcpClient(binary, workspaceRoot, version);
  wireDisconnectHandler(newRaw, proxy, context, workspaceRoot, version, channel, onDaemonFailed);
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
      await doRestart(proxy, context, workspaceRoot, version, channel, onDisconnect);
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

// Graph stats and registered-repos UIs moved to interactive webviews in
// commands.ts (travsr.showGraphStats / travsr.showRepos). `reindexNow` stays
// here because it needs the output channel + workspace root.

/**
 * VSCODE-247 #8: trigger a full re-index including Phase B semantic analysis.
 * Runs `travsr init` which walks all files, runs LSIF, and invokes any
 * registered Phase B language plugins — identical to what the user gets from
 * the terminal. This is required after installing a new language plugin so the
 * new semantic edges appear in the graph.
 */
async function reindexNow(
  workspaceRoot: string | undefined,
  channel: vscode.OutputChannel
): Promise<void> {
  if (!workspaceRoot) {
    void vscode.window.showWarningMessage("Open a workspace folder to re-index.");
    return;
  }
  const configured =
    vscode.workspace.getConfiguration("travsr").get<string>("binaryPath", "") ?? "";
  const binary = configured || "travsr";

  if (configured) {
    try {
      assertExecutableBinary(configured);
    } catch (e) {
      void vscode.window.showErrorMessage(`Travsr: invalid binaryPath — ${(e as Error).message}`);
      return;
    }
  }

  await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "Travsr: re-indexing…",
      cancellable: false,
    },
    () =>
      new Promise<void>((resolve) => {
        const proc = cp.spawn(binary, ["init"], {
          cwd: workspaceRoot,
          env: { ...process.env, TERM: "dumb", NO_COLOR: "1" },
        });
        proc.stdout?.on("data", (d: Buffer) => channel.appendLine(d.toString().trimEnd()));
        proc.stderr?.on("data", (d: Buffer) => channel.appendLine(d.toString().trimEnd()));
        const fail = (msg: string): void => {
          void vscode.window
            .showErrorMessage(`Travsr re-index failed: ${msg}`, "Show logs")
            .then((a) => {
              if (a === "Show logs") channel.show();
            });
          resolve();
        };
        proc.on("error", (e) => fail(e.message));
        proc.on("exit", (code) => {
          if (code === 0) {
            void vscode.window.showInformationMessage("Travsr re-index complete.");
          } else {
            fail(`exit code ${code ?? "unknown"}`);
          }
          resolve();
        });
      })
  );
}

interface FileListOpts {
  mode: "tree-sitter" | "semantic";
  semanticAvailable: boolean;
  installHint: string;
  loading?: boolean;
}

/** Strip the `<travsr-data>…</travsr-data>` MCP envelope and return trimmed non-empty lines. */
export function parseEnvelope(raw: string): string[] {
  return stripEnvelope(raw).split("\n").map((l) => l.trim()).filter(Boolean);
}

export function buildFileListHtml(
  title: string,
  items: string[],
  opts?: FileListOpts
): string {
  // When opts is absent (showCallers / legacy callers) render a plain list —
  // no toggle bar, no scripts. This keeps every existing caller working unchanged.
  if (!opts) {
    const rows = items
      .map((f) => `<li style="font-family:monospace;padding:2px 0">${escHtml(f)}</li>`)
      .join("\n");
    return `<!DOCTYPE html><html><body style="padding:16px">
<h3>${title}</h3>
<ul style="margin:0;padding-left:20px">${rows || "<li><em>none</em></li>"}</ul>
</body></html>`;
  }

  const { mode, semanticAvailable, installHint, loading = false } = opts;

  const pillBase =
    "display:inline-block;padding:4px 14px;border-radius:20px;font-size:12px;" +
    "font-family:var(--vscode-font-family,'Segoe UI',system-ui,sans-serif);" +
    "cursor:pointer;border:none;transition:background 120ms;";
  const activeStyle   = `${pillBase}background:#0078d4;color:#fff;font-weight:600;`;
  const inactiveStyle = `${pillBase}background:#3c3c3c;color:#ccc;`;
  const disabledStyle = `${pillBase}background:#555;color:#888;cursor:not-allowed;opacity:0.6;`;

  const tsPill = `<button id="pill-ts"
    style="${mode === "tree-sitter" ? activeStyle : inactiveStyle}"
    onclick="setMode('tree-sitter')"
    title="Basic analysis — single-file structure and best-effort calls (always available)"
    aria-pressed="${mode === "tree-sitter"}"
  >Basic</button>`;

  const semTooltip = semanticAvailable
    ? "Full analysis — precise cross-file calls and references"
    : installHint
      ? `Full analysis not yet available. ${installHint}`
      : "Full analysis not yet available for this language.";

  const semPill = semanticAvailable
    ? `<button id="pill-sem"
        style="${mode === "semantic" ? activeStyle : inactiveStyle}"
        onclick="setMode('semantic')"
        title="${escHtml(semTooltip)}"
        aria-pressed="${mode === "semantic"}"
      >Full</button>`
    : `<button id="pill-sem"
        style="${disabledStyle}"
        disabled
        title="${escHtml(semTooltip)}"
        aria-pressed="false"
        onclick="showInstallHint()"
      >Full ▾</button>`;

  const installHintHtml = installHint
    ? `<div id="install-hint" style="display:none;margin-top:8px;padding:8px 12px;` +
      `background:#2d2d00;border:1px solid #888600;border-radius:6px;` +
      `font-size:12px;color:#e0c060;font-family:monospace;">${escHtml(installHint)}</div>`
    : "";

  const toggleBar = `
<div style="display:flex;align-items:center;gap:8px;margin-bottom:14px;flex-wrap:wrap;">
  <span style="font-size:11px;color:#999;margin-right:4px;">Analysis:</span>
  ${tsPill}
  ${semPill}
</div>
${installHintHtml}`;

  const bodyHtml = loading
    ? `<p style="color:#aaa;font-style:italic;">Loading…</p>`
    : `<ul style="margin:0;padding-left:20px">${
        items
          .map((f) => `<li style="font-family:monospace;padding:2px 0">${escHtml(f)}</li>`)
          .join("\n") || "<li><em>none</em></li>"
      }</ul>`;

  const script = `<script>
    const vscode = acquireVsCodeApi();
    function setMode(m) { vscode.postMessage({ command: 'setMode', mode: m }); }
    function showInstallHint() {
      const el = document.getElementById('install-hint');
      if (el) el.style.display = el.style.display === 'none' ? 'block' : 'none';
    }
  </script>`;

  return `<!DOCTYPE html><html lang="en">
<head>
  <meta charset="UTF-8">
  <meta http-equiv="Content-Security-Policy"
        content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
</head>
<body style="background:#1e1e1e;color:#d4d4d4;font-family:var(--vscode-font-family,'Segoe UI',system-ui,sans-serif);padding:16px;font-size:13px">
  <h3 style="margin:0 0 12px">${title}</h3>
  ${toggleBar}
  ${bodyHtml}
  ${script}
</body></html>`;
}

function escHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}
