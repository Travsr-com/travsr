/**
 * VSCODE-203: Hover card showing callers and blast radius for a symbol.
 *
 * On hover over any identifier in a .ts/.tsx/.rs/.py file, calls
 * get_callers and get_blast_radius via MCP and renders a MarkdownString
 * card per the design spec (design/CONCEPT_UI.md §6).
 *
 * Max 5 callers shown inline; "… and N more" when > 5.
 * Cache is per symbol text, cleared on file save.
 */

import * as vscode from "vscode";
import type { McpClient } from "./mcp";

export const HOVER_SELECTOR: vscode.DocumentSelector = [
  { language: "typescript" },
  { language: "typescriptreact" },
  { language: "rust" },
  { language: "python" },
];

interface HoverData {
  callers: string[];
  blastCount: number;
}

function parseCallers(raw: string): string[] {
  return raw
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
}

function formatCallerLine(line: string): string {
  // Input: "[call] fn:bar (function) — src/bar.ts"
  // Output: "`[call] fn:bar — src/bar.ts`"
  const m = /^(\[(?:call|structural)\])\s+(\S+)\s+\([^)]+\)\s+—\s+(.+)$/.exec(
    line
  );
  if (m) return `\`${m[1]} ${m[2]} — ${m[3]}\``;
  return `\`${line}\``;
}

export class CallersHoverProvider implements vscode.HoverProvider {
  // Callers are per-symbol; blast radius is per-file — split caches to avoid
  // redundant get_blast_radius calls when hovering multiple symbols in one file.
  // null = fetched, empty result; key absent = not yet fetched.
  private readonly callerCache = new Map<string, string[] | null>();
  private readonly blastCache = new Map<string, number | null>();

  constructor(private readonly mcp: McpClient) {}

  async provideHover(
    document: vscode.TextDocument,
    position: vscode.Position,
    token: vscode.CancellationToken
  ): Promise<vscode.Hover | undefined> {
    const wordRange = document.getWordRangeAtPosition(position);
    if (!wordRange) return undefined;

    const symbol = document.getText(wordRange);
    if (!symbol || symbol.length < 2) return undefined;

    const file = vscode.workspace.asRelativePath(document.uri, false);

    const hasCallers = this.callerCache.has(symbol);
    const hasBlast = this.blastCache.has(file);

    try {
      const [callersRaw, blastRaw] = await Promise.all([
        hasCallers ? Promise.resolve("") : this.mcp.callTool("get_callers", { symbol }),
        hasBlast   ? Promise.resolve("") : this.mcp.callTool("get_blast_radius", { file }),
      ]);

      if (token.isCancellationRequested) return undefined;

      const callers = hasCallers
        ? (this.callerCache.get(symbol) ?? [])
        : parseCallers(callersRaw);
      if (!hasCallers) this.callerCache.set(symbol, callers.length > 0 ? callers : null);

      const blastCount = hasBlast
        ? (this.blastCache.get(file) ?? 0)
        : blastRaw.split("\n").map((l) => l.trim()).filter(Boolean).length;
      if (!hasBlast) this.blastCache.set(file, blastCount > 0 ? blastCount : null);

      if (callers.length === 0 && blastCount === 0) return undefined;

      return buildHover(symbol, file, { callers, blastCount }, wordRange);
    } catch {
      return undefined;
    }
  }

  clearCache(): void {
    this.callerCache.clear();
    this.blastCache.clear();
  }
}

function buildHover(
  symbol: string,
  file: string,
  data: HoverData,
  range: vscode.Range
): vscode.Hover {
  const md = new vscode.MarkdownString(undefined, true);
  md.isTrusted = true;

  md.appendMarkdown(`**${symbol}**\n\n`);
  md.appendMarkdown(`\`${file}\`\n\n`);
  md.appendMarkdown("---\n\n");

  if (data.callers.length > 0) {
    const shown = data.callers.slice(0, 5);
    const rest = data.callers.length - shown.length;
    md.appendMarkdown(`**Callers** (${data.callers.length})\n\n`);
    for (const line of shown) {
      md.appendMarkdown(`- ${formatCallerLine(line)}\n`);
    }
    if (rest > 0) {
      md.appendMarkdown(`\n… and ${rest} more\n\n`);
    } else {
      md.appendMarkdown("\n");
    }
  }

  if (data.blastCount > 0) {
    md.appendMarkdown(`**Blast radius:** ${data.blastCount} file${data.blastCount !== 1 ? "s" : ""}\n\n`);
  }

  md.appendMarkdown("---\n\n");

  const callersArg = encodeURIComponent(JSON.stringify([symbol]));
  const blastArg = encodeURIComponent(JSON.stringify([file]));
  md.appendMarkdown(
    `[Show callers](command:travsr.showCallers?${callersArg}) · [Blast radius](command:travsr.showBlastRadius?${blastArg})`
  );

  return new vscode.Hover(md, range);
}
