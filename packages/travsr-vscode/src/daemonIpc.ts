/**
 * Minimal client for the daemon's control socket (#688).
 *
 * The extension is otherwise a pure MCP client. This exists for one reason:
 * the daemon owns `.travsr/daemon.log.<DATE>`, and the diagnostics overlay is
 * computed here, in the extension host. Reporting it means reaching the daemon.
 *
 * Fire-and-forget by design. Every failure path — no daemon, stale socket,
 * daemon too old to parse the message, slow write — resolves to `false` and is
 * never surfaced. A debug report that cannot be delivered is not worth one
 * word of a user's attention, and must never delay or break a render.
 *
 * ## Why discovery is not a single path
 *
 * The socket name is `blake3(canonical_repo_path)[..8]` in hex
 * (`travsr-ipc/src/addr.rs`), which the extension cannot recompute without a
 * blake3 implementation. Two things follow:
 *
 * 1. We glob rather than derive. `.travsr/` routinely holds more than one
 *    `daemon-*.sock` because a repo that has moved leaves its old socket file
 *    behind, so the glob is a candidate list, not an answer. A stale socket
 *    file refuses the connection, which is exactly how we tell them apart:
 *    try each, first one that answers wins.
 * 2. The socket is not always in `.travsr` at all. Unix caps socket paths at
 *    `sun_path` (104 bytes on macOS/BSD, 108 on Linux), so a deep repo pushes
 *    the daemon to a short per-user runtime directory instead (travsr #592).
 *    Those bases are searched after the in-repo one, in the same preference
 *    order the daemon uses, or a deep checkout would silently report nothing.
 */

import * as fs from "fs";
import * as net from "net";
import * as os from "os";
import * as path from "path";

/** Per-attempt budget. A report is never worth stalling a render on. */
const CONNECT_TIMEOUT_MS = 250;

/**
 * How long the daemon should believe this window's report.
 *
 * Long enough that an editor sitting idle with an unchanged view is not
 * repeatedly declared gone, short enough that a window killed without a chance
 * to detach stops being quoted within the hour.
 */
export const REPORT_TTL_SECS = 900;

/**
 * Identity for this editor window, stable for as long as the extension host
 * lives, which is exactly one window. Two windows on one repo must not
 * overwrite each other's view, and the daemon cannot tell them apart unless
 * they say who they are.
 */
export const SESSION_ID = `vscode-${process.pid}-${Date.now().toString(36)}`;

export interface FileDiagnostics {
  /** Repo-relative, forward slashes, matching the graph's own path keys. */
  path: string;
  errors: number;
  warnings: number;
}

export interface LspDiagnosticsReport {
  /** Only files with something wrong. Clean files are absent. */
  files: FileDiagnostics[];
  /** Distinct files examined. */
  seen: number;
  /** Of `seen`, how many no provider reported on. */
  undiagnosed: number;
}

/**
 * Candidate control sockets for `repoRoot`, best-first.
 *
 * Exported for tests: the ordering is the contract, since the first candidate
 * that accepts a connection wins and an in-repo socket must be preferred over
 * a runtime-directory one.
 */
export function candidateSocketPaths(repoRoot: string): string[] {
  if (process.platform === "win32") {
    // Named pipes are not files, but the pipe filesystem is enumerable.
    const pipeDir = "\\\\.\\pipe\\";
    try {
      return fs
        .readdirSync(pipeDir)
        .filter((n) => n.startsWith("travsr-"))
        .map((n) => pipeDir + n);
    } catch {
      return [];
    }
  }

  const found: string[] = [];
  const scan = (dir: string): void => {
    try {
      for (const name of fs.readdirSync(dir)) {
        if (name.startsWith("daemon-") && name.endsWith(".sock")) {
          found.push(path.join(dir, name));
        }
      }
    } catch {
      // Missing or unreadable directory is a normal miss, not an error.
    }
  };

  scan(path.join(repoRoot, ".travsr"));

  // #592 fallback bases, in the daemon's own preference order.
  const uid = typeof process.getuid === "function" ? process.getuid() : 0;
  const leaf = `travsr-${uid}`;
  for (const base of [
    process.env["XDG_RUNTIME_DIR"],
    process.env["TMPDIR"],
    os.tmpdir(),
    "/tmp",
  ]) {
    if (base) scan(path.join(base, leaf));
  }

  return [...new Set(found)];
}

/** Write one control line to `socketPath`, resolving false on any failure. */
function sendLine(socketPath: string, line: string): Promise<boolean> {
  return new Promise((resolve) => {
    let settled = false;
    const done = (ok: boolean): void => {
      if (settled) return;
      settled = true;
      sock.destroy();
      resolve(ok);
    };

    const sock = net.connect(socketPath);
    sock.setTimeout(CONNECT_TIMEOUT_MS);
    sock.on("connect", () => sock.write(line, () => done(true)));
    // A stale socket file refuses; that is the signal to try the next candidate.
    sock.on("error", () => done(false));
    sock.on("timeout", () => done(false));
  });
}

/** Send one control message to whichever candidate socket answers first. */
async function send(repoRoot: string, payload: object): Promise<boolean> {
  const line = JSON.stringify(payload) + "\n";
  for (const candidate of candidateSocketPaths(repoRoot)) {
    if (await sendLine(candidate, line)) return true;
  }
  return false;
}

/**
 * Publish this window's view of what is currently broken, under a lease.
 *
 * Resolves true if some daemon accepted it, false in every other case, and
 * never rejects. A daemon that is stopped, older, or unreachable is a normal
 * state for an editor, not something to tell the user about.
 */
export async function reportLspDiagnostics(
  repoRoot: string,
  report: LspDiagnosticsReport
): Promise<boolean> {
  return send(repoRoot, {
    op: "report-lsp-diagnostics",
    session: SESSION_ID,
    ttl_secs: REPORT_TTL_SECS,
    ...report,
  });
}

/**
 * Drop this window's view now, rather than leaving it to expire.
 *
 * Without this a closed panel keeps asserting what it last saw for the rest of
 * the lease, which is the difference between a stale answer and no answer.
 */
export async function detachSession(repoRoot: string): Promise<boolean> {
  return send(repoRoot, {
    op: "report-lsp-diagnostics",
    session: SESSION_ID,
    ttl_secs: 0,
    files: [],
    seen: 0,
    undiagnosed: 0,
  });
}
