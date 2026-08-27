/**
 * Tests for the daemon control-socket client (#688).
 *
 * The valuable properties here are the failure ones. This client runs on every
 * graph render, so "never throws, never hangs, never bothers the user" matters
 * more than the happy path, and those are the cases a real daemon cannot
 * easily be made to produce on demand.
 */

import * as assert from "assert";
import * as fs from "fs";
import * as net from "net";
import * as os from "os";
import * as path from "path";
import {
  candidateSocketPaths,
  detachSession,
  reportLiveResolution,
  reportLspDiagnostics,
  SESSION_ID,
} from "../../daemonIpc";

const REPORT = {
  files: [{ path: "src/a.ts", errors: 2, warnings: 1 }],
  seen: 3,
  undiagnosed: 0,
};

function tempRepo(): string {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "travsr-ipc-test-"));
  fs.mkdirSync(path.join(root, ".travsr"));
  return root;
}

suite("daemonIpc: socket discovery (#688)", function () {
  test("finds every daemon-*.sock in .travsr", () => {
    const root = tempRepo();
    fs.writeFileSync(path.join(root, ".travsr", "daemon-aaaa.sock"), "");
    fs.writeFileSync(path.join(root, ".travsr", "daemon-bbbb.sock"), "");

    const found = candidateSocketPaths(root);

    if (process.platform === "win32") return; // named pipes, not files
    assert.strictEqual(found.filter((p) => p.startsWith(root)).length, 2);
  });

  test("ignores files that are not daemon sockets", () => {
    const root = tempRepo();
    for (const name of ["daemon.lock", "graph.db", "daemon.log.2026-08-14", "init.lock"]) {
      fs.writeFileSync(path.join(root, ".travsr", name), "");
    }

    const found = candidateSocketPaths(root).filter((p) => p.startsWith(root));

    assert.deepStrictEqual(found, [], `must not treat these as sockets: ${found}`);
  });

  test("the in-repo socket is preferred over a runtime-directory one", () => {
    if (process.platform === "win32") return;
    const root = tempRepo();
    fs.writeFileSync(path.join(root, ".travsr", "daemon-aaaa.sock"), "");

    const found = candidateSocketPaths(root);

    assert.ok(found.length > 0);
    assert.ok(
      found[0].startsWith(root),
      `#592 fallback must come after the in-repo socket, got ${found[0]}`
    );
  });

  test("a repo with no .travsr yields no candidates and does not throw", () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "travsr-ipc-empty-"));
    const found = candidateSocketPaths(root).filter((p) => p.startsWith(root));
    assert.deepStrictEqual(found, []);
  });
});

suite("daemonIpc: reporting (#688)", function () {
  // Real sockets, so a hang shows up as a failure rather than as a pass.
  this.timeout(10_000);

  test("delivers the report to a listening daemon", async () => {
    if (process.platform === "win32") return;
    const root = tempRepo();
    const sockPath = path.join(root, ".travsr", "daemon-live.sock");

    const received: string[] = [];
    const server = net.createServer((c) => {
      c.on("data", (b) => received.push(b.toString("utf8")));
    });
    await new Promise<void>((r) => server.listen(sockPath, r));

    try {
      const ok = await reportLspDiagnostics(root, REPORT);
      assert.strictEqual(ok, true, "a listening daemon must accept the report");
      await new Promise((r) => setTimeout(r, 100));
      const line = received.join("");
      assert.ok(line.endsWith("\n"), "control protocol is line-delimited");
      const parsed = JSON.parse(line);
      assert.strictEqual(parsed.op, "report-lsp-diagnostics");
      assert.strictEqual(parsed.session, SESSION_ID, "a report must say which window it is from");
      assert.ok(parsed.ttl_secs > 0, "a report must carry a lease");
      assert.deepStrictEqual(
        { files: parsed.files, seen: parsed.seen, undiagnosed: parsed.undiagnosed },
        REPORT
      );
    } finally {
      server.close();
    }
  });

  test("a stale socket file is skipped and the live one still wins", async () => {
    if (process.platform === "win32") return;
    const root = tempRepo();
    // A leftover socket file with nothing listening: exactly what a moved repo
    // leaves behind, and the reason discovery cannot just take the first hit.
    fs.writeFileSync(path.join(root, ".travsr", "daemon-0000stale.sock"), "");
    const livePath = path.join(root, ".travsr", "daemon-zzzzlive.sock");

    let got = "";
    const server = net.createServer((c) => {
      c.on("data", (b) => (got += b.toString("utf8")));
    });
    await new Promise<void>((r) => server.listen(livePath, r));

    try {
      const ok = await reportLspDiagnostics(root, REPORT);
      assert.strictEqual(ok, true, "must fall through the stale socket to the live one");
      await new Promise((r) => setTimeout(r, 100));
      assert.ok(got.includes("report-lsp-diagnostics"));
    } finally {
      server.close();
    }
  });

  test("detach sends a zero lease under the same session", async () => {
    if (process.platform === "win32") return;
    const root = tempRepo();
    const sockPath = path.join(root, ".travsr", "daemon-detach.sock");

    let got = "";
    const server = net.createServer((c) => {
      c.on("data", (b) => (got += b.toString("utf8")));
    });
    await new Promise<void>((r) => server.listen(sockPath, r));

    try {
      await detachSession(root);
      await new Promise((r) => setTimeout(r, 100));
      const parsed = JSON.parse(got);
      // Zero is what makes it a detach rather than a report, and the session
      // has to match or the daemon would drop some other window's view.
      assert.strictEqual(parsed.ttl_secs, 0);
      assert.strictEqual(parsed.session, SESSION_ID);
    } finally {
      server.close();
    }
  });

  test("no daemon at all resolves false rather than throwing", async () => {
    const root = tempRepo();
    const ok = await reportLspDiagnostics(root, REPORT);
    assert.strictEqual(ok, false, "a missing daemon is a normal state, not an error");
  });

  test("a socket that accepts but never replies does not hang the caller", async () => {
    if (process.platform === "win32") return;
    const root = tempRepo();
    const sockPath = path.join(root, ".travsr", "daemon-silent.sock");
    // Accept the connection and then say nothing, ever. A client that waits for
    // a response would block the render path here.
    const server = net.createServer(() => undefined);
    await new Promise<void>((r) => server.listen(sockPath, r));

    try {
      const started = Date.now();
      await reportLspDiagnostics(root, REPORT);
      assert.ok(
        Date.now() - started < 5_000,
        "must not wait on a reply it does not need"
      );
    } finally {
      server.close();
    }
  });
});

