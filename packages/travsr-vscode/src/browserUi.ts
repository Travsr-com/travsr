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
 *   - The extension owns the server: one per window, reused while the same
 *     binary, port and repo are asked for, replaced when the command is run
 *     with a different one (another repo picked with `Travsr: Select
 *     Repository`, or a changed `travsr.ui.port` or `travsr.binaryPath`), and
 *     stopped by `Travsr: Stop Browser UI` or when the extension deactivates.
 *     Picking another repo does not by itself restart a running server. A
 *     detached server would outlive the window with nothing left to stop it.
 *   - It spawns the native binary itself, never a bare `travsr` or the npm
 *     package's Node launcher, so `kill()` reaches the server.
 *   - If the editor crashes or is force-killed, `deactivate` never runs. The
 *     server is started with a piped stdin and `TRAVSR_UI_EXIT_ON_STDIN_EOF=1`,
 *     so it exits when the operating system closes that pipe. That needs a
 *     travsr binary that knows the variable; an older one ignores it and only
 *     the normal stop paths apply.
 */

import * as cp from "child_process";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import { assertExecutableBinary, resolveNpmShimExe } from "./installer";
import { resolveExportBinaryPath } from "./mcpRegister";
import { runTravsrCommand } from "./terminal";

/**
 * The native binary behind an npm install's Node wrapper, or `candidate` when
 * it is not one; `null` when it is a wrapper with no native binary beside it.
 *
 * On macOS and Linux npm puts a symlink to the package's `bin/travsr.js` on
 * PATH. That wrapper runs the native `bin/travsr` next to it with `spawnSync`,
 * so the server would be its grandchild: Stop would kill only the wrapper and
 * leave the server running (review of #1101). On Windows the `.cmd` shim is
 * already swapped for `travsr.exe` by `resolveExportBinaryPath`.
 */
export function unwrapNodeShim(
  candidate: string,
  platform: string = process.platform,
  realpath: (p: string) => string = fs.realpathSync,
  exists: (p: string) => boolean = fs.existsSync
): string | null {
  let real: string;
  try {
    real = realpath(candidate);
  } catch {
    return candidate;
  }
  if (!/\.[cm]?js$/i.test(real)) return candidate;
  const native = path.join(path.dirname(real), platform === "win32" ? "travsr.exe" : "travsr");
  return exists(native) ? native : null;
}

/**
 * The travsr binary to spawn `travsr ui` from, always the native executable
 * itself so `kill()` reaches the server: `travsr.binaryPath` when set (an npm
 * shim there is swapped for its packaged binary), else the same ladder `travsr
 * connect` exports (the extension's install dir, then PATH, then an npm shim's
 * packaged binary). A bare `travsr` is never spawned: on Windows it cannot run
 * a `.cmd` shim, and an install made by Download Binary is not on PATH.
 */
export function resolveUiBinary(
  configured: string,
  platform: string = process.platform,
  ladder: (configured: string) => string | null = resolveExportBinaryPath,
  unwrap: (p: string) => string | null = (p) => unwrapNodeShim(p, platform),
  shimExe: (p: string) => string | null = (p) => resolveNpmShimExe(p, platform)
): { binary: string } | { error: string } {
  let found: string | null;
  if (configured) {
    try {
      assertExecutableBinary(configured, platform);
      found = configured;
    } catch (e) {
      found = shimExe(configured);
      if (!found) return { error: `invalid binaryPath, ${(e as Error).message}` };
    }
  } else {
    found = ladder("");
    if (!found) {
      return {
        error:
          "travsr was not found (not installed by the extension and not on PATH). " +
          "Run Travsr: Download Binary, or set travsr.binaryPath.",
      };
    }
  }
  const native = unwrap(found);
  if (!native) {
    return {
      error:
        `${found} is the npm package's launcher, and its native binary is not installed yet. ` +
        "Run travsr once in a terminal so npm fetches it, or set travsr.binaryPath.",
    };
  }
  return { binary: native };
}

/** A start that `stop` (or a start for another repo) ended before it served.
 *  Not a failure: the caller ignores it rather than reporting it. */
export class StartCancelled extends Error {
  constructor() {
    super("travsr ui was stopped before it reported an address.");
    this.name = "StartCancelled";
  }
}

