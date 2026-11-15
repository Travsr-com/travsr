/**
 * `Travsr: Open Graph in Browser` (#1060): the extension's entry point to the
 * `travsr ui` browser view.
 *
 * `travsr ui` (#1038) serves the code-graph pages over loopback HTTP, but the
 * only way to reach it from VS Code was to know the command and run it in a
 * terminal. This module runs it for the user:
 *
 *   - It spawns `travsr ui --port <travsr.ui.port>` (0 by default, so the OS
 *     picks a free port and two windows never collide) with the chosen
 *     repository as the working directory. `travsr ui` preselects the repo it
 *     was started in, which is how the workspace repo reaches the page; the
 *     page's own picker still switches to any other indexed repo.
 *   - It reads the address from the `travsr ui: serving <url>` line the CLI
 *     prints, and opens it with `vscode.env.openExternal`.
 *   - The extension owns the server: one per window, reused while it serves
 *     the same repo, replaced when the target repo changes, and stopped by
 *     `Travsr: Stop Browser UI` or when the extension deactivates. A detached
 *     server would outlive the window with nothing left to stop it.
 *
 * The CLI needs no change for this; it already prints the URL and takes
 * `--port`.
 */

import * as cp from "child_process";
import * as vscode from "vscode";
import { assertExecutableBinary } from "./installer";
import { runTravsrCommand } from "./terminal";

/** How long to wait for the `serving` line before giving up. The server binds
 *  before printing anything, so this only runs out when the binary hangs. */
export const START_TIMEOUT_MS = 15_000;

/** After the `serving` line the CLI prints one line about the repository and
 *  then `press Ctrl-C to stop`. Waiting for that last line lets the caller say
 *  "not indexed yet" up front; this caps the wait if the format ever changes. */
export const STATUS_LINE_GRACE_MS = 1_000;

/** The address from a `travsr ui: serving <url>` line, or null for any other
 *  line. */
export function parseServingUrl(line: string): string | null {
  const m = /^travsr ui: serving (https?:\/\/\S+)$/.exec(line.trim());
  return m ? m[1] : null;
}

