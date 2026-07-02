/**
 * VSCODE-201: Always-fresh status bar indicator.
 *
 * Polls get_graph_stats every 30 s and derives a freshness state.
 * Switches to "indexing" on file save, then re-queries after 2 s.
 *
 * Design tokens (design/CONCEPT_UI.md §4):
 *   fresh    $(graph)      travsr.freshForeground  (#7cf2c5)
 *   indexing $(sync~spin)  default
 *   stale    $(warning)    travsr.staleForeground  (#f59e0b)
 *   error    $(error)      travsr.errorForeground  (#ef4444)
 */

import * as vscode from "vscode";
import type { McpClient } from "./mcp";

type State = "connecting" | "fresh" | "indexing" | "stale" | "error";

interface RepoStats {
  nodeCount: number;
}

function parseGraphStats(raw: string): RepoStats {
  let nodeCount = 0;
  for (const line of raw.split("\n")) {
    const m = /^nodes:\s*(\d+)/.exec(line.trim());
    if (m) { nodeCount = parseInt(m[1], 10); break; }
  }
  return { nodeCount };
}

function timeAgo(ms: number): string {
  const secs = Math.floor(ms / 1000);
  if (secs < 60) return `${secs}s ago`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  return `${Math.floor(mins / 60)}h ago`;
}

function formatCount(n: number): string {
  return n.toLocaleString("en-US");
}

export function createStatusBarItem(
  context: vscode.ExtensionContext,
  client: McpClient,
  onReconnect?: (cb: () => void) => { dispose(): void },
  position?: "left" | "right"
): vscode.StatusBarItem {
  const alignment =
    position === "right"
      ? vscode.StatusBarAlignment.Right
      : vscode.StatusBarAlignment.Left;
  // For Left: higher priority = further left (we want near-left = 100).
  // For Right: higher priority = further left of the right group (we want near-right = 10).
  const priority = position === "right" ? 10 : 100;
  const item = vscode.window.createStatusBarItem(alignment, priority);
  item.command = "travsr.showStatus";
  context.subscriptions.push(item);

  let state: State = "connecting";
  let lastStats: RepoStats | null = null;
  let lastUpdated: Date | null = null;
  let staleAt: Date | null = null;
  let saveDebounce: ReturnType<typeof setTimeout> | undefined;

  function render(): void {
    const staleLabel =
      staleAt ? ` · ${timeAgo(Date.now() - staleAt.getTime())}` : "";
    const nodeLabel =
      lastStats && lastStats.nodeCount > 0
        ? ` · ${formatCount(lastStats.nodeCount)} nodes`
        : "";

    switch (state) {
      case "connecting":
        item.text = "$(sync~spin) Travsr";
        item.color = undefined;
        item.tooltip = buildTooltip("connecting", lastStats, lastUpdated);
        break;
      case "fresh":
        item.text = `$(graph) Travsr · fresh${nodeLabel}`;
        item.color = new vscode.ThemeColor("travsr.freshForeground");
        item.tooltip = buildTooltip("fresh", lastStats, lastUpdated);
        break;
      case "indexing":
        item.text = "$(sync~spin) Travsr · indexing…";
        item.color = undefined;
        item.tooltip = buildTooltip("indexing", lastStats, lastUpdated);
        break;
      case "stale":
        item.text = `$(warning) Travsr · stale${staleLabel}`;
        item.color = new vscode.ThemeColor("travsr.staleForeground");
        item.tooltip = buildTooltip("stale", lastStats, lastUpdated);
        break;
      case "error":
        item.text = "$(error) Travsr · disconnected";
        item.color = new vscode.ThemeColor("travsr.errorForeground");
        item.tooltip = buildTooltip("error", lastStats, lastUpdated);
        break;
    }
    item.show();
  }

  function buildTooltip(
    s: State,
    stats: RepoStats | null,
    updated: Date | null
  ): vscode.MarkdownString {
    const md = new vscode.MarkdownString();
    md.appendMarkdown("**Travsr graph**\n\n");
    md.appendMarkdown(`Status: \`${s}\`  \n`);
    if (updated) {
      md.appendMarkdown(
        `Updated: ${timeAgo(Date.now() - updated.getTime())}  \n`
      );
    }
    if (stats && stats.nodeCount > 0) {
      md.appendMarkdown(`Nodes: ${formatCount(stats.nodeCount)}  \n`);
    }
    return md;
  }

  async function poll(force = false): Promise<void> {
    if (!force && state === "indexing") return;
    try {
      const raw = await client.callTool("get_graph_stats");
      if (raw.length > 0) {
        lastStats = parseGraphStats(raw);
        lastUpdated = new Date();
        staleAt = null;
        state = "fresh";
      } else if (!client.isConnected()) {
        // Process exited — genuine disconnect.
        if (state !== "stale" && state !== "error") staleAt = new Date();
        state = "error";
      } else {
        // Empty from a live server = poll timed out because the server stdin
        // queue is backed up with other in-flight queries. Keep current state.
        return;
      }
    } catch {
      if (state !== "stale" && state !== "error") staleAt = new Date();
      state = "error";
    }
    render();
  }

  // Initial render + connect
  render();
  void poll();

  const pollTimer = setInterval(() => void poll(), 30_000);
  context.subscriptions.push({ dispose: () => clearInterval(pollTimer) });

  // On reconnect (after binary install or daemon restart): reset to connecting
  // and immediately re-poll so the status bar turns green within one poll cycle.
  if (onReconnect) {
    context.subscriptions.push(
      onReconnect(() => {
        state = "connecting";
        render();
        void poll();
      })
    );
  }

  // On save: switch to indexing, re-query after 2 s
  context.subscriptions.push(
    vscode.workspace.onDidSaveTextDocument(() => {
      if (state === "fresh") {
        state = "indexing";
        render();
        clearTimeout(saveDebounce);
        saveDebounce = setTimeout(() => void poll(true), 2_000);
      }
    })
  );

  // On .travsr deletion: show "indexing" so the status bar reflects the
  // daemon's re-initialization cycle instead of staying frozen on "fresh".
  // On re-creation: re-poll immediately to pick up the rebuilt graph.
  const wsRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  if (wsRoot) {
    let deletionDebounce: ReturnType<typeof setTimeout> | undefined;
    const travsrWatcher = vscode.workspace.createFileSystemWatcher(
      new vscode.RelativePattern(wsRoot, ".travsr/**")
    );
    context.subscriptions.push(
      travsrWatcher,
      travsrWatcher.onDidDelete(() => {
        if (state === "fresh") {
          state = "indexing";
          render();
          clearTimeout(deletionDebounce);
          deletionDebounce = setTimeout(() => void poll(), 2_000);
        }
      }),
      travsrWatcher.onDidCreate(() => {
        clearTimeout(deletionDebounce);
        void poll(true);
      })
    );
  }

  return item;
}
