/**
 * Thin JSON-RPC-over-stdio MCP client for the Travsr daemon.
 *
 * Spawns `travsr mcp --stdio`, sends an `initialize` handshake, then
 * exposes `callTool` for the extension's status bar, code lens, and
 * hover providers. All errors are swallowed and surfaced as empty
 * strings so callers never need to handle rejection paths.
 */

import * as cp from "child_process";

export interface McpClient {
  callTool(name: string, args?: Record<string, string>): Promise<string>;
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
  private nextId = 1;
  private connected = false;
  private readonly disconnectListeners = new Set<() => void>();

  constructor(
    private readonly binary: string,
    private readonly cwd?: string,
    private readonly version: string = "0.0.0"
  ) {}

  onDisconnect(cb: () => void): { dispose(): void } {
    this.disconnectListeners.add(cb);
    return { dispose: () => this.disconnectListeners.delete(cb) };
  }

  async connect(): Promise<void> {
    this.proc = cp.spawn(this.binary, ["mcp", "--stdio"], {
      stdio: ["pipe", "pipe", "pipe"],
      cwd: this.cwd,
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

    await this.rpc("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "travsr-vscode", version: this.version },
    });
    if (!this.proc) throw new Error("travsr process exited during initialization");
    this.connected = true;
  }

  isConnected(): boolean {
    return this.connected && this.proc !== null;
  }

  async callTool(
    name: string,
    args: Record<string, string> = {}
  ): Promise<string> {
    if (!this.proc) return "";
    return this.rpc("tools/call", { name, arguments: args });
  }

  private rpc(method: string, params: unknown): Promise<string> {
    return new Promise((resolve) => {
      const id = this.nextId++;
      const line =
        JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n";
      this.pending.set(id, resolve);
      this.proc?.stdin?.write(line);
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
            resolve(msg.result?.content?.[0]?.text ?? "");
          }
        }
      } catch {
        // Malformed line — skip.
      }
    }
  }

  dispose(): void {
    this.connected = false;
    this.proc?.kill();
    this.proc = null;
  }
}