/** Whether a stdout line says the repository has no graph yet. */
export function isNotIndexedLine(line: string): boolean {
  return /^repository: .*\(not indexed yet/.test(line.trim());
}

/** A usable `--port` value from the setting: an integer from 0 to 65535, or 0
 *  (a free port) for anything else. */
export function normalizePort(value: unknown): number {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= 65535
    ? value
    : 0;
}

/** The argv after the binary. */
export function uiArgs(port: number): string[] {
  return ["ui", "--port", String(normalizePort(port))];
}

/** One sentence explaining why `travsr ui` exited before it served anything,
 *  from what it wrote to stderr. */
export function explainStartFailure(stderr: string, code: number | null): string {
  if (/unrecognized subcommand ['"]?ui\b/i.test(stderr)) {
    return (
      "This travsr binary has no `ui` command. Update travsr to v1.4.0 or later " +
      "(Travsr: Download Binary)."
    );
  }
  const last = stderr
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter(Boolean)
    .pop();
  if (last) return last.replace(/^error:\s*/i, "");
  return code === null
    ? "travsr ui stopped before it reported an address."
    : `travsr ui exited with code ${code} before it reported an address.`;
}

/** The slice of a child process this module uses, so tests can pass a fake. */
export interface UiProcess {
  readonly stdout: NodeJS.ReadableStream | null;
  readonly stderr: NodeJS.ReadableStream | null;
  kill(): boolean;
  on(event: string, listener: (...args: any[]) => void): unknown;
}

export type SpawnUi = (binary: string, args: string[], cwd: string) => UiProcess;

export const spawnUiProcess: SpawnUi = (binary, args, cwd) =>
  cp.spawn(binary, args, {
    cwd,
    env: { ...process.env, NO_COLOR: "1", TERM: "dumb" },
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });

export interface StartResult {
  url: string;
  /** The repository has no `.travsr/graph.db` yet; the page will be empty. */
  notIndexed: boolean;
}

/** Calls `fn` with each complete line of a stream. */
function onLines(stream: NodeJS.ReadableStream | null, fn: (line: string) => void): void {
  if (!stream) return;
  let buf = "";
  stream.on("data", (chunk: Buffer | string) => {
    buf += chunk.toString();
    let nl: number;
    while ((nl = buf.indexOf("\n")) >= 0) {
      fn(buf.slice(0, nl).replace(/\r$/, ""));
      buf = buf.slice(nl + 1);
    }
  });
}

/**
 * The window's single `travsr ui` server.
 *
 * `start` resolves with the address once the CLI reports it, and rejects with
 * an `Error` whose message is fit to show the user. `onUnexpectedExit` fires
 * when a server that was serving stops without `stop` being called.
 */
export class BrowserUiServer implements vscode.Disposable {
  private proc: UiProcess | undefined;
  private repo: string | undefined;
  private started: Promise<StartResult> | undefined;
  /** The current process has reported its address. */
  private serving = false;
  private readonly runningEmitter = new vscode.EventEmitter<boolean>();
  private readonly exitEmitter = new vscode.EventEmitter<number | null>();
  /** Fires `true` when a server starts serving, `false` when it stops. */
  readonly onDidChangeRunning = this.runningEmitter.event;
  readonly onUnexpectedExit = this.exitEmitter.event;

  constructor(
    private readonly spawn: SpawnUi = spawnUiProcess,
    private readonly log: (line: string) => void = () => undefined,
    private readonly timeoutMs: number = START_TIMEOUT_MS,
    private readonly graceMs: number = STATUS_LINE_GRACE_MS
  ) {}

  /** Whether a server process is alive (starting or serving). */
  get running(): boolean {
    return this.proc !== undefined;
  }

  /** The repository the current server was started in. */
  get repoRoot(): string | undefined {
    return this.proc ? this.repo : undefined;
  }

  /** Serve `repo`, reusing the running server when it already serves it. */
  start(binary: string, repo: string, port: number): Promise<StartResult> {
    if (this.proc && this.repo === repo && this.started) return this.started;
    this.stop();

    const args = uiArgs(port);
    this.log(`$ ${binary} ${args.join(" ")}   (in ${repo})`);
    let proc: UiProcess;
    try {
      proc = this.spawn(binary, args, repo);
    } catch (e) {
      return Promise.reject(cannotRun(binary, e));
    }
    this.proc = proc;
    this.repo = repo;

    const started = new Promise<StartResult>((resolve, reject) => {
      let url: string | undefined;
      let notIndexed = false;
      let settled = false;
      let stderr = "";
      let grace: ReturnType<typeof setTimeout> | undefined;
      const timer = setTimeout(() => {
        fail(new Error(`travsr ui did not report its address within ${this.timeoutMs / 1000} s.`));
        this.stop();
      }, this.timeoutMs);

      const succeed = (): void => {
        if (settled || url === undefined) return;
        settled = true;
        clearTimeout(timer);
        if (grace) clearTimeout(grace);
        if (this.proc === proc) {
          this.serving = true;
          this.runningEmitter.fire(true);
        }
        resolve({ url, notIndexed });
      };
      const fail = (err: Error): void => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        if (grace) clearTimeout(grace);
        reject(err);
      };

      onLines(proc.stdout, (line) => {
        this.log(line);
        if (url === undefined) {
          url = parseServingUrl(line) ?? undefined;
          if (url !== undefined) grace = setTimeout(succeed, this.graceMs);
          return;
        }
        if (isNotIndexedLine(line)) notIndexed = true;
        if (line.trim().startsWith("press Ctrl-C")) succeed();
      });
      onLines(proc.stderr, (line) => {
        this.log(line);
        stderr += line + "\n";
      });

      proc.on("error", (e: Error) => {
        this.forget(proc);
        fail(cannotRun(binary, e));
      });
      proc.on("exit", (code: number | null) => {
        const wasServing = this.proc === proc && this.serving;
        this.forget(proc);
        this.log(`travsr ui exited (${code === null ? "killed" : `code ${code}`})`);
        if (!settled) {
          fail(new Error(explainStartFailure(stderr, code)));
        } else if (wasServing) {
          this.runningEmitter.fire(false);
          this.exitEmitter.fire(code);
        }
      });
    });
    this.started = started;
    // A rejected start must not leave an unhandled rejection behind when the
    // caller only awaits a later one.
    started.catch(() => undefined);
    return started;
  }

  /** Stop the server. Returns whether one was running. */
  stop(): boolean {
    const proc = this.proc;
    if (!proc) return false;
    const wasServing = this.serving;
    this.forget(proc);
    proc.kill();
    if (wasServing) this.runningEmitter.fire(false);
    return true;
  }

  dispose(): void {
    this.stop();
    this.runningEmitter.dispose();
    this.exitEmitter.dispose();
  }

  /** Drop the state for `proc` if it is still the current process. */
  private forget(proc: UiProcess): void {
    if (this.proc !== proc) return;
    this.proc = undefined;
    this.repo = undefined;
    this.started = undefined;
    this.serving = false;
  }
}

function cannotRun(binary: string, e: unknown): Error {
  const msg = e instanceof Error ? e.message : String(e);
  return new Error(
    `Cannot run ${binary}: ${msg}. Set travsr.binaryPath or run Travsr: Download Binary.`
  );
}

function basename(p: string): string {
  return p.split(/[\\/]/).filter(Boolean).pop() ?? p;
}

/** Where the commands get the repository to serve. */
export interface RepoSource {
  ensureChosen(): Promise<string | undefined>;
}

/** Register `travsr.openBrowserUi` and `travsr.stopBrowserUi`. */
export function registerBrowserUi(
  context: vscode.ExtensionContext,
  repos: RepoSource,
  channel: vscode.OutputChannel,
  server: BrowserUiServer = new BrowserUiServer(spawnUiProcess, (l) => channel.appendLine(l))
): BrowserUiServer {
  const showLogs = async (thenable: Thenable<string | undefined>): Promise<void> => {
    if ((await thenable) === "Show logs") channel.show();
  };

  context.subscriptions.push(
    server,
    server.onDidChangeRunning((running) => {
      void vscode.commands.executeCommand("setContext", "travsr.browserUiRunning", running);
    }),
    server.onUnexpectedExit((code) => {
      const how = code === null ? "" : ` (exit code ${code})`;
      void showLogs(
        vscode.window.showWarningMessage(
          `Travsr: the browser UI server stopped${how}.`,
          "Show logs"
        )
      );
    }),

    vscode.commands.registerCommand("travsr.openBrowserUi", async () => {
      const repo = await repos.ensureChosen();
      if (!repo) {
        void vscode.window.showWarningMessage(
          "Travsr: open a folder inside a git repository to view its graph in the browser."
        );
        return;
      }
      const cfg = vscode.workspace.getConfiguration("travsr");
      const configured = cfg.get<string>("binaryPath", "") ?? "";
      if (configured) {
        try {
          assertExecutableBinary(configured);
        } catch (e) {
          void vscode.window.showErrorMessage(
            `Travsr: invalid binaryPath, ${(e as Error).message}`
          );
          return;
        }
      }
      const binary = configured || "travsr";
      const port = normalizePort(cfg.get<number>("ui.port", 0));

      let result: StartResult;
      try {
        result = await vscode.window.withProgress(
          {
            location: vscode.ProgressLocation.Window,
            title: "Travsr: starting the browser UI",
          },
          () => server.start(binary, repo, port)
        );
      } catch (e) {
        void showLogs(
          vscode.window.showErrorMessage(
            `Travsr: could not start the browser UI. ${(e as Error).message}`,
            "Show logs"
          )
        );
        return;
      }

      if (result.notIndexed) {
        void vscode.window
          .showWarningMessage(
            `Travsr: ${basename(repo)} is not indexed yet, so the browser UI has no graph ` +
              "for it. Index it, or pick another repository in the page.",
            "Index in terminal"
          )
          .then((a) => {
            if (a === "Index in terminal") runTravsrCommand(["init"], repo);
          });
      }

      const opened = await vscode.env.openExternal(vscode.Uri.parse(result.url));
      if (!opened) {
        const a = await vscode.window.showInformationMessage(
          `Travsr: the browser UI is serving ${result.url}`,
          "Copy URL"
        );
        if (a === "Copy URL") await vscode.env.clipboard.writeText(result.url);
      }
    }),

    vscode.commands.registerCommand("travsr.stopBrowserUi", () => {
      const repo = server.repoRoot;
      if (server.stop()) {
        void vscode.window.showInformationMessage(
          `Travsr: stopped the browser UI${repo ? ` for ${basename(repo)}` : ""}.`
        );
      } else {
        void vscode.window.showInformationMessage("Travsr: the browser UI is not running.");
      }
    })
  );
  return server;
}