/** How long to wait for the `serving` line before giving up. The server binds
 *  before printing anything, so this only runs out when the binary hangs. */
export const START_TIMEOUT_MS = 15_000;

/** After the `serving` line the CLI prints one line about the repository and
 *  then `press Ctrl-C to stop`. Waiting for that last line lets the caller say
 *  "not indexed yet" up front; this caps the wait if the format ever changes. */
export const STATUS_LINE_GRACE_MS = 1_000;

/** How long Stop waits after closing the server's stdin before killing it.
 *  A travsr that knows TRAVSR_UI_EXIT_ON_STDIN_EOF stops well inside this
 *  (it forces its own exit 5 s after stdin closes); one that predates the
 *  variable ignores the end of file and is killed instead. */
export const STOP_FALLBACK_MS = 8_000;

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
  /** Closed to stop the server gracefully (#1119); absent in some fakes. */
  readonly stdin?: { end(): void } | null;
  readonly stdout: NodeJS.ReadableStream | null;
  readonly stderr: NodeJS.ReadableStream | null;
  kill(): boolean;
  on(event: string, listener: (...args: any[]) => void): unknown;
}

export type SpawnUi = (binary: string, args: string[], cwd: string) => UiProcess;

/** Tells `travsr ui` to stop when its stdin closes. Older binaries ignore it. */
export const EXIT_ON_STDIN_EOF_ENV = "TRAVSR_UI_EXIT_ON_STDIN_EOF";

/** The environment `travsr ui` runs with. */
export function uiEnv(base: NodeJS.ProcessEnv = process.env): NodeJS.ProcessEnv {
  return { ...base, NO_COLOR: "1", TERM: "dumb", [EXIT_ON_STDIN_EOF_ENV]: "1" };
}

