/**
 * VSCODE-204: First-run welcome WebView panel.
 *
 * Shown once on first activation via context.globalState. Can be re-opened
 * at any time via the travsr.showWelcome command.
 */

import * as vscode from "vscode";

const WELCOME_SHOWN_KEY = "travsr.welcomeShown";

// Module-level ref so re-running the command reveals the existing panel.
let currentPanel: vscode.WebviewPanel | undefined;

export function showWelcomeIfFirstRun(context: vscode.ExtensionContext): void {
  if (context.globalState.get<boolean>(WELCOME_SHOWN_KEY, false)) return;
  void context.globalState.update(WELCOME_SHOWN_KEY, true);
  showWelcome();
}

export function showWelcome(): void {
  if (currentPanel) {
    currentPanel.reveal(vscode.ViewColumn.One);
    return;
  }
  currentPanel = vscode.window.createWebviewPanel(
    "travsrWelcome",
    "Welcome to Travsr",
    vscode.ViewColumn.One,
    { localResourceRoots: [], enableScripts: false }
  );
  currentPanel.onDidDispose(() => { currentPanel = undefined; });
  currentPanel.webview.html = getHtml();
}

function getHtml(): string {
  return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline';">
  <title>Welcome to Travsr</title>
  <style>
    body {
      font-family: var(--vscode-font-family);
      color: var(--vscode-foreground);
      background: var(--vscode-editor-background);
      padding: 48px 40px;
      max-width: 680px;
      margin: 0 auto;
      line-height: 1.6;
    }
    h1 { font-size: 28px; margin: 0 0 4px; }
    .tagline { color: var(--vscode-descriptionForeground); font-size: 15px; margin: 0 0 32px; }
    h2 { font-size: 15px; margin: 28px 0 8px; text-transform: uppercase; letter-spacing: 0.06em; color: var(--vscode-descriptionForeground); }
    ul { padding-left: 0; list-style: none; margin: 0; }
    li { padding: 5px 0; }
    li::before { content: "→ "; color: var(--vscode-textLink-foreground); font-weight: bold; }
    code {
      background: var(--vscode-textCodeBlock-background);
      padding: 2px 6px;
      border-radius: 3px;
      font-family: var(--vscode-editor-font-family);
      font-size: 13px;
    }
    .command-block {
      background: var(--vscode-textCodeBlock-background);
      border-left: 3px solid var(--vscode-textLink-foreground);
      padding: 10px 14px;
      margin: 12px 0;
      border-radius: 0 4px 4px 0;
      font-family: var(--vscode-editor-font-family);
      font-size: 13px;
    }
    .links { margin-top: 36px; display: flex; gap: 20px; flex-wrap: wrap; }
    a { color: var(--vscode-textLink-foreground); text-decoration: none; }
    a:hover { text-decoration: underline; }
    hr { border: none; border-top: 1px solid var(--vscode-widget-border); margin: 32px 0; }
  </style>
</head>
<body>
  <h1>Travsr</h1>
  <p class="tagline">The code graph that lives next to git.</p>

  <p>
    Travsr builds a deterministic graph of your codebase on every git commit.
    Instead of guessing from text chunks, your AI tools traverse real structure —
    call edges, import edges, type references — and return exactly the context
    they need.
  </p>

  <h2>Features in this extension</h2>
  <ul>
    <li><strong>Status bar</strong> — live graph health at the bottom of VS Code</li>
    <li><strong>Blast radius code lens</strong> — how many files break if this file changes</li>
    <li><strong>Callers hover</strong> — hover any symbol to see what calls it</li>
    <li><strong>Graph panel</strong> — Activity Bar view with live dependencies and callers for the active symbol</li>
  </ul>

  <h2>Getting started</h2>
  <p>Install the Travsr CLI and initialise your repo:</p>
  <div class="command-block">npm install -g travsr</div>
  <div class="command-block">cd your-repo &amp;&amp; travsr init</div>
  <p>The status bar turns green when the graph is ready. The Activity Bar panel updates as you move your cursor.</p>

  <hr>

  <div class="links">
    <a href="https://travsr.com">travsr.com</a>
    <a href="https://docs.travsr.com">Documentation</a>
    <a href="https://github.com/raj-rkv/travsr/issues">Report an issue</a>
    <a href="https://github.com/raj-rkv/travsr/discussions">Discussions</a>
  </div>
</body>
</html>`;
}
