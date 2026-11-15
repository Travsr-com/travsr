import * as assert from "assert";
import * as fs from "fs";
import * as http from "http";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import { EventEmitter } from "events";
import { PassThrough } from "stream";
import { execFileSync } from "child_process";
import {
  BrowserUiServer,
  SpawnUi,
  UiProcess,
  explainStartFailure,
  isNotIndexedLine,
  normalizePort,
  parseServingUrl,
  spawnUiProcess,
  uiArgs,
} from "../../browserUi";

/** A child process the test drives by hand. */
class FakeProc extends EventEmitter implements UiProcess {
  readonly stdout = new PassThrough();
  readonly stderr = new PassThrough();
  killed = false;
  kill(): boolean {
    this.killed = true;
    setImmediate(() => this.emit("exit", null));
    return true;
  }
  /** Write stdout lines the way `travsr ui` prints them. */
  say(...lines: string[]): void {
    this.stdout.write(lines.map((l) => l + "\n").join(""));
  }
}

interface Spawned {
  binary: string;
  args: string[];
  cwd: string;
  proc: FakeProc;
}

function fakeSpawner(): { spawn: SpawnUi; calls: Spawned[] } {
  const calls: Spawned[] = [];
  const spawn: SpawnUi = (binary, args, cwd) => {
    const proc = new FakeProc();
    calls.push({ binary, args, cwd, proc });
    return proc;
  };
  return { spawn, calls };
}

const tick = (): Promise<void> => new Promise((r) => setImmediate(r));

suite("#1060 browser UI: output parsing", () => {
  test("parseServingUrl reads the address from the serving line only", () => {
    assert.strictEqual(
      parseServingUrl("travsr ui: serving http://localhost:7330/"),
      "http://localhost:7330/"
    );
    assert.strictEqual(
      parseServingUrl("travsr ui: serving http://[::1]:51234/\r"),
      "http://[::1]:51234/"
    );
    assert.strictEqual(parseServingUrl("travsr ui: stopped"), null);
    assert.strictEqual(parseServingUrl("repository: /r"), null);
    assert.strictEqual(parseServingUrl("travsr ui: serving ftp://x/"), null);
  });

  test("isNotIndexedLine spots the CLI's not-indexed repository line", () => {
    assert.ok(isNotIndexedLine("repository: /r (not indexed yet; run `travsr init` there)"));
    assert.ok(!isNotIndexedLine("repository: /r"));
    assert.ok(!isNotIndexedLine("not inside a repository; pick an indexed repo in the page"));
  });

  test("normalizePort keeps a valid port and falls back to 0 (a free port)", () => {
    assert.strictEqual(normalizePort(7330), 7330);
    assert.strictEqual(normalizePort(0), 0);
    for (const bad of [-1, 65536, 1.5, "7330", undefined, null, NaN]) {
      assert.strictEqual(normalizePort(bad), 0, String(bad));
    }
    assert.deepStrictEqual(uiArgs(0), ["ui", "--port", "0"]);
    assert.deepStrictEqual(uiArgs(99999), ["ui", "--port", "0"]);
  });

  test("explainStartFailure names an old binary, else quotes the CLI's error", () => {
    assert.match(
      explainStartFailure("error: unrecognized subcommand 'ui'\n\nUsage: travsr <COMMAND>\n", 2),
      /no `ui` command\. Update travsr to v1\.4\.0/
    );
    assert.strictEqual(
      explainStartFailure(
        "error: port 7330 is already in use; pass --port <other> to pick another\n",
        1
      ),
      "port 7330 is already in use; pass --port <other> to pick another"
    );
    assert.match(explainStartFailure("", 3), /exited with code 3/);
    assert.match(explainStartFailure("", null), /stopped before/);
  });
});