// stdin is a pipe this process holds open and never writes to or closes. A
// crash or force-kill of the extension host skips `deactivate`, so `stop` never
// runs, but the operating system still closes the pipe, and with
// EXIT_ON_STDIN_EOF_ENV set `travsr ui` exits on that end of file instead of
// serving on with nobody left to stop it.
export const spawnUiProcess: SpawnUi = (binary, args, cwd) => {
  const child = cp.spawn(binary, args, {
    cwd,
    env: uiEnv(),
    stdio: ["pipe", "pipe", "pipe"],
    windowsHide: true,
  });
  // Ending stdin after the server has already gone reports EPIPE here; that
  // is expected, and an unhandled 'error' would take the extension host down.
  child.stdin?.on("error", () => undefined);
  return child;
};

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
  /** Binary, port and repo of the current server: a server is reused only
   *  when all three match, so a changed setting takes effect on the next
   *  Open rather than after a reload (review of #1101). */
  private key: string | undefined;
  private started: Promise<StartResult> | undefined;
  /** Settles a start that is still waiting for its address as cancelled. */
  private cancelStart: (() => void) | undefined;
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
    private readonly graceMs: number = STATUS_LINE_GRACE_MS,
    private readonly stopFallbackMs: number = STOP_FALLBACK_MS
  ) {}

  /** Resolves when the last server stopped through its stdin has exited:
   *  a server on a fixed port must be gone before its successor binds it. */
  private retiring: Promise<void> | undefined;

  /** Whether a server process is alive (starting or serving). */
  get running(): boolean {
    return this.proc !== undefined;
  }

  /** The repository the current server was started in. */
  get repoRoot(): string | undefined {
    return this.proc ? this.repo : undefined;
  }

  /** Serve `repo`, reusing the running server when it was started with the
   *  same binary, port and repo. */
  start(binary: string, repo: string, port: number): Promise<StartResult> {
    const key = [binary, normalizePort(port), repo].join("\u0000");
    if (this.proc && this.key === key && this.started) return this.started;
    this.stop();
    const previous = normalizePort(port) !== 0 ? this.retiring : undefined;
    if (!previous) return this.launch(binary, repo, port, key);

    // A fixed port stays bound until the server being replaced has exited,
    // which a graceful stop no longer makes immediate (#1119). Launch after it.
    let cancelled = false;
    const pending = previous.then(() => {
      if (cancelled) throw new StartCancelled();
      return this.launch(binary, repo, port, key);
    });
    this.key = key;
    this.repo = repo;
    this.started = pending;
    this.cancelStart = () => {
      cancelled = true;
    };
    pending.catch(() => undefined);
    return pending;
  }

  /** Spawn `travsr ui` and wait for its address. */
  private launch(binary: string, repo: string, port: number, key: string): Promise<StartResult> {
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
    this.key = key;

    let cancel: (() => void) | undefined;
    const started = new Promise<StartResult>((resolve, reject) => {
      let url: string | undefined;
      let notIndexed = false;
      let settled = false;
      let stderr = "";
      let grace: ReturnType<typeof setTimeout> | undefined;
      const timer = setTimeout(() => {
        fail(new Error(`travsr ui did not report its address within ${this.timeoutMs / 1000} s.`));
        // Only ever this start's process: by now `this.proc` may be a newer
        // server that this timer must not take down (review of #1101).
        if (this.proc === proc) this.stop();
        else proc.kill();
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
      cancel = () => fail(new StartCancelled());

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
        // Read only to explain a failed start; a server that logs for hours
        // must not grow this.
        if (!settled) stderr += line + "\n";
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
    this.cancelStart = cancel;
    // A rejected start must not leave an unhandled rejection behind when the
    // caller only awaits a later one.
    started.catch(() => undefined);
    return started;
  }

  /** Stop the server. Returns whether one was running. A start still waiting
   *  for its address settles with `StartCancelled`, not as a failure. */
  stop(): boolean {
    const proc = this.proc;
    const cancel = this.cancelStart;
    // A start waiting for the previous server on its port has no process yet.
    if (!proc && !cancel) return false;
    const wasServing = this.serving;
    this.clear();
    // Settled before the process goes, so its exit is not taken for a failed
    // start (review of #1101).
    cancel?.();
    if (proc) {
      const exited = this.end(proc);
      this.retiring = exited;
      void exited?.then(() => {
        if (this.retiring === exited) this.retiring = undefined;
      });
    }
    if (wasServing) this.runningEmitter.fire(false);
    return true;
  }

  /**
   * Stop `proc` the graceful way: close its stdin, so `travsr ui` (started
   * with TRAVSR_UI_EXIT_ON_STDIN_EOF=1) stops the page's running commands and
   * shuts down. A plain `kill()` is SIGTERM on macOS and Linux and
   * TerminateProcess on Windows, and either one used to end the server at once
   * and leave a running Terminal or `lang list` command behind (#1119).
   * Killed after STOP_FALLBACK_MS if it is still there (a travsr that predates
   * the variable). Resolves once it has exited; `undefined` when it had no
   * stdin to close and was killed outright.
   */
  private end(proc: UiProcess): Promise<void> | undefined {
    const stdin = proc.stdin;
    if (!stdin) {
      proc.kill();
      return undefined;
    }
    return new Promise<void>((resolve) => {
      let fallback: ReturnType<typeof setTimeout> | undefined;
      let giveUp: ReturnType<typeof setTimeout> | undefined;
      const done = (): void => {
        if (fallback) clearTimeout(fallback);
        if (giveUp) clearTimeout(giveUp);
        resolve();
      };
      proc.on("exit", done);
      fallback = setTimeout(() => {
        proc.kill();
        // A process that ignores even that must not hold a successor forever.
        giveUp = setTimeout(done, 2_000);
        giveUp.unref?.();
      }, this.stopFallbackMs);
      // Never keeps the extension host alive on its own.
      fallback.unref?.();
      try {
        stdin.end();
      } catch {
        proc.kill();
      }
    });
  }

  dispose(): void {
    this.stop();
    this.runningEmitter.dispose();
    this.exitEmitter.dispose();
  }

  /** Drop the state for `proc` if it is still the current process. */
  private forget(proc: UiProcess): void {
    if (this.proc !== proc) return;
    this.clear();
  }

  private clear(): void {
    this.proc = undefined;
    this.repo = undefined;
    this.key = undefined;
    this.started = undefined;
    this.cancelStart = undefined;
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
      const resolved = resolveUiBinary(cfg.get<string>("binaryPath", "") ?? "");
      if ("error" in resolved) {
        void vscode.window.showErrorMessage(`Travsr: ${resolved.error}`);
        return;
      }
      const binary = resolved.binary;
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
        // Stop, or an Open for another repo, ended this start on purpose.
        if (e instanceof StartCancelled) return;
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
