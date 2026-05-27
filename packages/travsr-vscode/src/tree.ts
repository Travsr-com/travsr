/**
 * VSCODE-204: travsrGraph Activity Bar tree view.
 *
 * Two subtrees for the active file / cursor symbol:
 *   Dependencies — get_dependencies(activeFile)
 *   Callers      — get_callers(wordAtCursor)
 *
 * Refreshes on active editor change (both subtrees) and on text-editor
 * selection change (callers subtree only, debounced to avoid thrashing).
 * Degrades gracefully when the daemon is unavailable.
 */

import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import type { McpClient } from "./mcp";

// ── tree node types ────────────────────────────────────────────────────────

type SectionKind = "deps" | "callers";

class SectionNode extends vscode.TreeItem {
  constructor(readonly kind: SectionKind) {
    super(
      kind === "deps" ? "Dependencies" : "Callers",
      vscode.TreeItemCollapsibleState.Expanded
    );
    this.contextValue = "travsrSection";
  }
}

class EntryNode extends vscode.TreeItem {
  constructor(label: string, description: string, filePath?: string) {
    super(label, vscode.TreeItemCollapsibleState.None);
    this.description = description;
    this.tooltip = filePath ?? label;
    if (filePath) {
      this.command = {
        command: "vscode.open",
        title: "Open file",
        arguments: [vscode.Uri.file(filePath)],
      };
      this.resourceUri = vscode.Uri.file(filePath);
    }
  }
}

class PlaceholderNode extends vscode.TreeItem {
  constructor(message: string) {
    super(message, vscode.TreeItemCollapsibleState.None);
    this.contextValue = "travsrPlaceholder";
  }
}

export type TreeNode = SectionNode | EntryNode | PlaceholderNode;

// ── provider ───────────────────────────────────────────────────────────────

export class TravsrTreeDataProvider
  implements vscode.TreeDataProvider<TreeNode>
{
  private readonly _onDidChangeTreeData = new vscode.EventEmitter<
    TreeNode | undefined | void
  >();
  readonly onDidChangeTreeData = this._onDidChangeTreeData.event;

  private activeFile: string | undefined;
  private activeSymbol: string | undefined;

  // Lazy caches — cleared on refresh.
  private depCache: EntryNode[] | undefined;
  private callerCache: EntryNode[] | undefined;
  private selectionDebounce: ReturnType<typeof setTimeout> | undefined;

  constructor(
    private readonly mcp: McpClient,
    context: vscode.ExtensionContext
  ) {
    const editor = vscode.window.activeTextEditor;
    if (editor) {
      this.activeFile = vscode.workspace.asRelativePath(editor.document.uri, false);
    }

    context.subscriptions.push(
      vscode.window.onDidChangeActiveTextEditor((editor) => {
        this.activeFile = editor
          ? vscode.workspace.asRelativePath(editor.document.uri, false)
          : undefined;
        this.activeSymbol = undefined;
        this.depCache = undefined;
        this.callerCache = undefined;
        this._onDidChangeTreeData.fire();
      }),

      // Debounced selection change — only re-fetch callers, not deps.
      vscode.window.onDidChangeTextEditorSelection((e) => {
        const word = e.textEditor.document.getWordRangeAtPosition(
          e.selections[0].active
        );
        const sym = word
          ? e.textEditor.document.getText(word)
          : undefined;
        if (!sym || sym === this.activeSymbol) return;
        this.activeSymbol = sym;
        this.callerCache = undefined;
        if (this.selectionDebounce) clearTimeout(this.selectionDebounce);
        this.selectionDebounce = setTimeout(() => {
          this._onDidChangeTreeData.fire();
        }, 300);
      }),

      { dispose: () => { clearTimeout(this.selectionDebounce); this._onDidChangeTreeData.dispose(); } }
    );
  }

  /** Called by extension.ts when the daemon reconnects or a file is saved. */
  refresh(): void {
    this.depCache = undefined;
    this.callerCache = undefined;
    this._onDidChangeTreeData.fire();
  }

  getTreeItem(element: TreeNode): vscode.TreeItem {
    return element;
  }

  async getChildren(element?: TreeNode): Promise<TreeNode[]> {
    if (!element) {
      return [new SectionNode("deps"), new SectionNode("callers")];
    }

    if (!(element instanceof SectionNode)) return [];

    if (element.kind === "deps") {
      if (!this.activeFile) return [new PlaceholderNode("No active file")];
      if (!this.depCache) this.depCache = await this.loadDeps(this.activeFile);
      return this.depCache.length > 0
        ? this.depCache
        : [new PlaceholderNode("No dependencies found")];
    }

    // callers section
    if (!this.activeSymbol) {
      return [new PlaceholderNode("Move cursor to a symbol")];
    }
    if (!this.callerCache) {
      this.callerCache = await this.loadCallers(this.activeSymbol);
    }
    return this.callerCache.length > 0
      ? this.callerCache
      : [new PlaceholderNode("No callers found")];
  }

  // ── private data fetchers ──────────────────────────────────────────────

  private async loadDeps(file: string): Promise<EntryNode[]> {
    try {
      const raw = await this.mcp.callTool("get_dependencies", { file });
      const lines = raw.split("\n").map((l) => l.trim()).filter(Boolean);
      return lines.map((line) => {
        // Format: "import:./mcp", "import:@scope/pkg", "type-import:./bar"
        const m = /^([^:]+):(.+)$/.exec(line);
        const kind = m?.[1] ?? "import";
        const dep = (m?.[2] ?? line).trim();
        const filePath = dep.startsWith(".")
          ? this.resolveLocalDep(file, dep)
          : undefined;
        return new EntryNode(dep, kind, filePath);
      });
    } catch {
      return [];
    }
  }

  private async loadCallers(symbol: string): Promise<EntryNode[]> {
    try {
      const raw = await this.mcp.callTool("get_callers", { symbol });
      const lines = raw.split("\n").map((l) => l.trim()).filter(Boolean);
      return lines.map((line) => {
        // Format: "[call] fn:bar (function) — src/bar.ts"
        const m = /^(\[[^\]]+\])\s+(\S+)(?:\s+\([^)]+\))?\s+—\s+(.+)$/.exec(
          line
        );
        if (m) {
          const [, edgeKind, sym, filePart] = m;
          const wsRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
          const filePath = wsRoot
            ? path.join(wsRoot, filePart.trim())
            : undefined;
          return new EntryNode(sym, `${edgeKind} ${filePart.trim()}`, filePath);
        }
        return new EntryNode(line, "", undefined);
      });
    } catch {
      return [];
    }
  }

  private resolveLocalDep(fromFile: string, dep: string): string | undefined {
    const wsRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    if (!wsRoot) return undefined;
    const base = path.resolve(wsRoot, path.dirname(fromFile), dep);
    for (const ext of [".ts", ".tsx", ".js", ".jsx"]) {
      if (fs.existsSync(base + ext)) return base + ext;
    }
    return undefined; // dep could not be resolved — no navigation is better than a wrong path
  }
}
