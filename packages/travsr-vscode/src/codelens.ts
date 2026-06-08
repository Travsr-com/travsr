/**
 * VSCODE-202: Blast radius code lens.
 *
 * Renders a single lens at the top of every .ts/.tsx/.rs/.py file showing
 * how many files would be affected if this file changed, based on
 * get_blast_radius. Clicking opens the affected-file list via
 * travsr.showBlastRadius.
 *
 * Design spec (design/CONCEPT_UI.md §4):
 *   "blast: N files"  → command: travsr.showBlastRadius
 *   Omit lens when count = 0 or daemon unavailable.
 *   Use "99+" when count > 99.
 */

import * as vscode from "vscode";
import type { McpClient } from "./mcp";

export const BLAST_RADIUS_SELECTOR: vscode.DocumentSelector = [
  { language: "typescript" },
  { language: "typescriptreact" },
  { language: "rust" },
  { language: "python" },
  { language: "go" },
  { language: "java" },
  { language: "kotlin" },
  { language: "ruby" },
  { language: "php" },
  { language: "scala" },
  { language: "cpp" },
  { language: "c" },
  { language: "swift" },
  { language: "dart" },
];

function formatBlastCount(n: number): string {
  return n > 99 ? "99+" : String(n);
}

// Carries the document's relative path so resolveCodeLens doesn't depend on
// activeTextEditor (which may point to a different document when lenses are
// resolved in the background or in a side-by-side view).
class BlastRadiusLens extends vscode.CodeLens {
  constructor(
    range: vscode.Range,
    readonly file: string
  ) {
    super(range);
  }
}

export class BlastRadiusCodeLensProvider implements vscode.CodeLensProvider {
  // null = resolved to 0 files (lens omitted). undefined = not yet cached.
  private readonly cache = new Map<string, vscode.CodeLens | null>();

  constructor(private readonly mcp: McpClient) {}

  provideCodeLenses(document: vscode.TextDocument): vscode.CodeLens[] {
    const file = vscode.workspace.asRelativePath(document.uri, false);
    const range = new vscode.Range(0, 0, 0, 0);
    return [new BlastRadiusLens(range, file)];
  }

  async resolveCodeLens(
    lens: vscode.CodeLens,
    token: vscode.CancellationToken
  ): Promise<vscode.CodeLens | undefined> {
    if (token.isCancellationRequested) return undefined;
    if (!(lens instanceof BlastRadiusLens)) return undefined;

    const { file } = lens;

    const cached = this.cache.get(file);
    if (cached === null) return undefined; // known-empty
    if (cached !== undefined) return cached; // cache hit

    try {
      const raw = await this.mcp.callTool("get_blast_radius", { file });
      if (token.isCancellationRequested) return undefined;

      const files = raw
        .split("\n")
        .map((l) => l.trim())
        .filter(Boolean);
      const count = files.length;

      if (count === 0) {
        this.cache.set(file, null);
        return undefined;
      }

      lens.command = {
        title: `🩻 blast: ${formatBlastCount(count)} file${count !== 1 ? "s" : ""}`,
        command: "travsr.showBlastRadius",
        arguments: [file, files],
      };
      this.cache.set(file, lens);
    } catch {
      return undefined;
    }

    return lens;
  }

  clearCache(): void {
    this.cache.clear();
  }
}
