/**
 * One shared terminal for the commands the panels offer to run.
 *
 * Every remedy the extension surfaced used to be text: the panel printed
 * `travsr embed init`, and the user retyped it. This runs it instead, in a
 * terminal named "Travsr" that is reused while it is alive and recreated once
 * the user has closed it.
 *
 * Two things this module refuses to do, both deliberate:
 *
 *   1. It never forwards a string straight from a webview to a shell. Panel
 *      content is built from daemon output and from log files, and a log file
 *      is not a trusted input just because it is local. `parseTravsrInvocation`
 *      admits only `travsr` followed by argument-shaped tokens, and the caller
 *      passes the parsed argv, not the sentence it came from.
 *   2. It never emits a `cd`. Changing directory needs shell-specific syntax
 *      (`cd /d` on cmd, `Set-Location` on PowerShell), so a run against a
 *      different cwd disposes the terminal and opens a new one there.
 */

import * as vscode from "vscode";
import { resolveExportBinaryPath } from "./mcpRegister";

export const TRAVSR_TERMINAL_NAME = "Travsr";

/** The slice of `vscode.window` this module touches, so tests can pass a fake. */
export interface TerminalHost {
  readonly terminals: readonly vscode.Terminal[];
  createTerminal(options: vscode.TerminalOptions): vscode.Terminal;
}

export type ShellKind = "powershell" | "cmd" | "posix";

/** Classify the shell VS Code will actually open, falling back by platform.
 *
 *  `vscode.env.shell` reflects the default profile, which is also what
 *  `createTerminal()` opens when given no `shellPath`, so the quoting below
 *  matches the terminal it is aimed at. Git Bash on Windows classifies as
 *  posix, which is correct: a backslash path inside single quotes passes
 *  through bash untouched and Windows exec accepts it. */
export function detectShellKind(
  shellPath: string | undefined,
  platform: string = process.platform
): ShellKind {
  if (!shellPath) return platform === "win32" ? "powershell" : "posix";
  const base = shellPath.replace(/\\/g, "/").split("/").pop() ?? "";
  if (/^(pwsh|powershell)(\.exe)?$/i.test(base)) return "powershell";
  if (/^cmd(\.exe)?$/i.test(base)) return "cmd";
  return "posix";
}

/** Quote one argument, leaving already-safe tokens alone so the command stays
 *  readable in the terminal's scrollback. */
export function quoteForShell(arg: string, kind: ShellKind): string {
  if (/^[A-Za-z0-9_.:=/\\@+-]+$/.test(arg)) return arg;
  if (kind === "cmd") return `"${arg.replace(/"/g, '""')}"`;
  if (kind === "powershell") return `'${arg.replace(/'/g, "''")}'`;
  return `'${arg.replace(/'/g, `'\\''`)}'`;
}

/** Build the command line. PowerShell needs the call operator when the
 *  executable is quoted, or it treats the string as a value to echo. */
export function formatCommandLine(binary: string, args: string[], kind: ShellKind): string {
  const quotedBinary = quoteForShell(binary, kind);
  const wasQuoted = quotedBinary !== binary;
  const head = kind === "powershell" && wasQuoted ? `& ${quotedBinary}` : quotedBinary;
  return [head, ...args.map((a) => quoteForShell(a, kind))].join(" ");
}

/** Accept only a plain `travsr <args>` invocation, and return its arguments.
 *
 *  This is the boundary between text the daemon produced and a shell. Anything
 *  carrying `;`, `|`, `&`, `$`, a backtick, parentheses, redirection or quotes
 *  is rejected rather than escaped, because a remedy hint has no business
 *  containing them and the safe reading of one that does is "not a command we
 *  offer to run". */
export function parseTravsrInvocation(raw: string): string[] | null {
  const trimmed = raw.trim();
  if (!/^travsr(\s+[A-Za-z0-9_.:=/\\@+-]+)*$/.test(trimmed)) return null;
  const parts = trimmed.split(/\s+/);
  return parts.slice(1);
}

/** The same ladder `travsr connect` exports, rather than a third copy of it. */
export function resolveTerminalBinary(configured?: string): string {
  const cfg =
    configured ?? (vscode.workspace.getConfiguration("travsr").get<string>("binaryPath", "") || "");
  return resolveExportBinaryPath(cfg) ?? "travsr";
}

function cwdOf(t: vscode.Terminal): string | undefined {
  const c = (t.creationOptions as vscode.TerminalOptions).cwd;
  if (c === undefined) return undefined;
  return typeof c === "string" ? c : c.fsPath;
}

/** Send one command line to the shared terminal, creating or replacing it as
 *  needed, and show it so the output is not hidden behind another panel. */
export function runInTravsrTerminal(
  cmd: string,
  cwd?: string,
  host: TerminalHost = vscode.window
): vscode.Terminal {
  // Scanned per call rather than cached: a terminal the user closed leaves the
  // list, and one whose shell exited carries an exitStatus. A cached reference
  // would keep pointing at either.
  const live = host.terminals.find(
    (t) => t.name === TRAVSR_TERMINAL_NAME && t.exitStatus === undefined
  );
  const reusable = live !== undefined && (cwd === undefined || cwdOf(live) === cwd);
  if (live !== undefined && !reusable) live.dispose();

  const term = reusable
    ? (live as vscode.Terminal)
    : host.createTerminal({
        name: TRAVSR_TERMINAL_NAME,
        cwd,
        iconPath: new vscode.ThemeIcon("type-hierarchy"),
      });
  term.show(true);
  term.sendText(cmd, true);
  return term;
}

/** Run `travsr <args>` in the shared terminal, quoted for the user's shell. */
export function runTravsrCommand(
  args: string[],
  cwd?: string,
  host: TerminalHost = vscode.window
): vscode.Terminal {
  const line = formatCommandLine(
    resolveTerminalBinary(),
    args,
    detectShellKind(vscode.env.shell)
  );
  return runInTravsrTerminal(line, cwd, host);
}