suite("#1060 browser UI: server lifecycle", () => {
  test("start runs `travsr ui --port N` in the repo and resolves with the address", async () => {
    const { spawn, calls } = fakeSpawner();
    const server = new BrowserUiServer(spawn);
    const running: boolean[] = [];
    server.onDidChangeRunning((r) => running.push(r));

    const started = server.start("/bin/travsr", "/repo/a", 0);
    assert.strictEqual(calls.length, 1);
    assert.deepStrictEqual(
      [calls[0].binary, calls[0].args, calls[0].cwd],
      ["/bin/travsr", ["ui", "--port", "0"], "/repo/a"]
    );
    calls[0].proc.say(
      "travsr ui: serving http://localhost:51234/",
      "repository: /repo/a",
      "press Ctrl-C to stop"
    );
    assert.deepStrictEqual(await started, {
      url: "http://localhost:51234/",
      notIndexed: false,
    });
    assert.ok(server.running);
    assert.strictEqual(server.repoRoot, "/repo/a");
    assert.deepStrictEqual(running, [true]);
    server.dispose();
  });

  test("a line split across chunks is still read", async () => {
    const { spawn, calls } = fakeSpawner();
    const server = new BrowserUiServer(spawn);
    const started = server.start("travsr", "/r", 0);
    calls[0].proc.stdout.write("travsr ui: serv");
    calls[0].proc.stdout.write("ing http://localhost:1/\r\nrepository: /r\r\npress Ctrl-C");
    calls[0].proc.stdout.write(" to stop\r\n");
    assert.strictEqual((await started).url, "http://localhost:1/");
    server.dispose();
  });

  test("a repository with no graph is reported as not indexed", async () => {
    const { spawn, calls } = fakeSpawner();
    const server = new BrowserUiServer(spawn);
    const started = server.start("travsr", "/r", 0);
    calls[0].proc.say(
      "travsr ui: serving http://localhost:2/",
      "repository: /r (not indexed yet; run `travsr init` there)",
      "press Ctrl-C to stop"
    );
    assert.strictEqual((await started).notIndexed, true);
    server.dispose();
  });

  test("the address alone resolves after the grace period", async () => {
    const { spawn, calls } = fakeSpawner();
    const server = new BrowserUiServer(spawn, undefined, 5_000, 20);
    const started = server.start("travsr", "/r", 0);
    calls[0].proc.say("travsr ui: serving http://localhost:3/");
    assert.strictEqual((await started).url, "http://localhost:3/");
    server.dispose();
  });

  test("the same repo reuses the server; another repo replaces it", async () => {
    const { spawn, calls } = fakeSpawner();
    const server = new BrowserUiServer(spawn);
    const serve = (i: number, url: string): void =>
      calls[i].proc.say(`travsr ui: serving ${url}`, "repository: x", "press Ctrl-C to stop");

    const first = server.start("travsr", "/repo/a", 0);
    serve(0, "http://localhost:10/");
    await first;
    const again = await server.start("travsr", "/repo/a", 0);
    assert.strictEqual(calls.length, 1, "same repo must not spawn a second server");
    assert.strictEqual(again.url, "http://localhost:10/");

    const exits: Array<number | null> = [];
    server.onUnexpectedExit((c) => exits.push(c));
    const other = server.start("travsr", "/repo/b", 0);
    assert.strictEqual(calls.length, 2);
    assert.ok(calls[0].proc.killed, "the server for the old repo is stopped");
    serve(1, "http://localhost:11/");
    assert.strictEqual((await other).url, "http://localhost:11/");
    await tick();
    assert.deepStrictEqual(exits, [], "replacing a server is not an unexpected exit");
    assert.strictEqual(server.repoRoot, "/repo/b");
    server.dispose();
  });

  test("an exit before the address rejects with the CLI's own error", async () => {
    const { spawn, calls } = fakeSpawner();
    const server = new BrowserUiServer(spawn);
    const started = server.start("travsr", "/r", 7330);
    calls[0].proc.stderr.write(
      "error: port 7330 is already in use; pass --port <other> to pick another\n"
    );
    await tick();
    calls[0].proc.emit("exit", 1);
    await assert.rejects(started, /port 7330 is already in use/);
    assert.ok(!server.running);
    server.dispose();
  });

  test("a binary that cannot be spawned rejects with the fix", async () => {
    const { spawn, calls } = fakeSpawner();
    const server = new BrowserUiServer(spawn);
    const started = server.start("nope-travsr", "/r", 0);
    calls[0].proc.emit("error", new Error("spawn nope-travsr ENOENT"));
    await assert.rejects(started, /Cannot run nope-travsr: spawn nope-travsr ENOENT\. Set travsr\.binaryPath/);
    assert.ok(!server.running);
    server.dispose();
  });

  test("no address within the timeout rejects and stops the process", async () => {
    const { spawn, calls } = fakeSpawner();
    const server = new BrowserUiServer(spawn, undefined, 30);
    const started = server.start("travsr", "/r", 0);
    await assert.rejects(started, /did not report its address/);
    assert.ok(calls[0].proc.killed);
    assert.ok(!server.running);
    server.dispose();
  });

  test("stop ends the server without an unexpected-exit warning", async () => {
    const { spawn, calls } = fakeSpawner();
    const server = new BrowserUiServer(spawn);
    const running: boolean[] = [];
    const exits: Array<number | null> = [];
    server.onDidChangeRunning((r) => running.push(r));
    server.onUnexpectedExit((c) => exits.push(c));
    const started = server.start("travsr", "/r", 0);
    calls[0].proc.say("travsr ui: serving http://localhost:4/", "press Ctrl-C to stop");
    await started;

    assert.strictEqual(server.stop(), true);
    assert.ok(calls[0].proc.killed);
    await tick();
    assert.deepStrictEqual(running, [true, false]);
    assert.deepStrictEqual(exits, []);
    assert.strictEqual(server.stop(), false, "nothing left to stop");
    server.dispose();
  });

  test("a server that dies while serving fires onUnexpectedExit", async () => {
    const { spawn, calls } = fakeSpawner();
    const server = new BrowserUiServer(spawn);
    const running: boolean[] = [];
    const exits: Array<number | null> = [];
    server.onDidChangeRunning((r) => running.push(r));
    server.onUnexpectedExit((c) => exits.push(c));
    const started = server.start("travsr", "/r", 0);
    calls[0].proc.say("travsr ui: serving http://localhost:5/", "press Ctrl-C to stop");
    await started;

    calls[0].proc.emit("exit", 101);
    assert.deepStrictEqual(running, [true, false]);
    assert.deepStrictEqual(exits, [101]);
    assert.ok(!server.running);
    server.dispose();
  });

  test("dispose stops the server (the extension deactivating)", async () => {
    const { spawn, calls } = fakeSpawner();
    const server = new BrowserUiServer(spawn);
    const started = server.start("travsr", "/r", 0);
    calls[0].proc.say("travsr ui: serving http://localhost:6/", "press Ctrl-C to stop");
    await started;
    server.dispose();
    assert.ok(calls[0].proc.killed);
  });
});

