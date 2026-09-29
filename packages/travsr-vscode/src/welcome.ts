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
    .links { margin-top: 36px; display: flex; gap: 20px; flex-wrap: wrap; }
    a { color: var(--vscode-textLink-foreground); text-decoration: none; }
    a:hover { text-decoration: underline; }
    hr { border: none; border-top: 1px solid var(--vscode-widget-border); margin: 32px 0; }
  </style>
</head>
<body>
  <h1>Travsr</h1>
  <p class="tagline">Your AI sees how your code connects.</p>

  <p>
    Travsr reads your project and keeps track of what calls what. Your AI asks
    it instead of guessing from text, and it stays up to date on every commit.
  </p>

  <h2>Get started</h2>
  <ul>
    <li>Open a folder that is a Git project.</li>
    <li>Click <strong>Set up</strong> when Travsr asks, or run <strong>Travsr: Re-index Now</strong> from the Command Palette.</li>
    <li>Wait for <strong>Ready.</strong> Claude Code and Cursor are connected for you.</li>
  </ul>
  <p>If a language needs something installed first, <strong>Travsr: Health</strong> lists it with the one thing to do.</p>

  <h2>In the editor</h2>
  <ul>
    <li><strong>Status bar</strong>: whether this project is ready and up to date</li>
    <li><strong>Above each file</strong>: how many files are affected if it changes</li>
    <li><strong>Hover a function</strong>: what calls it</li>
    <li><strong>Travsr view in the Activity Bar</strong>: what the current function calls and what calls it</li>
  </ul>

  <p>Prefer the terminal? Run <code>travsr init</code> in your project.</p>

  <hr>

  <div class="links">
    <a href="https://travsr.com">travsr.com</a>
    <a href="https://docs.travsr.com">Documentation</a>
    <a href="https://github.com/Travsr-com/travsr/issues">Report an issue</a>
    <a href="https://github.com/Travsr-com/travsr/discussions">Discussions</a>
  </div>
</body>
</html>`;
}
