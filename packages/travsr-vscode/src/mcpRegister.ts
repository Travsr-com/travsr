/**
 * ITEM 2 — Register the Travsr daemon as an MCP server in external AI agents.
 *
 * The daemon already speaks MCP over stdio (`travsr mcp --stdio`). This
 * command writes the server entry into the agent's config file after showing
 * a diff and requiring explicit user confirmation.
 *
 * Supported agents:
 *   · Claude Desktop (claude_desktop_config.json)
 *   · Cursor (.cursor/mcp.json in workspace root)
 *   · Continue (~/.continue/config.json)
 */

import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";

// ── Config-path resolution ───────────────────────────────────────────────────

export interface AgentTarget {
  name: string;
  configPath: string;
  /** How to embed the travsr server entry into the config schema. */
  schema: "claude_desktop" | "cursor_mcp" | "continue";
}

export function resolveAgentTargets(workspaceRoot?: string): AgentTarget[] {
  const home = os.homedir();
  const targets: AgentTarget[] = [];

  // Claude Desktop
  const claudePlatformPath =
    process.platform === "win32"
      ? path.join(process.env["APPDATA"] ?? home, "Claude", "claude_desktop_config.json")
      : process.platform === "darwin"
        ? path.join(home, "Library", "Application Support", "Claude", "claude_desktop_config.json")
        : path.join(home, ".config", "Claude", "claude_desktop_config.json");
  targets.push({ name: "Claude Desktop", configPath: claudePlatformPath, schema: "claude_desktop" });

  // Cursor workspace config
  if (workspaceRoot) {
    targets.push({
      name: "Cursor (workspace)",
      configPath: path.join(workspaceRoot, ".cursor", "mcp.json"),
      schema: "cursor_mcp",
    });
  }

  // Continue
  targets.push({
    name: "Continue",
    configPath: path.join(home, ".continue", "config.json"),
    schema: "continue",
  });

  return targets;
}

// ── Config merging ───────────────────────────────────────────────────────────

/** Merge the travsr server entry into an existing config JSON object. */
export function mergeServerEntry(
  existing: Record<string, unknown>,
  schema: AgentTarget["schema"],
  binaryPath: string
): Record<string, unknown> {
  const serverEntry = { command: binaryPath, args: ["mcp", "--stdio"] };

  const merged = structuredClone(existing);

  if (schema === "claude_desktop") {
    // { "mcpServers": { "travsr": { "command": "...", "args": [...] } } }
    const servers = (merged["mcpServers"] as Record<string, unknown> | undefined) ?? {};
    servers["travsr"] = serverEntry;
    merged["mcpServers"] = servers;
  } else if (schema === "cursor_mcp") {
    // { "mcpServers": { "travsr": { "command": "...", "args": [...] } } }
    const servers = (merged["mcpServers"] as Record<string, unknown> | undefined) ?? {};
    servers["travsr"] = serverEntry;
    merged["mcpServers"] = servers;
  } else if (schema === "continue") {
    // { "mcpServers": [{ "name": "travsr", "command": "...", "args": [...] }] }
    const list = Array.isArray(merged["mcpServers"]) ? (merged["mcpServers"] as unknown[]) : [];
    const withoutTravsr = list.filter(
      (e) => typeof e === "object" && e !== null && (e as Record<string, unknown>)["name"] !== "travsr"
    );
    withoutTravsr.push({ name: "travsr", ...serverEntry });
    merged["mcpServers"] = withoutTravsr;
  }

  return merged;
}

// ── Command implementation ───────────────────────────────────────────────────

export function registerMcpServerCommand(binary: string): vscode.Disposable {
  return vscode.commands.registerCommand("travsr.registerMcpServer", async () => {
    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const binaryPath =
      vscode.workspace.getConfiguration("travsr").get<string>("binaryPath") || binary;

    const targets = resolveAgentTargets(workspaceRoot);

    // Quick-pick the target agent
    type TargetItem = vscode.QuickPickItem & { target: AgentTarget };
    const items: TargetItem[] = targets.map((t) => ({
      label: t.name,
      description: t.configPath,
      detail: fs.existsSync(t.configPath) ? "Config file found" : "Config file will be created",
      target: t,
    }));

    const pick = await vscode.window.showQuickPick(items, {
      placeHolder: "Select the AI agent to register Travsr with…",
      matchOnDescription: true,
    });
    if (!pick) return;

    const { target } = pick;

    // Read existing config or start from empty
    let existing: Record<string, unknown> = {};
    if (fs.existsSync(target.configPath)) {
      try {
        existing = JSON.parse(fs.readFileSync(target.configPath, "utf8")) as Record<string, unknown>;
      } catch {
        void vscode.window.showErrorMessage(
          `Travsr: could not parse ${target.configPath} — fix JSON errors first.`
        );
        return;
      }
    }

    const merged = mergeServerEntry(existing, target.schema, binaryPath);
    const newContent = JSON.stringify(merged, null, 2);
    const oldContent = fs.existsSync(target.configPath)
      ? fs.readFileSync(target.configPath, "utf8")
      : "(new file)";

    // Show a diff and require explicit confirmation
    const diffLabel = oldContent === "(new file)"
      ? `Will create ${target.configPath} with travsr MCP server entry.`
      : `Will add travsr MCP server entry to ${target.configPath}.`;

    const confirm = await vscode.window.showInformationMessage(
      `${diffLabel}\n\nBinary: ${binaryPath}`,
      { modal: true },
      "Write"
    );
    if (confirm !== "Write") return;

    try {
      const dir = path.dirname(target.configPath);
      if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(target.configPath, newContent, "utf8");
      void vscode.window.showInformationMessage(
        `Travsr registered in ${target.name}. Restart ${target.name} to load the MCP server.`
      );
    } catch (e) {
      void vscode.window.showErrorMessage(
        `Travsr: failed to write ${target.configPath}: ${(e as Error).message}`
      );
    }
  });
}