suite("daemonIpc: #698 review P1", () => {
  // Discovery enumerates a namespace, not a repo, so delivery cannot be
  // "first to accept". Every report names the repo it is for and the daemon
  // drops foreign ones; broadcasting is what makes that correct.
  test("every report carries the repo root it describes", () => {
    const src = fs.readFileSync(
      path.join(__dirname, "..", "..", "daemonIpc.js"),
      "utf8"
    );
    // Counts every `op:` this client can send, not one hardcoded op, so a new
    // message type is covered the day it is added rather than silently
    // exempted. RFC-027's report-live-resolution is why this generalized.
    const ops = src.split(/\bop:\s*"/).length - 1;
    const roots = src.split("repo_root:").length - 1;

    assert.ok(ops >= 3, `expected report, detach and live-resolution ops, saw ${ops}`);
    assert.strictEqual(
      roots,
      ops,
      "every send must name its repo, detach and live resolution included"
    );
  });

  test("delivery goes to every candidate, not the first that accepts", () => {
    const src = fs.readFileSync(
      path.join(__dirname, "..", "..", "daemonIpc.js"),
      "utf8"
    );
    const fn = src.indexOf("async function send(");
    const body = src.slice(fn, src.indexOf("\n}", fn));

    assert.ok(
      !/return true;/.test(body),
      "short-circuiting on the first accept is what misrouted reports"
    );
    assert.ok(
      body.includes("Promise.all"),
      "candidates are tried concurrently, or each pays a connect timeout"
    );
  });
});

suite("daemonIpc: live resolution (RFC-027)", function () {
  // Real sockets, so a hang shows up as a failure rather than as a pass.
  this.timeout(10_000);

  const RESOLUTIONS = [
    {
      ref_line: 42,
      ref_col: 8,
      name: "save",
      target_path: "src/user.ts",
      target_line: 17,
      buffer_version: 9,
    },
  ];

  // The daemon parses this line with a hand-written serde contract test
  // (travsr-ipc/src/message.rs). Neither side shares a serializer, so the wire
  // tag and field names are the contract and both tests must agree.
  test("sends the wire shape the daemon parses", async () => {
    if (process.platform === "win32") return;
    const root = tempRepo();
    const sockPath = path.join(root, ".travsr", "daemon-live-res.sock");

    const received: string[] = [];
    const server = net.createServer((c) => {
      c.on("data", (b) => received.push(b.toString("utf8")));
    });
    await new Promise<void>((r) => server.listen(sockPath, r));

    try {
      const ok = await reportLiveResolution(root, "src/order.ts", RESOLUTIONS);
      assert.strictEqual(ok, true, "a listening daemon must accept the report");
      await new Promise((r) => setTimeout(r, 100));

      const line = received.join("");
      assert.ok(line.endsWith("\n"), "control protocol is line-delimited");
      const parsed = JSON.parse(line);
      assert.strictEqual(parsed.op, "report-live-resolution");
      assert.strictEqual(parsed.repo_root, root, "the daemon drops a report for another repo");
      assert.strictEqual(parsed.session, SESSION_ID);
      assert.strictEqual(parsed.file, "src/order.ts");
      assert.deepStrictEqual(parsed.resolutions, RESOLUTIONS);
      // A live edge's lifetime is bounded by commit ratification, not a lease,
      // so a ttl here would be a second expiry mechanism with no consumer.
      assert.strictEqual(parsed.ttl_secs, undefined, "live reports carry no lease");
    } finally {
      server.close();
    }
  });

  // The property that matters is "never rejects". The boolean is deliberately
  // not asserted: discovery enumerates a per-user namespace, so an unrelated
  // daemon on the same machine can accept the bytes (and then drop the report
  // by repo), which would make an assertion on it pass or fail with the
  // developer's environment rather than with the code.
  test("a missing daemon is silent, never a throw", async () => {
    const root = tempRepo();
    const ok = await reportLiveResolution(root, "src/order.ts", RESOLUTIONS);
    assert.strictEqual(typeof ok, "boolean", "must resolve, never reject");
  });
});
