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
 *
 * ITEM 3 (parity plan §ITEM 3 part 2): passive high-blast marker. When the
 * dependent count reaches `travsr.blastRiskThreshold` (default 20), the lens
 * escalates to a "⚠️ high blast … review before editing" warning so the dev
 * *sees* risk before touching a high-impact file — no rename hook required.
 */

import * as vscode from "vscode";
import type { McpClient } from "./mcp";
import { parseEnvelope } from "./extension";

export const BLAST_RADIUS_SELECTOR: vscode.DocumentSelector = [
  { language: "typescript" },
  { language: "typescriptreact" },
  { language: "javascript" },
  { language: "javascriptreact" },
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
  { language: "csharp" },
  { language: "swift" },
  { language: "dart" },
];

function formatBlastCount(n: number): string {
  return n > 99 ? "99+" : String(n);
}

/** Default risk threshold when the setting is absent (mirrors package.json). */
const DEFAULT_BLAST_RISK_THRESHOLD = 20;

/** Read the (validated) high-blast threshold from settings. */
function blastRiskThreshold(): number {
  const raw = vscode.workspace
    .getConfiguration("travsr")
    .get<number>("blastRiskThreshold", DEFAULT_BLAST_RISK_THRESHOLD);
  // Guard against a user setting a non-positive/NaN value in settings.json;
  // package.json enforces minimum 1 in the UI but raw JSON can bypass that.
  return Number.isFinite(raw) && raw >= 1 ? Math.floor(raw) : DEFAULT_BLAST_RISK_THRESHOLD;
}

/** Build the lens command for a resolved blast count against the given threshold. */
export function blastCommand(file: string, files: string[], threshold: number): vscode.Command {
  const count = files.length;
  const plural = count !== 1 ? "s" : "";
  const title =
    count >= threshold
      ? `⚠️ high blast: ${formatBlastCount(count)} file${plural}; review before editing`
      : `🩻 blast: ${formatBlastCount(count)} file${plural}`;
  return { title, command: "travsr.showBlastRadius", arguments: [file, files] };
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
  // Cache the resolved dependent-file list per document, not the built lens, so
  // a threshold change can re-title without re-querying the daemon.
  //   null      = resolved to 0 files (lens omitted)
  //   undefined = not yet cached
  private readonly cache = new Map<string, string[] | null>();

  // Fired to make VS Code re-request lenses (on graph refresh or threshold change).
  private readonly _onDidChange = new vscode.EventEmitter<void>();
  readonly onDidChangeCodeLenses = this._onDidChange.event;

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
    const threshold = blastRiskThreshold();

    const cached = this.cache.get(file);
    if (cached === null) return undefined; // known-empty
    if (cached !== undefined) {
      // Rebuild the title against the current threshold — cheap, no I/O.
      lens.command = blastCommand(file, cached, threshold);
      return lens;
    }

    try {
      const raw = await this.mcp.callTool("get_blast_radius", { file });
      if (token.isCancellationRequested) return undefined;

      const files = parseEnvelope(raw);

      if (files.length === 0) {
        this.cache.set(file, null);
        return undefined;
      }

      this.cache.set(file, files);
      lens.command = blastCommand(file, files, threshold);
    } catch {
      return undefined;
    }

    return lens;
  }

  /** Drop cached counts (graph changed) and force a re-render. */
  clearCache(): void {
    this.cache.clear();
    this._onDidChange.fire();
  }

  /** Re-render titles without dropping counts (e.g. threshold setting changed). */
  refresh(): void {
    this._onDidChange.fire();
  }

  dispose(): void {
    this._onDidChange.dispose();
  }
}
