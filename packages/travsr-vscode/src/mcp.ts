/**
 * Thin JSON-RPC-over-stdio MCP client for the Travsr daemon.
 *
 * Spawns `travsr mcp --stdio`, sends an `initialize` handshake, then
 * exposes `callTool` for the extension's status bar, code lens, and
 * hover providers. All errors are swallowed and surfaced as empty
 * strings so callers never need to handle rejection paths.
 */

import * as cp from "child_process";
import { assertExecutableBinary } from "./installer";

export interface McpClient {
  callTool(name: string, args?: Record<string, string>, signal?: AbortSignal, timeoutMs?: number): Promise<string>;
  isConnected(): boolean;
  dispose(): void;
}

interface RpcResponse {
  id?: number;
  result?: { content?: Array<{ text?: string }> };
}

export class StdioMcpClient implements McpClient {
  private proc: cp.ChildProcess | null = null;
  private buffer = "";
  private pending = new Map<number, (text: string) => void>();
  private readonly pendingTimers = new Map<number, ReturnType<typeof setTimeout>>();
  private nextId = 1;
  private connected = false;
  private readonly disconnectListeners = new Set<() => void>();

  constructor(
    private readonly binary: string,
    private readonly cwd?: string,
    private readonly version: string = "0.6.0"
  ) {}

  onDisconnect(cb: () => void): { dispose(): void } {
    this.disconnectListeners.add(cb);
    return { dispose: () => this.disconnectListeners.delete(cb) };
  }

  async connect(): Promise<void> {
    assertExecutableBinary(this.binary);
    this.proc = cp.spawn(this.binary, ["mcp", "--stdio"], {
      stdio: ["pipe", "pipe", "pipe"],
      cwd: this.cwd,
      windowsHide: true,
    });

    this.proc.stdout?.setEncoding("utf8");
    this.proc.stdout?.on("data", (chunk: string) => {
      this.buffer += chunk;
      this.flush();
    });

    const onExit = (): void => {
      const wasConnected = this.connected;
      this.connected = false;
      this.proc = null;
      for (const resolve of this.pending.values()) resolve("");
      this.pending.clear();
      if (wasConnected) {
        for (const cb of this.disconnectListeners) cb();
      }
    };
    this.proc.on("exit", onExit);
    this.proc.on("error", onExit);

    // Allow 30 s: sidecar startup (loading HNSW index) can take 15-25 s on large repos.
    await this.rpc("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "travsr-vscode", version: this.version },
    }, undefined, 30_000);
    if (!this.proc) throw new Error("travsr process exited during initialization");
    this.connected = true;
  }

  isConnected(): boolean {
    return this.connected && this.proc !== null;
  }

  async callTool(
    name: string,
    args: Record<string, string> = {},
    signal?: AbortSignal,
    timeoutMs?: number
  ): Promise<string> {
    if (!this.proc) return "";
    return this.rpc("tools/call", { name, arguments: args }, signal, timeoutMs ?? 10_000);
  }

  private rpc(method: string, params: unknown, signal?: AbortSignal, timeoutMs = 10_000): Promise<string> {
    return new Promise((resolve) => {
      if (signal?.aborted) { resolve(""); return; }

      const id = this.nextId++;
      const line =
        JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n";

      // Wrap resolve so the abort listener is removed when the response arrives first.
      const wrappedResolve = (text: string): void => {
        if (onAbort) signal?.removeEventListener("abort", onAbort);
        resolve(text);
      };

      // eslint-disable-next-line prefer-const
      let onAbort: (() => void) | undefined;
      if (signal) {
        onAbort = (): void => {
          if (!this.pending.has(id)) return;
          this.pending.delete(id);
          clearTimeout(this.pendingTimers.get(id));
          this.pendingTimers.delete(id);
          const cancel = JSON.stringify({ jsonrpc: "2.0", method: "$/cancelRequest", params: { id } }) + "\n";
          try { this.proc?.stdin?.write(cancel); } catch { /* daemon may ignore */ }
          resolve("");
        };
        signal.addEventListener("abort", onAbort, { once: true });
      }

      this.pending.set(id, wrappedResolve);
      this.proc?.stdin?.write(line);

      // Guard against a hung daemon: resolve with "" after timeoutMs so callers
      // never wait indefinitely and the pending entry is cleaned up.
      const timer = setTimeout(() => {
        if (this.pending.has(id)) {
          this.pending.delete(id);
          this.pendingTimers.delete(id);
          if (onAbort) signal?.removeEventListener("abort", onAbort);
          resolve("");
        }
      }, timeoutMs);
      this.pendingTimers.set(id, timer);
    });
  }

  private flush(): void {
    const lines = this.buffer.split("\n");
    this.buffer = lines.pop() ?? "";
    for (const line of lines) {
      if (!line.trim()) continue;
      try {
        const msg = JSON.parse(line) as RpcResponse;
        if (msg.id != null) {
          const resolve = this.pending.get(msg.id);
          if (resolve) {
            this.pending.delete(msg.id);
            clearTimeout(this.pendingTimers.get(msg.id));
            this.pendingTimers.delete(msg.id);
            resolve(msg.result?.content?.[0]?.text ?? "");
          }
        }
      } catch {
        // Try to rescue the pending entry so it doesn't leak on malformed JSON.
        const m = /"id"\s*:\s*(\d+)/.exec(line);
        if (m) {
          const id = parseInt(m[1], 10);
          const resolve = this.pending.get(id);
          if (resolve) {
            this.pending.delete(id);
            clearTimeout(this.pendingTimers.get(id));
            this.pendingTimers.delete(id);
            resolve("");
          }
        }
      }
    }
  }

  dispose(): void {
    this.connected = false;
    for (const timer of this.pendingTimers.values()) clearTimeout(timer);
    this.pendingTimers.clear();
    this.proc?.kill();
    this.proc = null;
  }
}
