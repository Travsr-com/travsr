/**
 * Repo Files sidebar tree view.
 *
 * Three-or-more-level hierarchy:
 *   top-level package (DirNode, prefix="pkg/")
 *     └── sub-directory  (DirNode, prefix="pkg/api/")
 *           └── sub-sub  (DirNode, prefix="pkg/api/pod/")
 *                 └── file.go  (FileNode)
 *
 * Data strategy: one MCP call per top-level package fetches ALL files under it
 * (mode=overview + path_prefix). Sub-directory expansion is then pure in-memory
 * grouping — no additional MCP calls. This keeps the tree responsive even for
 * packages with thousands of files.
 *
 * Search: travsr.searchRepoFile opens a VS Code QuickPick over all indexed files
 * with native fuzzy matching. Files are loaded lazily per package.
 */

import * as vscode from "vscode";
import type { McpClient } from "./mcp";

// ── Shared file info ───────────────────────────────────────────────────────

interface FileInfo {
  relPath: string; // e.g. "pkg/api/pod/util.go"
  label: string;   // e.g. "util.go"
}

function fileName(relPath: string): string {
  const i = relPath.lastIndexOf("/");
  return i === -1 ? relPath : relPath.slice(i + 1);
}

/** Guard against path traversal in MCP-returned node paths. */
function isSafePath(p: string): boolean {
  return !!p && !p.includes("..") && !p.startsWith("/") && !p.startsWith("\\");
}

// ── Tree node types ────────────────────────────────────────────────────────

export class DirNode extends vscode.TreeItem {
  constructor(
    /** Full path prefix including trailing slash, e.g. "pkg/api/pod/" */
    readonly prefix: string,
    /**
     * Top-level package prefix used as the cache key, e.g. "pkg/".
     * Sub-dirs share the same cache as their root package.
     */
    readonly topPkg: string,
    displayName: string,
    fileCount: number
  ) {
    super(displayName, vscode.TreeItemCollapsibleState.Collapsed);
    this.description = `${fileCount}`;
    this.tooltip = `${prefix} (${fileCount} files)`;
    this.iconPath = new vscode.ThemeIcon("folder");
    this.contextValue = "travsrDir";
  }
}

export class FileNode extends vscode.TreeItem {
  constructor(readonly relPath: string) {
    super(fileName(relPath), vscode.TreeItemCollapsibleState.None);
    this.tooltip = relPath;
    this.iconPath = new vscode.ThemeIcon("file-code");
    this.contextValue = "travsrFile";
    this.command = {
      command: "travsr.openFileGraph",
      title: "Open in Travsr Graph",
      arguments: [relPath],
    };
  }
}

class PlaceholderNode extends vscode.TreeItem {
  constructor(msg: string) {
    super(msg, vscode.TreeItemCollapsibleState.None);
    this.contextValue = "travsrPlaceholder";
    this.iconPath = new vscode.ThemeIcon("info");
  }
}

export type RepoFileNode = DirNode | FileNode | PlaceholderNode;

// ── Parsed shapes from get_graph_json ─────────────────────────────────────

interface OverviewNode {
  id: string;
  label?: string;
  kind: string;
  file_count?: number;
  path?: string;
  ghost?: boolean;
}

interface OverviewData {
  nodes?: OverviewNode[];
}

// ── Provider ───────────────────────────────────────────────────────────────