suite("#1060 browser UI: contributed and registered", () => {
  test("both commands are contributed, registered, and Stop is shown only while running", async () => {
    const ext = vscode.extensions.getExtension("travsr.travsr-vscode");
    assert.ok(ext, "extension must be discoverable in the test host");
    await ext!.activate();
    const registered = await vscode.commands.getCommands(true);
    const pkg = ext!.packageJSON as {
      contributes: {
        commands: Array<{ command: string; title: string }>;
        menus: Record<string, Array<{ command: string; when?: string }>>;
        configuration: { properties: Record<string, { default?: unknown }> };
      };
    };
    for (const id of ["travsr.openBrowserUi", "travsr.stopBrowserUi"]) {
      assert.ok(registered.includes(id), `${id} must be registered`);
      assert.ok(
        pkg.contributes.commands.some((c) => c.command === id),
        `${id} must be contributed`
      );
    }
    const palette = pkg.contributes.menus.commandPalette ?? [];
    assert.deepStrictEqual(
      palette.find((m) => m.command === "travsr.stopBrowserUi")?.when,
      "travsr.browserUiRunning"
    );
    assert.strictEqual(pkg.contributes.configuration.properties["travsr.ui.port"]?.default, 0);
    // Nothing is running in the test host, so Stop just says so.
    await vscode.commands.executeCommand("travsr.stopBrowserUi");
  });
});

/**
 * Against a real `travsr ui`: set TRAVSR_UI_E2E_BINARY to a travsr binary
 * that has the `ui` command. Skipped otherwise (CI has no binary).
 */
suite("#1060 browser UI: real travsr ui (TRAVSR_UI_E2E_BINARY)", function () {
  this.timeout(30_000);
  const binary = process.env.TRAVSR_UI_E2E_BINARY;

  const get = (url: string): Promise<{ status: number; body: string }> =>
    new Promise((resolve, reject) => {
      http
        .get(url, (res) => {
          let body = "";
          res.on("data", (d) => (body += d));
          res.on("end", () => resolve({ status: res.statusCode ?? 0, body }));
        })
        .on("error", reject);
    });

  test("serves the page on a free port, flags a fresh repo, and stops", async function () {
    if (!binary) this.skip();
    const repo = fs.mkdtempSync(path.join(os.tmpdir(), "travsr-ui-e2e-"));
    execFileSync("git", ["init", "-q"], { cwd: repo });
    const server = new BrowserUiServer(spawnUiProcess);
    try {
      const { url, notIndexed } = await server.start(binary as string, repo, 0);
      assert.match(url, /^http:\/\/(localhost|\[::1\]):\d+\/$/);
      assert.ok(!url.endsWith(":0/"), "port 0 must be replaced by the bound port");
      assert.strictEqual(notIndexed, true, "a repo with no graph.db is flagged");
      const page = await get(url);
      assert.strictEqual(page.status, 200);
      assert.match(page.body, /<html/i);

      const stopped = new Promise<void>((r) => server.onDidChangeRunning((x) => !x && r()));
      assert.ok(server.stop());
      await stopped;
      await new Promise((r) => setTimeout(r, 500));
      await assert.rejects(get(url), "the server must be gone after stop");
    } finally {
      server.dispose();
      fs.rmSync(repo, { recursive: true, force: true });
    }
  });
});