export class TravsrRepoFileTreeProvider
  implements vscode.TreeDataProvider<RepoFileNode>
{
  private readonly _onDidChangeTreeData = new vscode.EventEmitter<
    RepoFileNode | undefined | void
  >();
  readonly onDidChangeTreeData = this._onDidChangeTreeData.event;

  // Root-level package list cache.
  private rootCache: DirNode[] | undefined;

  // Per top-level package: prefix → all files under it.
  // Sub-dirs do NOT add separate entries — they filter their topPkg's list.
  private filesByPkg = new Map<string, FileInfo[]>();

  constructor(private readonly mcp: McpClient) {}

  refresh(): void {
    this.rootCache = undefined;
    this.filesByPkg.clear();
    this._onDidChangeTreeData.fire();
  }

  getTreeItem(element: RepoFileNode): vscode.TreeItem {
    return element;
  }

  async getChildren(element?: RepoFileNode): Promise<RepoFileNode[]> {
    if (!element) return this.loadRoot();
    if (element instanceof DirNode) return this.getDirChildren(element);
    return [];
  }

  // ── Search support ────────────────────────────────────────────────────────

  /**
   * Open a QuickPick search over all indexed files.
   * Called by the travsr.searchRepoFile command (search button in the view title).
   */
  async search(openGraph: (relPath: string) => void): Promise<void> {
    const qp = vscode.window.createQuickPick();
    qp.placeholder = "Search indexed files…";
    qp.matchOnDescription = true;
    qp.busy = true;
    qp.show();

    // Load all files — use cached data where available, fetch missing pkgs
    try {
      const pkgs = await this.ensureRootLoaded();
      const allFiles = await this.loadAllFiles(pkgs, (done, total) => {
        qp.busy = done < total;
      });

      qp.items = allFiles.map((f) => ({
        label: f.label,
        description: f.relPath,
        detail: f.relPath,
      }));
      qp.busy = false;

      qp.onDidAccept(() => {
        const sel = qp.selectedItems[0];
        if (sel?.description) {
          openGraph(sel.description);
        }
        qp.dispose();
      });

      qp.onDidHide(() => qp.dispose());
    } catch {
      qp.dispose();
      void vscode.window.showWarningMessage(
        "Travsr: file search unavailable — daemon not connected"
      );
    }
  }

  // ── Private loaders ──────────────────────────────────────────────────────

  private async loadRoot(): Promise<RepoFileNode[]> {
    if (this.rootCache) return this.rootCache;

    let data: OverviewData;
    try {
      const raw = await this.mcp.callTool("get_graph_json", {
        query: "",
        direction: "both",
        depth: "2",
        kind_filter: "",
        mode: "overview",
        path_prefix: "",
      });
      data = raw ? (JSON.parse(raw) as OverviewData) : {};
    } catch {
      return [new PlaceholderNode("Travsr daemon unavailable")];
    }

    const pkgNodes = (data.nodes ?? []).filter(
      (n) => n.kind === "pkg" && !n.ghost
    );

    if (!pkgNodes.length) {
      return [new PlaceholderNode("No indexed files — run travsr init")];
    }

    pkgNodes.sort((a, b) => (b.file_count ?? 0) - (a.file_count ?? 0));

    this.rootCache = pkgNodes.map((n) => {
      const name = n.label ?? n.id.replace(/^pkg:/, "");
      const prefix = name === "(root)" ? "" : name + "/";
      return new DirNode(prefix, prefix, name, n.file_count ?? 0);
    });
    return this.rootCache;
  }

  /** Return all files for a DirNode's sub-tree, grouping into child nodes. */
  private async getDirChildren(dir: DirNode): Promise<RepoFileNode[]> {
    const files = await this.ensureFilesForPkg(dir.topPkg, dir.prefix);
    if (!files.length) return [new PlaceholderNode(`Empty`)];
    return this.buildChildNodes(files, dir.prefix, dir.topPkg);
  }

  /**
   * Fetch and cache all files for a top-level package.
   * For sub-dirs, filter from the already-cached top-level list.
   */
  private async ensureFilesForPkg(
    topPkg: string,
    prefix: string
  ): Promise<FileInfo[]> {
    // If we have the top-level pkg cached, filter it for the requested prefix.
    if (this.filesByPkg.has(topPkg)) {
      const all = this.filesByPkg.get(topPkg)!;
      return prefix === topPkg
        ? all
        : all.filter((f) => f.relPath.startsWith(prefix));
    }

    // Fetch the entire top-level package in one shot.
    let data: OverviewData;
    try {
      const raw = await this.mcp.callTool("get_graph_json", {
        query: "",
        direction: "both",
        depth: "2",
        kind_filter: "",
        mode: "overview",
        path_prefix: topPkg,
      });
      data = raw ? (JSON.parse(raw) as OverviewData) : {};
    } catch {
      return [];
    }

    const files: FileInfo[] = (data.nodes ?? [])
      .filter((n) => n.kind === "file" && !n.ghost && n.path && isSafePath(n.path))
      .map((n) => ({ relPath: n.path!, label: fileName(n.path!) }))
      .sort((a, b) => a.relPath.localeCompare(b.relPath));

    this.filesByPkg.set(topPkg, files);

    return prefix === topPkg
      ? files
      : files.filter((f) => f.relPath.startsWith(prefix));
  }

  /**
   * Group a flat file list into directory and file child nodes.
   * Files directly at `prefix` level → FileNode.
   * Files deeper → grouped by next path segment → DirNode.
   */
  private buildChildNodes(
    files: FileInfo[],
    prefix: string,
    topPkg: string
  ): RepoFileNode[] {
    const subdirs = new Map<string, number>(); // segment → count
    const directFiles: FileInfo[] = [];

    for (const f of files) {
      const rel = f.relPath.slice(prefix.length);
      const slash = rel.indexOf("/");
      if (slash === -1) {
        directFiles.push(f);
      } else {
        const seg = rel.slice(0, slash);
        subdirs.set(seg, (subdirs.get(seg) ?? 0) + 1);
      }
    }

    const nodes: RepoFileNode[] = [];

    // Directories first, sorted
    for (const [seg, count] of [...subdirs.entries()].sort(([a], [b]) =>
      a.localeCompare(b)
    )) {
      nodes.push(new DirNode(prefix + seg + "/", topPkg, seg, count));
    }

    // Files at this level, sorted
    directFiles.sort((a, b) => a.label.localeCompare(b.label));
    for (const f of directFiles) {
      nodes.push(new FileNode(f.relPath));
    }

    return nodes;
  }

  // ── Search helpers ────────────────────────────────────────────────────────

  private async ensureRootLoaded(): Promise<DirNode[]> {
    if (!this.rootCache) await this.loadRoot();
    return this.rootCache ?? [];
  }

  private async loadAllFiles(
    pkgs: DirNode[],
    onProgress: (done: number, total: number) => void
  ): Promise<FileInfo[]> {
    let done = 0;
    const results = await Promise.all(
      pkgs.map(async (pkg) => {
        const files = await this.ensureFilesForPkg(pkg.topPkg, pkg.prefix);
        onProgress(++done, pkgs.length);
        return files;
      })
    );
    return results.flat();
  }
}
